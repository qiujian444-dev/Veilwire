package io.anonchat.app

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.util.LruCache

/**
 * 聊天里图片/视频缩略图的内存缓存。
 *
 * 为什么必须有：LazyColumn 每滑一下都会重新组合可见项，没有缓存的话
 * 同一张图会被反复解码（大图解码一次几十毫秒 + 几 MB 内存），滑动直接掉帧。
 *
 * 为什么按像素而不是张数来限：一张 200×200 的图和一张 1080×1080 的图差 29 倍，
 * 按张数限制根本约束不住内存。12MB 大约能放 10~20 张聊天尺寸的图。
 */
object Thumbs {

    private const val MAX_BYTES = 12 * 1024 * 1024

    private val cache = object : LruCache<String, Bitmap>(MAX_BYTES) {
        override fun sizeOf(key: String, value: Bitmap): Int = value.byteCount
    }

    /** 图片缩略图。[maxPx] 是最长边像素上限，按显示尺寸的 2 倍取就够清晰 */
    fun image(path: String, maxPx: Int): Bitmap? {
        val key = "i:$path@$maxPx"
        cache.get(key)?.let { return it }
        if (!java.io.File(path).exists()) return null

        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        runCatching { BitmapFactory.decodeFile(path, bounds) }
        if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return null

        val opts = BitmapFactory.Options().apply {
            inSampleSize = Media.inSampleSize(bounds.outWidth, bounds.outHeight, maxPx)
            inPreferredConfig = Bitmap.Config.ARGB_8888
        }
        val raw = runCatching { BitmapFactory.decodeFile(path, opts) }.getOrNull() ?: return null
        val (tw, th) = Media.fitWithin(raw.width, raw.height, maxPx.coerceAtLeast(64))
        val out = if (tw != raw.width || th != raw.height) {
            runCatching { Bitmap.createScaledBitmap(raw, tw, th, true) }.getOrDefault(raw)
        } else raw
        cache.put(key, out)
        return out
    }

    /** 视频首帧缩略图 */
    fun video(path: String, maxPx: Int): Bitmap? {
        val key = "v:$path@$maxPx"
        cache.get(key)?.let { return it }
        val bmp = VideoProbe.frame(path, maxPx) ?: return null
        cache.put(key, bmp)
        return bmp
    }

    /** 退出聊天页时调用：这里存的是别的会话也用不到的缩略图，留着只是白占内存 */
    fun clear() = cache.evictAll()
}
