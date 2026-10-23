package io.anonchat.app

import org.json.JSONObject
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * 语音消息里与平台无关的那部分：命名、时长格式、手势阈值、摘要 JSON。
 *
 * 为什么单独抽一个 object：MediaRecorder / MediaPlayer / Bitmap 在 JVM 单测里
 * 只会抛 "not mocked"，把这些判断留在 Compose 文件里就等于永久测不到。
 * 抽出来之后录音器和播放器只剩下"开/停/读电平"，逻辑都在这里，能被单测盯住。
 */
object Voice {

    /** 单条语音最长 60 秒，到时自动停并发出去 */
    const val MAX_MS = 60_000L

    /**
     * 低于 700ms 当误触丢弃。
     *
     * 不取 1 秒：真按一下就松的「嗯 / 好」通常也在 0.8 秒上下。
     * 也不能再往下压——MediaRecorder 在极短录制下 stop() 会抛异常，
     * 写出来的 mp4 缺 moov box，根本播不了。700ms 是"有内容"和"文件可用"的交界。
     */
    const val MIN_MS = 700L

    /** 上滑多少 dp 算取消（换算成 px 后交给 [shouldCancel]） */
    const val CANCEL_DP = 64

    /**
     * 语音文件名。
     *
     * 用纯 ASCII：这个文件要经过 inbox 目录、adb 拉取、CLI 对拍，
     * 中文名在那些环节只会添乱，而且气泡上显示的是时长不是文件名。
     */
    fun fileName(ts: Long = System.currentTimeMillis()): String =
        "voice_" + SimpleDateFormat("yyyyMMdd_HHmmss", Locale.US).format(Date(ts)) + ".m4a"

    /** 录音浮层里的计时："0:07"、"1:03" */
    fun clock(ms: Long): String {
        val total = (ms.coerceAtLeast(0) / 1000).toInt()
        return "%d:%02d".format(total / 60, total % 60)
    }

    /** 气泡上的秒数。不足 1 秒按 1 秒算，别让用户看到 0" */
    fun seconds(ms: Long): Int =
        ((ms.coerceAtLeast(0) + 999) / 1000).toInt().coerceAtLeast(1)

    /** 气泡上显示的时长："7"" */
    fun bubbleLabel(ms: Long): String = "${seconds(ms)}\""

    /**
     * 上滑是否到了取消的程度。[upPx] 是手指相对按下点上移的距离（上移为正）。
     *
     * 阈值 <= 0 时一律不取消：真机上都量不出距离的场合（比如手势被打断），
     * 宁可发出去让用户重录，也别默默把录音扔掉。
     */
    fun shouldCancel(upPx: Float, thresholdPx: Float): Boolean =
        thresholdPx > 0f && upPx >= thresholdPx

    fun tooShort(ms: Long): Boolean = ms < MIN_MS

    /**
     * 语音气泡的最小宽度：随秒数略增，但起点要够放下
     * 「▶ + 波形 + 时长」，否则短语音的时长会被挤没（模拟器上抓到过）。
     */
    fun bubbleWidthDp(ms: Long): Int = (140 + seconds(ms) * 2).coerceIn(140, 240)

    /**
     * 语音消息的摘要 JSON（聊天记录里 text 字段存的就是它）。
     *
     * 字段和文件消息刻意保持一致（fid/name/size/path），只多一个 dur：
     * 传输层完全复用文件那条链路，不需要第二套分块逻辑。
     */
    fun summary(fid: String, name: String, size: Long, path: String?, durMs: Long): JSONObject {
        val o = JSONObject()
            .put("fid", fid)
            .put("name", name)
            .put("size", size)
            .put("dur", durMs)
        if (path != null) o.put("path", path)
        return o
    }

    data class Meta(
        val fid: String,
        val name: String,
        val size: Long,
        /** 本机副本路径。null = 这条语音在这台手机上已经没有文件了 */
        val path: String?,
        val durMs: Long,
    )

    /** 解析摘要。坏 JSON 返回 null，界面按"坏消息"处理，不要崩 */
    fun parse(text: String): Meta? = runCatching {
        val o = JSONObject(text)
        Meta(
            fid = o.optString("fid"),
            // 名字留空交给 UI 兜底：这是显示文案，数据层不该写死某一种语言
            name = o.optString("name"),
            size = o.optLong("size", 0),
            path = o.optString("path").takeIf { it.isNotBlank() },
            durMs = o.optLong("dur", 0),
        )
    }.getOrNull()
}
