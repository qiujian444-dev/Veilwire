package io.anonchat.app

import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File
import java.nio.file.Files

/**
 * 消息日志的回归测试。
 *
 * 这一层的 bug 都是"用户的历史没了"级别的，而且只在升级/崩溃/改名时才现形，
 * 手工测很难覆盖，所以单独抽成纯 JVM 逻辑（不依赖 Android）用单测钉住。
 */
class MsgStoreTest {
    private fun tmpDir(): File = Files.createTempDirectory("anonmsg").toFile()

    private fun msg(t: String, ts: Long, out: Boolean = false) =
        Msg(t, ts, out, "text")

    @Test
    fun append_then_reload_keeps_order_and_content() {
        val dir = tmpDir()
        val s1 = MsgStore(dir)
        s1.load()
        s1.append("Iris", msg("第一条", 100))
        s1.append("Iris", msg("第二条", 200, out = true))
        s1.append("Bob", msg("另一个会话", 150))

        val s2 = MsgStore(dir)
        s2.load()
        assertEquals(2, s2.count("Iris"))
        assertEquals(listOf("第一条", "第二条"), s2.slice("Iris").map { it.text })
        assertEquals(listOf(false, true), s2.slice("Iris").map { it.outgoing })
        assertEquals(1, s2.count("Bob"))
        assertEquals("第二条", s2.last("Iris")?.text)
    }

    @Test
    fun append_is_append_only_not_rewrite() {
        val dir = tmpDir()
        val s = MsgStore(dir)
        s.load()
        s.append("Iris", msg("a", 1))
        val log = File(dir, "messages.log")
        val sizeAfterFirst = log.length()
        s.append("Iris", msg("b", 2))
        // 第二条只追加一行，文件变大而不是被覆盖成一个新文件
        assertTrue("日志应该是追加的", log.length() > sizeAfterFirst)
        assertEquals(2, log.readLines().count { it.isNotBlank() })
    }

    @Test
    fun move_is_replayed_on_load() {
        val dir = tmpDir()
        val s1 = MsgStore(dir)
        s1.load()
        s1.append("陌生人 abc12345", msg("你好", 100))
        s1.append("陌生人 abc12345", msg("在吗", 200))
        // 加为联系人 → 会话改名
        s1.move("陌生人 abc12345", "Iris")

        assertEquals(0, s1.count("陌生人 abc12345"))
        assertEquals(2, s1.count("Iris"))

        // 重启后必须还在 Iris 名下，否则用户会看到"历史丢了"
        val s2 = MsgStore(dir)
        s2.load()
        assertEquals(0, s2.count("陌生人 abc12345"))
        assertEquals(listOf("你好", "在吗"), s2.slice("Iris").map { it.text })
    }

    @Test
    fun move_merges_into_existing_conversation_sorted_by_time() {
        val dir = tmpDir()
        val s = MsgStore(dir)
        s.load()
        s.append("Iris", msg("新会话", 300))
        s.append("陌生人 x", msg("旧消息", 100))
        s.move("陌生人 x", "Iris")
        assertEquals(listOf("旧消息", "新会话"), s.slice("Iris").map { it.text })
    }

    @Test
    fun legacy_json_is_migrated_and_kept() {
        val dir = tmpDir()
        val legacy = JSONObject()
        val arr = JSONArray()
        arr.put(JSONObject().put("t", "老消息A").put("ts", 10).put("out", false).put("k", "text"))
        arr.put(JSONObject().put("t", "老消息B").put("ts", 20).put("out", true).put("k", "text"))
        legacy.put("Iris", arr)
        File(dir, "messages.json").writeText(legacy.toString())

        val s = MsgStore(dir)
        assertTrue("应该报告发生了迁移", s.load())
        assertEquals(listOf("老消息A", "老消息B"), s.slice("Iris").map { it.text })
        assertTrue(File(dir, "messages.log").exists())
        // 原文件改名保留：新格式万一有 bug，历史还能人工抢救
        assertFalse(File(dir, "messages.json").exists())
        assertTrue(File(dir, "messages.json.migrated").exists())

        // 再启动一次不能再迁一遍（否则会重复导入）
        val s2 = MsgStore(dir)
        s2.load()
        assertEquals(2, s2.count("Iris"))
    }

