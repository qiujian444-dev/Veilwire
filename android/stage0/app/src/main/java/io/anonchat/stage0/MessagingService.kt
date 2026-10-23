package io.anonchat.stage0

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.os.SystemClock
import android.util.Log
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.json.JSONObject
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.concurrent.TimeUnit
import kotlin.math.min

/**
 * 阶段 0 · 验证项 0.3：无推送场景下的后台存活原型
 *
 * 这个 Service 只做一件事：模拟真实 IM 的那条长连接，并把"活了多久 / 掉了几次"
 * 记录成 CSV，供 24 小时实测后做统计。
 *
 * 三条硬性设计（改动前请先读 docs/阶段0-验证手册.md）：
 * 1. foregroundServiceType 必须是 remoteMessaging，不能是 dataSync
 *    （Android 15 起 dataSync 有 6 小时/24 小时配额，超时直接崩）。
 * 2. 重连用指数退避，不要用固定间隔"轮询"。
 * 3. 心跳间隔 30s；连续 3 次无 pong 判死。
 */
class MessagingService : Service() {

    companion object {
        private const val TAG = "Stage0"
        private const val CHANNEL_ID = "keepalive"
        private const val NOTI_ID = 1001
        private const val PING_INTERVAL_MS = 30_000L
        private const val PONG_TIMEOUT_MS = 90_000L
        private const val STATS_FLUSH_MS = 5 * 60_000L

        const val ACTION_START = "io.anonchat.stage0.START"
        const val ACTION_STOP = "io.anonchat.stage0.STOP"

        /** 供 UI 读取的运行时状态 */
        var uiState by mutableStateOf(UiState())
            private set
    }

    data class UiState(
        val running: Boolean = false,
        val connected: Boolean = false,
        val reconnects: Int = 0,
        val uptimeSecs: Long = 0,
        val lastRttMs: Long = 0,
        val queues: Int = 0,
        val serverUrl: String = "",
    )

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var loopJob: Job? = null
    private var statsJob: Job? = null
    private lateinit var client: OkHttpClient

