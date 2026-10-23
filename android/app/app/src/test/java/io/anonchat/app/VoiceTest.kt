package io.anonchat.app

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * 语音消息的纯逻辑单测。
 *
 * 录音/播放本身要真机才验得了，但这些边界（太短、时长显示、取消阈值、
 * 摘要往返）是踩过坑的地方：时长格式化算错会让 60 秒显示成 1:0，
 * 太短阈值判断漏掉会留下一个点开就崩的坏文件。
 */
class VoiceTest {

    @Test
    fun clock_formats_minutes_and_seconds() {
        assertEquals("0:00", Voice.clock(0))
        assertEquals("0:07", Voice.clock(7_000))
        assertEquals("0:59", Voice.clock(59_999))
        assertEquals("1:00", Voice.clock(60_000))
        assertEquals("1:03", Voice.clock(63_400))
    }

    @Test
    fun clock_never_negative() {
        assertEquals("0:00", Voice.clock(-5_000))
    }

    @Test
    fun seconds_round_up_and_never_zero() {
        assertEquals(1, Voice.seconds(0))
        assertEquals(1, Voice.seconds(1))
        assertEquals(1, Voice.seconds(999))
        assertEquals(1, Voice.seconds(1_000))
        assertEquals(2, Voice.seconds(1_001))
        assertEquals(60, Voice.seconds(60_000))
    }

    @Test
    fun bubble_label_is_seconds_quoted() {
        assertEquals("7\"", Voice.bubbleLabel(7_000))
        assertEquals("1\"", Voice.bubbleLabel(300))
    }

    @Test
    fun too_short_boundary() {
        assertTrue(Voice.tooShort(Voice.MIN_MS - 1))
        assertFalse(Voice.tooShort(Voice.MIN_MS))
        assertFalse(Voice.tooShort(5_000))
    }

    @Test
    fun cancel_needs_a_positive_threshold() {
        assertTrue(Voice.shouldCancel(200f, 160f))
        assertTrue(Voice.shouldCancel(160f, 160f))
        assertFalse(Voice.shouldCancel(159f, 160f))
        // 阈值量不出来时一律不取消：宁可发出去让用户重录，也别默默吞掉录音
        assertFalse(Voice.shouldCancel(9_999f, 0f))
        assertFalse(Voice.shouldCancel(9_999f, -1f))
    }

    @Test
    fun bubble_width_grows_with_duration_but_stays_bounded() {
        // 下限定在 140dp + 每秒 2dp：再窄的话「▶ + 波形 + 时长」放不下，时长会被裁掉
        assertEquals(142, Voice.bubbleWidthDp(1_000))
        val short = Voice.bubbleWidthDp(2_000)
        val long = Voice.bubbleWidthDp(60_000)
        assertTrue(long > short)
        assertEquals(240, long)
    }

    @Test
    fun summary_round_trip_keeps_everything() {
        val json = Voice.summary("ab12cd34", "voice_1.m4a", 12_345L, "/data/x/inbox/a.m4a", 7_400L)
        val m = Voice.parse(json.toString())
        assertEquals("ab12cd34", m!!.fid)
        assertEquals("voice_1.m4a", m.name)
        assertEquals(12_345L, m.size)
        assertEquals("/data/x/inbox/a.m4a", m.path)
        assertEquals(7_400L, m.durMs)
    }

    @Test
    fun summary_omits_path_when_absent() {
        val json = Voice.summary("f", "n", 1L, null, 100L)
        assertFalse(JSONObject(json.toString()).has("path"))
        assertNull(Voice.parse(json.toString())!!.path)
    }

    @Test
    fun parse_rejects_garbage_and_fills_defaults() {
        assertNull(Voice.parse("not json"))
        assertNull(Voice.parse(""))
        val m = Voice.parse("""{"fid":"x"}""")
        // 名字留空，由 UI 层按当前语言兜底（数据层不写死语言）
        assertEquals("", m!!.name)
        assertEquals(0L, m.durMs)
    }

    @Test
    fun file_name_is_ascii_and_timestamped() {
        // 2026-10-22 10:05:11 本地时区
        val ts = java.util.Calendar.getInstance().apply {
            set(2026, 9, 22, 10, 5, 11)
            set(java.util.Calendar.MILLISECOND, 0)
        }.timeInMillis
        val n = Voice.fileName(ts)
        assertTrue(n, Regex("""^voice_\d{8}_\d{6}\.m4a$""").matches(n))
        assertTrue(n.contains("20261022"))
        assertTrue(n.contains("100511"))
    }
}
