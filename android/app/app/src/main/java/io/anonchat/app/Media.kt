package io.anonchat.app

import org.json.JSONObject
import kotlin.math.max
import kotlin.math.roundToInt

/**
 * 图片/视频消息里与平台无关的那部分：类型判定、缩放尺寸、气泡大小、摘要 JSON。
 *
 * 和 [Voice] 同样的理由抽出来：BitmapFactory / MediaMetadataRetriever 在 JVM 单测里
 * 只会抛 "not mocked"，留在这两个类里就永远测不到。这里只管算术和字符串，
 * 于是"竖图别被压扁""9:16 别把气泡撑出屏幕""解析坏 JSON 不崩"这些都能被用例盯住。
 */
object Media {

    /**
     * 图片长边上限。
     *
     * 1600px 是权衡：朋友圈/聊天里看图的屏幕宽度通常 1080，1600 已经够放大看清细节，
     * 而一张 12MP 的照片（4032×3024）压到这尺寸大约 300KB —— 不压的话 4MB 起，
     * 20MB 的通道传五张就爆。原图需求属于另一个功能（"发送原图"），不该是默认行为。
     */
    const val IMAGE_MAX_EDGE = 1600

    /** JPEG 质量。80 在手机上肉眼几乎看不出差别，体积约为 95 的一半 */
    const val JPEG_QUALITY = 80

    /** 压缩后仍然超过这个大小就不发（正常 1600px 的图不会有 12MB） */
    const val IMAGE_MAX_BYTES = 12L * 1024 * 1024

    /**
     * 原图直传的体积门槛：JPEG 已经小于这个、长边也不超 1600、EXIF 方向正常，
     * 就原样发，不重编码。
     *
     * 800KB 而不是 [IMAGE_MAX_BYTES]：既然尺寸已经合格，再往上重编码几乎只剩坏处
     * （画质掉一档、体积可能还变大），但"本来就只有几十 KB 的图"重压一遍是纯亏。
     * 门槛取 800KB 的理由是——这类图重编码后确实能小一些，值得压。
     */
    const val PASSTHROUGH_MAX_BYTES = 800L * 1024

    /** ExifInterface.ORIENTATION_NORMAL 的值。抽出来是为了单测不碰 android.media */
    const val ORIENT_NORMAL = 1

    /**
     * 这张图能不能免压缩直传。
     *
     * 判断全在这里，[ImageCompress] 只负责执行——这样"到底什么情况才不压"
     * 能被单测钉住，而不是散在带 BitmapFactory 的代码里永远测不到。
     */
    fun canPassThrough(
        mime: String,
        name: String,
        w: Int,
        h: Int,
        size: Long,
        orient: Int,
    ): Boolean {
        val isJpeg = mime == "image/jpeg" || mime == "image/jpg" || extOf(name) in setOf("jpg", "jpeg")
        if (!isJpeg) return false
        if (w <= 0 || h <= 0) return false
        if (max(w, h) > IMAGE_MAX_EDGE) return false
        if (size !in 1..PASSTHROUGH_MAX_BYTES) return false
        return orient == ORIENT_NORMAL
    }

    private val IMAGE_EXT = setOf("jpg", "jpeg", "png", "webp", "gif", "bmp", "heic", "heif", "avif")
    private val VIDEO_EXT = setOf("mp4", "m4v", "mov", "3gp", "3gpp", "mkv", "webm", "avi")

    fun extOf(name: String): String = name.substringAfterLast('.', "").lowercase()

    /** 按扩展名猜消息类型。猜不出就是普通文件 */
    fun kindOfName(name: String): String = when (extOf(name)) {
        in IMAGE_EXT -> "image"
        in VIDEO_EXT -> "video"
        else -> "file"
    }

    fun isImageName(name: String): Boolean = kindOfName(name) == "image"
    fun isVideoName(name: String): Boolean = kindOfName(name) == "video"

    /** 压缩时用的文件名：统一落成 .jpg（GIF 除外，压成 JPEG 会把动画压没） */
    fun compressedName(name: String): String {
        val base = name.substringBeforeLast('.', name).ifBlank { "image" }
        return "$base.jpg"
    }

    /**
     * 把 [w]×[h] 等比缩到长边不超过 [maxEdge]。已经够小就原样返回。
     * 只缩不放：把 200px 的图放大到 1600 只会多占体积，一个像素的信息都没多。
     */
    fun fitWithin(w: Int, h: Int, maxEdge: Int): Pair<Int, Int> {
        if (w <= 0 || h <= 0 || maxEdge <= 0) return w to h
        val longest = max(w, h)
        if (longest <= maxEdge) return w to h
        val s = maxEdge.toFloat() / longest
        return (w * s).roundToInt().coerceAtLeast(1) to (h * s).roundToInt().coerceAtLeast(1)
    }

