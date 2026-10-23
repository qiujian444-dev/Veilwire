package io.anonchat.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.SystemClock
import android.util.Log
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import kotlin.math.min

/**
 * 消息监听前台服务。
 *
 * 这是整个 App 能不能当日常 IM 用的关键：
 * - 类型必须是 remoteMessaging（Android 14+）。**不能用 dataSync**——
 *   Android 15 起 dataSync 24 小时只有 6 小时配额，超时直接崩。
 * - 循环调用 Rust 侧的 recvOnce（阻塞但带超时），连接由 Rust 侧复用，不会每条消息重连。
 * - 断线指数退避重连，绝不搞固定间隔轮询（Doze 下最快也要 9 分钟一次）。
 *
 * 同时把运行统计写进 CSV，供 0.3 的 24 小时实测分析。
 */
class MessagingService : Service() {

    companion object {
        private const val TAG = "AnonChat"
    private const val CHANNEL_ID = "msg"
    /** 新消息通知专用渠道（带提示音）。旧渠道「msg」是静音的，换 ID 强制重建 */
    private const val MSG_CHANNEL_ID = "msg-sound"
    private const val NOTI_ID = 2001
        private const val RECV_TIMEOUT_SECS = 30L
        private const val STATS_FLUSH_MS = 5 * 60_000L

        const val ACTION_START = "io.anonchat.app.START"
        const val ACTION_STOP = "io.anonchat.app.STOP"

        var state by mutableStateOf(ServiceState())
            private set

        /**
         * 每收到一条消息 +1。UI 层（聊天列表/聊天页）观察这个值实现实时刷新——
         * 之前消息到达后界面不动，要切 Tab 或重进才能看到，就是因为没有这条通路。
         */
        var inboxTick by mutableStateOf(0)
            private set
    }

    data class ServiceState(
        val running: Boolean = false,
        val reconnects: Int = 0,
        val received: Int = 0,
        val uptimeSecs: Long = 0,
        val lastError: String? = null,
    )

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var loopJob: Job? = null
    private var statsJob: Job? = null