    @Test
    fun truncated_last_line_is_skipped() {
        val dir = tmpDir()
        val log = File(dir, "messages.log")
        log.writeText(
            """{"o":"m","w":"Iris","t":"完整","ts":1,"out":false,"k":"text"}""" + "\n" +
                // 进程被杀时留下的半行
                """{"o":"m","w":"Iris","t":"半"""
        )
        val s = MsgStore(dir)
        s.load()
        assertEquals(1, s.count("Iris"))
        assertEquals("完整", s.slice("Iris")[0].text)

        // 半行不能拖累后续写入
        s.append("Iris", msg("之后", 2))
        val s2 = MsgStore(dir)
        s2.load()
        assertEquals(2, s2.count("Iris"))
    }

    @Test
    fun compact_keeps_content_and_shrinks_log() {
        val dir = tmpDir()
        val s = MsgStore(dir)
        s.load()
        s.append("Iris", msg("一条", 1))
        // 制造大量改名记录（真实场景：反复改名会留下痕迹）
        repeat(400) { i -> s.move("别名$i", "Iris") }
        val before = File(dir, "messages.log").length()

        s.compactIfNeeded()

        val s2 = MsgStore(dir)
        s2.load()
        assertEquals(1, s2.count("Iris"))
        assertEquals("一条", s2.slice("Iris")[0].text)
        assertTrue("压缩后日志应变小", File(dir, "messages.log").length() < before)

        // 压缩完还能继续追加
        s2.append("Iris", msg("压缩后", 2))
        val s3 = MsgStore(dir)
        s3.load()
        assertEquals(2, s3.count("Iris"))
    }

    @Test
    fun slice_clamps_out_of_range() {
        val dir = tmpDir()
        val s = MsgStore(dir)
        s.load()
        (0 until 10).forEach { s.append("Iris", msg("m$it", it.toLong())) }

        assertEquals(10, s.count("Iris"))
        assertEquals(listOf("m5", "m6"), s.slice("Iris", 5, 7).map { it.text })
        assertEquals(3, s.slice("Iris", 7, 99).size)   // to 越界自动截断
        assertTrue(s.slice("Iris", 20, 30).isEmpty())  // from 越界返回空
        assertEquals(0, s.count("不存在"))
        assertTrue(s.slice("不存在").isEmpty())
    }

    @Test
    fun keys_list_unknown_after_move_is_clean() {
        val dir = tmpDir()
        val s = MsgStore(dir)
        s.load()
        s.append("会话A", msg("a", 1))
        s.move("会话A", "会话B")
        assertEquals(listOf("会话B"), s.keys())
    }

    // ------------------------------------------------------------ 主键与失败状态（0.8.0）

    @Test
    fun append_assigns_unique_ids_that_survive_reload() {
        val dir = tmpDir()
        val s1 = MsgStore(dir)
        s1.load()
        val m1 = s1.append("Iris", msg("一", 1))
        val m2 = s1.append("Iris", msg("二", 2))
        val m3 = s1.append("Bob", msg("三", 3))
        assertTrue("id 必须非零（0 是老数据的占位）", m1.id > 0)
        assertTrue("id 必须递增", m1.id < m2.id && m2.id < m3.id)

        val s2 = MsgStore(dir)
        s2.load()
        assertEquals(listOf(m1.id, m2.id), s2.slice("Iris").map { it.id })
        assertEquals(m3.id, s2.slice("Bob")[0].id)
    }

