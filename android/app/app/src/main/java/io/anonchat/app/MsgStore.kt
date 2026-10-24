package io.anonchat.app

import org.json.JSONObject
import java.io.BufferedWriter
import java.io.File
import java.io.FileWriter

/**
 * 消息历史的落盘层：**append-only 日志**。
 *
 * 为什么不是"每次重写整个文件"：之前的实现是 messages.json 全量重写，
 * 发一条消息就把所有会话的所有历史重新序列化一遍再整文件覆盖。消息攒到
 * 几千条以后，每发一条都要几十毫秒到上百毫秒的 IO，而且这些 IO 全在
 * 收消息的线程上——表现为"聊得越久，发一条越卡"。更糟的是整文件覆盖
 * 期间进程被杀，会留下一个截断的 JSON，整个历史全丢。
 *
 * 现在每条消息只追加一行 JSON（JSON Lines），单条成本恒定，且已写下的
 * 内容不会被后续写入破坏（最多丢最后半行，回放时跳过）。
 *
 * 文件格式（每行一个对象，靠 `o` 区分类型）：
 * - `{"o":"m","w":会话,"t":文本,"ts":秒,"out":是否我发的,"k":kind,"i":id,"f":是否发送失败}`
 * - `{"o":"mv","f":旧会话,"t":新会话}`  会话改名（回邀"加为联系人"用）
 * - `{"o":"st","w":会话,"i":id}`        把某条消息标记为已发出（重发成功后回填）
 *
 * 每条消息有**本地自增 id**（`i`）：append-only 日志没法改写旧行，
 * "重发成功要把失败标记抹掉"就只能靠追加一条 st 记录、回放时应用。
 * id 只在本机有意义，不参与协议。老日志里的消息没有 `i`，回放时按顺序补号——
 * 这些补出来的号**不落盘**，重启后可能变化，但它们都是"已发出"的消息，
 * 没有人会拿 id 去定位它们，所以无害。失败的Msg 落盘时带着自己的 id，重启发出的 st 能对上。
 *
 * 改名必须记进日志：否则重启后历史会分裂成两个会话。代价是日志里留下
 * 一条历史记录，所以 [compactIfNeeded] 会在日志膨胀时把它折叠掉。
 *
 * 兼容：老版本的 messages.json 会在首次启动时导入，原文件改名保留。
 */
class MsgStore(private val dir: File) {
    private val logFile = File(dir, "messages.log")
    private val legacyFile = File(dir, "messages.json")
    private var writer: BufferedWriter? = null

    /**
     * 最近一次落盘失败的原因（null = 正常）。磁盘满/IO 错误时内存已改、
     * 日志没写，用户看到的是"发过又没了"——必须能被上层发现并告警，
     * 全静默等于数据丢失无人知晓。drainWarnings 会把它带出去。
     */
    @Volatile
    var lastIoError: String? = null
        private set

    /** 告警被上层取走后清零，避免同一个错误每轮重复上报 */
    fun clearIoError() {
        lastIoError = null
    }

    /** 所有会话 → 消息（按时间升序）。**只能通过本类的方法访问**，内部同步 */
    private val data = linkedMapOf<String, MutableList<Msg>>()

    /** 日志里已有的记录条数（含 mv 记录），用于判断是否值得压缩 */
    private var lines = 0

    /** 已分配的最大消息 id。append-only：新 id 只增不减，落盘后永不复用 */
    private var maxId = 0L

    // ------------------------------------------------------------ 读

    /**
     * 紧急清除：清空内存与日志文件。**身份/联系人不受影响**（不在本类管辖）。
     * 幂等：重复调无害。下次 append 会从 1 号重新编号（文件都没了）。
     */
    fun wipe() {
        synchronized(this) {
            data.clear()
            lines = 0
            maxId = 0
            runCatching { writer?.close() }
            writer = null
            runCatching { logFile.delete() }
        }
    }

    fun keys(): List<String> = synchronized(this) { data.keys.toList() }

    fun count(who: String): Int = synchronized(this) { data[who]?.size ?: 0 }

    /** 最近一条（会话列表用，避免为了一个预览把整个会话拷贝一遍） */
    fun last(who: String): Msg? = synchronized(this) {
        data[who]?.lastOrNull()
    }

    /**
     * 取 [from, to) 区间。UI 分页就靠它：只把视口附近的那几十条交给 Compose，
     * 而不是每次都把几千条塞进 LazyColumn 的入参（那样每次刷新都要 diff 整个列表）。
     */
    fun slice(who: String, from: Int = 0, to: Int = Int.MAX_VALUE): List<Msg> = synchronized(this) {
        val list = data[who] ?: return emptyList()
        if (from >= list.size) return emptyList()
        list.subList(from.coerceAtLeast(0), to.coerceAtMost(list.size)).toList()
    }

