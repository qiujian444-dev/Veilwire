package io.anonchat.app

import java.security.MessageDigest
import kotlin.random.Random

/**
 * 紧急清除（对暗号清空）的纯逻辑。
 *
 * 语义（0.11.0 定稿）：
 * - **暗号按身份派生**：SHA-256("panic" + 排序后的双方 dh 公钥) → 6 位数字。
 *   加上好友那一刻双方自动拥有同一个暗号，零配置、每对联系人唯一、
 *   不可预测、天然对称（双方都能发起）。刻意不做全局固定暗号——
 *   全网皆知的暗号等于"任何人都能触发你的清除弹窗"。
 * - **仅发起方验证**：发起端弹算术码，答对即执行；**对端无感**——
 *   通过一条 `clear.exec` 加密消息通知对端自动清除，不弹任何确认。
 *   伪造 clear-exec 需要会话密钥，而对端本来就掌握暗号，无额外攻击面。
 * - **触发是本地的**：暗号消息不真正发出（命中即拦截、答对直接执行），
 *   暗号永远不会出现在对方设备和中继日志里。
 * - **执行顺序是硬约束**：先发 clear-exec（身份密钥还在，能加密投递）→
 *   再盲 ack 清自己的中继队列 → 最后删本机数据层（身份/联系人/会话保留，
 *   清完无缝继续聊）。对端收到 clear.exec 后同样先盲 ack 再删。
 */
object Panic {
    /** 算术码最大尝试次数，全错即取消（暗号消息不发、不清除） */
    const val MAX_ATTEMPTS = 3

    /**
     * 派生一对联系人的暗号。双方各自算出同一个值：
     * 输入按字典序排序后拼接，保证顺序无关。
     */
    fun codeOf(myDh: String, peerDh: String): String {
        val pair = listOf(myDh.trim(), peerDh.trim()).sorted()
        val digest = MessageDigest.getInstance("SHA-256")
            .digest("panic".toByteArray() + pair[0].toByteArray() + pair[1].toByteArray())
        val n = ((digest[0].toLong() and 0xFF) shl 16) or
            ((digest[1].toLong() and 0xFF) shl 8) or
            (digest[2].toLong() and 0xFF)
        return "%06d".format(n % 1_000_000)
    }

    /** 触发匹配：**完整消息精确匹配**（trim 后 equals），不做子串——防日常聊天误触发 */
    fun matches(text: String, code: String): Boolean =
        code.isNotBlank() && text.trim() == code

    /** 出一道两位数加法题。随机源由调用方注入，单测可复现 */
    fun quiz(rnd: Random): Pair<String, Int> {
        val a = rnd.nextInt(90) + 10
        val b = rnd.nextInt(90) + 10
        return "$a + $b = ?" to (a + b)
    }
}