    @Test
    fun failed_flag_is_persisted_and_cleared_by_markSent() {
        val dir = tmpDir()
        val s1 = MsgStore(dir)
        s1.load()
        val failed = s1.append("Iris", Msg("没发出去", 1, outgoing = true, kind = "text", failed = true))
        val ok = s1.append("Iris", msg("正常", 2))

        // 重启后失败标记还在（不能重启一次就"假装发成功"了）
        val s2 = MsgStore(dir)
        s2.load()
        val loaded = s2.slice("Iris").first { it.id == failed.id }
        assertTrue(loaded.failed)
        assertFalse(s2.slice("Iris").first { it.id == ok.id }.failed)

        // 重发成功 → markSent 抹掉标记；内存和重启后都要是 false
        s2.markSent("Iris", failed.id)
        assertFalse(s2.slice("Iris").first { it.id == failed.id }.failed)
        val s3 = MsgStore(dir)
        s3.load()
        assertFalse("st 记录必须在回放时生效", s3.slice("Iris").first { it.id == failed.id }.failed)
        // st 记录不产生新消息，两条还是两条
        assertEquals(2, s3.count("Iris"))
    }

    @Test
    fun markSent_for_unknown_id_is_noop() {
        val dir = tmpDir()
        val s = MsgStore(dir)
        s.load()
        s.append("Iris", msg("a", 1))
        s.markSent("Iris", 99999)
        s.markSent("不存在的会话", 1)
        assertEquals(1, s.count("Iris"))
    }

    @Test
    fun compact_preserves_ids_and_failed_state() {
        val dir = tmpDir()
        val s = MsgStore(dir)
        s.load()
        s.append("Iris", Msg("失败的", 1, outgoing = true, failed = true))
        s.append("Iris", msg("好的", 2))
        repeat(400) { i -> s.move("别名$i", "Iris") }
        s.compactIfNeeded()

        val s2 = MsgStore(dir)
        s2.load()
        val msgs = s2.slice("Iris")
        assertEquals(2, msgs.size)
        assertTrue(msgs.first { it.text == "失败的" }.failed)
        assertTrue(msgs.first { it.text == "好的" }.id > 0)
    }

    @Test
    fun preview_marks_failed_outgoing() {
        // 预览是结构化数据（不是拼好的句子）：文案归 UI 层，这里只测结构
        val ok = Msg("你好", 1, outgoing = true).preview
        val bad = Msg("你好", 1, outgoing = true, failed = true).preview
        assertEquals(Preview.Text("你好"), ok)
        assertEquals(Preview.Text("你好", failed = true), bad)
        // 收到的消息没有"未发出"一说
        val incoming = Msg("x", 1, outgoing = false, failed = true).preview
        assertEquals(Preview.Text("x"), incoming)
    }

    @Test
    fun preview_keeps_kind_and_payload() {
        // 文件：带不带文件名都要能表达
        assertEquals(
            Preview.File("a.pdf"),
            Msg("{\"name\":\"a.pdf\"}", 1, outgoing = true, kind = "file").preview
        )
        assertEquals(Preview.File(""), Msg("{}", 1, outgoing = true, kind = "file").preview)
        // 图片/视频
        assertEquals(Preview.Image(), Msg("", 1, outgoing = false, kind = "image").preview)
        assertEquals(Preview.Video(), Msg("", 1, outgoing = false, kind = "video").preview)
        // 撤回：方向不同
        assertEquals(Preview.Recall(byMe = true), Msg("", 1, outgoing = true, kind = "recall").preview)
        assertEquals(Preview.Recall(byMe = false), Msg("", 1, outgoing = false, kind = "recall").preview)
    }

    // ------------------------------------------------------------ 撤回（0.9.0）

    @Test
    fun rid_round_trips_through_the_log() {
        val dir = tmpDir()
        val s1 = MsgStore(dir)
        s1.load()
        s1.append("Iris", Msg("带引用", 1, outgoing = true, rid = "rid-abc"))
        s1.append("Iris", msg("不带引用", 2))
        val s2 = MsgStore(dir)
        s2.load()
        assertEquals("rid-abc", s2.slice("Iris")[0].rid)
        assertEquals("", s2.slice("Iris")[1].rid)
    }

