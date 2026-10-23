package io.anonchat.app

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import androidx.core.content.ContextCompat

/** 开机自启。remoteMessaging 类型不受 Android 15 针对 dataSync 的 BOOT_COMPLETED 限制。 */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Intent.ACTION_BOOT_COMPLETED &&
            intent.action != Intent.ACTION_MY_PACKAGE_REPLACED
        ) return
        Repo.init(context)
        if (!Repo.hasIdentity()) return
        val i = Intent(context, MessagingService::class.java).apply {
            action = MessagingService.ACTION_START
        }
        runCatching { ContextCompat.startForegroundService(context, i) }
    }
}
