package io.anonchat.app

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Matrix
import android.media.ExifInterface
import android.net.Uri
import android.provider.OpenableColumns
import java.io.File
import java.io.FileOutputStream

/**
 * 发图片前的预处理：解码 → 按 EXIF 摆正 → 缩到长边 1600 → 压成 JPEG。
 *
 * 为什么必须重编码而不是"原图直接发"：
 * - 手机拍的照片 3~8MB，20MB 的通道传两三张就满，而且接收方要收几分钟；
 * - content:// 的流只能顺序读，边发边读没问题，但没法边读边缩，
 *   只能在本地先落一份压缩后的副本，发完再删。
 *
 * GIF 例外：压成 JPEG 动画就没了，所以原样拷贝。代价是气泡里只显示首帧
 * （要看动图得靠长按菜单里的"打开"交给系统相册）。
 */
object ImageCompress {

    data class Out(
        val file: File,
        /** 压缩后的真实像素宽（已按 EXIF 摆正） */
        val w: Int,
        val h: Int,
        val size: Long,
    )

    /**
     * 把 [uri] 处理成一个可以直接发的临时文件，放在 [outDir]。
     * 调用方负责在发送完成后删掉它（sent/ 里已经留了副本）。
     */
    fun prepare(ctx: Context, uri: Uri, outDir: File): Res<Out> {
        val rawName = queryName(ctx, uri) ?: "image.jpg"
        val mime = runCatching { ctx.contentResolver.getType(uri) }.getOrNull().orEmpty()
        outDir.mkdirs()

        // GIF 原样发：动画保不住就失去意义了
        if (mime == "image/gif" || Media.extOf(rawName) == "gif") {
            val dst = File(outDir, rawName)
            val n = copyTo(ctx, uri, dst)
            if (n <= 0) return Res(null, err = AppError(ErrKind.IMG_SIZE_UNKNOWN))
            val (w, h) = boundsOf(dst)
            return Res(Out(dst, w, h, n), null)
        }

        // 已经是"能直接发"的 JPEG 就别再压一遍。
        // 重编码只会掉画质，体积甚至可能变大——实测一张 252KB / 1200×1600 的照片，
        // 按 JPEG 80 重压后成了 272KB。只有"太大"或"方向不对"才值得重编码。
        run {
            val (ow, oh) = boundsOf(ctx, uri)
            val srcSize = querySize(ctx, uri)
            val orient = orientationOf(ctx, uri)
            if (Media.canPassThrough(mime, rawName, ow, oh, srcSize, orient)) {
                // 名字可能没有扩展名（mime 是 jpeg 但叫 "photo"），补齐成 .jpg，
                // 否则下游按扩展名判类型会把它当普通文件。
                val keepName =
                    if (Media.extOf(rawName) in setOf("jpg", "jpeg")) rawName
                    else Media.compressedName(rawName)
                val dst = File(outDir, keepName)
                val n = copyTo(ctx, uri, dst)
                if (n > 0) return Res(Out(dst, ow, oh, n), null)
                dst.delete()
            }
        }

        val (ow, oh) = boundsOf(ctx, uri)
        if (ow <= 0 || oh <= 0) {
            return Res(null, err = AppError(ErrKind.IMG_UNREADABLE))
        }
        if (ow.toLong() * oh > 100_000_000L) {
            return Res(null, err = AppError(ErrKind.IMG_PIXELS_TOO_MANY, "${ow}×${oh}"))
        }

        val orient = orientationOf(ctx, uri)
        val sample = Media.inSampleSize(ow, oh, Media.IMAGE_MAX_EDGE)
        val decoded = runCatching { decode(ctx, uri, sample) }.getOrNull()
            ?: return Res(null, err = AppError(ErrKind.IMG_DECODE_FAILED))

        val upright = rotate(decoded, orient)
        val (tw, th) = Media.fitWithin(upright.width, upright.height, Media.IMAGE_MAX_EDGE)
        val scaled = if (tw != upright.width || th != upright.height) {
            runCatching { Bitmap.createScaledBitmap(upright, tw, th, true) }.getOrDefault(upright)
        } else upright

        // 先记住尺寸再压：压完就不该再碰这张 Bitmap 了（下面的判断要用宽高）
        val finalW = scaled.width
        val finalH = scaled.height
        val dst = File(outDir, Media.compressedName(rawName))
        val ok = runCatching {
            FileOutputStream(dst).use { out ->
                scaled.compress(Bitmap.CompressFormat.JPEG, Media.JPEG_QUALITY, out)
            }
        }.isSuccess
        // 刻意不 recycle：中间几张贴图可能互相引用（createScaledBitmap 在尺寸相同时
        // 会直接返回原对象），按"谁不等于谁"去回收极容易把还在用的那张回收掉，
        // 表现为压缩成功但文件是空的。这点内存交给 GC，GC 不会搞错。

        if (!ok || !dst.exists() || dst.length() <= 0L) {
            dst.delete()
            return Res(null, err = AppError(ErrKind.IMG_COMPRESS_FAILED))
        }
        if (dst.length() > Media.IMAGE_MAX_BYTES) {
            dst.delete()
            return Res(null, err = AppError(ErrKind.IMG_STILL_TOO_BIG, Media.sizeLabel(dst.length())))
        }
        return Res(Out(dst, finalW, finalH, dst.length()), null)
    }