    @Test
    fun recall_replaces_in_place_and_survives_reload() {
        val dir = tmpDir()
        val s1 = MsgStore(dir)
        s1.load()
        s1.append("Iris", msg("第一条", 100))
        s1.append("Iris", Msg("要撤的", 200, outgoing = true, rid = "rid-1"))
        s1.append("Iris", msg("第三条", 300))

        val orig = s1.recallByRid("Iris", "rid-1", byMe = true)
        requireNotNull(orig)
        assertEquals("要撤的", orig.text)

        // 内存里原位变灰字记录：条数不变、位置不变（按 ts 排序还在中间）
        val after = s1.slice("Iris")
        assertEquals(3, after.size)
        assertEquals("recall", after[1].kind)
        assertEquals("", after[1].text)
        assertTrue(after[1].outgoing)   // byMe
        assertEquals("rid-1", after[1].rid)
        assertEquals(200L, after[1].ts)

        // 重启后回放 del 记录，结果一致
        val s2 = MsgStore(dir)
        s2.load()
        val replayed = s2.slice("Iris")
        assertEquals(3, replayed.size)
        assertEquals("recall", replayed[1].kind)
        assertEquals("rid-1", replayed[1].rid)
        assertTrue(replayed[1].outgoing)
    }

    @Test
    fun recall_by_peer_shows_incoming_note() {
        val dir = tmpDir()
        val s = MsgStore(dir)
        s.load()
        s.append("Iris", Msg("对方发的", 1, outgoing = false, rid = "rid-p"))
        s.recallByRid("Iris", "rid-p", byMe = false)
        val m = s.slice("Iris")[0]
        assertEquals("recall", m.kind)
        assertFalse(m.outgoing)

        val s2 = MsgStore(dir)
        s2.load()
        assertFalse(s2.slice("Iris")[0].outgoing)
    }

    @Test
    fun recall_unknown_rid_and_double_recall_are_noop() {
        val dir = tmpDir()
        val s = MsgStore(dir)
        s.load()
        s.append("Iris", Msg("普通", 1, outgoing = true, rid = "rid-x"))
        assertNull(s.recallByRid("Iris", "不存在的", byMe = true))
        assertNull(s.recallByRid("Iris", "", byMe = true))   // 空 rid 直接拒绝
        s.recallByRid("Iris", "rid-x", byMe = true)
        // 已经是灰字记录的不会再被匹配（kind != "recall" 的守卫）
        assertNull(s.recallByRid("Iris", "rid-x", byMe = true))
        assertEquals(1, s.count("Iris"))
    }

    @Test
    fun delete_by_id_removes_and_replays() {
        val dir = tmpDir()
        val s1 = MsgStore(dir)
        s1.load()
        val m = s1.append("Iris", Msg("失败的", 1, outgoing = true, failed = true))
        s1.append("Iris", msg("后面的", 2))
        s1.deleteById("Iris", m.id)

        assertEquals(1, s1.count("Iris"))
        val s2 = MsgStore(dir)
        s2.load()
        assertEquals(listOf("后面的"), s2.slice("Iris").map { it.text })
    }

    @Test
    fun recall_preview_depends_on_direction() {
        // 撤回灰字的具体措辞由 UI 层按语言生成，数据层只给方向
        assertTrue(Msg("", 1, outgoing = true, kind = "recall").preview is Preview.Recall)
        assertTrue((Msg("", 1, outgoing = true, kind = "recall").preview as Preview.Recall).byMe)
        assertFalse((Msg("", 1, outgoing = false, kind = "recall").preview as Preview.Recall).byMe)
    }

    // ------------------------------------------------------------ 阅后即焚（0.10.0）

    @Test
    fun burn_and_seenAt_round_trip_through_the_log() {
        val dir = tmpDir()
        val s1 = MsgStore(dir)
        s1.load()
        s1.append("Iris", Msg("会烧的", 1, outgoing = true, burn = 30L))
        s1.append("Iris", msg("普通的", 2))
        val s2 = MsgStore(dir)
        s2.load()
        assertEquals(30L, s2.slice("Iris")[0].burn)
        assertEquals(0L, s2.slice("Iris")[0].seenAt)   // 还没展示
        assertEquals(0L, s2.slice("Iris")[1].burn)
    }

