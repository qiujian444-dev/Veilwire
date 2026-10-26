package io.anonchat.app

import android.content.Context
import android.os.SystemClock
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import io.anonchat.core.Native
import org.json.JSONArray
import org.json.JSONObject
import java.io.File

data class Contact(
    val name: String,
    val pk: String,
    val dh: String,
    val qid: String,
    /** 本机备注名（只在本人设备上生效）。空串 = 用对方自己的昵称 */
    val alias: String = "",
    /**
     * 最近一次收到对方任何东西的本地时间（秒，0 = 从没收到）。
     * 只能推出"最近活跃过"，推不出"现在在线"——后者要向中继查询，
     * 那会把社交图谱交给运营者。
     */
    val lastSeen: Long = 0,
) {
    /** 界面上该显示的名字：备注优先 */
    val display: String get() = alias.ifBlank { name }
}

/** 换中继的结果：通知到几个 / 应该通知几个 */
data class RelaySwitch(val notified: Int = 0, val total: Int = 0)

data class Msg(
    val text: String,
    val ts: Long,
    val outgoing: Boolean,
    /**
     * "text" 普通消息；"call" 通话记录（居中灰字，不算未读）；
     * "file" 文件；"voice" 语音；"image" 图片；"video" 视频。
     *
     * 后四种的 text 字段都是一段 JSON 摘要——它们走的是同一条分块传输链路，
     * 区别只在 kind（决定气泡长什么样）和摘要里多出来的 dur / w / h。
     */
    val kind: String = "text",
    /**
     * 本地消息 id（MsgStore 分配，只在本机有意义）。
     * 重发成功回填状态、将来做删除/转发都靠它定位到具体某一行。
     */
    val id: Long = 0,
    /**
     * 发送失败（目前只有文字消息会落到这个状态）。
     * 文件/语音/图片发送失败根本不落记录，直接弹错误——它们的来源
     * （相册/文档）可能已经不在，没有"重发"可谈。
     */
    val failed: Boolean = false,
    /**
     * 撤回引用：发送方生成、随消息传输，两边设备一致（详见 Recall.kt）。
     * 文字消息来自加密载荷的 id；文件类消息就是分块摘要里的 fid。
     * 空串 = 不可撤回（发送失败的、太老的没带 rid 的消息）。
     */
    val rid: String = "",
    /**
     * 阅后即焚档位（秒）。0 = 普通消息。
     * 双端各自首次展示时记 seenAt（见下），到期后本地删除。
     */
    val burn: Long = 0,
    /**
     * 首次展示的本地时间戳（秒）。0 = 还没展示过、计时未开始。
     * 必须落盘：App 被杀期间倒计时不能凭空重来，回放时读出来继续算。
     */
    val seenAt: Long = 0,
    /**
     * 对方设备已确认收到（双勾）。只有自己发的**文字**消息会点亮：
     * 文件分块不回执（几百块逐块回执纯浪费），通话/撤回也不需要。
     * 没点亮 = 单勾（已投递到对方中继）；failed = 感叹号（连中继都没投进去）。
     */
    val delivered: Boolean = false,
) {
    /**
     * 会话列表里那一行预览的**结构**（不是文案）。
     *
     * 文件/语音消息的 text 是一段 JSON 概要，直接拿去当预览会在列表上显示
     * `{"fid":"77f49602","name":"big20.bin",...}` —— 模拟器验收时抓到的。
     *
     * 为什么不给现成字符串：预览是显示文案，必须随系统语言变。数据层不该
     * 拼好中文再塞给界面（那等于把语言写死在存储层）。这里只给"是什么类型的
     * 消息 + 必要参数"，具体句子由 UI 层按当前语言生成（见 MainActivity.previewText）。
     */
    val preview: Preview
        get() {
            val base = when (kind) {
                "recall" -> Preview.Recall(byMe = outgoing)
                "file" -> {
                    val n = runCatching { JSONObject(text).optString("name") }.getOrDefault("")
                    Preview.File(name = n)
                }
                "voice" -> Preview.Voice(durMs = Voice.parse(text)?.durMs ?: 0L)
                "image" -> Preview.Image()
                "video" -> Preview.Video()
                "call" -> {
                    val o = runCatching { JSONObject(text) }.getOrNull()
                    if (o == null) Preview.Text(text) // 老数据是拼好的中文，原样显示
                    else Preview.Call(
                        reason = o.optString("r"),
                        durSec = o.optLong("d", 0L),
                        fromPeer = o.optBoolean("fp"),
                    )
                }
                else -> Preview.Text(text)
            }
            // "未发出"是显示层的后缀，不是消息本身的一部分
            if (!failed || !outgoing) return base
            return when (base) {
                is Preview.Text -> base.copy(failed = true)
                is Preview.File -> base.copy(failed = true)
                is Preview.Voice -> base.copy(failed = true)
                is Preview.Image -> base.copy(failed = true)
                is Preview.Video -> base.copy(failed = true)
                is Preview.Recall -> base.copy(failed = true)
                // 通话记录不存在"发送失败"这个状态：它是本机写的
                is Preview.Call -> base
            }
        }
}

/**
 * 会话列表预览的结构化描述。
 *
 * [failed] 单独拎出来：它只是给预览加个后缀，不改变消息类型。
 */
sealed interface Preview {
    val failed: Boolean get() = false

    /** 普通文字消息 */
    data class Text(val text: String, override val failed: Boolean = false) : Preview

    /** 文件消息。[name] 为空表示对方没给文件名 */
    data class File(val name: String, override val failed: Boolean = false) : Preview

    /** 语音消息 */
    data class Voice(val durMs: Long, override val failed: Boolean = false) : Preview

    /** 图片（没有可变化的字段，所以用 object） */
    data class Image(override val failed: Boolean = false) : Preview

    /** 视频 */
    data class Video(override val failed: Boolean = false) : Preview

    /** 撤回提示。[byMe] = 是我撤的 */
    data class Recall(val byMe: Boolean, override val failed: Boolean = false) : Preview

    /**
     * 通话记录。
     *
     * 必须单独一个分支：它的 text 是结构化 JSON（`{"r":...}`），
     * 落到 Text 分支就会把 `{"r":"local_ended","fp":false,"d":8}` 原样显示在
     * 会话列表里——模拟器验收时就是这么撞见的。具体句子由 UI 按语言拼。
     */
    data class Call(val reason: String, val durSec: Long, val fromPeer: Boolean) : Preview
}

data class Incoming(
    val fromName: String?,
    val fromId: String,
    val text: String,
    val ts: Long,
    val dh: String,
    val kind: String = "text",
    val body: String? = null,
    val msgId: String = "",
    /** 发送方载荷里的消息 id，撤回引用用（详见 Recall.kt） */
    val rid: String = "",
    /** 阅后即焚档位（秒）。文字消息从 body 解析；0 = 普通消息 */
    val burn: Long = 0,
    /** 对方自报的显示名。陌生人会话里这是唯一能显示给人看的称呼 */
    val selfName: String? = null,
    /** 回信要投到对方的哪个队列 */
    val replyQid: String? = null,
    /** 回信可用的中继候选（投递中继 + 对方自报的中继） */
    val replyRelays: List<String> = emptyList(),
) {
    val isText get() = kind == "text"
    val isCall get() = kind.startsWith("call.")
    val isFile get() = kind == "file"

    /** 语音走的是同一套分块链路，只有 kind 和摘要字段多一个 dur 的区别 */
    val isVoice get() = kind == "voice"

    /** 图片、视频：同样是分块链路，摘要里多了 w/h（视频还有 dur） */
    val isImage get() = kind == "image"
    val isVideo get() = kind == "video"

    /** 凡是"拆成多块传、收齐才落一条气泡"的消息 */
    val isBlob get() = isFile || isVoice || isImage || isVideo

    /** 对方有没有留下足够线索让我回过去（老版本客户端不带） */
    val canReply get() = !replyQid.isNullOrBlank() && replyRelays.isNotEmpty()
}

/**
 * 陌生人的"回信名片"。
 *
 * 为什么必须存下来：这条信息只存在于收到消息的那一刻——载荷里带的一次性线索，
 * 消息落盘后就不再有了。不记住它，用户点"加为联系人"时只能干瞪眼等对方再发一条。
 *
 * 信任边界：只有 [dh] 是可信的（加密层确认过身份），其余字段都是对方自报的。
 */
data class PeerCard(
    val name: String,
    val pk: String,
    val dh: String,
    val qid: String,
    val relays: List<String>,
    /**
     * 用户是否当面核对过安全码。
     *
     * 存这个而不是"重新算一遍没问题"：核对是一次社交行为，机器无从判断。
     * 只有用户点过"我已核对"才算数，所以它是**用户声明的状态**，
     * 一旦对方换了身份公钥（安全码变了），这个标记必须自动作废。
     */
    val verified: Boolean = false,
    /** 核对时的安全码。用来发现"对方换身份了、我之前核对的已经不是这个人" */
    val verifiedCode: String = "",
)

/**
 * 中继签发的 TURN 临时凭据。
 *
 * 为什么要临时凭据而不是固定账号：DHT 只公布 TURN 的 IP 和端口
 * （BEP5 记录固定 6 字节），放不下账号密码；而固定凭据硬编码进 App
 * 等于把 TURN 变成公开免费中继。所以由中继在 WS 通道里按需签发，
 * 过期自动作废，也能按账号限速。
 */
data class TurnCredsData(
    val url: String,
    val user: String,
    val pass: String,
    val ttl: Long,
    val expiresAt: Long,
) {
    /** 剩余有效秒数 */
    val remaining: Long get() = (expiresAt - System.currentTimeMillis() / 1000).coerceAtLeast(0)
}