    /**
     * 解码时的采样倍数（2 的幂）。
     *
     * BitmapFactory 只认 inSampleSize，直接按原图解码 4032×3024 是 48MB 内存，
     * 低端机当场 OOM。这里按"缩完仍不小于目标长边"取，剩下的差额再用 createScaledBitmap 补，
     * 这样既省内存又不至于先缩过头再放大（那样会糊）。
     */
    fun inSampleSize(w: Int, h: Int, maxEdge: Int): Int {
        if (w <= 0 || h <= 0 || maxEdge <= 0) return 1
        var s = 1
        while (max(w, h) / (s * 2) >= maxEdge) s *= 2
        return s
    }

    /**
     * 气泡里的显示尺寸（dp）。
     *
     * 竖图、横图、全景图都要在 [maxW]×[maxH] 这个框里保持原比例，
     * 否则用户发的照片会被裁成一样大的方块（微信的图墙能这么干，聊天气泡不能）。
     * 极端长宽比（截图、全景）会被夹到最小 72dp，免得出现一根细条。
     */
    fun bubbleDims(maxW: Float, maxH: Float, srcW: Int, srcH: Int): Pair<Float, Float> {
        if (srcW <= 0 || srcH <= 0) {
            // 老版本发的消息没有宽高：按 4:3 给个稳妥的框，图会按 Crop 铺满
            return maxW to (maxW * 0.75f).coerceAtMost(maxH)
        }
        val r = srcW.toFloat() / srcH
        var w = maxW
        var h = w / r
        if (h > maxH) {
            h = maxH
            w = h * r
        }
        if (w < 72f) {
            w = 72f
            h = (w / r).coerceAtMost(maxH)
        }
        if (h < 72f) {
            h = 72f
            w = (h * r).coerceAtMost(maxW)
        }
        return w to h
    }

    /** 视频角标上的时长："0:12"、"1:03" */
    fun durLabel(ms: Long): String {
        val total = (ms.coerceAtLeast(0) / 1000).toInt()
        return "%d:%02d".format(total / 60, total % 60)
    }

    /** 纯数字的字节格式化（MainActivity 里那个是 file-private，这里也要用） */
    fun sizeLabel(n: Long): String = when {
        n >= 1024 * 1024 -> "%.1f MB".format(n / 1024f / 1024f)
        n >= 1024 -> "%.1f KB".format(n / 1024f)
        else -> "$n B"
    }

    /**
     * 视频超限时要告诉用户的两件事：本条多大、通道能传多大。
     *
     * 只给数字不给句子——文案在 UI 层（R.string.media_too_big），随语言变。
     */
    fun tooBigInfo(size: Long, limit: Long): Pair<String, Int> =
        sizeLabel(size) to (limit / 1024 / 1024).toInt()

    /** 图片/视频消息的摘要 JSON（聊天记录里 text 字段存的就是它） */
    fun summary(
        fid: String,
        name: String,
        size: Long,
        path: String?,
        saved: String?,
        w: Int,
        h: Int,
        durMs: Long = 0,
    ): JSONObject {
        val o = JSONObject()
            .put("fid", fid)
            .put("name", name)
            .put("size", size)
        if (w > 0 && h > 0) {
            o.put("w", w).put("h", h)
        }
        if (durMs > 0) o.put("dur", durMs)
        if (path != null) o.put("path", path)
        if (saved != null) o.put("saved", saved)
        return o
    }

    data class Meta(
        val fid: String,
        val name: String,
        val size: Long,
        /** 本机副本路径。null = 这台手机上已经没有这份文件了 */
        val path: String?,
        val saved: String?,
        val w: Int,
        val h: Int,
        val durMs: Long,
    )

    /** 解析摘要。坏 JSON 返回 null，界面按"打不开"处理，不要崩 */
    fun parse(text: String): Meta? = runCatching {
        val o = JSONObject(text)
        Meta(
            fid = o.optString("fid"),
            // 名字留空交给 UI 兜底：这是显示文案，数据层不该写死某一种语言
            name = o.optString("name"),
            size = o.optLong("size", 0),
            path = o.optString("path").takeIf { it.isNotBlank() },
            saved = o.optString("saved").takeIf { it.isNotBlank() },
            w = o.optInt("w", 0),
            h = o.optInt("h", 0),
            durMs = o.optLong("dur", 0),
        )
    }.getOrNull()
}