    /** 把某个会话整体取走（改名用，内部） */
    private fun take(who: String): MutableList<Msg>? = data.remove(who)

    // ------------------------------------------------------------ 写

    fun append(who: String, m: Msg): Msg {
        synchronized(this) {
            // id 在这里统一分配：调用方不传，避免两处（发送/接收）各搞一套编号
            val stored = m.copy(id = ++maxId)
            data.getOrPut(who) { mutableListOf() }.add(stored)
            writeLine(
                msgLine(who, stored)
            )
            return stored
        }
    }

    /**
     * 把某条消息的"发送失败"标记抹掉（重发成功后调用）。
     *
     * 日志是 append-only 的，改不了旧行，所以追加一条 st 记录：
     * 回放时按 id 找到那条消息，把 failed 置回 false。内存里同步改，
     * 这样 UI 立刻能看到，不用等下次重启。
     */
    fun markSent(who: String, id: Long) {
        synchronized(this) {
            val list = data[who] ?: return
            val idx = list.indexOfFirst { it.id == id }
            if (idx < 0) return
            list[idx] = list[idx].copy(failed = false)
            writeLine(JSONObject().put("o", "st").put("w", who).put("i", id))
        }
    }

    private fun msgLine(who: String, m: Msg): JSONObject {
        val o = JSONObject()
            .put("o", "m").put("w", who)
            .put("t", m.text).put("ts", m.ts)
            .put("out", m.outgoing).put("k", m.kind)
            .put("i", m.id)
        // 失败标记只有失败时才写：绝大多数消息都是成功的，别让日志每行都多一个字段
        if (m.failed) o.put("f", true)
        // 撤回引用同理（空串不写）
        if (m.rid.isNotBlank()) o.put("r", m.rid)
        // 焚毁档位和首次展示时间（0 不写）
        if (m.burn > 0) o.put("b", m.burn)
        if (m.seenAt > 0) o.put("sa", m.seenAt)
        return o
    }

    fun move(from: String, to: String) {
        if (from == to) return
        synchronized(this) {
            val moved = take(from)
            if (moved != null) {
                val dst = data.getOrPut(to) { mutableListOf() }
                dst.addAll(moved)
                dst.sortBy { it.ts }
            }
            writeLine(JSONObject().put("o", "mv").put("f", from).put("t", to))
        }
    }

    /**
     * 把旧版本的"显示名做键"迁移到 dh 做键。
     *
     * 为什么要迁：0.12.0 之前会话键是联系人显示名，改名/备注会让会话裂开。
     * 迁移用 [move] 写 mv 记录，回放逻辑已经支持，旧数据不需要重写。
     * 幂等：旧键不存在就跳过，可以每次启动都调。
     */
    fun migrateKeysToDh(map: Map<String, String>): Int {
        if (map.isEmpty()) return 0
        val existing = synchronized(this) { data.keys.toSet() }
        var n = 0
        map.forEach { (oldKey, dh) ->
            if (oldKey != dh && oldKey in existing && dh.isNotBlank()) {
                move(oldKey, dh)
                n++
            }
        }
        return n
    }

    /**
     * 整会话删除：从内存移除并重写日志文件（真正释放空间，不只是打个删除标记）。
     * 返回删掉的条数。
     */
    fun dropConversation(who: String): Int = synchronized(this) {
        val n = data[who]?.size ?: 0
        data.remove(who)
        if (n > 0) rewrite()
        n
    }

    // ------------------------------------------------------------ 落盘

    private fun writeLine(o: JSONObject) {
        // 打开失败也要让内存状态一致：调用方已经改了 data，这里只是落盘失败
        runCatching {
            val w = writer ?: openWriter().also { writer = it }
            w.write(o.toString())
            w.newLine()
            // 每条都 flush：进程随时可能被系统杀掉，缓冲里的消息就是丢的消息。
            // 单条只有几百字节，一次 write 系统调用，代价可以接受。
            w.flush()
            lines++
        }.onFailure { lastIoError = "消息日志写入失败: ${it.message}" }
    }

    /**
     * 打开追加句柄。
     *
     * **必须先确认文件以换行结尾**：上一次写入如果被进程被杀打断，日志末尾会留下
     * 半行且没有换行符。这时候直接追加，新记录会被拼到半行后面变成一整行垃圾，
     * 回放时两边的消息一起丢——不是"丢最后半条"，而是"后续新消息也全丢"。
     * 补一个换行，半行彻底作废，新记录干净。
     */
    private fun openWriter(): BufferedWriter {
        val w = BufferedWriter(FileWriter(logFile, true))
        runCatching {
            if (logFile.length() > 0) {
                val last = java.io.RandomAccessFile(logFile, "r").use { raf ->
                    raf.seek(logFile.length() - 1)
                    raf.read()
                }
                if (last != '\n'.code) w.newLine()
            }
        }
        return w
    }