/** Rust 侧通话状态机交回的事件 */
data class CallEvent(
    val state: String,
    val callId: String,
    val peerDh: String,
    val video: Boolean,
    val reason: String,
    val remoteSdp: String?,
    val iceCandidate: String?,
    val iceMid: String,
    val iceMline: Int,
    /** 正在重新协商（对端换网络，通话没断） */
    val renegotiating: Boolean = false,
)

/**
 * 结果包装：拿 data 或看 error。
 *
 * 必须是 data class —— 调用点用 `val (data, err) = ...` 解构，
 * 普通 class 不会生成 componentN()。
 */
/**
 * 操作结果。
 *
 * [error] 是 Rust 核心转发的**技术信息**（不本地化，日志/调试用）；
 * [err] 是应用层结构化错误，文案由 UI 层按当前语言生成（见 MainActivity.errText）。
 * 两者一般只会有一个：核心转发走 error，应用自身的校验失败走 err。
 */
data class Res<T>(val data: T?, val error: String? = null, val err: AppError? = null) {
    val ok get() = error == null && err == null
}

/**
 * 应用侧的数据层。
 *
 * 所有加密/协议相关的事都在 Rust 里，这里只负责：
 * - 拼参数、解信封
 * - 消息历史（明文，仅供 UI 展示用；真正的密钥材料在 filesDir/device 下）
 */
object Repo {
    private lateinit var deviceDir: File

    /**
     * 保存到「下载」目录要用 ContentResolver，得留个 ApplicationContext。
     * 只用于 FileSaver，别拿它干别的事（尤其别存到能活过进程的静态变量里做 UI 操作）。
     */
    private lateinit var appCtx: Context

    var relay: String = ""
        private set

    /** 消息历史（append-only 日志，自带同步）。见 [MsgStore] 为什么不再整文件重写 */
    private lateinit var store: MsgStore

    /**
     * 未读/名片表的读写锁：写发生在收消息的 IO 线程，读主要在 UI 线程
     * （Compose 重组时实时刷新会频繁读）。不加锁的话并发迭代会偶发
     * ConcurrentModificationException，而且这种崩溃只在"正在看聊天页时
     * 恰好来消息"出现，极难复现。
     *
     * 消息历史本身由 [MsgStore] 内部同步，不再用这把锁。
     */
    private val msgLock = Any()

    /** 未读数：who → 条数。正开着这个会话（前台）时不计数 */
    private val unread = HashMap<String, Int>()

    fun markUnread(who: String) {
        if (MainActivity.resumed && MainActivity.openPeer == who) return
        synchronized(msgLock) { unread[who] = (unread[who] ?: 0) + 1 }
    }

    fun clearUnread(who: String) {
        synchronized(msgLock) { unread.remove(who) }
    }

    fun unreadOf(who: String): Int = synchronized(msgLock) { unread[who] ?: 0 }

    fun totalUnread(): Int = synchronized(msgLock) { unread.values.sum() }

    /**
     * 初始化。**幂等且线程安全**：Activity、前台服务、开机接收器都可能调用它，
     * 且 BootReceiver 在 IO 线程调、Activity 在主线程调——两边并发进入时
     * 早期 `if (ready) return` 会双双放行，产生两个 MsgStore 指向同一个
     * messages.log（双份后台线程、rewrite 互踩、最坏整份历史损坏）。
     * 双检锁保证只初始化一次。
     */
    @Volatile
    private var ready = false
    private val initLock = Any()

    fun init(ctx: Context) {
        if (ready) return
        synchronized(initLock) {
            if (ready) return
            appCtx = ctx.applicationContext
            deviceDir = File(ctx.filesDir, "device")
            store = MsgStore(ctx.filesDir)
            cardFile = File(ctx.filesDir, "peer_cards.json")
            relay = ctx.getSharedPreferences("anon", Context.MODE_PRIVATE)
                .getString("relay", "") ?: ""
            // 载入历史（顺便把老版本 messages.json 迁过来）；压缩放到后台，
            // 别让它拖慢冷启动——它只在日志膨胀时才真干活。
            store.load()
            // 0.12.0 起会话键从"显示名"改成 dh。这里做一次性迁移：
            // 旧日志里 `w` 是名字，靠联系人表 + 陌生人来信卡片（都带 dh）反查。
            // 找不到映射的（比如对方卡片也没了）保留原名，不丢数据。
            runCatching { migrateConversationKeys() }
            Thread { runCatching { sweepOrphanParts() } }.start()
            Thread { runCatching { store.compactIfNeeded() } }.start()
            // 启动清扫：被杀期间到期的焚毁消息，回来第一件事就是删掉
            Thread { runCatching { sweepExpired() } }.start()
            loadCards()
            ready = true
        }
    }

    /** 名字 → dh 的映射，用于把旧会话键迁移过来 */
    private fun migrateConversationKeys() {
        val map = HashMap<String, String>()
        runCatching { contacts() }.getOrDefault(emptyList()).forEach { c ->
            if (c.name.isNotBlank() && c.dh.isNotBlank()) map[c.name] = c.dh
        }
        synchronized(msgLock) {
            peerCards.forEach { (k, card) ->
                if (card.dh.isNotBlank()) {
                    // 联系人优先：陌生人的自报名与某个联系人显示名相同时
                    // （昵称重名很常见），后者不能覆盖前者——否则存量"名字键"
                    // 会话会被迁到错误的人名下（P3）。putIfAbsent 保证先写入的
                    // 联系人映射获胜。
                    map.putIfAbsent(k, card.dh)
                    if (card.name.isNotBlank()) map.putIfAbsent(card.name, card.dh)
                }
            }
        }
        val n = store.migrateKeysToDh(map)
        if (n > 0) android.util.Log.i("AnonChat", "conversation keys migrated to dh: $n")
    }

    /** 通话用的 ICE 服务器配置，都在 SharedPreferences 里 */
    fun iceConfig(ctx: Context): Map<String, String> {
        val p = ctx.getSharedPreferences("anon", Context.MODE_PRIVATE)
        return mapOf(
            "stun" to (p.getString("stun", "") ?: ""),
            "turn" to (p.getString("turn", "") ?: ""),
            "turn_user" to (p.getString("turn_user", "") ?: ""),
            "turn_pass" to (p.getString("turn_pass", "") ?: ""),
        )
    }

    fun setIceConfig(ctx: Context, stun: String, turn: String, user: String, pass: String) {
        ctx.getSharedPreferences("anon", Context.MODE_PRIVATE).edit()
            .putString("stun", stun.trim())
            .putString("turn", turn.trim())
            .putString("turn_user", user.trim())
            .putString("turn_pass", pass.trim())
            .apply()
    }

    /**
     * 切换中继地址。
     *
     * **必须走 Rust**：中继地址在磁盘上有三份（self.json 的 relays、
     * 每个联系人的 relays、App 的 SharedPreferences）。只改 SP 的话消息仍按
     * 联系人的旧地址投，表现为"显示已保存但怎么都不通"——2026-10-22 实测踩到，
     * 当时只能清数据重装。Rust 侧会先验证新中继可达再落盘。
     *
     * 旧中继队列里未 ack 的消息**不会迁移**，调用方需要先向用户确认。
     */
    fun setRelay(ctx: Context, url: String): Res<RelaySwitch> {
        val (d, e) = envelope(Native.setRelay(dir, url.trim()))
        if (e == null) {
            relay = url.trim()
            ctx.getSharedPreferences("anon", Context.MODE_PRIVATE)
                .edit().putString("relay", relay).apply()
        }
        val o = d as? org.json.JSONObject
        return Res(
            o?.let { RelaySwitch(it.optInt("notified"), it.optInt("total")) },
            e
        )
    }

    /**
     * 补发换中继通知。切换时对方中继可能正好不可达，那条通知会排队在
     * `pending_routes.json` 里，由收消息循环定期重试——否则对方永远不知道
     * 我的新地址，之后他一直往废弃队列发，两边都看不出问题。
     */
    fun flushPendingRoutes(): Int {
        val (d, _) = envelope(Native.flushPendingRoutes(dir))
        return (d as? Number)?.toInt() ?: 0
    }

    /** 把攒下的"已到达"回执发出去（对方拿它点亮双勾） */
    fun flushReceipts(): Int {
        val (d, _) = envelope(Native.flushReceipts(dir))
        return (d as? Number)?.toInt() ?: 0
    }

    /**
     * 对方确认收到（`rcpt` 回执）→ 把对应的自己发出的消息标为已送达。
     * 返回点亮的条数（0 = 没有匹配的消息，无需刷新界面）。
     */
    fun markDelivered(peer: String, rids: List<String>): Int {
        if (rids.isEmpty()) return 0
        return store.markDelivered(peer, rids)
    }

    /** 删除联系人：Rust 侧删 contacts 条目 + Olm 会话；会话记录与文件副本由调用方另行清理 */
    fun removeContact(dh: String): Res<Unit> {
        val (_, e) = envelope(Native.removeContact(dir, dh))
        return Res(if (e == null) Unit else null, e)
    }

    /** 设置备注名（空串清除） */
    fun setAlias(dh: String, alias: String): Res<String?> {
        val (d, e) = envelope(Native.setContactAlias(dir, dh, alias))
        return Res(d as? String, e)
    }

    /**
     * 只删聊天记录（保留联系人）：消息 + 该会话的文件副本 + 陌生人来信卡片 + 未读。
     * 返回删掉的消息条数。
     *
     * 未读必须一起清：删联系人后该键从列表消失，clearUnread 永远没有调用机会，
     * 导航角标会永久计入一个已不存在的会话（只能清数据才能消掉）。
     */
    fun deleteConversation(key: String): Int {
        val msgs = store.slice(key, 0, Int.MAX_VALUE)
        msgs.forEach { m -> deleteBlobFiles(m) }
        val n = synchronized(msgLock) {
            store.dropConversation(key)
            peerCards.remove(key)
            unread.remove(key)
            0 + msgs.size
        }
        saveCards()
        return n
    }

