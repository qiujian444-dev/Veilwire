package io.anonchat.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class RecallTest {

    @Test
    fun window_is_ten_minutes() {
        assertTrue(Recall.withinWindow(1000L, 1000L + Recall.WINDOW_SECS))
        assertFalse(Recall.withinWindow(1000L, 1000L + Recall.WINDOW_SECS + 1))
        assertTrue(Recall.withinWindow(1000L, 1000L))       // 刚发出
        assertTrue(Recall.withinWindow(1000L, 999L))        // 时钟回拨按未超时
        assertEquals(600L, Recall.WINDOW_SECS)
    }

    @Test
    fun window_boundary_is_inclusive() {
        // 正好 600 秒仍在窗口内（边界含端点），601 秒就超时了
        assertTrue(Recall.withinWindow(1000L, 1000L + 600L))
        assertFalse(Recall.withinWindow(1000L, 1000L + 601L))
    }
}
