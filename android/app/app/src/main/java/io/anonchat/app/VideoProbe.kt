package io.anonchat.app

import android.content.Context
import android.graphics.Bitmap
import android.media.MediaMetadataRetriever
import android.net.Uri
import android.provider.OpenableColumns
import java.io.File

/**
 * 视频元信息探测 + 首帧抽取。
 *
 * 没有转码：手机上转视频要自己写 MediaCodec 编码器（几十行配置 + 机型兼容坑），
 * 而收益只是"能把大视频压小"。0.6.3 的取舍是**不压、如实告知上限**——
 * 20MB 大约等于 720p 十几秒，超了就提示用户先用相册裁剪。
 */
object VideoProbe {

    data class Meta(
        val name: String,
        val size: Long,
        /** 毫秒。0 = 读不出来（部分封装没有时长元数据） */
        val durMs: Long,
        /** 已按旋转角摆正的宽高。可能为 0（读不出来时） */
        val w: Int,
        val h: Int,
    )

    fun probe(ctx: Context, uri: Uri): Res<Meta> {
        val (name, size) = query(ctx, uri)
        if (size <= 0) return Res(null, err = AppError(ErrKind.VIDEO_SIZE_UNKNOWN))
        val mmr = MediaMetadataRetriever()
        return try {
            mmr.setDataSource(ctx, uri)
            val dur = mmr.extractMetadata(MediaMetadataRetriever.METADATA_KEY_DURATION)
                ?.toLongOrNull() ?: 0L
            var w = mmr.extractMetadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_WIDTH)
                ?.toIntOrNull() ?: 0
            var h = mmr.extractMetadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_HEIGHT)
                ?.toIntOrNull() ?: 0
            val rot = mmr.extractMetadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_ROTATION)
                ?.toIntOrNull() ?: 0
            // 竖着拍的视频：元数据里的宽高还是传感器的横向尺寸，旋转角才是真相
            if (rot == 90 || rot == 270) {
                val t = w; w = h; h = t
            }
            Res(Meta(name, size, dur, w, h), null)
        } catch (t: Throwable) {
            Res(null, err = AppError(ErrKind.VIDEO_UNREADABLE))
        } finally {
            runCatching { mmr.release() }
        }
    }

    /**
     * 抽一帧当缩略图。[maxPx] 是最长边像素上限。
     *
     * 取 OPTION_CLOSEST_SYNC（最近的关键帧）而不是精确帧：关键帧解码快得多，
     * 缩略图差一两秒无所谓，但列表滚动时每帧都全解码会明显卡。
     */
    fun frame(path: String, maxPx: Int): Bitmap? {
        if (!File(path).exists()) return null
        val mmr = MediaMetadataRetriever()
        return try {
            mmr.setDataSource(path)
            val raw = mmr.getFrameAtTime(-1, MediaMetadataRetriever.OPTION_CLOSEST_SYNC)
                ?: return null
            val (tw, th) = Media.fitWithin(raw.width, raw.height, maxPx.coerceAtLeast(64))
            if (tw == raw.width && th == raw.height) raw
            else runCatching { Bitmap.createScaledBitmap(raw, tw, th, true) }.getOrDefault(raw)
        } catch (t: Throwable) {
            null
        } finally {
            runCatching { mmr.release() }
        }
    }

    /**
     * 从本地文件读像素宽高（视频顺带读时长）。
     *
     * 收消息时用：对方可能是 CLI / 老版本客户端，摘要里没带 w/h 或 dur，
     * 但那几个字段只影响气泡比例和角标——本地读一次就能补上，
     * 比在界面上按 4:3 瞎猜好得多。
     */
    fun dimsOf(path: String, kind: String): Triple<Int, Int, Long> {
        if (!File(path).exists()) return Triple(0, 0, 0L)
        if (kind == "image") {
            val o = android.graphics.BitmapFactory.Options().apply { inJustDecodeBounds = true }
            runCatching { android.graphics.BitmapFactory.decodeFile(path, o) }
            return Triple(o.outWidth.coerceAtLeast(0), o.outHeight.coerceAtLeast(0), 0L)
        }
        val mmr = MediaMetadataRetriever()
        return try {
            mmr.setDataSource(path)
            var w = mmr.extractMetadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_WIDTH)
                ?.toIntOrNull() ?: 0
            var h = mmr.extractMetadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_HEIGHT)
                ?.toIntOrNull() ?: 0
            val rot = mmr.extractMetadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_ROTATION)
                ?.toIntOrNull() ?: 0
            if (rot == 90 || rot == 270) {
                val t = w; w = h; h = t
            }
            val dur = mmr.extractMetadata(MediaMetadataRetriever.METADATA_KEY_DURATION)
                ?.toLongOrNull() ?: 0L
            Triple(w, h, dur)
        } catch (t: Throwable) {
            Triple(0, 0, 0L)
        } finally {
            runCatching { mmr.release() }
        }
    }

    private fun query(ctx: Context, uri: Uri): Pair<String, Long> = runCatching {
        var name = "video.mp4"
        var size = -1L
        ctx.contentResolver.query(uri, null, null, null, null)?.use { c ->
            if (c.moveToFirst()) {
                val ni = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                val si = c.getColumnIndex(OpenableColumns.SIZE)
                if (ni >= 0 && !c.isNull(ni)) name = c.getString(ni) ?: name
                if (si >= 0 && !c.isNull(si)) size = c.getLong(si)
            }
        }
        name to size
    }.getOrDefault("video.mp4" to -1L)
}