    /**
     * 删除联系人（连带聊天记录与文件）。
     *
     * 顺序：先删联系人（Rust 侧同时清 Olm 会话），再删本地记录。
     * 反过来的话，如果 Rust 删失败，本地记录已经没了但联系人还在——
     * 用户会看到"联系人还在但记录空了"，比失败更困惑。
     */
    fun deleteContact(dh: String): Res<Int> {
        val r = removeContact(dh)
        if (!r.ok) return Res(null, r.error)
        val n = deleteConversation(dh)
        return Res(n, null)
    }

    /**
     * 本地删除消息（只清自己这边，不通知对方）。
     * 与「撤回」的区别：撤回会发指令让对方也删；删除只影响本机记录与文件副本。
     */
    fun deleteMessages(who: String, ids: Collection<Long>): Int {
        if (ids.isEmpty()) return 0
        val msgs = store.slice(who, 0, Int.MAX_VALUE).filter { it.id in ids }
        msgs.forEach { deleteBlobFiles(it) }
        return store.deleteMany(who, ids)
    }

    /**
     * 转发消息到另一个会话：文本原样发，文件类按本机副本重发。
     * 返回 (成功条数, 跳过条数)——文件副本不在本机的会被跳过（旧版本消息没留副本）。
     */
    fun forward(to: String, msgs: List<Msg>): Res<Pair<Int, Int>> {
        var ok = 0
        var skip = 0
        // 记住第一个真实失败：全失败时报它，而不是一律"文件副本不在本机"。
        // 纯文本因中继断连失败时，那个错误码会误导用户去查文件（P3）。
        var firstErr: String? = null
        for (m in msgs) {
            when {
                m.kind == "text" -> {
                    val r = send(to, m.text)
                    if (r.ok) ok++ else {
                        skip++
                        if (firstErr == null) firstErr = r.error
                    }
                }
                m.kind in setOf("file", "voice", "image", "video") -> {
                    val v = runCatching { JSONObject(m.text) }.getOrNull()
                    val p = v?.optString("path")?.takeIf { it.isNotBlank() }
                    val f = p?.let { File(it) }
                    if (f == null || !f.exists()) {
                        skip++
                        continue
                    }
                    val r = sendFile(
                        to = to,
                        name = v?.optString("name") ?: f.name,
                        size = f.length(),
                        open = { f.inputStream() },
                        kind = m.kind,
                        durMs = v?.optLong("dur", 0) ?: 0,
                        width = v?.optInt("w", 0) ?: 0,
                        height = v?.optInt("h", 0) ?: 0,
                    )
                    if (r.ok) ok++ else {
                        skip++
                        if (firstErr == null) firstErr = r.error
                    }
                }
                // 通话记录、撤回灰字没有转发意义
                else -> skip++
            }
        }
        return Res(
            ok to skip,
            err = when {
                ok > 0 -> null
                // 有真实发送错误就透传（网络/中继问题），否则才是"没有可转发的文件"
                firstErr != null -> AppError(ErrKind.CORE_FAILURE, firstErr)
                else -> AppError(ErrKind.FORWARD_NO_FILE)
            }
        )
    }

    val dir: String get() = deviceDir.absolutePath

    fun hasIdentity(): Boolean = File(deviceDir, "device.key").exists()

    // ---------------------------------------------------------------- 身份

    fun createIdentity(name: String?): Res<String> {
        val (d, e) = envelope(Native.init(dir, name))
        if (e == null) ensureQueue()   // 有中继就顺手建收件队列
        return Res(d as? String, e)
    }

    fun identity(): String? {
        val (d, _) = envelope(Native.identity(dir))
        return d as? String
    }

    fun invite(): Res<String> {
        if (relay.isBlank()) return Res(null, err = AppError(ErrKind.RELAY_NOT_SET))
        val (d, e) = envelope(Native.invite(dir, relay))
        return Res(d as? String, e)
    }

    fun add(code: String, alias: String?): Res<String> {
        // 合并重装身份：同一身份（pk）换了 dh 时，Rust 侧会把联系人条目换成新 dh。
        // 这里记下添加前同 pk 条目的旧 dh，添加成功后把旧 dh 键下的会话历史
        // 一并搬到新 dh 下——否则旧会话会以"未加联系人的陌生人"残留在列表里，
        // 而它对应的路由早已是死信箱。
        val before = contacts().filter { it.pk.isNotBlank() }.associateBy { it.pk }
        val (d, e) = envelope(Native.add(dir, code.trim(), alias))
        if (e == null) {
            for (c in contacts()) {
                val old = before[c.pk]
                if (old != null && old.dh != c.dh) {
                    moveConversation(old.dh, c.dh, renameCard = false)
                }
            }
            ensureQueue()
        }
        return Res(d as? String, e)
    }

    /**
     * 确保自己有收件队列。失败不致命：离线加好友/离线启动都该照常工作，
     * 下次启动或下次加好友会再试。
     */
    fun ensureQueue(): Res<String> {
        if (relay.isBlank() || !hasIdentity()) {
            return Res(null, err = AppError(ErrKind.RELAY_OR_IDENTITY_MISSING))
        }
        val (d, e) = envelope(Native.ensureQueue(dir, relay))
        return Res(d as? String, e)
    }

    fun contacts(): List<Contact> {
        val (d, _) = envelope(Native.contacts(dir))
        if (d !is JSONArray) return emptyList()
        return (0 until d.length()).mapNotNull { i ->
            val o = d.optJSONObject(i) ?: return@mapNotNull null
            Contact(
                name = o.optString("name"),
                pk = o.optString("pk"),
                dh = o.optString("dh"),
                qid = o.optString("qid"),
                alias = o.optString("alias"),
                lastSeen = o.optLong("last_seen"),
            )
        }
    }

    /** 某联系人最近一次活跃的本地时间（0 = 从没收到过） */
    fun lastSeenOf(dh: String): Long = contacts().firstOrNull { it.dh == dh }?.lastSeen ?: 0

    fun selfName(): String? {
        val (d, _) = envelope(Native.selfName(dir))
        return d as? String
    }

    /** 恢复短语（12 个词）。调用方负责先警告用户 */
    fun backupPhrase(): Res<String> {
        val (d, e) = envelope(Native.backupPhrase(dir))
        return Res(d as? String, e)
    }

    fun restore(phrase: String, name: String?): Res<String> {
        val (d, e) = envelope(Native.restore(dir, phrase, name))
        return Res(d as? String, e)
    }

    /** 中继探测：WS 握手耗时（毫秒）。失败返回 error */
    fun relayProbe(url: String): Res<Long> {
        val (d, e) = envelope(Native.relayProbe(url))
        // 必须是 `as? Number` 再 toLong()：org.json 把能放进 Int 的数
        // （比如 45ms）解析成 Integer，`as? Long` 会得到 null ——
        // 于是**每一次**探测都变成"不可达"，中继列表里永远全是 Unreachable。
        // 这个 bug 之前一直藏着，因为列表原本是空的，没人看见过探测结果。
        return Res((d as? Number)?.toLong(), e)
    }

    /**
     * 从 Mainline DHT 自动发现中继地址。
     *
     * 只返回候选，**不写任何本地配置**：调用方拿到后应当先测速，
     * 再决定要不要采用（采用要走 [setRelay]）。
     * 阻塞式调用，必须在 IO 线程。
     */
    /**
     * 中继下发的 TURN 临时凭据。
     *
     * 中继没开 TURN、或网络不通时返回 null：调用方应当回退到用户手填的配置。
     * 阻塞式调用（可能连一次中继），必须在 IO 线程。
     */
    /**
     * 只读本地缓存的 TURN 凭据（`turn.json`），**不发任何网络请求**。
     *
     * 存在的意义：进程重启后内存里的凭据没了，但磁盘上那份可能还有效。
     * 早先 `CallManager` 只认内存，重启后要等一次成功的网络刷新才有 TURN——
     * 那次刷新要是失败（刚开机没网），就会一直卡在"没有 TURN"，通话必然不通，
     * 只有再重启一次才可能恢复。
     */
    fun turnCredsCached(): TurnCredsData? {
        val f = File(deviceDir, "turn.json")
        if (!f.exists()) return null
        return runCatching {
            val o = JSONObject(f.readText())
            TurnCredsData(
                url = o.optString("url"),
                user = o.optString("user"),
                pass = o.optString("pass"),
                ttl = o.optLong("ttl"),
                expiresAt = o.optLong("expires_at"),
            ).takeIf { it.url.isNotBlank() && it.user.isNotBlank() }
        }.getOrNull()
    }

    fun turnCreds(): Res<TurnCredsData?> {
        val (d, e) = envelope(Native.turnCreds(dir))
        if (d !is JSONObject) return Res(null, e)
        return Res(
            TurnCredsData(
                url = d.optString("url"),
                user = d.optString("user"),
                pass = d.optString("pass"),
                ttl = d.optLong("ttl"),
                expiresAt = d.optLong("expires_at"),
            ),
            e,
        )
    }

    fun discoverRelays(timeoutSecs: Long = 45): Res<List<String>> {
        val (d, e) = envelope(Native.discoverRelays(dir, timeoutSecs))
        if (d !is JSONArray) return Res(emptyList(), e)
        return Res((0 until d.length()).mapNotNull { d.optString(it).takeIf { s -> s.isNotBlank() } }, e)
    }

    fun selfQueue(): String? {
        val (d, _) = envelope(Native.selfQueue(dir))
        return d as? String
    }

    // ---------------------------------------------------------------- 收发