    @Test
    fun markSeen_is_idempotent_and_survives_reload() {
        val dir = tmpDir()
        val s1 = MsgStore(dir)
        s1.load()
        val m = s1.append("Iris", Msg("焚毁消息", 1, outgoing = true, burn = 30L))
        s1.markSeen("Iris", m.id, 1000)
        // 重复 seen 不能改时间（改了等于无限续命）
        s1.markSeen("Iris", m.id, 9999)

        val s2 = MsgStore(dir)
        s2.load()
        val loaded = s2.slice("Iris")[0]
        assertEquals(1000L, loaded.seenAt)
        // 重启后到期判定继续成立（倒计时不能凭空重来）
        assertTrue(Burn.expired(loaded.burn, loaded.seenAt, 1030L))
    }

    @Test
    fun burn_message_can_be_deleted_by_id_like_any_other() {
        val dir = tmpDir()
        val s = MsgStore(dir)
        s.load()
        val m = s.append("Iris", Msg("会烧的", 1, outgoing = true, burn = 30L))
        s.markSeen("Iris", m.id, 1000)
        s.deleteById("Iris", m.id)   // sweepExpired 到期就是走这个
        assertEquals(0, s.count("Iris"))
        val s2 = MsgStore(dir)
        s2.load()
        assertEquals(0, s2.count("Iris"))
    }
    // ---------------------------------------------------------------- 0.12.0 新增

    @Test
    fun migrate_keys_to_dh_merges_and_sorts() {
        val dir = tmpDir()
        val s1 = MsgStore(dir)
        s1.load()
        s1.append("Iris", msg("旧名字下的消息", 200))
        s1.append("陌生人 abc12345", msg("陌生时期的消息", 100))

        // 两个旧键都映射到同一个 dh：应当合并并按时间排序
        val n = s1.migrateKeysToDh(
            mapOf("Iris" to "DH_IRIS", "陌生人 abc12345" to "DH_IRIS")
        )
        assertEquals(2, n)
        assertEquals(listOf("陌生时期的消息", "旧名字下的消息"),
            s1.slice("DH_IRIS").map { it.text })
        assertEquals(0, s1.count("Iris"))

        // 重启后仍然是迁移后的状态（mv 记录回放）
        val s2 = MsgStore(dir)
        s2.load()
        assertEquals(2, s2.count("DH_IRIS"))
        assertEquals(0, s2.count("Iris"))
    }

    @Test
    fun migrate_is_idempotent() {
        val dir = tmpDir()
        val s = MsgStore(dir)
        s.load()
        s.append("Iris", msg("hi", 1))
        assertEquals(1, s.migrateKeysToDh(mapOf("Iris" to "DH")))
        // 再迁一次：旧键已不存在，不应重复搬运或报错
        assertEquals(0, s.migrateKeysToDh(mapOf("Iris" to "DH")))
        assertEquals(1, s.count("DH"))
    }

    @Test
    fun delete_many_removes_and_persists() {
        val dir = tmpDir()
        val s1 = MsgStore(dir)
        s1.load()
        val a = s1.append("DH", msg("a", 1))
        val b = s1.append("DH", msg("b", 2))
        s1.append("DH", msg("c", 3))
        assertEquals(2, s1.deleteMany("DH", listOf(a.id, b.id)))
        assertEquals(listOf("c"), s1.slice("DH").map { it.text })

        val s2 = MsgStore(dir)
        s2.load()
        assertEquals(listOf("c"), s2.slice("DH").map { it.text })
    }

    @Test
    fun drop_conversation_removes_all_and_persists() {
        val dir = tmpDir()
        val s1 = MsgStore(dir)
        s1.load()
        s1.append("DH1", msg("x", 1))
        s1.append("DH1", msg("y", 2))
        s1.append("DH2", msg("z", 3))
        assertEquals(2, s1.dropConversation("DH1"))
        assertEquals(0, s1.count("DH1"))
        assertEquals(1, s1.count("DH2"))

        val s2 = MsgStore(dir)
        s2.load()
        assertEquals(0, s2.count("DH1"))
        assertEquals(1, s2.count("DH2"))
    }

    @Test
    fun contact_display_prefers_alias() {
        val c = Contact(name = "alice", pk = "P", dh = "D", qid = "Q", alias = "Alice (work)")
        assertEquals("Alice (work)", c.display)
        assertEquals("alice", c.copy(alias = "").display)
    }
}
