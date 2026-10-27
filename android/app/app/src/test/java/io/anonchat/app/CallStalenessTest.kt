package io.anonchat.app

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * 迟到来电的判定边界。
 *
 * 这段逻辑错一次就是"漏接电话"或"幽灵来电"，而它依赖两端墙上时钟，
 * 边界比看上去多，所以单测覆盖：
 *  - 振铃窗口内（≤45s）绝不能判迟到
 *  - 窗口外但仍在阈值内，也不能判迟到（对方可能刚重连，电话还在响）
 *  - 明显过期的（进程被杀后很久才打开 App）必须丢
 *  - 缺字段/时钟偏快时一律放行
 */
class CallStalenessTest {

    private val now = 1_800_000_000L

    @Test
    fun fresh_offer_is_not_stale() {
        assertFalse(CallStaleness.isStale(now - 1, now))
        assertFalse(CallStaleness.isStale(now, now))
    }

    @Test
    fun within_ring_window_is_not_stale() {
        // 45 秒是 Rust 侧的振铃窗口：这段时间内对方确实还在响
        assertFalse(CallStaleness.isStale(now - 45, now))
        assertFalse(CallStaleness.isStale(now - 90, now))
    }

    @Test
    fun just_past_threshold_is_stale() {
        assertFalse(CallStaleness.isStale(now - CallStaleness.STALE_SECS, now))
        assertTrue(CallStaleness.isStale(now - CallStaleness.STALE_SECS - 1, now))
    }

    @Test
    fun long_dead_call_is_stale() {
        // 实测场景：进程被杀 20 分钟，用户打开 App 才把积压的 offer 拉下来
        assertTrue(CallStaleness.isStale(now - 20 * 60, now))
    }

    @Test
    fun missing_timestamp_is_never_stale() {
        // 老客户端不带 ts：不能拦，否则它打来的电话全被吞掉
        assertFalse(CallStaleness.isStale(0, now))
    }

    @Test
    fun sender_clock_in_the_future_is_not_stale() {
        // 对方时钟偏快（本机偏慢）时 age 为负，不能误判
        assertFalse(CallStaleness.isStale(now + 600, now))
    }
}