    /**
     * 发一条文字消息。
     *
     * **失败也要落记录**（failed=true）：之前只在成功时落盘，失败只在输入栏上方
     * 提示 4 秒——消息直接消失，用户根本不知道对方没收到。现在失败的消息留在
     * 列表里，气泡带红色感叹号，点一下就能重发。
     *
     * 成功时 Rust 返回载荷 ID（rid）：撤回功能靠它，存进 Msg 两边才能对上。
     * [burn] > 0 时是阅后即焚消息（走 sendBurn，参数随密文传输）。
     */
    fun send(to: String, text: String, burn: Long = 0): Res<Unit> {
        val (d, e) = if (burn > 0) {
            envelope(Native.sendBurn(dir, to, text, burn))
        } else {
            envelope(Native.send(dir, to, text))
        }
        val now = System.currentTimeMillis() / 1000
        if (e == null) {
            append(to, Msg(text, now, outgoing = true, rid = d as? String ?: "", burn = burn))
        } else {
            // 失败消息也要记住 burn 档位：重发时按原档重发。丢掉它的话，
            // "失败→重发"之后阅后即焚会静默变成永久消息（隐私语义变更，
            // 用户无从发现——二轮审计 P2）。
            append(to, Msg(text, now, outgoing = true, failed = true, burn = burn))
        }
        return Res(if (e == null) Unit else null, e)
    }

    /**
     * 重发一条失败的消息。
     *
     * 成功后**删掉失败那条、在末尾追加一条新的**（带新 rid）而不是原位回填：
     * 重发是第二次传输，对方收到的是新载荷新 id，原位的旧记录留着 rid 也是空的，
     * 撤回时对不上。排到末尾和微信"重发"的观感一致。
     * 焚毁档位随失败记录保留，重发按原档走（burn > 0 走 sendBurn）。
     */
    fun resend(who: String, m: Msg): Res<Unit> {
        if (!m.failed) return Res(Unit, null)
        if (m.id == 0L) return Res(null, err = AppError(ErrKind.RESEND_TOO_OLD))
        val (d, e) = if (m.burn > 0) {
            envelope(Native.sendBurn(dir, who, m.text, m.burn))
        } else {
            envelope(Native.send(dir, who, m.text))
        }
        if (e != null) return Res(null, e)
        store.deleteById(who, m.id)
        append(
            who,
            Msg(m.text, System.currentTimeMillis() / 1000, outgoing = true, rid = d as? String ?: "", burn = m.burn)
        )
        return Res(Unit, null)
    }

    // ---------------------------------------------------------------- 撤回

    /** 这条消息现在能不能撤回（UI 用）：自己发的、带 rid、在 10 分钟窗口内 */
    fun canRecall(m: Msg): Boolean =
        m.outgoing && !m.failed && m.rid.isNotBlank() &&
            m.kind in setOf("text", "file", "voice", "image", "video") &&
            Recall.withinWindow(m.ts, System.currentTimeMillis() / 1000)

    /**
     * 撤回一条消息：给对方发 kind=recall（body 带 rid），成功后本地原位替换成灰字。
     * 窗口在发送方校验；接收端不二次校验（消息本来就到了，晚到的撤回也只是把
     * 已显示的换掉，没有"没收到的消息被撤走"的泄露问题）。
     */
    fun recall(who: String, m: Msg): Res<Unit> {
        if (!m.outgoing) return Res(null, err = AppError(ErrKind.RECALL_NOT_OUTGOING))
        if (m.rid.isBlank()) return Res(null, err = AppError(ErrKind.RECALL_NO_RID))
        if (!Recall.withinWindow(m.ts, System.currentTimeMillis() / 1000)) {
            return Res(null, err = AppError(ErrKind.RECALL_WINDOW_PASSED))
        }
        val body = JSONObject().put("rid", m.rid).toString()
        val (_, e) = envelope(
            Native.sendBatch(dir, who, "recall", JSONArray().put(body).toString())
        )
        // e 一定非空（进 if 的条件），中文兜底是死代码
        if (e != null) return Res(null, e)
        applyRecall(who, m.rid, byMe = true)
        return Res(Unit, null)
    }

    /** 收到对方的撤回消息：按 rid 找到本地那条，替换成灰字；找不到就忽略 */
    private fun handleIncomingRecall(who: String, inc: Incoming) {
        val rid = runCatching { JSONObject(inc.body ?: "").optString("rid") }.getOrDefault("")
        if (rid.isBlank()) return
        applyRecall(who, rid, byMe = false)
    }

    /** 本地执行撤回：换灰字 + 把撤掉的消息引用的文件副本删干净（不留"已撤回"的尸体） */
    private fun applyRecall(who: String, rid: String, byMe: Boolean) {
        val orig = store.recallByRid(who, rid, byMe) ?: return
        deleteBlobFiles(orig)
    }

    /** 删掉文件类消息引用的本机副本（私有目录 + 已存下载的那份）。普通文件之外都算 */
    private fun deleteBlobFiles(m: Msg) {
        if (m.kind !in setOf("file", "voice", "image", "video")) return
        val v = runCatching { JSONObject(m.text) }.getOrNull() ?: return
        listOfNotNull(
            v.optString("path").takeIf { it.isNotBlank() },
            v.optString("saved").takeIf { it.isNotBlank() },
        ).forEach { p -> runCatching { File(p).delete() } }
    }

    // ---------------------------------------------------------------- 阅后即焚

    /** 记录首次展示时刻（阅后即焚计时起点）。幂等：重复调无害 */
    fun markSeen(who: String, m: Msg) {
        if (m.burn <= 0 || m.seenAt > 0) return
        store.markSeen(who, m.id, System.currentTimeMillis() / 1000)
    }

    /**
     * 扫描并焚毁到期的消息，返回焚毁条数。
     *
     * 调用时机：收消息循环每轮、聊天页每秒 tick、App 启动。三层兜底保证
     * 无论哪条路径活着，到期的消息都活不过下一个检查点。
     * [who] 传 null 时扫全部会话（启动/服务循环用），传会话名只扫一个（聊天页用）。
     */
    fun sweepExpired(who: String? = null): Int {
        val now = System.currentTimeMillis() / 1000
        val targets = synchronized(msgLock) {
            val keys = if (who != null) listOf(who) else store.keys()
            keys.flatMap { k ->
                store.slice(k).filter { Burn.expired(it.burn, it.seenAt, now) }.map { k to it }
            }
        }
        targets.forEach { (k, m) ->
            store.deleteById(k, m.id)
            deleteBlobFiles(m)
        }
        if (who == null) sweepStaleFileRx()
        return targets.size
    }

    /**
     * 清理死亡的在途传输：超过 [FILE_RX_TIMEOUT_MS] 没有新块到达的条目
     * 连同 .part 文件一起删。fileRx 是纯内存表、无断点续传——收一半进程
     * 被杀后重启用 BitSet 归零的空表接续，got 永远到不了 total，进度行会
     * 永久卡在半途（二轮审计 P2）。把死传输变成干净的失败。
     * 挂在 sweepExpired 的调用节奏上（收消息循环每轮/聊天页每秒），无需独立线程。
     */
    private fun sweepStaleFileRx() {
        val now = SystemClock.elapsedRealtime()
        val dead = synchronized(msgLock) {
            val it = fileRx.entries.iterator()
            val out = mutableListOf<RxState>()
            while (it.hasNext()) {
                val e = it.next()
                if (now - e.value.first.lastChunkAt > FILE_RX_TIMEOUT_MS) {
                    out.add(e.value.first)
                    it.remove()
                }
            }
            out
        }
        if (dead.isNotEmpty()) {
            dead.forEach { runCatching { it.raf?.close() } }
            dead.forEach { runCatching { it.tmp.delete() } }
            bumpFileRx()
            android.util.Log.w("AnonChat", "file transfer ${dead.joinToString { it.fid }} timed out, cleaned")
        }
    }

    /** 启动清扫：inbox 下残留的 .part 半成品（进程被杀留下的）直接删 */
    private fun sweepOrphanParts() {
        runCatching {
            val dir = File(deviceDir.parentFile, "inbox")
            dir.listFiles { f -> f.name.startsWith(".") && f.name.endsWith(".part") }
                ?.forEach { it.delete() }
        }
    }

    // -------------------------------------------------------------- 文件传输
    //
    // 设计（docs/协议规范-v0.md §任务⑤）：不引入新通道，文件拆成多条 kind="file"
    // 的消息，body 为 JSON {fid, name, size, idx, total, data(base64)}。
    // Rust 侧 send_batch 一次连接推一批块（逐块 send 每块都要一次 WS 握手，不可用）。

    /** 单块原始数据大小。24KB → base64 后约 32KB，加密后仍远小于中继 64KB 单条上限 */
    private const val FILE_CHUNK = 24 * 1024
    /**
     * 单次 sendBatch 推多少块。
     *
     * 上限来自中继的速率限制：每连接 120 条/分钟，且每条新连接重新计数。
     * 一次批推就是一个连接，所以要压在这条线以内——40 块留了三倍余量。
     * （改大反而会被限流，表现为"传一半失败"。）
     */
    private const val FILE_BATCH = 40
    /**
     * 文件大小上限：20MB。
     *
     * 为什么不是更大：文件走的是消息通道，20MB 要拆成约 854 条消息塞进中继队列，
     * 收发两端都要逐条解密。再大就不该走这条路了（该上 DataChannel / 断点续传）。
     */
    const val FILE_MAX_BYTES = 20L * 1024 * 1024

