package io.anonchat.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * TURN 链接解析。用户是照着自己 coturn 配置手抄的，格式五花八门，
 * 写错一个字符的表现是"电话打通了但没声音"，排查成本极高——所以这里盯死。
 */
class TurnLinkTest {

    @Test
    fun plainHostPortGetsSchemeAndPort() {
        val p = TurnLink.parse("1.2.3.4:3478")!!
        assertEquals("turn:1.2.3.4:3478", p.url)
        assertEquals("", p.user)
        assertEquals("", p.pass)
    }

    @Test
    fun bareHostGetsDefaultPort() {
        assertEquals("turn:turn.example.com:3478", TurnLink.parse("turn.example.com")!!.url)
        assertEquals("turns:turn.example.com:5349", TurnLink.parse("turns:turn.example.com")!!.url)
    }

    @Test
    fun credentialsAreSplitOut() {
        val p = TurnLink.parse("turn://alice:s3cret@1.2.3.4:3478")!!
        assertEquals("turn:1.2.3.4:3478", p.url)
        assertEquals("alice", p.user)
        assertEquals("s3cret", p.pass)
    }

    @Test
    fun passwordContainingAtSignIsNotTruncated() {
        val p = TurnLink.parse("turn://bob:p@ss@turn.example.com:3478")!!
        assertEquals("turn:turn.example.com:3478", p.url)
        assertEquals("bob", p.user)
        assertEquals("p@ss", p.pass)
    }

    @Test
    fun transportQueryIsPreserved() {
        // TURN over TCP：443 + transport=tcp 是绕开 UDP 封锁的常见组合
        val p = TurnLink.parse("turns://u:p@turn.example.com:443?transport=tcp")!!
        assertEquals("turns:turn.example.com:443?transport=tcp", p.url)
    }

    @Test
    fun coturnConfigLineIsUnderstood() {
        // 用户经常直接从 turnserver.conf 里复制一整行过来
        val p = TurnLink.parse("url=turn:1.2.3.4:3478?transport=udp")!!
        assertEquals("turn:1.2.3.4:3478?transport=udp", p.url)
    }

    @Test
    fun garbageReturnsNull() {
        assertNull(TurnLink.parse(""))
        assertNull(TurnLink.parse("   "))
        assertNull(TurnLink.parse("turn:"))
    }

    @Test
    fun normalizeStripsCredentialsForWebRtc() {
        // WebRTC 的 IceServer builder 不接受带凭据的 URL，必须剥掉
        assertEquals(
            "turn:1.2.3.4:3478",
            TurnLink.normalize("turn://user:pass@1.2.3.4:3478")
        )
    }
}
