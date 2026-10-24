package io.anonchat.app

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import androidx.core.content.ContextCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch

/** 开机自启。remoteMessaging 类型不受 Android 15 针对 dataSync 的 BOOT_COMPLETED 限制。 */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Intent.ACTION_BOOT_COMPLETED &&
            intent.action != Intent.ACTION_MY_PACKAGE_REPLACED
        ) return
        // Repo.init 要加载 Rust 设备状态（JNI 首次加载 + 文件 IO），百毫秒级。
        // 广播主线程只给 10 秒，而覆盖安装后 MY_PACKAGE_REPLACED 与首帧/其他广播
        // 挤在同一条主线程上，实测在这里 ANR 过（am_anr: Broadcast of Intent
        // {act=MY_PACKAGE_REPLACED}）。goAsync 换独立预算，重活扔 IO 线程。
        val pending = goAsync()
        CoroutineScope(Dispatchers.IO).launch {
            try {
                Repo.init(context)
                if (!Repo.hasIdentity()) return@launch
                val i = Intent(context, MessagingService::class.java).apply {
                    action = MessagingService.ACTION_START
                }
                runCatching { ContextCompat.startForegroundService(context, i) }
            } finally {
                pending.finish()
            }
        }
    }
}
