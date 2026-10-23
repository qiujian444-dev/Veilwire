package io.anonchat.app

/**
 * TURN 链接解析：把用户粘贴的一行文本拆成 WebRTC 认识的 URL + 账号 + 密码。
 *
 * 为什么单独一个文件：这是个纯字符串函数，抽出来才能在 JVM 单测里覆盖
 * （放在 CallManager 里的话，测试一加载类就会碰 WebRTC 的 native 代码）。
 * 用户手抄地址时写错一个字符就"打不通电话"，这类格式容错值得有测试盯着。
 *
 * 认得的写法：
 * - `turn:1.2.3.4:3478`
 * - `1.2.3.4:3478`（不带 scheme，按 turn 补）
 * - `turn://user:pass@1.2.3.4:3478`
 * - `turns://user:pass@turn.example.com:5349?transport=tcp`
 * - coturn 配置里的 `url=turn:host:port`
 */
object TurnLink {

    data class Parsed(val url: String, val user: String, val pass: String)

    /** 只取地址，丢掉凭据（凭据要走 setUsername/setPassword，混在 URL 里 WebRTC 不认） */
    fun normalize(raw: String): String = parse(raw)?.url ?: raw.trim()

    fun parse(raw: String): Parsed? {
        var s = raw.trim()
        if (s.isEmpty()) return null

        // 不是以 turn 开头也不代表是垃圾：用户很可能直接粘了 turnserver.conf 的一行
        // （`url=turn:1.2.3.4:3478?transport=udp`），把前缀抠掉就行。
        if (!s.startsWith("turn", ignoreCase = true)) {
            val m = Regex("turns?:", RegexOption.IGNORE_CASE).find(s)
            if (m != null && m.range.first > 0) s = s.substring(m.range.first)
        }

        var scheme = "turn"
        when {
            s.startsWith("turns://", ignoreCase = true) -> { scheme = "turns"; s = s.substring(8) }
            s.startsWith("turn://", ignoreCase = true) -> s = s.substring(7)
            s.startsWith("turns:", ignoreCase = true) -> { scheme = "turns"; s = s.substring(6) }
            s.startsWith("turn:", ignoreCase = true) -> s = s.substring(5)
        }
        if (s.isBlank()) return null

        var user = ""
        var pass = ""
        // 密码里可能有 @，取最后一个 @ 才不会被切错
        val at = s.lastIndexOf('@')
        if (at >= 0) {
            val cred = s.substring(0, at)
            s = s.substring(at + 1)
            val c = cred.indexOf(':')
            if (c >= 0) {
                user = cred.substring(0, c)
                pass = cred.substring(c + 1)
            } else {
                user = cred
            }
        }
        if (s.isBlank()) return null

        val q = s.indexOf('?')
        val hostPart = if (q >= 0) s.substring(0, q) else s
        val query = if (q >= 0) s.substring(q) else ""
        if (hostPart.isBlank()) return null

        // 没写端口就补默认端口：turn 3478 / turns 5349
        val withPort = if (hostPart.count { it == ':' } == 0) {
            "$hostPart:${if (scheme == "turns") 5349 else 3478}"
        } else {
            hostPart
        }
        return Parsed("$scheme:$withPort$query", user, pass)
    }
}
