package io.anonchat.stage0

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat

/**
 * 阶段 0 测试台：只负责启停 Service、申请白名单、显示状态。
 * 真正的连接逻辑在 MessagingService。
 */
class MainActivity : ComponentActivity() {

    private val notiPermission = registerForActivityResult(
        ActivityResultContracts.RequestPermission()
    ) { }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        ensureNotificationPermission()

        setContent {
            MaterialTheme {
                Surface(modifier = Modifier.fillMaxSize()) {
                    TestScreen(
                        onStart = { url -> startService(url) },
                        onStop = { startService(null, stop = true) },
                        onWhitelist = { BatteryOpt.requestIgnore(this) },
                        onOem = { BatteryOpt.openOemSettings(this) },
                        onOpenBattery = { BatteryOpt.openSettings(this) },
                        whitelisted = BatteryOpt.isIgnoring(this),
                    )
                }
            }
        }
    }

    private fun ensureNotificationPermission() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        val granted = ContextCompat.checkSelfPermission(
            this, Manifest.permission.POST_NOTIFICATIONS
        ) == PackageManager.PERMISSION_GRANTED
        if (!granted) notiPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
    }

    private fun startService(url: String?, stop: Boolean = false) {
        val i = Intent(this, MessagingService::class.java)
        if (stop) {
            i.action = MessagingService.ACTION_STOP
            startService(i)
            return
        }
        if (url != null) {
            getSharedPreferences("stage0", MODE_PRIVATE).edit().putString("url", url).apply()
            i.putExtra("url", url)
        }
        i.action = MessagingService.ACTION_START
        ContextCompat.startForegroundService(this, i)
        Toast.makeText(this, "服务已启动", Toast.LENGTH_SHORT).show()
    }
}

@Composable
private fun TestScreen(
    onStart: (String) -> Unit,
    onStop: () -> Unit,
    onWhitelist: () -> Unit,
    onOem: () -> Unit,
    onOpenBattery: () -> Unit,
    whitelisted: Boolean,
) {
    var url by remember { mutableStateOf("ws://192.168.1.10:8080") }
    val st = MessagingService.uiState

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(20.dp),
        verticalArrangement = Arrangement.spacedBy(14.dp)
    ) {
        Text("阶段 0 · 后台存活验证", style = MaterialTheme.typography.titleLarge)
        Text(
            "目的：验证无推送时，长连接在锁屏静置 24 小时内存活多久。",
            style = MaterialTheme.typography.bodySmall
        )

        Card {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Text(
                    if (whitelisted) "电池白名单：已授予" else "电池白名单：未授予（息屏后大概率断线）",
                    fontWeight = FontWeight.Medium,
                    color = if (whitelisted) MaterialTheme.colorScheme.primary
                    else MaterialTheme.colorScheme.error
                )
                Text("这一项是前置条件，不是优化项。Doze 期间系统会暂停所有应用网络，前台服务也不例外。",
                    style = MaterialTheme.typography.bodySmall)
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Button(onClick = onWhitelist) { Text("申请白名单") }
                    OutlinedButton(onClick = onOpenBattery) { Text("电池优化列表") }
                }
                OutlinedButton(onClick = onOem, modifier = Modifier.fillMaxWidth()) {
                    Text("打开厂商自启动设置（小米/华为/OPPO/vivo）")
                }
            }
        }

        Card {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Text("连接状态", fontWeight = FontWeight.Medium)
                HorizontalDivider()
                StatRow("服务运行", if (st.running) "是" else "否")
                StatRow("WebSocket", if (st.connected) "已连接" else "断开")
                StatRow("累计在线", "${st.uptimeSecs} 秒")
                StatRow("重连次数", st.reconnects.toString())
                StatRow("最近 RTT", "${st.lastRttMs} ms")
                StatRow("已建队列", st.queues.toString())
            }
        }

        OutlinedTextField(
            value = url,
            onValueChange = { url = it },
            label = { Text("中继地址（relay serve 的地址）") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth()
        )

        Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
            Button(onClick = { onStart(url) }) { Text("启动") }
            OutlinedButton(onClick = onStop) { Text("停止") }
        }

        Spacer(Modifier.height(8.dp))
        Text(
            "统计会每 5 分钟写入：\nAndroid/data/io.anonchat.stage0/files/stage0-stats.csv",
            style = MaterialTheme.typography.bodySmall
        )
    }
}

@Composable
private fun StatRow(k: String, v: String) {
    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
        Text(k, style = MaterialTheme.typography.bodyMedium)
        Text(v, style = MaterialTheme.typography.bodyMedium, fontWeight = FontWeight.Medium)
    }
}
