package io.anonchat.app

import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * "未加为联系人" 会话的过滤规则。
 *
 * 这条规则错过一次的实际表现（真机截图确认过）：同一个联系人出现两遍，
 * 一条在联系人区、点进去能正常聊；另一条在陌生人区、点进去还带着
 * "加为好友"按钮。两条其实是同一段历史，只是会话键不一样。
 */
class UnverifiedFilterTest {

    @Test
    fun contact_sessions_are_not_listed_as_strangers() {
        val keys = listOf("dh-alice", "dh-bob")
        val out = filterUnverifiedKeys(
            keys,
            knownDh = setOf("dh-alice", "dh-bob"),
            displayNames = emptySet(),
        )
        assertEquals(emptyList<String>(), out)
    }

    @Test
    fun unknown_sessions_are_listed() {
        val out = filterUnverifiedKeys(
            listOf("dh-stranger"),
            knownDh = setOf("dh-alice"),
            displayNames = emptySet(),
        )
        assertEquals(listOf("dh-stranger"), out)
    }

    /**
     * 存量脏数据：0.15.1 及更早版本加好友时把会话键搬成了人名。
     * 键对不上任何 dh，但和某个联系人的显示名一样——不能当成陌生人会话。
     */
    @Test
    fun sessions_keyed_by_a_contacts_name_are_hidden() {
        val out = filterUnverifiedKeys(
            keys = listOf("dh-alice", "张三", "陌生人 4f3a9c21"),
            knownDh = setOf("dh-alice"),
            displayNames = setOf("张三", "ggyy"),
        )
        // 只剩那个真正的陌生会话
        assertEquals(listOf("陌生人 4f3a9c21"), out)
    }

    /** 备注名也要算进去：用户给联系人起了别名，历史键可能用的是别名 */
    @Test
    fun aliases_are_honoured() {
        val out = filterUnverifiedKeys(
            keys = listOf("小王"),
            knownDh = setOf("dh-bob"),
            displayNames = setOf("小王"),
        )
        assertEquals(emptyList<String>(), out)
    }
}
