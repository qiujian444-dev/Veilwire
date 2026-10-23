package io.anonchat.app

import android.content.Context
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import org.json.JSONArray
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL

/** 中继列表里的一条 */
data class RelayEntry(
    val url: String,
    val region: String,
    val note: String,
)

/**
 * 三级中继来源（docs 第 9 节）落地：
 *
 * ① 引导层：启动时拉一次官方静态 JSON（境外托管），失败就用内置缓存；
 * ② 社区层：列表内容来自社区仓库，App 明确声明"不背书、不保证可用"；
 * ③ 自定义层：用户手输 URL（自己家 NAS / 云主机），永远保留入口。
 *
 * 拉取设计上有意做得很"轻"：
 * - 一个普通 HTTPS GET，5 秒超时，失败静默降级到内置列表；
 * - 不带任何设备指纹（没有 UA 之外的头，没有上报），这和无身份的定位一致。
 */
object RelayList {

    /** 官方收录列表地址（社区仓库维护，App 内置的是它的快照） */
    private const val BOOTSTRAP_URL =
        "https://raw.githubusercontent.com/anonchat/relay-list/main/relays.json"

    private const val PREFS = "anon"
    private const val KEY_CACHE = "relay_list_cache"

    /** 内置快照：离线/首启网络不好时的兜底，保证 App 永远"有得选或能手输" */
    fun bundled(ctx: Context): List<RelayEntry> = try {
        val txt = ctx.assets.open("relays.json").bufferedReader().readText()
        parse(txt)
    } catch (_: Exception) {
        emptyList()
    }

    /**
     * 取列表：优先用远程的最新列表；失败则用上次缓存的；再失败用内置快照。
     * 返回 (列表, 来源)。
     */
    fun load(ctx: Context): Pair<List<RelayEntry>, String> {
        val cached = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getString(KEY_CACHE, null)?.let { runCatching { parse(it) }.getOrNull() }

        try {
            val conn = URL(BOOTSTRAP_URL).openConnection() as HttpURLConnection
            conn.connectTimeout = 5000
            conn.readTimeout = 5000
            conn.setRequestProperty("Accept", "application/json")
            if (conn.responseCode == 200) {
                val txt = conn.inputStream.bufferedReader().readText()
                val list = parse(txt)
                if (list.isNotEmpty()) {
                    ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
                        .edit().putString(KEY_CACHE, txt).apply()
                    return list to ctx.getString(R.string.relay_src_online)
                }
            }
        } catch (_: Exception) {
            // 网络不通是常态（尤其首次启动还没配网/在境内），静默降级
        }

        // isNullOrEmpty() 不做智能转换，这里必须先判 null 再判空，
        // 否则 cached 保持可空类型，Pair 类型对不上声明的返回值
        if (cached != null && cached.isNotEmpty()) return cached to ctx.getString(R.string.relay_src_cache)
        val bundled = bundled(ctx)
        if (bundled.isNotEmpty()) return bundled to ctx.getString(R.string.relay_src_bundled)
        return emptyList<RelayEntry>() to ctx.getString(R.string.relay_src_none)
    }

    /** 并发探测一组中继，返回 url -> 毫秒（失败的返回 -1） */
    suspend fun probeAll(urls: List<String>): Map<String, Long> {
        val out = java.util.concurrent.ConcurrentHashMap<String, Long>()
        val jobs = urls.map { url ->
            kotlinx.coroutines.GlobalScope.launch(Dispatchers.IO) {
                out[url] = Repo.relayProbe(url).data ?: -1L
            }
        }
        jobs.forEach { it.join() }
        return out
    }

    private fun parse(txt: String): List<RelayEntry> {
        val root = JSONObject(txt)
        val arr: JSONArray = root.optJSONArray("relays") ?: return emptyList()
        return (0 until arr.length()).mapNotNull { i ->
            val o = arr.optJSONObject(i) ?: return@mapNotNull null
            val url = o.optString("url")
            if (url.isBlank()) null
            else RelayEntry(url, o.optString("region"), o.optString("note"))
        }
    }
}
