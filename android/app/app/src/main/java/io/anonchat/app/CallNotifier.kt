package io.anonchat.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Build

/**
 * 来电通知。
 *
 * 为什么必须有：App 在后台时（绝大多数时间）用户看不到任何界面，
 * 不弹通知就等于"电话静默漏接"。这也直接违反"不用推送，靠前台服务"的架构前提——
 * 前台服务保证**连接**活着，但**通知**才是把人叫过来的手段。
 *
 * 用 fullScreenIntent：锁屏/灭屏时直接把通话界面顶上来，这是系统给电话类应用的通道。
 *
 * **Android 14+ 的行为边界（实测踩过）**：屏幕亮着且已解锁时，FSI 一律降级为
 * heads-up 横幅，不真正全屏弹出——官方防广告钓鱼的策略（fsi-limits 文档），
 * 不是 bug。所以"亮屏在后台来电只见横幅"是系统预期；锁屏/灭屏才会全屏拉起
 * 接听界面。CallStyle（API 31+）让系统把它当真电话对待：FSI 资格判定更宽松、
 * 横幅优先级最高，是能拿到的最强呈现。
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

        val builder = Notification.Builder(ctx, ensureChannel(ctx))
            .setContentTitle(ctx.getString(R.string.notif_call_title, who, kind))
            .setContentText(ctx.getString(R.string.notif_call_answer))
            .setSmallIcon(android.R.drawable.sym_action_call)
            .setCategory(Notification.CATEGORY_CALL)
            .setPriority(Notification.PRIORITY_MAX)
            .setOngoing(true)
            .setAutoCancel(false)
            .setContentIntent(pi)
            .setFullScreenIntent(pi, true)

        // CallStyle 需要 Person + 通话态。振铃用 forIncomingCall（系统给出
        // "接听/拒接" 两个动作按钮）；早先振铃也用了 forOngoingCall，pill 只有
        // 挂断、通知上没有接听入口，用户只能点进 App（P3）。
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            val person = android.app.Person.Builder()
                .setName(who)
                .setImportant(true)
                .build()
            val decline = PendingIntent.getBroadcast(
                ctx, 2,
                Intent(ctx, CallActionReceiver::class.java).setAction(CallActionReceiver.ACTION_DECLINE),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
            )
            val answer = PendingIntent.getBroadcast(
                ctx, 3,
                Intent(ctx, CallActionReceiver::class.java).setAction(CallActionReceiver.ACTION_ANSWER),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
            )
            builder.setStyle(Notification.CallStyle.forIncomingCall(person, decline, answer))
        }

        runCatching { nm.notify(NOTI_ID, builder.build()) }
    }

    fun cancel(ctx: Context?) {
        ctx?.getSystemService(NotificationManager::class.java)?.cancel(NOTI_ID)
    }
}