    /** 接收中的文件进度。语音/图片/视频也走这条，靠 [kind] 区分文案与落盘策略 */
    data class FileRx(
        val who: String,
        val fid: String,
        val name: String,
        val total: Int,
        val got: Int,
        val size: Long,
        val kind: String = "file",
        val durMs: Long = 0,
        val w: Int = 0,
        val h: Int = 0,
        val burn: Long = 0,
    ) {
        val isVoice: Boolean get() = kind == "voice"
        val isImage: Boolean get() = kind == "image"
        val isVideo: Boolean get() = kind == "video"
        val percent: Int get() = if (total <= 0) 0 else (got * 100 / total).coerceIn(0, 100)
        /** 已收到的字节（最后一块可能不满，按 size 截断） */
        val bytes: Long get() = (got.toLong() * FILE_CHUNK).coerceAtMost(size)
    }

    /**
     * 接收中的文件状态。
     *
     * 块**直接写进临时文件的对应偏移**，不再攒在内存里：20MB 文件攒内存里
     * 就是 20MB 常驻 + 一堆小 ByteArray 的 GC 压力，低端机直接 OOM。
     * 用 RandomAccessFile 按 idx 定位，乱序到达、重复到达都能正确处理。
     */
    private class RxState(
        val who: String,
        val fid: String,
        val name: String,
        val total: Int,
        val size: Long,
        val tmp: File,
        /** "file" / "voice" / "image" / "video"：收齐后落聊天记录时要用它决定摘要字段与落盘策略 */
        val kind: String = "file",
        /** 语音/视频时长（毫秒），随每块一起带过来 */
        val durMs: Long = 0,
        /** 图片/视频的像素宽高，气泡要按比例显示 */
        val w: Int = 0,
        val h: Int = 0,
        /** 阅后即焚档位（秒），随每块一起带过来 */
        val burn: Long = 0,
    ) {
        val have = java.util.BitSet(total)
        var got = 0
        var raf: java.io.RandomAccessFile? = null
        /** 最后一块到达的时刻（uptimeMs）。超时清理用它判定传输已死 */
        var lastChunkAt: Long = SystemClock.elapsedRealtime()
    }

    /** 在途传输超时：超过此时长没有新块到达，判定死亡并清理（无断点续传设计下的干净失败） */
    private val FILE_RX_TIMEOUT_MS = 5 * 60 * 1000L

    /** 接收进度表：fid → 状态 + 给 UI 的快照。UI 观察 fileRxTick 实时刷新 */
    private val fileRx = linkedMapOf<String, Pair<RxState, FileRx>>()
    var fileRxTick by androidx.compose.runtime.mutableStateOf(0)
        private set

    fun incomingFiles(): List<FileRx> = synchronized(msgLock) { fileRx.values.map { it.second } }

    /** 通知 UI 一个正在接收的文件有新进展 */
    private fun bumpFileRx() { fileRxTick++ }

    /**
     * 发送一个文件、语音、图片或视频。
     *
     * 用 [open] 而不是 ByteArray：20MB 整块读进内存，在低端机上就是一次 OOM 的风险，
     * 而且读完才开传，用户要干等好几秒才有反应。这里边读边发，内存占用恒定一块。
     *
     * [kind] 取 "file" / "voice" / "image" / "video"：四者共用同一套分块协议，
     * 区别只在消息类型和摘要里的附加字段（dur / w / h）。
     * 接收端靠 kind 决定气泡样式，以及**要不要自动落「下载」目录**——
     * 只有普通文件自动落，语音/图片/视频都只活在 App 私有目录。
     *
     * onProgress 参数是 (已发字节, 总字节)，每批回调一次。
     */
    fun sendFile(
        to: String,
        name: String,
        size: Long,
        open: () -> java.io.InputStream,
        kind: String = "file",
        /** 语音/视频时长（毫秒）。其他类型传 0 */
        durMs: Long = 0,
        /** 图片/视频像素宽高。不知道就传 0，气泡会退回 4:3 */
        width: Int = 0,
        height: Int = 0,
        /** 阅后即焚档位（秒）。0 = 普通；>0 时双端首次展示后到时即删 */
        burn: Long = 0,
        onProgress: (Long, Long) -> Unit = { _, _ -> },
    ): Res<Unit> {
        if (size <= 0) return Res(null, err = AppError(ErrKind.FILE_EMPTY))
        if (size > FILE_MAX_BYTES) {
            return Res(
                null,
                err = AppError(ErrKind.FILE_TOO_BIG, (FILE_MAX_BYTES / 1024 / 1024).toString())
            )
        }
        val fid = java.util.UUID.randomUUID().toString().take(8)
        val total = ((size + FILE_CHUNK - 1) / FILE_CHUNK).toInt()
        val buf = ByteArray(FILE_CHUNK)
        var batch = ArrayList<String>(FILE_BATCH)
        var sent = 0L

        // 发出去的文件在本机留一份副本。
        // 只记原始路径不行——来源是 content:// 或用户随时会删的临时文件，
        // 过几天聊天记录里那条气泡就打不开了。边发边写，不多读一遍磁盘。
        val sentDir = File(deviceDir.parentFile, "sent").apply { mkdirs() }
        val keepFile = File(sentDir, "${fid}_${sanitize(name)}")
        var keepOut: java.io.OutputStream? = null

        /** 推一批。返回 null 表示成功 */
        fun flush(): String? {
            if (batch.isEmpty()) return null
            val (_, e) = envelope(Native.sendBatch(dir, to, kind, JSONArray(batch).toString()))
            batch = ArrayList(FILE_BATCH)
            return e
        }

        val readErr = runCatching {
            keepOut = java.io.FileOutputStream(keepFile)
            open().use { ins ->
                var idx = 0
                while (idx < total) {
                    var read = 0
                    while (read < FILE_CHUNK) {
                        val n = ins.read(buf, read, FILE_CHUNK - read)
                        if (n <= 0) break
                        read += n
                    }
                    if (read == 0) break   // 文件比声明的短：按实际读到的发
                    keepOut?.write(buf, 0, read)
                    val piece = if (read == FILE_CHUNK) buf else buf.copyOf(read)
                    batch.add(
                        JSONObject()
                            .put("fid", fid).put("name", name).put("size", size)
                            .put("idx", idx).put("total", total)
                            // 元数据每块都带：块是乱序到达的，第一条到的未必是第 0 块，
                            // 只在头部带的话接收端可能永远拿不到
                            .put("dur", durMs).put("w", width).put("h", height)
                            .put("burn", burn)
                            .put("data", android.util.Base64.encodeToString(piece, android.util.Base64.NO_WRAP))
                            .toString()
                    )
                    sent += read
                    idx++
                    if (batch.size >= FILE_BATCH) {
                        flush()?.let { throw java.io.IOException(it) }
                        onProgress(sent, size)
                    }
                }
            }
        }.exceptionOrNull()
        runCatching { keepOut?.close() }
        if (readErr != null) {
            // 没发出去就别留着半个副本，否则聊天里会出现一个"看起来发过"的坏文件
            keepFile.delete()
            return Res(
                null,
                err = AppError(
                    ErrKind.FILE_SEND_FAILED,
                    readErr.message ?: readErr.javaClass.simpleName
                )
            )
        }
        flush()?.let { e ->
            keepFile.delete()
            return Res(null, e)
        }
        onProgress(sent, size)

        // 文件在"选取 → 发送"之间被截断/改小：total 是按声明 size 算的，
        // 实际发出去的块少于 total，接收端 got 永远到不了 total（靠 5 分钟
        // 超时自愈清理，但用户会看到一条"卡住又消失"的进度）。这里直接报错，
        // 发送方不留"看起来发过"的本地记录（P3）。
        if (sent != size) {
            keepFile.delete()
            return Res(
                null,
                err = AppError(ErrKind.FILE_SEND_FAILED, "文件在发送过程中被修改（$sent/$size 字节）")
            )
        }

        val summary = blobSummary(
            fid, name, sent, keepFile.absolutePath, null, kind, durMs, width, height, burn
        ).toString()
        append(to, Msg(summary, System.currentTimeMillis() / 1000, outgoing = true, kind = kind, rid = fid, burn = burn))
        return Res(Unit, null)
    }

    /**
     * 文件类消息的概要 JSON（聊天记录里 text 字段存的就是它）。
     *
     * - `path`：本机副本的绝对路径（发送方的 sent/、接收方的 inbox/）。
     * - `saved`：已经存到公共「下载/AnonChat」的路径。有它说明文件在 App 之外也有一份，
     *   卸载 App 也不会丢；界面据此显示「已存 下载/AnonChat」。
     * - `dur` / `w` / `h`：只有语音和图片视频才带，普通文件不写多余字段
     *   （老版本 App 读到不认识的字段也不会炸，它只 opt 自己关心的那几个）。
     */
    private fun blobSummary(
        fid: String,
        name: String,
        size: Long,
        path: String?,
        saved: String?,
        kind: String,
        durMs: Long,
        w: Int,
        h: Int,
        burn: Long = 0,
    ): JSONObject {
        val o = JSONObject()
            .put("fid", fid)
            .put("name", name)
            .put("size", size)
        if (durMs > 0) o.put("dur", durMs)
        if (w > 0 && h > 0) o.put("w", w).put("h", h)
        if (burn > 0) o.put("burn", burn)
        if (path != null) o.put("path", path)
        if (saved != null) o.put("saved", saved)
        return o
    }