    // ------------------------------------------------------------------ 内部

    private fun queryName(ctx: Context, uri: Uri): String? = runCatching {
        ctx.contentResolver.query(uri, null, null, null, null)?.use { c ->
            if (!c.moveToFirst()) return@use null
            val i = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
            if (i >= 0 && !c.isNull(i)) c.getString(i) else null
        }
    }.getOrNull()

    /**
     * 文件字节数。
     *
     * 优先读 [OpenableColumns.SIZE]（不用打开流，快），读不到就用 AssetFileDescriptor
     * 的长度兜底——个别第三方相册/网盘 provider 的 SIZE 列是空的。
     * 拿不到就返回 0，调用方按"不确定"处理（[Media.canPassThrough] 会因此走压缩路径，
     * 这是安全的一侧：宁可多压一次，不要放过一张 30MB 的原图）。
     */
    private fun querySize(ctx: Context, uri: Uri): Long {
        val fromColumn = runCatching {
            ctx.contentResolver.query(uri, null, null, null, null)?.use { c ->
                if (!c.moveToFirst()) return@use 0L
                val i = c.getColumnIndex(OpenableColumns.SIZE)
                if (i >= 0 && !c.isNull(i)) c.getLong(i) else 0L
            } ?: 0L
        }.getOrDefault(0L)
        if (fromColumn > 0L) return fromColumn
        return runCatching {
            ctx.contentResolver.openAssetFileDescriptor(uri, "r")?.use { it.length } ?: 0L
        }.getOrDefault(0L).coerceAtLeast(0L)
    }

    private fun boundsOf(ctx: Context, uri: Uri): Pair<Int, Int> = runCatching {
        val o = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        ctx.contentResolver.openInputStream(uri)?.use { BitmapFactory.decodeStream(it, null, o) }
        o.outWidth to o.outHeight
    }.getOrDefault(0 to 0)

    private fun boundsOf(f: File): Pair<Int, Int> = runCatching {
        val o = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeFile(f.absolutePath, o)
        o.outWidth to o.outHeight
    }.getOrDefault(0 to 0)

    /**
     * EXIF 方向。安卓相机默认把照片存成"横向传感器数据 + 一个旋转标记"，
     * 不按它转的话，竖着拍的照片在气泡里会躺着。
     */
    private fun orientationOf(ctx: Context, uri: Uri): Int = runCatching {
        ctx.contentResolver.openInputStream(uri)?.use { ins ->
            ExifInterface(ins).getAttributeInt(
                ExifInterface.TAG_ORIENTATION, ExifInterface.ORIENTATION_NORMAL
            )
        } ?: ExifInterface.ORIENTATION_NORMAL
    }.getOrDefault(ExifInterface.ORIENTATION_NORMAL)

    private fun decode(ctx: Context, uri: Uri, sample: Int): Bitmap? = runCatching {
        val o = BitmapFactory.Options().apply {
            inSampleSize = sample
            inPreferredConfig = Bitmap.Config.ARGB_8888
        }
        ctx.contentResolver.openInputStream(uri)?.use { BitmapFactory.decodeStream(it, null, o) }
    }.getOrNull()

    private fun rotate(src: Bitmap, orient: Int): Bitmap {
        val m = Matrix()
        when (orient) {
            ExifInterface.ORIENTATION_ROTATE_90 -> m.postRotate(90f)
            ExifInterface.ORIENTATION_ROTATE_180 -> m.postRotate(180f)
            ExifInterface.ORIENTATION_ROTATE_270 -> m.postRotate(270f)
            ExifInterface.ORIENTATION_FLIP_HORIZONTAL -> m.postScale(-1f, 1f)
            ExifInterface.ORIENTATION_FLIP_VERTICAL -> m.postScale(1f, -1f)
            ExifInterface.ORIENTATION_TRANSPOSE -> {
                m.postRotate(90f); m.postScale(-1f, 1f)
            }
            ExifInterface.ORIENTATION_TRANSVERSE -> {
                m.postRotate(270f); m.postScale(-1f, 1f)
            }
            else -> return src
        }
        return runCatching { Bitmap.createBitmap(src, 0, 0, src.width, src.height, m, true) }
            .getOrDefault(src)
    }

    private fun copyTo(ctx: Context, uri: Uri, dst: File): Long = runCatching {
        ctx.contentResolver.openInputStream(uri)?.use { ins ->
            FileOutputStream(dst).use { out ->
                val buf = ByteArray(64 * 1024)
                var total = 0L
                while (true) {
                    val n = ins.read(buf)
                    if (n <= 0) break
                    out.write(buf, 0, n)
                    total += n
                }
                total
            }
        } ?: 0L
    }.getOrDefault(0L)
}
