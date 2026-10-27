package io.anonchat.app

/**
 * 判一条来电是不是"已经迟到"的纯逻辑。
 *
 * 为什么单独拎出来：这段判断依赖"两台设备的墙上时钟"这个前提，边界很容易写错，
 * 而它一旦写错要么误杀正常来电、要么放任幽灵来电。放在 CallManager 里没法单测
 * （那边有 WebRTC 和 Android 依赖），所以拆成纯函数。
 */
internal object CallStaleness {

    /**
     * 超过这个年龄的 offer 直接丢弃（秒）。
     *
     * 必须明显大于 Rust 侧的振铃窗口（45s）：否则"对方还在响、只是我刚重连上"
     * 这种正常来电会被误杀。120s 是振铃窗口的两倍多，留出重连与时钟抖动的余量。
     */
    const val STALE_SECS = 120L

    /**
     * @param sentTs 发送方载荷里的时间戳（unix 秒）。0 表示对方是老客户端、
     *               没有这个字段——**不能拦**，否则老版本打来的电话全被吞掉。
     * @param nowSecs 本机当前时间（unix 秒）
     */
    fun isStale(sentTs: Long, nowSecs: Long): Boolean {
        if (sentTs <= 0) return false
        // 发送时间在"未来"说明对方时钟偏快（或本机偏慢），按新鲜处理：
        // 宁可多响一次，也不要因为时钟问题漏接。
        val age = nowSecs - sentTs
        return age > STALE_SECS
    }
}