    /**
     * 处理收到的文件块。收齐时写盘并落一条聊天记录；没收齐时只更新进度。
     * 必须在 recvOnce 的调用线程（IO）里跑。
     */
    private fun receiveFileChunk(who: String, inc: Incoming) {
        val v = runCatching { JSONObject(inc.body ?: "") }.getOrNull() ?: return
        val fid = v.optString("fid")
        // 兜底文件名用 ASCII：这是数据（落盘 + 进消息摘要），不是界面文案
        val name = v.optString("name", "file")
        val total = v.optInt("total", 0)
        val idx = v.optInt("idx", -1)
        val size = v.optLong("size", 0)
        val durMs = v.optLong("dur", 0)
        val w = v.optInt("w", 0)
        val h = v.optInt("h", 0)
        val burn = v.optLong("burn", 0)
        // 只认我们自己定义的四种载荷类型：对面万一发来别的 kind，
        // 按普通文件落盘总比丢消息好
        val kind = when (inc.kind) {
            "voice", "image", "video" -> inc.kind
            else -> "file"
        }
        val data = runCatching {
            android.util.Base64.decode(v.optString("data"), android.util.Base64.NO_WRAP)
        }.getOrNull() ?: return
        if (fid.isBlank() || total <= 0 || idx !in 0 until total) return
        // 空块不能算"已收"：Base64.decode("") 返回空数组而非 null，写上不报错，
        // 仍会 have.set+got++——损坏/伪造的末块能让文件"收齐"但内容有渣洞（P3）。
        if (data.isEmpty() && size > 0) return
        if (size > FILE_MAX_BYTES) {
            android.util.Log.w("AnonChat", "oversized file rejected $name ($size bytes)")
            return
        }

        var done: Pair<File, FileRx>? = null
        synchronized(msgLock) {
            val (st, _) = fileRx.getOrPut(fid) {
                val dir = File(deviceDir.parentFile, "inbox").apply { mkdirs() }
                val safeName = name.replace(Regex("[\\\\/:*?\"<>|]"), "_")
                val tmp = File(dir, ".$fid.part")
                RxState(who, fid, name, total, size, tmp, kind, durMs, w, h, burn) to
                    FileRx(who, fid, name, total, 0, size, kind, durMs, w, h, burn)
            }
            if (!st.have.get(idx)) {
                // 落在正确偏移上：乱序到达也不用等，重复到达直接忽略
                runCatching {
                    val raf = st.raf ?: java.io.RandomAccessFile(st.tmp, "rw").also { st.raf = it }
                    raf.seek(idx.toLong() * FILE_CHUNK)
                    raf.write(data)
                    st.have.set(idx)
                    st.got++
                    st.lastChunkAt = SystemClock.elapsedRealtime()
                }.onFailure { t ->
                    android.util.Log.w("AnonChat", "write file chunk failed ${t.message}")
                }
            }
            val prog = FileRx(st.who, st.fid, st.name, st.total, st.got, st.size, st.kind, st.durMs, st.w, st.h, st.burn)
            fileRx[fid] = st to prog
            if (st.got == total) {
                runCatching {
                    st.raf?.close()
                    st.raf = null
                    val finalFile = File(st.tmp.parentFile, "${st.fid}_${sanitize(st.name)}")
                    if (!st.tmp.renameTo(finalFile)) {
                        st.tmp.copyTo(finalFile, overwrite = true)
                        st.tmp.delete()
                    }
                    done = finalFile to prog
                    fileRx.remove(fid)
                }.onFailure { t ->
                    android.util.Log.w("AnonChat", "file write failed: ${t.message}")
                }
            }
        }
        bumpFileRx()

        done?.let { (out, prog) ->
            // 普通文件顺手在「下载/AnonChat」留一份：私有目录那份只有 App 自己能读、
            // 卸载就没了，用户要的"存到手机"只能是公共目录。API 29 以下返回 null
            // （写公共目录要全盘权限），那种机器上靠气泡里的"保存到手机"走系统选择器。
            //
            // 语音/图片/视频**都不落公共目录**：它们的内容属于聊天正文，不是用户
            // 主动索要的文件。偷偷往下载目录丢一堆 voice_*.m4a / IMG_*.jpg，
            // 既像泄露隐私，也会把相册和文件管理器塞满碎片。想导出走长按菜单。
            val saved = if (prog.kind == "file") runCatching {
                FileSaver.saveToDownloads(appCtx, prog.name, out)
            }.getOrNull() else null
            // 对面没带宽高/时长（CLI、老版本客户端）就在本地补一次：
            // 只影响气泡比例和角标，但缺了会让竖图被按 4:3 裁掉
            var fw = prog.w
            var fh = prog.h
            var fdur = prog.durMs
            if ((prog.kind == "image" || prog.kind == "video") && (fw <= 0 || fh <= 0 || fdur <= 0)) {
                val (mw, mh, md) = VideoProbe.dimsOf(out.absolutePath, prog.kind)
                if (fw <= 0 || fh <= 0) { fw = mw; fh = mh }
                if (prog.kind == "video" && fdur <= 0) fdur = md
            }
            val summary = blobSummary(
                prog.fid, prog.name, prog.size, out.absolutePath, saved,
                prog.kind, if (prog.kind == "voice") prog.durMs else fdur, fw, fh,
                prog.burn,
            )
            append(
                who,
                Msg(
                    summary.toString(),
                    System.currentTimeMillis() / 1000,
                    outgoing = false,
                    kind = prog.kind,
                    // 文件类的撤回引用就是 fid：分块摘要里本来就有，两边天然一致
                    rid = prog.fid,
                    burn = prog.burn,
                )
            )
            markUnread(who)
            // 收齐才通知一次（分块期间不弹）：回调由 service 层注入
            onFileDone?.invoke(who, prog.name)
        }
    }

    /**
     * 文件收齐落盘后的回调（who, name）。MessagingService 注入它来发一条
     * "收到文件"通知——分块本身的每一条消息在 listenLoop 被跳过（否则
     * 20MB 文件就是 854 条通知），通知必须挪到这里才只发一条。
     */
    @Volatile
    var onFileDone: ((String, String) -> Unit)? = null

    // ---------------------------------------------------------------- 紧急清除

    /** 当前会话的紧急清除暗号（双方各自算出同一个值）。拿不到双方公钥返回 null */
    fun panicCodeOf(peer: String): String? {
        val me = identity() ?: return null
        val peerPk = synchronized(msgLock) { peerCards[peer]?.pk }
            ?: contacts().firstOrNull { it.dh == peer }?.pk
        if (peerPk.isNullOrBlank()) return null
        return Panic.codeOf(me, peerPk)
    }

    /**
     * 执行紧急清除。**顺序是硬约束**：
     * ① 先发 clear.exec 给对方（此时身份密钥还在，能加密投递；对方离线就在
     *    中继排队，TTL 内上线即清）——对方收到后**无确认直接清**，不弹任何验证；
     * ② 盲 ack 清空自己的中继队列（不解密，内容是什么不重要）；
     * ③ 删本机数据层（聊天记录/文件副本/下载目录副本）——**保留身份、联系人、
     *    会话棘轮**，清完双方无缝继续聊。
     *
     * [notifyPeer]=false 用于收到 clear.exec 的一方（对方已发起，幂等不再回发）。
     * 调用方负责在返回后杀进程重启（数据已空，重启即初始状态）。
     */
    fun executePanic(peer: String, notifyPeer: Boolean) {
        if (notifyPeer) {
            val (_, e) = envelope(
                Native.sendBatch(dir, peer, "clear.exec", JSONArray().put("{}").toString())
            )
            // 发不出去也继续清本机：对方仍可用暗号自己触发，本机清除不等网络
            if (e != null) android.util.Log.w("AnonChat", "clear.exec send failed: $e")
        }
        runCatching { envelope(Native.purgeQueue(dir)) }
            .onFailure { android.util.Log.w("AnonChat", "purge relay queue failed: ${it.message}") }
        wipeData()
    }

    /** 删数据层。保留 device/（身份、联系人、会话棘轮）与暗号，幂等 */
    private fun wipeData() {
        synchronized(msgLock) {
            unread.clear()
            fileRx.clear()
        }
        store.wipe()
        runCatching {
            File(deviceDir.parentFile, "inbox").deleteRecursively()
            File(deviceDir.parentFile, "sent").deleteRecursively()
        }
        runCatching { FileSaver.purgeDownloads(appCtx) }
        bumpFileRx()
    }

    private fun sanitize(name: String) = name.replace(Regex("[\\\\/:*?\"<>|]"), "_").ifBlank { "file" }

    /** 已收完的文件消息能不能打开（有 path 且文件还在） */
    fun fileMsgPath(summaryText: String): String? = runCatching {
        JSONObject(summaryText).optString("path").takeIf { it.isNotBlank() }
    }.getOrNull()

    /**
     * 收一条。timeoutSecs 内没有消息返回 null（不是错误）。
     * 前台服务会循环调用这个方法。
     */
    fun recvOnce(timeoutSecs: Long): Res<Incoming?> {
        val (d, e) = envelope(Native.recvOnce(dir, timeoutSecs))
        if (e != null) return Res(null, e)
        if (d !is JSONObject) return Res(null, null)
        val inc = Incoming(
            fromName = if (d.isNull("from_name")) null else d.optString("from_name"),
            fromId = d.optString("from_id"),
            text = d.optString("text"),
            ts = d.optLong("ts"),
            dh = d.optString("dh"),
            kind = d.optString("kind", "text"),
            body = if (d.isNull("body")) null else d.optString("body"),
            msgId = d.optString("msg_id"),
            rid = d.optString("rid"),
            // 文字消息的焚毁参数在 body 里（{"burn":秒}）；文件类不走这里
            burn = if (d.optString("kind", "text") == "text")
                runCatching { JSONObject(d.optString("body", "{}")).optLong("burn") }.getOrDefault(0)
            else 0,
            selfName = if (d.isNull("self_name")) null else d.optString("self_name"),
            replyQid = if (d.isNull("reply_qid")) null else d.optString("reply_qid"),
            replyRelays = (d.optJSONArray("reply_relays") ?: JSONArray()).let { arr ->
                (0 until arr.length()).map { arr.optString(it) }.filter { s -> s.isNotBlank() }
            },
        )
        // 通话信令不是聊天内容，不能写进消息记录（否则会出现一堆空白消息）
        if (inc.isCall) return Res(inc, null)

        // 撤回消息：不是聊天内容本身，按 rid 找到本地那条替换成灰字；
        // 找不到（对方撤回了我这边没有的消息 / 已处理过）就静默忽略
        if (inc.kind == "recall") {
            handleIncomingRecall(whoOf(inc), inc)
            return Res(inc, null)
        }

        // 文件/语音块：聚合并（收齐时）落盘。不落"每块一条"的聊天记录，
        // 收齐才出现一条气泡，中间进度走 incomingFiles()。
        if (inc.isBlob) {
            receiveFileChunk(whoOf(inc), inc)
            return Res(inc, null)
        }

        if (!inc.isText) return Res(inc, null)

        // 陌生人消息也要能落到一个可读的会话里，否则用户在 UI 上永远看不到
        val who = whoOf(inc)
        // 顺手记住回信线索：这是唯一一次能拿到它的机会，消息落盘之后就没了
        if (inc.fromName == null && inc.canReply) rememberCard(who, inc)
        append(who, Msg(inc.text, inc.ts, outgoing = false, rid = inc.rid, burn = inc.burn))
        return Res(inc, null)
    }