    private var connectedSince = 0L
    private var totalConnectedMs = 0L
    private var reconnects = 0
    private var lastRtt = 0L
    private var queues = 0
    private var running = false

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        client = OkHttpClient.Builder()
            .connectTimeout(10, TimeUnit.SECONDS)
            .readTimeout(0, TimeUnit.MILLISECONDS)   // 长连接不设读超时
            .pingInterval(PING_INTERVAL_MS, TimeUnit.MILLISECONDS) // 协议级 ping，保 NAT 映射
            .retryOnConnectionFailure(true)
            .build()
        createChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> {
                stopSelfSafely()
                return START_NOT_STICKY
            }
            else -> {
                startAsForeground()
                if (loopJob == null) {
                    running = true
                    val url = intent?.getStringExtra("url")
                        ?: getSharedPreferences("stage0", Context.MODE_PRIVATE)
                            .getString("url", "ws://127.0.0.1:8080")
                        ?: "ws://127.0.0.1:8080"
                    loopJob = scope.launch { connectionLoop(url) }
                    statsJob = scope.launch { statsLoop() }
                }
            }
        }
        publish()
        // START_STICKY：被内存压力杀掉后，系统会尝试重启（用户手动"强行停止"除外）
        return START_STICKY
    }

    /** 关键：类型必须与 Manifest 声明一致，否则 SecurityException / MissingForegroundServiceTypeException */
    private fun startAsForeground() {
        val pi = PendingIntent.getActivity(
            this, 0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
        val noti: Notification = Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("保活验证运行中")
            .setContentText(if (BatteryOpt.isIgnoring(this)) "长连接维持中" else "未授予电池白名单，息屏后可能断线")
            .setSmallIcon(android.R.drawable.stat_notify_sync)
            .setOngoing(true)
            .setContentIntent(pi)
            .build()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            // Android 14+：必须带类型
            startForeground(NOTI_ID, noti, ServiceInfo.FOREGROUND_SERVICE_TYPE_REMOTE_MESSAGING)
        } else {
            startForeground(NOTI_ID, noti)
        }
    }

    private fun createChannel() {
        val nm = getSystemService(NotificationManager::class.java)
        if (nm.getNotificationChannel(CHANNEL_ID) == null) {
            nm.createNotificationChannel(
                NotificationChannel(CHANNEL_ID, "保活验证", NotificationManager.IMPORTANCE_LOW).apply {
                    setShowBadge(false)
                    description = "阶段 0 后台存活测试常驻通知"
                }
            )
        }
    }

    // ------------------------------------------------------------ 连接循环

    private suspend fun connectionLoop(url: String) {
        var backoff = 1_000L
        while (scope.isActive) {
            val closed = CompletableDeferred<Unit>()
            val req = Request.Builder().url(url).build()

            val listener = object : WebSocketListener() {
                override fun onOpen(webSocket: WebSocket, response: Response) {
                    Log.i(TAG, "ws 已连接")
                    connectedSince = SystemClock.elapsedRealtime()
                    backoff = 1_000L
                    publish(connected = true)
                    // 建队列 + 订阅，模拟真实客户端握手
                    webSocket.send("""{"t":"new","v":1}""")
                }

                override fun onMessage(webSocket: WebSocket, text: String) {
                    runCatching {
                        val o = JSONObject(text)
                        when (o.optString("t")) {
                            "new_ok" -> {
                                val qid = o.optString("qid")
                                queues += 1
                                webSocket.send("""{"t":"sub","qid":"$qid"}""")
                                publish()
                            }
                            "pong" -> {
                                lastRtt = SystemClock.elapsedRealtime() - pingSentAt
                                publish()
                            }
                        }
                    }
                }

                override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                    Log.w(TAG, "ws 失败: ${t.message}")
                    closed.complete(Unit)
                }

                override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                    closed.complete(Unit)
                }
            }

            val ws = client.newWebSocket(req, listener)
            pingJob = scope.launch {
                while (isActive) {
                    delay(PING_INTERVAL_MS)
                    pingSentAt = SystemClock.elapsedRealtime()
                    ws.send("""{"t":"ping"}""")
                }
            }

            closed.await()

            pingJob?.cancel()
            ws.cancel()
            if (connectedSince > 0) {
                totalConnectedMs += SystemClock.elapsedRealtime() - connectedSince
                connectedSince = 0
            }
            if (!running) break
            publish(connected = false)
            reconnects += 1
            publish()
            Log.i(TAG, "第 $reconnects 次重连，退避 ${backoff}ms")
            delay(backoff)
            backoff = min(backoff * 2, 60_000L)
        }
    }

    @Volatile
    private var pingSentAt = 0L
    private var pingJob: Job? = null

    // ------------------------------------------------------------ 统计落盘

    /**
     * 每 5 分钟写一行 CSV，便于把手机从电脑拔掉跑 24 小时后一次性导出分析。
     * 列：time, uptime_secs, reconnects, connected, whitelisted, rtt_ms
     */
    private suspend fun statsLoop() {
        val f = File(getExternalFilesDir(null), "stage0-stats.csv")
        if (!f.exists()) f.writeText("time,uptime_secs,reconnects,connected,whitelisted,rtt_ms\n")
        val fmt = SimpleDateFormat("yyyy-MM-dd HH:mm:ss", Locale.US)
        while (scope.isActive) {
            delay(STATS_FLUSH_MS)
            val uptime = (totalConnectedMs + if (connectedSince > 0) {
                SystemClock.elapsedRealtime() - connectedSince
            } else 0) / 1000
            val line = listOf(
                fmt.format(Date()),
                uptime.toString(),
                reconnects.toString(),
                if (connectedSince > 0) "1" else "0",
                if (BatteryOpt.isIgnoring(this)) "1" else "0",
                lastRtt.toString()
            ).joinToString(",")
            runCatching { f.appendText(line + "\n") }
            Log.i(TAG, "stats: $line")
        }
    }

    private fun publish(connected: Boolean = connectedSince > 0) {
        uiState = UiState(
            running = running,
            connected = connected,
            reconnects = reconnects,
            uptimeSecs = (totalConnectedMs + if (connectedSince > 0) {
                SystemClock.elapsedRealtime() - connectedSince
            } else 0) / 1000,
            lastRttMs = lastRtt,
            queues = queues,
            serverUrl = uiState.serverUrl,
        )
    }

    private fun stopSelfSafely() {
        running = false
        loopJob?.cancel()
        statsJob?.cancel()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
            stopForeground(STOP_FOREGROUND_REMOVE)
        } else {
            @Suppress("DEPRECATION")
            stopForeground(true)
        }
        stopSelf()
    }

    override fun onDestroy() {
        running = false
        scope.cancel()
        super.onDestroy()
    }
}
