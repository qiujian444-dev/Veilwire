package io.anonchat.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * 确定性头像的纯逻辑。
 *
 * 头像只有一条硬要求：**同一个名字永远同一个颜色**。
 * 颜色如果跟着启动次数漂，用户会把"换颜色"读成"换人了"。
 */
class AvatarTest {

    @Test
    fun hue_is_stable_for_same_name() {
        assertEquals(AvatarLogic.hueFor("Iris"), AvatarLogic.hueFor("Iris"), 0.0001f)
        assertEquals(AvatarLogic.hueFor("陌生人 3a1f92bc"), AvatarLogic.hueFor("陌生人 3a1f92bc"), 0.0001f)
    }

    @Test
    fun hue_is_in_range_and_uses_whole_name() {
        // 全名参与哈希：只有首字相同、结尾不同的名字要能分出颜色
        val a = AvatarLogic.hueFor("小明A")
        val b = AvatarLogic.hueFor("小明B")
        assertTrue("相邻名字的颜色应该不同（哈希按整名算）", a != b)
        for (n in listOf("", "A", "张三", "陌生人 deadbeef", "VERY-LONG-NAME-0123456789")) {
            val h = AvatarLogic.hueFor(n)
            assertTrue("色相必须在 0..360 内: $h", h >= 0f && h < 360f)
        }
    }

    @Test
    fun hue_matches_hash_modulo_360() {
        // 公式锁死：hash % 360。改哈希算法会让所有人的头像同时换色，得知道自己在改
        assertEquals((AvatarLogic.hash("Iris") % 360).toFloat(), AvatarLogic.hueFor("Iris"), 0.0001f)
    }

    @Test
    fun initial_takes_first_grapheme() {
        assertEquals("I", AvatarLogic.initial("Iris"))
        assertEquals("张", AvatarLogic.initial("张三"))
        // 生僻字/emoji（代理对）取完整码点，不能劈成半个乱码
        assertEquals("𠀀", AvatarLogic.initial("𠀀测试"))
        assertEquals("#", AvatarLogic.initial(""))
        assertEquals("#", AvatarLogic.initial("   "))
    }

    @Test
    fun hash_is_fnv1a_reference_vector() {
        // FNV-1a 32 位的公开测试向量（空串与 "a"），
        // 钉住算法本身，防止哪天被"顺手重构"成不稳定的实现
        assertEquals(2166136261L, AvatarLogic.hash(""))
        assertEquals(0xE40C292CL.toLong() and 0xFFFFFFFFL, AvatarLogic.hash("a"))
    }
}
