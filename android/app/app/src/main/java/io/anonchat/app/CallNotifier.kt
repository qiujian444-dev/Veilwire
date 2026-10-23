package io.anonchat.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent

/**
 * 来电通知。
 *
 * 为什么必须有：App 在后台时（绝大多数时间）用户看不到任何界面，
 * 不弹通知就等于"电话静默漏接"。这也直接违反"不用推送，靠前台服务"的架构前提——
 * 前台服务保证**连接**活着，但**通知**才是把人叫过来的手段。
 *
 * 用 fullScreenIntent：锁屏/后台时直接把通话界面顶上来，这是系统给电话类应用的通道。
 * Android 14 起该权限默认不授予公开应用商店的分发，但我们走 APK 直装，不受影响。
 */
object CallNotifier {
    private const val CHANNEL = "call"
    private const val NOTI_ID = 3001

    private fun ensureChannel(ctx: Context): String {
        val nm = ctx.getSystemService(NotificationManager::class.java)
        if (nm.getNotificationChannel(CHANNEL) == null) {
            nm.createNotificationChannel(
                NotificationChannel(
                    CHANNEL,
                    ctx.getString(R.string.notif_call_channel),
                    NotificationManager.IMPORTANCE_HIGH
                ).apply {
                    description = ctx.getString(R.string.notif_call_channel_desc)
                    setShowBadge(true)
                    enableVibration(true)
                    lockscreenVisibility = Notification.VISIBILITY_PUBLIC
                }
            )
        }
        return CHANNEL
    }

    fun showIncoming(ctx: Context?) {
        if (ctx == null) return
        val nm = ctx.getSystemService(NotificationManager::class.java)

        val open = Intent(ctx, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP
            putExtra(MainActivity.EXTRA_SHOW_CALL, true)
        }
        val pi = PendingIntent.getActivity(
            ctx, 1, open, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )

        val who = CallManager.ui.peerName.ifBlank { ctx.getString(R.string.notif_call_someone) }
        val kind = if (CallManager.ui.video) ctx.getString(R.string.notif_call_kind_video)
        else ctx.getString(R.string.notif_call_kind_voice)

        val n = Notification.Builder(ctx, ensureChannel(ctx))
            .setContentTitle(ctx.getString(R.string.notif_call_title, who, kind))
            .setContentText(ctx.getString(R.string.notif_call_answer))
            .setSmallIcon(android.R.drawable.sym_action_call)
            .setCategory(Notification.CATEGORY_CALL)
            .setPriority(Notification.PRIORITY_MAX)
            .setOngoing(true)
            .setAutoCancel(false)
            .setContentIntent(pi)
            .setFullScreenIntent(pi, true)
            .build()

        runCatching { nm.notify(NOTI_ID, n) }
    }

    fun cancel(ctx: Context?) {
        ctx?.getSystemService(NotificationManager::class.java)?.cancel(NOTI_ID)
    }
}
