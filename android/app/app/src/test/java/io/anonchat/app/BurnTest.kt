package io.anonchat.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class BurnTest {

    @Test
    fun options_are_off_30s_5min() {
        assertEquals(listOf(0L, 30L, 300L), Burn.OPTIONS)
    }

    @Test
    fun expiry_needs_both_burn_and_seen() {
        // 没档位：永不烧
        assertFalse(Burn.expired(0L, 1000L, 999_999L))
        // 有档位没展示过：不烧
        assertFalse(Burn.expired(30L, 0L, 999_999L))
        // 展示后 29 秒：没到
        assertFalse(Burn.expired(30L, 1000L, 1029L))
        // 整 30 秒：烧
        assertTrue(Burn.expired(30L, 1000L, 1030L))
        // 超了：烧
        assertTrue(Burn.expired(30L, 1000L, 5000L))
    }

    @Test
    fun remaining_counts_down_and_never_negative() {
        // 还没展示过 = 计时未开始，没有"剩余时间"概念（UI 层自己显示全额档位）
        assertEquals(Long.MAX_VALUE, Burn.remaining(30L, 0L, 9999L))
        assertEquals(25L, Burn.remaining(30L, 1000L, 1005L))
        assertEquals(0L, Burn.remaining(30L, 1000L, 1030L))
        assertEquals(0L, Burn.remaining(30L, 1000L, 2000L))
        // 非焚毁消息同样没有
        assertEquals(Long.MAX_VALUE, Burn.remaining(0L, 0L, 0L))
    }
}
