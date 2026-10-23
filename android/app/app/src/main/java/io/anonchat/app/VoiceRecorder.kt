package io.anonchat.app

import android.content.Context
import android.media.MediaRecorder
import android.os.SystemClock
import java.io.File

/**
 * 录音器。
 *
 * 只在按住说话的那几秒存活，不做常驻、不加前台服务：录音是被用户手势
 * 严格包住的短动作，一旦手指抬起来就必须结束（这也是隐私上的硬要求）。
 *
 * 输出 AAC-LC / 单声道 / 44.1kHz / 64kbps，封装成 mp4(.m4a)：
 * 60 秒约 480KB，走文件分块链路只要 20 块，一次批推就能发完。
 */
object VoiceRecorder {

    private var rec: MediaRecorder? = null
    private var out: File? = null
    private var startedAt = 0L

    val running: Boolean get() = rec != null

    /** 已录时长（毫秒） */
    fun elapsedMs(): Long = if (rec == null) 0L else SystemClock.elapsedRealtime() - startedAt

    /**
     * 录音启动失败的原因。文案在 UI 层（[MainActivity] 按 kind 取资源）。
     */
    data class StartError(val kind: Kind, val detail: String? = null) {
        enum class Kind { ALREADY_RECORDING, OPEN_FAILED }
    }

    /**
     * 开始录。返回 null 表示成功。
     *
     * 失败必须显式返回而不是静默：真机上麦克风被通话/别的 App 占用时
     * `prepare()` 会直接抛，用户按着按钮却什么都没发生是最糟的反馈。
     */
    @Suppress("DEPRECATION")
    fun start(ctx: Context): StartError? {
        if (rec != null) return StartError(StartError.Kind.ALREADY_RECORDING)
        val dir = File(ctx.filesDir, "voice_tmp").apply { mkdirs() }
        // 清掉上次异常退出留下的残片（超过 1 小时的一定是垃圾）
        val now = System.currentTimeMillis()
        runCatching {
            dir.listFiles()?.forEach { f ->
                if (now - f.lastModified() > 3_600_000L) f.delete()
            }
        }
        val target = File(dir, "rec_$now.m4a")
        val mr = MediaRecorder()
        return try {
            mr.setAudioSource(MediaRecorder.AudioSource.MIC)
            mr.setOutputFormat(MediaRecorder.OutputFormat.MPEG_4)
            mr.setAudioEncoder(MediaRecorder.AudioEncoder.AAC)
            mr.setAudioSamplingRate(44_100)
            mr.setAudioChannels(1)
            mr.setAudioEncodingBitRate(64_000)
            mr.setOutputFile(target.absolutePath)
            mr.prepare()
            mr.start()
            rec = mr
            out = target
            startedAt = SystemClock.elapsedRealtime()
            null
        } catch (t: Throwable) {
            runCatching { mr.release() }
            runCatching { target.delete() }
            StartError(
                StartError.Kind.OPEN_FAILED,
                t.message ?: t.javaClass.simpleName
            )
        }
    }

    /** 当前电平（0..32767），用来画波形。取不到就返回 0，界面上是一条平线 */
    fun amplitude(): Int = runCatching { rec?.maxAmplitude ?: 0 }.getOrDefault(0)

    /**
     * 停止并返回文件。返回 null 说明这次录音不可用（太短 / 编码器没收到帧）。
     *
     * MediaRecorder.stop() 在录制过短时会抛 RuntimeException，且文件已经写坏；
     * 这里必须把文件删掉，否则会留下一个"能点但播不了"的语音气泡。
     */
    fun stop(): File? {
        val mr = rec ?: return null
        val f = out
        rec = null
        out = null
        return try {
            mr.stop()
            mr.release()
            f?.takeIf { it.exists() && it.length() > 0 }
        } catch (t: Throwable) {
            runCatching { mr.release() }
            runCatching { f?.delete() }
            null
        }
    }

    /** 用户上滑反悔：什么都不留 */
    fun cancel() {
        val mr = rec
        val f = out
        rec = null
        out = null
        if (mr != null) {
            runCatching { mr.stop() }
            runCatching { mr.release() }
        }
        runCatching { f?.delete() }
    }

    /** 页面被销毁/切后台时兜底，避免麦克风被一个已经看不见的界面占着 */
    fun abort() {
        if (rec != null) cancel()
    }
}