    /**
     * 取走 Rust 侧攒下的告警。前台服务每轮收消息后调一次，
     * 这样"消息解密失败被丢弃"这类在 Rust 里静默发生的事就能出现在 logcat。
     */
    fun drainWarnings(): List<String> {
        val (d, _) = envelope(Native.takeWarnings())
        val fromCore = if (d is JSONArray) {
            (0 until d.length()).mapNotNull { d.optString(it).takeIf { s -> s.isNotEmpty() } }
        } else emptyList()
        // 本地消息日志的 IO 失败（磁盘满等）一并带出：内存已改、日志没写，
        // 不说出去就是"消息发过又没了"且无人知晓。取走即清，避免每轮重复报。
        val storeErr = store.lastIoError?.let {
            store.clearIoError()
            listOf("本地存储：$it")
        } ?: emptyList()
        return fromCore + storeErr
    }

    /** 把发送方 dh 映射成界面上显示的名字 */
    /**
     * 会话键 = 对方 DH 公钥。
     *
     * 为什么不再用显示名：改名/备注名会让同一个会话裂成两段——
     * 备注名功能一上线，用名字当键就会把历史消息"丢"在新名字之外的旧会话里。
     * dh 是对方身份的唯一稳定标识，从加好友那天起就不变。
     */
    fun whoOf(inc: Incoming): String = inc.dh

    /**
     * 陌生人会话名的前缀。
     *
     * **这是存储语义，不要改**：peer_cards.json 里存的名字是
     * `conv.removePrefix("陌生人 ")` 算出来的，前缀变了老名片就反查不出来。
     *
     * 界面上要显示本地化版本时走 [MainActivity.displayNameOf]，它按当前语言
     * 换掉这个前缀，但存的东西原样不动。
     */
    const val STRANGER_PREFIX = "陌生人 "

    /** 会话键 → 界面显示名（备注 > 对方昵称 > 陌生人+前缀） */
    fun displayName(key: String): String {
        val c = contacts().firstOrNull { it.dh == key }
        if (c != null) return c.display
        return peerCards[key]?.name?.takeIf { it.isNotBlank() } ?: (STRANGER_PREFIX + key.take(8))
    }

    // ---------------------------------------------------------------- 通话

    fun callStart(to: String, video: Boolean): Res<String> {
        val (d, e) = envelope(Native.callStart(dir, to, video))
        return Res(d as? String, e)
    }

    fun callSetOffer(sdp: String): Res<CallEvent> = callEvent(Native.callSetOffer(dir, sdp))
    fun callRenegotiate(sdp: String): Res<CallEvent> = callEvent(Native.callRenegotiate(dir, sdp))
    fun callAccept(sdp: String): Res<CallEvent> = callEvent(Native.callAccept(dir, sdp))
    fun callReject(reason: String = "declined"): Res<CallEvent> =
        callEvent(Native.callReject(dir, reason))

    fun callHangup(reason: String = "local_ended"): Res<CallEvent> =
        callEvent(Native.callHangup(dir, reason))

    fun callIce(candidate: String, mid: String, mline: Int): Res<CallEvent> =
        callEvent(Native.callIce(dir, candidate, mid, mline))

    fun callTick(): Res<CallEvent> = callEvent(Native.callTick(dir))
    fun callState(): CallEvent? = callEvent(Native.callState(dir)).data
    fun callReset() { envelope(Native.callReset(dir)) }

    fun callInbound(inc: Incoming): Res<CallEvent> = callEvent(
        Native.callInbound(dir, inc.dh, inc.kind, inc.body ?: "", inc.msgId)
    )

    private fun callEvent(raw: String): Res<CallEvent> {
        val (d, e) = envelope(raw)
        if (e != null || d !is JSONObject) return Res(null, e)
        return Res(
            CallEvent(
                state = d.optString("state", "idle"),
                callId = d.optString("call_id"),
                peerDh = d.optString("peer_dh"),
                video = d.optBoolean("video"),
                reason = d.optString("reason"),
                remoteSdp = if (d.isNull("remote_sdp")) null else d.optString("remote_sdp"),
                iceCandidate = d.optJSONObject("ice")?.optString("candidate"),
                iceMid = d.optJSONObject("ice")?.optString("mid") ?: "",
                iceMline = d.optJSONObject("ice")?.optInt("mline") ?: 0,
                renegotiating = d.optBoolean("renegotiating"),
            ), null
        )
    }

    /** 按 dh 找联系人名 */
    fun nameOfDh(dh: String): String? = contacts().firstOrNull { it.dh == dh }?.display

    // ---------------------------------------------------------------- 消息历史
    //
    // 存储是 append-only 日志（[MsgStore]），所以这里**不再有"保存全部消息"这回事**：
    // 每加一条只追加一行。UI 那边按窗口取，见 messageCount/messagesOf(from,to)。

    /** 某个会话有多少条消息。聊天页靠它算分页窗口 */
    fun messageCount(who: String): Int = store.count(who)

    /** 会话列表的最后一条（只要一条，不拷贝整个会话） */
    fun lastMsgOf(who: String): Msg? = store.last(who)

    /**
     * 取 [from, to) 区间的消息，默认全部。
     *
     * 聊天页只取最近几十条，往上滚再取更多——几千条历史全塞给 LazyColumn
     * 的话，每次刷新都要把整个列表做一次 diff，滑起来会明显掉帧。
     */
    fun messagesOf(who: String, from: Int = 0, to: Int = Int.MAX_VALUE): List<Msg> =
        store.slice(who, from, to)

    /** 有消息但还没加为联系人的会话（只能看，不能回） */
    fun unverifiedConversations(): List<String> {
        val cs = contacts()
        // 会话键现在是 dh，已知集合也要按 dh 比
        val known = cs.map { it.dh }.toSet()

        // 防御存量脏数据：0.15.1 及更早版本在"加为联系人"时会把会话键
        // 从 dh 搬到人名。键一旦变成人名就对不上 dh 集合，于是同一个人
        // 会同时出现在"联系人"和"未加为联系人"两处，各自点进去行为还不一样。
        // 重启时 migrateConversationKeys() 会把键迁回去，但用户不该为此重启。
        // 这里把"键正好是某个联系人的名字/备注名"的一并排除。
        val names = cs.mapNotNull { c ->
            c.display.takeIf { it.isNotBlank() }
        }.toSet() + cs.mapNotNull { c -> c.name.takeIf { it.isNotBlank() } }.toSet()

        return store.keys()
            .filter { store.count(it) > 0 }
            .let { filterUnverifiedKeys(it, known, names) }
    }

    // 未读数见 unread/totalUnread()

    // ---------------------------------------------------------------- 回邀

    /** 陌生人会话名 → 回信名片。key 用会话名，因为它就是 UI 上唯一的标识 */
    private val peerCards = HashMap<String, PeerCard>()
    private lateinit var cardFile: File

    private fun rememberCard(conv: String, inc: Incoming) {
        val qid = inc.replyQid ?: return
        if (inc.replyRelays.isEmpty()) return
        // 对方没自报昵称时的兜底名：conv 现在是 dh（0.12.0 起），removePrefix
        // 是名字键时代的遗物、对 dh 不命中，会把整串 64 位哈希当名字显示在
        // 列表和聊天页标题上（P3）。取前 8 位，与陌生人会话的显示规则一致。
        val card = PeerCard(
            name = inc.selfName?.takeIf { it.isNotBlank() } ?: conv.take(8),
            pk = inc.fromId,
            dh = inc.dh,
            qid = qid,
            relays = inc.replyRelays,
        )
        val changed = synchronized(msgLock) { peerCards.put(conv, card) != card }
        if (changed) saveCards()
    }

    fun cardOf(conv: String): PeerCard? = synchronized(msgLock) { peerCards[conv] }

    /**
     * 取联系人卡片；没有就从联系人表补一张。
     *
     * 两条来源不一样：**经邀请码加进来的**联系人在 Rust 侧 contacts.json 里，
     * **对方加你**的陌生人才会在 peer_cards.json 留卡片。安全码两种都要能算，
     * 所以这里统一成"先看卡片，没有就按联系人表补一张"。
     */
    private fun cardOrBuild(conv: String): PeerCard? {
        val cached = synchronized(msgLock) { peerCards[conv] }
        if (cached != null && cached.pk.isNotBlank()) return cached
        val c = contacts().firstOrNull { it.dh == conv } ?: return cached
        val card = PeerCard(
            name = c.display, pk = c.pk, dh = c.dh, qid = c.qid, relays = emptyList(),
            verified = cached?.verified ?: false,
            verifiedCode = cached?.verifiedCode ?: "",
        )
        synchronized(msgLock) { peerCards[conv] = card }
        saveCards()
        return card
    }

