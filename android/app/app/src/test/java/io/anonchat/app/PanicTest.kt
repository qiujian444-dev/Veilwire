package io.anonchat.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import kotlin.random.Random

class PanicTest {

    private val dhA = "3f79bb7b435b05321651daefd374cdc681dc06faa65e374e38337b88ca4c62a1"
    private val dhB = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90"
    private val dhC = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"

    @Test
    fun code_is_symmetric_for_both_sides() {
        // 双方各自算（参数顺序相反）必须得到同一个暗号——否则永远对不上
        assertEquals(Panic.codeOf(dhA, dhB), Panic.codeOf(dhB, dhA))
    }

    @Test
    fun code_is_6_digits_and_pair_specific() {
        val code = Panic.codeOf(dhA, dhB)
        assertTrue("暗号必须是 6 位数字：$code", code.length == 6 && code.all { it.isDigit() })
        // 不同联系人对的暗号不同
        assertNotEquals(Panic.codeOf(dhA, dhB), Panic.codeOf(dhA, dhC))
        // 前后空格不影响（输入来源可能带空白）
        assertEquals(code, Panic.codeOf(" $dhA ", dhB))
    }

    @Test
    fun matching_is_exact_full_message_only() {
        val code = Panic.codeOf(dhA, dhB)
        // 精确命中（含 trim）
        assertTrue(Panic.matches(code, code))
        assertTrue(Panic.matches("  $code  ", code))
        // 子串/前后缀不命中：防日常聊天误触发
        assertFalse(Panic.matches("记得给我发$code", code))
        assertFalse(Panic.matches("$code 吗", code))
        assertFalse(Panic.matches("", code))
        // 没有暗号（拿不到对方公钥）永不命中
        assertFalse(Panic.matches(code, ""))
    }

    @Test
    fun quiz_is_two_digit_addition() {
        // 固定种子可复现：出题确定性（可测的关键）
        val (q, a) = Panic.quiz(Random(42))
        val m = Regex("(\\d+) \\+ (\\d+) = \\?").find(q)!!
        assertEquals(a, m.groupValues[1].toInt() + m.groupValues[2].toInt())
        // 两位数范围
        assertTrue(m.groupValues[1].toInt() in 10..99)
        assertTrue(m.groupValues[2].toInt() in 10..99)
    }

    @Test
    fun attempts_budget_is_three() {
        assertEquals(3, Panic.MAX_ATTEMPTS)
    }
}
