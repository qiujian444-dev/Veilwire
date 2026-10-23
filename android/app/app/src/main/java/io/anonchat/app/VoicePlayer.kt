package io.anonchat.app

import android.media.MediaPlayer
import android.os.Handler
import android.os.Looper
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import java.io.File

/**
 * 语音播放。
 *
 * 全局单例而不是每个气泡一个播放器：
 * - 同时只该有一条语音在响，第二个点下去必须把第一个掐掉（否则两条叠着放，听不清）；
 * - 播放进度是全局面板状态，任何一个气泡都能读到同一份进度。
 *
 * 播放状态用 Compose 的 mutableStateOf 暴露，气泡读它就自动重组。
 */
object VoicePlayer {

    private var mp: MediaPlayer? = null
    private val handler = Handler(Looper.getMainLooper())
    private var ticker: Runnable? = null

    /** 正在播放的本地路径。null = 没在播 */
    var playingPath by mutableStateOf<String?>(null)
        private set

    var posMs by mutableStateOf(0)
        private set

    var durMs by mutableStateOf(0)
        private set

    /** 0..1。气泡里的波形靠它高亮 */
    val progress: Float
        get() = if (durMs <= 0) 0f else (posMs.toFloat() / durMs).coerceIn(0f, 1f)

    /** 点一下：正在播这条就停，否则换成这条播 */
    fun toggle(path: String) {
        if (playingPath == path) {
            stop()
            return
        }
        stop()
        if (!File(path).exists()) return
        val p = MediaPlayer()
        try {
            p.setDataSource(path)
            p.setOnCompletionListener { stop() }
            p.setOnErrorListener { _, _, _ ->
                stop()
                true
            }
            p.prepare()
            p.start()
            mp = p
            playingPath = path
            durMs = runCatching { p.duration }.getOrDefault(0).coerceAtLeast(0)
            posMs = 0
            startTicker()
        } catch (t: Throwable) {
            runCatching { p.release() }
            stop()
        }
    }

    fun stop() {
        ticker?.let { handler.removeCallbacks(it) }
        ticker = null
        runCatching { mp?.stop() }
        runCatching { mp?.release() }
        mp = null
        playingPath = null
        posMs = 0
    }

    private fun startTicker() {
        val r = object : Runnable {
            override fun run() {
                val p = mp ?: return
                posMs = runCatching { p.currentPosition }.getOrDefault(0)
                handler.postDelayed(this, 100)
            }
        }
        ticker = r
        handler.post(r)
    }
}
