package io.anonchat.app

/**
 * 阅后即焚的纯逻辑：档位、到期判定、剩余时间。
 *
 * 语义（L1 方案）：消息带焚毁档位（burn 秒），双端各自**首次展示**时记
 * `seenAt` 并落盘，倒计时到期后**本地删除**——不发生任何网络交互，
 * 中继在消息被 ack 时就已经删干净了，天然无残留。
 *
 * 计时各自独立、只依赖本地时钟：对方三天不打开 App，消息就躺着不烧，
 * 打开看到才开始计时——没有已读回执，这是 L1 的语义边界（要真·已读
 * 得加回执协议，那是元数据泄露，不做，见 README）。
 *
 * 防不住的：对方截图、录屏、相机翻拍。所以系统通知里焚毁消息必须脱敏，
 * 否则通知历史就成了永久的备份——这是本功能唯一的真安全边界。
 */
object Burn {
    /** 档位：关 / 30 秒 / 5 分钟。按条生效（发送面板里选中，发一条自动复位） */
    val OPTIONS = listOf(0L, 30L, 5L * 60)

    /** 是否已到期。seenAt <= 0 表示还没展示过，不烧 */
    fun expired(burn: Long, seenAt: Long, nowSecs: Long): Boolean =
        burn > 0 && seenAt > 0 && nowSecs - seenAt >= burn

    /** 剩余秒数。非焚毁消息返回 Long.MAX_VALUE（永不显示倒计时） */
    fun remaining(burn: Long, seenAt: Long, nowSecs: Long): Long =
        if (burn <= 0 || seenAt <= 0) Long.MAX_VALUE
        else (burn - (nowSecs - seenAt)).coerceAtLeast(0)
}