    /**
     * 日志超过"消息数 × 2"或 1 万行时重写一次，把 mv 记录和重复内容折叠掉。
     *
     * 阈值取得宽：压缩本身是一次全量写，太频繁就退回到旧实现的老路上。
     * 正常聊天很难触发（要一半以上是改名记录）。
     */
    fun compactIfNeeded() {
        synchronized(this) {
            val msgs = data.values.sumOf { it.size }
            if (lines < 10_000 && lines <= msgs * 2 + 256) return
            rewrite()
        }
    }

    private fun rewrite() {
        val tmp = File(dir, "messages.log.tmp")
        runCatching {
            writer?.close()
            writer = null
            var n = 0
            BufferedWriter(FileWriter(tmp, false)).use { w ->
                data.forEach { (who, list) ->
                    list.forEach { m ->
                        w.write(msgLine(who, m).toString())
                        w.newLine()
                        n++
                    }
                }
            }
            // 先落临时文件再改名：中途被杀顶多丢这次压缩，不会毁掉原日志
            if (!tmp.renameTo(logFile)) {
                tmp.copyTo(logFile, overwrite = true)
                tmp.delete()
            }
            lines = n
        }.onFailure {
            tmp.delete()
            lastIoError = "消息日志重写失败: ${it.message}"
        }
    }

    // ------------------------------------------------------------ 载入

    /** 载入历史。返回是否发生了老格式迁移（调用方可能要提示/记录） */
    fun load(): Boolean {
        synchronized(this) {
            data.clear()
            lines = 0
            maxId = 0
            if (logFile.exists()) {
                replay()
                return false
            }
            if (legacyFile.exists()) {
                return migrateLegacy()
            }
            return false
        }
    }

    /** 逐行回放。最后半行（写入时进程被杀）解析失败直接跳过，不影响前面的数据 */
    private fun replay() {
        runCatching {
            logFile.bufferedReader().useLines { seq ->
                seq.forEach { line ->
                    if (line.isBlank()) return@forEach
                    val o = runCatching { JSONObject(line) }.getOrNull() ?: return@forEach
                    when (o.optString("o")) {
                        "m" -> {
                            val m = msgOf(o)
                            data.getOrPut(o.optString("w")) { mutableListOf() }.add(m)
                            // 老日志没有 i，按回放顺序补号；这些号不落盘，但保证后续
                            // append 分配的 id 一定比它们大，不会撞车
                            if (m.id > maxId) maxId = m.id
                        }
                        "mv" -> {
                            val from = o.optString("f")
                            val to = o.optString("t")
                            val moved = data.remove(from) ?: return@forEach
                            val dst = data.getOrPut(to) { mutableListOf() }
                            dst.addAll(moved)
                            dst.sortBy { it.ts }
                        }
                        "st" -> {
                            // 重发成功的回填：按 id 找到那条消息，抹掉失败标记
                            val who = o.optString("w")
                            val id = o.optLong("i")
                            val list = data[who] ?: return@forEach
                            val idx = list.indexOfFirst { it.id == id }
                            if (idx >= 0) list[idx] = list[idx].copy(failed = false)
                        }
                        "seen" -> {
                            // 阅后即焚的计时起点：幂等（重复 seen 不改），回放时继续算
                            val who = o.optString("w")
                            val id = o.optLong("i")
                            val sa = o.optLong("sa")
                            val list = data[who] ?: return@forEach
                            val idx = list.indexOfFirst { it.id == id }
                            if (idx >= 0 && list[idx].seenAt == 0L && sa > 0) {
                                list[idx] = list[idx].copy(seenAt = sa)
                            }
                        }
                        "del" -> {
                            // 撤回的回放：带 "r" 的是按 rid 定位（原位置替换成灰字记录），
                            // 不带 "r" 的是按本地 id 直接删（重发成功后清掉失败那条）
                            val who = o.optString("w")
                            val list = data[who] ?: return@forEach
                            if (o.has("r")) {
                                val rid = o.optString("r")
                                val byMe = o.optString("by") == "me"
                                val idx = list.indexOfFirst { it.rid == rid && it.kind != "recall" }
                                if (idx >= 0) {
                                    val orig = list[idx]
                                    list[idx] = Msg(
                                        text = "",
                                        ts = orig.ts,
                                        outgoing = byMe,
                                        kind = "recall",
                                        id = orig.id,
                                        rid = orig.rid,
                                    )
                                }
                            } else {
                                val id = o.optLong("i")
                                val idx = list.indexOfFirst { it.id == id }
                                if (idx >= 0) list.removeAt(idx)
                            }
                        }
                    }
                    lines++
                }
            }
        }
        // 回放后立刻压一次：日志里可能积累了大量 mv 记录
        if (lines > data.values.sumOf { it.size } + 256) rewrite()
    }

