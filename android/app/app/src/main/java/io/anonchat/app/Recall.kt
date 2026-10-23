package io.anonchat.app

/**
 * 撤回的纯逻辑：只管窗口判定。
 *
 * 撤回引用（rid）跨设备定位消息的原理：
 * - 文字消息：发送方在加密载荷里生成 `id`（Rust `Payload.id`，v1 就有），接收端落盘为 `Msg.rid`；
 * - 文件/语音/图片/视频：分块摘要里本来就有 `fid`（整条消息一个），直接当 rid 用。
 * 两条设备各自的本地行号对不上，只有这个 id 两边一致，撤回消息（kind=recall）就带它。
 *
 * 灰字文案不在这里：那是显示文案，得随系统语言变，由 UI 层按方向取资源。
 */
object Recall {
    /** 撤回窗口：发出后 10 分钟内 */
    const val WINDOW_SECS = 600L

    /**
     * 是否还在窗口内。时钟回拨（now < ts）差值为负、必然 ≤ 窗口，按未超时处理：
     * 撤回是客户端自律的社交礼仪，不是安全边界，别因为几秒的时钟偏差误伤用户
     */
    fun withinWindow(tsSecs: Long, nowSecs: Long): Boolean =
        nowSecs - tsSecs <= WINDOW_SECS

}