    private var running = false
    private var reconnects = 0
    private var received = 0
    private var onlineSince = 0L
    private var totalOnlineMs = 0L
    private var lastError: String? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> {
                shutdown()
                return START_NOT_STICKY
            }
            else -> {
                // 服务可能被系统单独拉起（进程被杀后 START_STICKY 重启），
                // 这时 MainActivity.onCreate 没有跑过，必须自己初始化一次
                Repo.init(this)
                // 通话说到底也靠这个服务收信令：WebRTC 需要 PeerConnectionFactory，
                // 初始化放在这里，保证服务先起来时也不会漏掉第一通电话
                CallManager.init(this)
                startAsForeground()
                if (loopJob == null) {
                    running = true
                    onlineSince = SystemClock.elapsedRealtime()
                    // 老版本升级上来的设备可能只有身份、没有收件队列（队列是生成邀请码时才申请的）。
                    // 没有队列就收不到任何消息，所以每轮启动都补一次（已有则只补中继）。
                    scope.launch {
                        val r = withContext(Dispatchers.IO) { Repo.ensureQueue() }
                        if (!r.ok) Log.i(TAG, "ensureQueue failed (offline or no relay, retry later): ${r.error}")
                    }
                    loopJob = scope.launch { listenLoop() }
                    statsJob = scope.launch { statsLoop() }
                }
                publish()
            }
        }
        return START_STICKY
    }

    private fun startAsForeground() {
        val pi = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
        val noti: Notification = Notification.Builder(this, ensureChannel())
            .setContentTitle(getString(R.string.msg_svc_title))
            .setContentText(
                if (BatteryOpt.isIgnoring(this)) getString(R.string.msg_svc_text_ok)
                else getString(R.string.msg_svc_text_battery)
            )
            .setSmallIcon(android.R.drawable.stat_notify_chat)
            .setOngoing(true)
            .setContentIntent(pi)
            .build()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(NOTI_ID, noti, ServiceInfo.FOREGROUND_SERVICE_TYPE_REMOTE_MESSAGING)
        } else {
            startForeground(NOTI_ID, noti)
        }
    }

    private fun ensureChannel(): String {
        val nm = getSystemService(NotificationManager::class.java)
        if (nm.getNotificationChannel(CHANNEL_ID) == null) {
            nm.createNotificationChannel(
                NotificationChannel(CHANNEL_ID, getString(R.string.msg_svc_channel), NotificationManager.IMPORTANCE_LOW).apply {
                    setShowBadge(false)
                    description = getString(R.string.msg_svc_channel_desc)
                }
            )
        }
        return CHANNEL_ID
    }

    /**
     * 新消息通知渠道。
     *
     * 为什么单独一个渠道：之前新消息和前台服务共用 CHANNEL_ID，而服务渠道是
     * IMPORTANCE_LOW（Android 规范：LOW = 静音、不横幅），于是**每条新消息都无声**——
     * 用户反馈"新消息怎么一点动静都没有"就是这么来的。渠道设置在创建时就固定了，
     * 后改 IMPORTANCE 无效，所以换新 ID「msg-sound」强制重建。
     *
     * IMPORTANCE_HIGH：横幅 + 声音。声音/震动用系统默认（渠道不 setSound(null) 即响），
     * 用户在系统设置里改"通知 → 畅聊 → 新消息"的铃声即可自定义。
     */
    private fun ensureMsgChannel(): String {
        val nm = getSystemService(NotificationManager::class.java)
        if (nm.getNotificationChannel(MSG_CHANNEL_ID) == null) {
            nm.createNotificationChannel(
                NotificationChannel(MSG_CHANNEL_ID, getString(R.string.msg_channel), NotificationManager.IMPORTANCE_HIGH).apply {
                    setShowBadge(true)
                    enableVibration(true)
                    lockscreenVisibility = Notification.VISIBILITY_PUBLIC
                    description = getString(R.string.msg_channel_desc)
                }
            )
        }
        return MSG_CHANNEL_ID
    }

    /** 主循环：收 → 失败退避 → 再收。 */
    private suspend fun listenLoop() {
        var backoff = 2_000L
        while (scope.isActive && running) {
            // 循环体整体兜住异常：任何一次未捕获的异常都会让协程结束，
            // 而服务还"活着"、loopJob 也不为 null，于是再也不会重试 —— 表现为永久静默。
            val res = try {
                withContext(Dispatchers.IO) { Repo.recvOnce(RECV_TIMEOUT_SECS) }
            } catch (t: Throwable) {
                Log.e(TAG, "receive loop error", t)
                // 技术信息走 error 通道（语言中性），这条只在后台异常时进日志
                Res<Incoming?>(null, "receive failed: ${t.message}")
            }
            if (res.ok) {
                backoff = 2_000L
                // 处理段也必须兜住：通知/信令/状态发布的任何异常如果逃出这里，
                // 协程直接结束，服务还"活着"但再也不收消息（永久静默）。
                try {
                    // Rust 侧的解密失败等告警：不打出来就永远查不到
                    Repo.drainWarnings().forEach { Log.w(TAG, "core warning: $it") }

                    // 阅后即焚到期清扫：收消息循环每轮一次（空闲时每 RECV_TIMEOUT_SECS
                    // 一轮），配合聊天页的每秒 tick 和启动清扫，三层兜底
                    Repo.sweepExpired()

                    if (res.data != null) {
                        received += 1
                        inboxTick += 1
                        val inc = res.data
                        if (inc.kind == "clear.exec") {
                            // 紧急清除指令：对方已在发起端通过暗号+算术码验证。
                            // 无确认直接清本机（listenLoop 是顺序循环，执行期间
                            // 没有并发 recv，不存在抢消息竞态）。清完杀进程重启，
                            // App 回到初始状态；前台服务随进程被系统重新拉起。
                            Log.w(TAG, "panic wipe command received, wiping locally")
                            Repo.executePanic(Repo.whoOf(inc), notifyPeer = false)
                            Runtime.getRuntime().exit(0)
                        } else if (inc.isCall) {
                            // 通话信令：交给状态机 + WebRTC，不弹"新消息"通知
                            CallManager.onSignal(inc)
                        } else {
                            // 未读数：正开着这个会话就不计（markUnread 内部判断）
                            Repo.markUnread(Repo.whoOf(inc))
                            notifyMessage(inc)
                        }
                        publish()
                    }
                } catch (t: Throwable) {
                    Log.e(TAG, "message handling failed", t)
                }
                // 超时也正常，继续下一轮（连接在 Rust 侧复用）
            } else {
                lastError = res.error
                Log.w(TAG, "receive failed: ${res.error}")
                reconnects += 1
                publish()
                delay(backoff)
                backoff = min(backoff * 2, 60_000L)
            }
        }
        totalOnlineMs += SystemClock.elapsedRealtime() - onlineSince
        onlineSince = 0
    }

    private fun notifyMessage(inc: Incoming) {
        // 通话中不再为普通消息弹通知，避免盖住通话界面
        if (CallManager.ui.visible) return
        // 用户正开着这个会话的聊天页（App 在前台）：消息实时显示在界面上，
        // 再弹一条通知纯属打扰。注意只在"前台 + 正是这个会话"时抑制。
        // 会话键要用 whoOf（fromName 可能为空，落库键和聊天页标题都用它）。
        if (MainActivity.resumed && MainActivity.openPeer != null
            && MainActivity.openPeer == Repo.whoOf(inc)
        ) return
        val nm = getSystemService(NotificationManager::class.java)
        val who = inc.fromName ?: getString(R.string.notif_new_message)
        // 通知栏是阅后即焚最大的洞：内容进了通知历史（Android 13+ 可回翻）
        // 就等于永久留了一份，焚毁删不掉它。所以焚毁消息必须脱敏；
        // 顺手把附件消息的 JSON 串也换成一句人话（之前通知里显示的是原始 JSON）。
        val body = when {
            inc.burn > 0 -> getString(R.string.notif_burn_body)
            inc.isBlob -> getString(R.string.notif_blob_body)
            else -> inc.text
        }
        val n = Notification.Builder(this, ensureMsgChannel())
            .setContentTitle(who)
            .setContentText(body)
            .setSmallIcon(android.R.drawable.stat_notify_chat)
            .setAutoCancel(true)
            .build()
        nm.notify((System.currentTimeMillis() % 10000).toInt(), n)
    }

    private suspend fun statsLoop() {
        val f = File(getExternalFilesDir(null) ?: filesDir, "stage0-stats.csv")
        if (!f.exists()) {
            f.writeText("time,uptime_secs,reconnects,received,whitelisted\n")
        }
        val fmt = SimpleDateFormat("yyyy-MM-dd HH:mm:ss", Locale.US)
        while (scope.isActive && running) {
            delay(STATS_FLUSH_MS)
            val up = uptimeSecs()
            runCatching {
                f.appendText(
                    "${fmt.format(Date())},$up,$reconnects,$received,${if (BatteryOpt.isIgnoring(this)) 1 else 0}\n"
                )
            }
            publish()
        }
    }

    private fun uptimeSecs(): Long {
        val extra = if (onlineSince > 0) SystemClock.elapsedRealtime() - onlineSince else 0
        return (totalOnlineMs + extra) / 1000
    }

    private fun publish() {
        state = ServiceState(
            running = running,
            reconnects = reconnects,
            received = received,
            uptimeSecs = uptimeSecs(),
            lastError = lastError,
        )
    }

    private fun shutdown() {
        running = false
        loopJob?.cancel()
        statsJob?.cancel()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
            stopForeground(STOP_FOREGROUND_REMOVE)
        } else {
            @Suppress("DEPRECATION")
            stopForeground(true)
        }
        publish()
        stopSelf()
    }

    override fun onDestroy() {
        running = false
        scope.cancel()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?) = null
}
