package io.anonchat.core

import org.json.JSONObject

/**
 * Rust core 的 JNI 入口。
 *
 * 约定：每个方法都返回统一信封的 JSON 字符串
 *   成功 {"ok":true,"data":...}
 *   失败 {"ok":false,"error":"..."}
 * 这样 Kotlin 侧不需要处理 Java 异常跨 JNI 的问题。
 */
object Native {
    init {
        System.loadLibrary("anonchat")
    }

    external fun init(dir: String, name: String?): String
    external fun identity(dir: String): String
    external fun invite(dir: String, relay: String): String
    external fun add(dir: String, code: String, alias: String?): String
    /**
     * 回邀：把刚给我发过消息的人加为联系人。
     *
     * trust 锚点是 [dh]——Rust 侧会拒绝没有任何加密会话的身份，
     * 所以 dh 只能来自真正收到过的消息，不能凭空调一个别人的公钥来加人。
     */
    external fun addFromCard(
        dir: String,
        name: String,
        dh: String,
        pk: String,
        qid: String,
        relaysJson: String,
    ): String
    /**
     * 确保自己有收件队列（收消息的唯一入口），返回队列 ID。
     *
     * 队列是中继分配的，只有生成邀请码时才会申请；只"加别人"的设备
     * 此前压根没有收件地址（收不到消息、也没人回得了）。加好友成功后补一次。
     */
    external fun ensureQueue(dir: String, relay: String): String
    external fun send(dir: String, to: String, text: String): String
    /** 发送阅后即焚的文字消息。burnSecs = 首次展示后的焚毁倒计时（秒） */
    external fun sendBurn(dir: String, to: String, text: String, burnSecs: Long): String
    /** 盲 ack 清空自己的中继收件队列（紧急清除用），返回清除条数 */
    external fun purgeQueue(dir: String): String
    /** 批量发送（文件分块）。bodiesJson 是 JSON 字符串数组，如 ["{...}","{...}"] */
    external fun sendBatch(dir: String, to: String, kind: String, bodiesJson: String): String
    external fun recvOnce(dir: String, timeoutSecs: Long): String
    external fun contacts(dir: String): String
    /** 切换中继：同步 self.relays 与所有联系人的 relays，连不上新中继会直接报错。返回受影响联系人数 */
    external fun setRelay(dir: String, url: String): String
    /** 补发上次换中继没推成功的通知（对方中继当时不可达）。返回本次成功条数 */
    external fun flushPendingRoutes(dir: String): String
    /** 把攒下的送达回执发出去。返回发出的条数（0 = 没有待发或仍失败） */
    external fun flushReceipts(dir: String): String
    /** 删除联系人（连带清 Olm 会话状态）。返回 [是否存在, 清掉的会话数] */
    external fun removeContact(dir: String, dh: String): String
    /** 设置联系人备注名，空串 = 清除 */
    external fun setContactAlias(dir: String, dh: String, alias: String): String
    external fun selfQueue(dir: String): String

    /**
     * 安全码。参数是两个 Ed25519 身份 ID（自己的 + 对方的），顺序无关。
     * 返回 60 位数字字符串，双方各自算出同一个数才算"没有中间人"。
     */
    external fun safetyCode(aPk: String, bPk: String): String

    // ------------------------------------------------------------ 通话
    //
    // 信令的编码与发送全在 Rust 侧完成（它持有会话密钥和队列），
    // Kotlin 只负责 WebRTC 本体和界面。返回的都是 CallEvent 的 JSON。

    /** 发起通话，返回 call_id */
    external fun callStart(dir: String, to: String, video: Boolean): String
    /** 主叫：本地 SDP 就绪后把 offer 发出去 */
    external fun callSetOffer(dir: String, sdp: String): String
    /** 被叫：接听（需先 setRemoteDescription + createAnswer） */
    external fun callAccept(dir: String, sdp: String): String
    /** 被叫：拒接 */
    external fun callReject(dir: String, reason: String): String
    /** 本端挂断 */
    external fun callHangup(dir: String, reason: String): String
    /** 本端产生了 ICE candidate */
    external fun callIce(dir: String, candidate: String, mid: String, mline: Int): String
    /** 换网络后重新协商（SDP 需带 IceRestart） */
    external fun callRenegotiate(dir: String, sdp: String): String

    /** 振铃超时驱动，每秒一次 */
    external fun callTick(dir: String): String
    /** 当前通话状态 */
    external fun callState(dir: String): String
    /** 结束态回到空闲 */
    external fun callReset(dir: String): String
    /** 本机昵称 */
    external fun selfName(dir: String): String

    /** 查看恢复短语（12 个词，空格分隔）。UI 必须先警告"这就是身份本体" */
    external fun backupPhrase(dir: String): String
    /** 用恢复短语恢复身份到空目录 */
    external fun restore(dir: String, phrase: String, name: String?): String
    /** 探测中继：WS 握手计时，返回毫秒 */
    external fun relayProbe(url: String): String

    /**
     * 从 Mainline DHT 自动发现中继（BEP5 get_peers）。
     *
     * 返回 JSON 数组（ws:// 地址）。**只查询、不改本地配置**，采用与否由上层决定。
     * 会阻塞到超时（秒级到几十秒），必须在后台线程调用。
     */
    external fun discoverRelays(dir: String, timeoutSecs: Long): String

    /**
     * 取 TURN 临时凭据（缓存优先，不足 5 分钟才向中继重新索取）。
     *
     * 返回 `{url,user,pass,ttl,expires_at}`，或 `null`（中继没开 TURN / 网络不通）。
     * 会发起网络请求，必须在后台线程调用。
     */
    external fun turnCreds(dir: String): String

    /** 取走 Rust 侧告警（解密失败、信令发送失败等），调用后清空 */
    external fun takeWarnings(): String

    /** 收到一条通话信令 */
    external fun callInbound(
        dir: String, fromDh: String, kind: String, body: String, msgId: String
    ): String
}

/** 统一信封解析结果 */
data class Envelope<T>(val data: T?, val error: String?) {
    val ok: Boolean get() = error == null
}

fun <T> call(block: () -> String, parse: (Any?) -> T?): Envelope<T> {
    return try {
        val raw = block()
        val o = JSONObject(raw)
        if (o.optBoolean("ok")) {
            val d = if (o.isNull("data")) null else o.get("data")
            Envelope(parse(d), null)
        } else {
            Envelope(null, o.optString("error", "unknown error"))
        }
    } catch (t: Throwable) {
        Envelope(null, "call failed: ${t.message}")
    }
}