    /**
     * 从老的 messages.json 导入。
     *
     * 原文件**改名保留**而不是删除：万一新格式有 bug，用户的历史还在硬盘上
     * 能人工抢救。磁盘代价只有一份历史。
     */
    private fun migrateLegacy(): Boolean {
        var ok = false
        runCatching {
            val root = JSONObject(legacyFile.readText())
            val tmp = File(dir, "messages.log.tmp")
            var n = 0
            BufferedWriter(FileWriter(tmp, false)).use { w ->
                root.keys().forEach { who ->
                    val arr = root.optJSONArray(who) ?: return@forEach
                    val list = mutableListOf<Msg>()
                    for (i in 0 until arr.length()) {
                        val o = arr.optJSONObject(i) ?: continue
                        val m = Msg(
                            text = o.optString("t"),
                            ts = o.optLong("ts"),
                            outgoing = o.optBoolean("out"),
                            kind = o.optString("k", "text"),
                            id = ++maxId,
                        )
                        list.add(m)
                        w.write(msgLine(who, m).toString())
                        w.newLine()
                        n++
                    }
                    if (list.isNotEmpty()) data[who] = list
                }
            }
            if (!tmp.renameTo(logFile)) {
                tmp.copyTo(logFile, overwrite = true)
                tmp.delete()
            }
            lines = n
            legacyFile.renameTo(File(dir, "messages.json.migrated"))
            ok = true
        }
        return ok
    }

    /**
     * 按本地 id 删除一条消息（重发成功后清掉失败的那条用）。
     * 日志追加 del 记录（不带 "r"），回放时直接移除。
     */
    fun deleteById(who: String, id: Long) {
        synchronized(this) {
            val list = data[who] ?: return
            val idx = list.indexOfFirst { it.id == id }
            if (idx < 0) return
            list.removeAt(idx)
            writeLine(JSONObject().put("o", "del").put("w", who).put("i", id))
        }
    }

    /**
     * 批量删除（多选删除用）。一次加锁、逐条写 del 记录。
     * 返回实际删掉的条数。
     */
    fun deleteMany(who: String, ids: Collection<Long>): Int = synchronized(this) {
        val list = data[who] ?: return 0
        val set = ids.toSet()
        val before = list.size
        list.removeAll { it.id in set }
        val n = before - list.size
        // 每条一个 del 记录：回放逻辑已有，不需要新的记录类型
        ids.forEach { id -> writeLine(JSONObject().put("o", "del").put("w", who).put("i", id)) }
        n
    }

    /**
     * 记录"首次展示"时刻（阅后即焚计时起点）。
     * 落盘成 seen 记录：App 被杀后回放，倒计时从上次的 seenAt 继续算，
     * 不会因为重启重新计时——重新计时等于无限续命。
     */
    fun markSeen(who: String, id: Long, atSecs: Long) {
        synchronized(this) {
            val list = data[who] ?: return
            val idx = list.indexOfFirst { it.id == id }
            if (idx < 0) return
            if (list[idx].seenAt > 0) return   // 幂等：首次展示只记一次
            list[idx] = list[idx].copy(seenAt = atSecs)
            writeLine(
                JSONObject().put("o", "seen").put("w", who).put("i", id).put("sa", atSecs)
            )
        }
    }

    /**
     * 撤回一条消息（按 rid 定位）：原位置替换成一条灰字记录（kind="recall"）。
     * 返回被撤回的原消息（调用方可能要顺手删它引用的本地文件）；找不到返回 null。
     *
     * 日志是 append-only 的，改不了旧行，所以追加一条 del 记录、回放时应用。
     * `by` 记是谁撤的（我 / 对方），灰字文案靠它区分。
     */
    fun recallByRid(who: String, rid: String, byMe: Boolean): Msg? {
        if (rid.isBlank()) return null
        synchronized(this) {
            val list = data[who] ?: return null
            val idx = list.indexOfFirst { it.rid == rid && it.kind != "recall" }
            if (idx < 0) return null
            val orig = list[idx]
            list[idx] = Msg(
                text = "",
                ts = orig.ts,
                outgoing = byMe,
                kind = "recall",
                id = orig.id,
                rid = orig.rid,
            )
            writeLine(
                JSONObject()
                    .put("o", "del").put("w", who).put("r", rid)
                    .put("by", if (byMe) "me" else "peer")
            )
            return orig
        }
    }

    private fun msgOf(o: JSONObject) = Msg(
        text = o.optString("t"),
        ts = o.optLong("ts"),
        outgoing = o.optBoolean("out"),
        kind = o.optString("k", "text"),
        id = o.optLong("i"),
        failed = o.optBoolean("f"),
        rid = o.optString("r"),
        burn = o.optLong("b"),
        seenAt = o.optLong("sa"),
    )
}
