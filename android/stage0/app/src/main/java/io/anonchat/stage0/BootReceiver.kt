package io.anonchat.stage0

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import androidx.core.content.ContextCompat

/**
 * 开机自启。
 *
 * 注意 Android 15 对"从 BOOT_COMPLETED 启动前台服务"的限制：
 * 该限制针对 dataSync 等类型，remoteMessaging 不受影响——但这条要在真机上验证，
 * 因为国产 ROM 的自启动管理会另算一套规则。
 */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Intent.ACTION_BOOT_COMPLETED &&
            intent.action != Intent.ACTION_MY_PACKAGE_REPLACED
        ) return

        val i = Intent(context, MessagingService::class.java).apply {
            action = MessagingService.ACTION_START
        }
        runCatching { ContextCompat.startForegroundService(context, i) }
    }
}