    /**
     * 这个会话的安全码。双方各自算一遍，数字一样就说明中间没有第三方。
     *
     * 需要对方的身份公钥——只有收到过对方消息（或邀请码）才拿得到。
     * 没有卡片时返回 null，由界面提示"等对方发一条消息再核对"。
     */
    fun safetyCodeOf(conv: String): String? {
        val me = identity() ?: return null
        val card = cardOrBuild(conv) ?: return null
        if (card.pk.isBlank()) return null
        val (d, _) = envelope(Native.safetyCode(me, card.pk))
        return d as? String
    }

    /** 界面上"我已核对"的状态：核对的码和当前码一致才算数（对方换身份就自动作废） */
    fun isVerified(conv: String): Boolean {
        val c = cardOrBuild(conv) ?: return false
        if (!c.verified) return false
        val now = safetyCodeOf(conv) ?: return false
        return now == c.verifiedCode
    }

    /** 记录"我当面核对过了"。code 传当前算出来的那串，将来对不上就自动失效。 */
    fun setVerified(conv: String, ok: Boolean) {
        val c = cardOrBuild(conv) ?: return
        val code = if (ok) (safetyCodeOf(conv) ?: return) else ""
        synchronized(msgLock) { peerCards[conv] = c.copy(verified = ok, verifiedCode = code) }
        saveCards()
    }

    /**
     * 把陌生人会话里的对方加为联系人，并把聊天记录迁到新名字下。
     *
     * 这是"只能看不能回"的出口：对方加了你、给你发消息，你点一下就能回过去，
     * 不用再让对方重新生成一次邀请码。
     */
    fun addPeer(conv: String): Res<String> {
        val card = cardOf(conv)
            ?: return Res(null, err = AppError(ErrKind.ADD_NO_CLUE))
        // 同一身份（pk）已有别的 dh 条目 = 对方重装恢复过：回邀合并后要把
        // 旧 dh 键下的历史搬到新 dh（card.dh）下，理由同 [add]。
        val oldDh = card.pk.takeIf { it.isNotBlank() }?.let { pk ->
            contacts().firstOrNull { it.pk == pk && it.dh != card.dh }?.dh
        }
        val (d, e) = envelope(
            Native.addFromCard(dir, card.name, card.dh, card.pk, card.qid, JSONArray(card.relays).toString())
        )
        val name = d as? String
        if (e != null || name.isNullOrBlank()) {
            return Res(null, error = e, err = if (e == null) AppError(ErrKind.ADD_FAILED) else null)
        }
        // 会话键是 dh 的时候**不搬**：加好友只改变了显示名，会话本身没变。
        //
        // 早先这里无条件 `moveConversation(conv, name)`，把键从 dh 搬到人名。
        // 而 `unverifiedConversations()` 是拿 store 的键跟联系人 dh 集合比对的，
        // 搬完之后"人名"这个键对不上任何 dh，于是同一人被当成两条会话：
        // 一条在联系人区（正常可聊），一条在"未加为联系人"区（还带"加为好友"按钮）。
        // 重启时 `migrateConversationKeys()` 按"名字→dh"把它迁回去，所以重启就好了。
        //
        // 只有当 conv 还是旧版本的名字键时才搬，且目标是 dh 而不是人名。
        val target = card.dh
        if (conv != target) {
            moveConversation(conv, target, renameCard = false)
        }
        // 重装合并：旧 dh 的历史也归到新 dh 下
        if (oldDh != null && oldDh != target) {
            moveConversation(oldDh, target, renameCard = false)
        }
        return Res(name, null)
    }

    /**
     * 搬会话键。消息、未读、名片一起搬过去，别让用户在界面上"丢历史"。
     *
     * [renameCard] 为 true 时才把卡片名改成新键——键是 dh 的时候不能改，
     * 否则卡片名会变成一个哈希串，界面上直接显示它。
     *
     * 卡片里的 `dh` 字段**必须同步改写为目标键**（当目标键是 dh 时）：
     * 只搬不改的话，peer_cards.json 里存的是 `peerCards[newDh].dh == 旧dh`，
     * 重启时 migrateConversationKeys 会构建 [新dh → 旧dh] 的映射，把刚搬过来
     * 的会话再搬回死信箱——联系人历史清空、以陌生人会话重现，且每次重启
     * 复发（0.15.5 合并功能引入的回归，二轮审计 P1-6）。
     */
    private fun moveConversation(from: String, to: String, renameCard: Boolean = true) {
        if (from == to) return
        // 消息迁移只写一条 mv 记录（日志里回放），不再整文件重写
        store.move(from, to)
        synchronized(msgLock) {
            val u = unread.remove(from)
            if (u != null) unread[to] = (unread[to] ?: 0) + u
            // 卡片跟着搬，并把 dh 改写为新键；目标键已有卡片时做字段合并——
            // 路由（qid/relays）取新键侧（旧键指向死信箱），核对状态保留原值
            // （pk 未变则安全码不变，verified 语义不丢）。
            val moved = peerCards.remove(from)
            if (moved != null) {
                val merged = peerCards[to]?.let { keep ->
                    // 新键已有卡片：路由以新键为准，核对状态沿用旧卡
                    moved.copy(
                        dh = to,
                        qid = keep.qid,
                        relays = keep.relays,
                        verified = keep.verified,
                        verifiedCode = keep.verifiedCode,
                    )
                } ?: moved.copy(dh = to)
                peerCards[to] = merged
            }
        }
        saveCards()
    }

    private fun saveCards() {
        runCatching {
            val snapshot = synchronized(msgLock) {
                peerCards.mapValues { (_, c) ->
                    JSONObject()
                        .put("name", c.name).put("pk", c.pk).put("dh", c.dh)
                        .put("qid", c.qid).put("relays", JSONArray(c.relays))
                        .put("verified", c.verified).put("vcode", c.verifiedCode)
                }
            }
            val root = JSONObject()
            snapshot.forEach { (k, v) -> root.put(k, v) }
            cardFile.writeText(root.toString())
        }
    }

    private fun loadCards() {
        if (!cardFile.exists()) return
        runCatching {
            val root = JSONObject(cardFile.readText())
            synchronized(msgLock) {
                root.keys().forEach { k ->
                    val o = root.optJSONObject(k) ?: return@forEach
                    val arr = o.optJSONArray("relays") ?: JSONArray()
                    peerCards[k] = PeerCard(
                        name = o.optString("name"),
                        pk = o.optString("pk"),
                        dh = o.optString("dh"),
                        qid = o.optString("qid"),
                        relays = (0 until arr.length()).map { arr.optString(it) },
                        verified = o.optBoolean("verified", false),
                        verifiedCode = o.optString("vcode", ""),
                    )
                }
                // 自愈存量损坏：0.15.5 的合并 bug 会在文件里留下 `dh ≠ 键`
                // 的卡片（键是 dh、dh 字段还是旧值），重启时迁移逻辑会把会话
                // 搬回死信箱。卡片键本身就是 dh（联系人卡与回邀卡都按此约定），
                // 不一致的一律就地改写。改完落盘一次。
                var dirty = false
                for ((k, c) in peerCards) {
                    if (c.dh != k && k.length == 43) {
                        peerCards[k] = c.copy(dh = k)
                        dirty = true
                    }
                }
                if (dirty) saveCards()
            }
        }
    }

    /**
     * 写一条通话记录进聊天。
     *
     * [peerDh] 必须是**会话键（dh）**，不是显示名：会话列表按 dh 取最后一条、
     * 陌生人过滤按 dh 比对——传显示名的话联系人来电记录双向不可见、陌生人
     * 来电产生"陌生人 陌生人 x"幽灵会话（二轮审计 P1-3）。显示名由列表的
     * displayNameOf(dh) 统一解析，调用方不用管。
     *
     * 为什么必须写记录：不留的话，未接来电在界面上**完全没有痕迹**，
     * 用户只会觉得"手机没响过"。这是通话功能和消息功能之间最容易漏掉的衔接点。
     */
    fun appendCallLog(peerDh: String, text: String) {
        if (peerDh.isBlank()) return
        append(peerDh, Msg(text, System.currentTimeMillis() / 1000, outgoing = true, kind = "call"))
        // 未接来电要有角标：来电记录不经过 notifyMessage，只能在这里补
        markUnread(peerDh)
    }

    private fun append(who: String, m: Msg) {
        store.append(who, m)
    }

    // ---------------------------------------------------------------- 信封

    private fun envelope(raw: String): Pair<Any?, String?> {
        return try {
            val o = JSONObject(raw)
            if (o.optBoolean("ok")) {
                (if (o.isNull("data")) null else o.get("data")) to null
            } else {
                // 技术信息走 error 通道，语言中性（不进本地化资源）
                null to o.optString("error", "unknown core error")
            }
        } catch (t: Throwable) {
            null to "unparsable core response: ${t.message}"
        }
    }
}

/**
 * "未加为联系人" 会话的过滤规则。
 *
 * 单独抽出来是为了能被单测覆盖——这段规则错过一次，代价是同一个联系人
 * 在列表里出现两遍：一遍正常可聊，一遍还带着"加为好友"按钮。
 *
 * 两个排除条件各有来历：
 * - [knownDh]：已经加为联系人，不该再出现在陌生人区；
 * - [displayNames]：**存量脏数据防御**。0.15.1 及更早版本加好友时把会话键
 *   从 dh 搬成了人名，键对不上 dh 集合，于是同一个人出现两次。
 *   重启时 migrateConversationKeys() 会迁回去，但不该让用户靠重启解决。
 */
internal fun filterUnverifiedKeys(
    keys: List<String>,
    knownDh: Set<String>,
    displayNames: Set<String>,
): List<String> = keys.filter { it !in knownDh && it !in displayNames }
