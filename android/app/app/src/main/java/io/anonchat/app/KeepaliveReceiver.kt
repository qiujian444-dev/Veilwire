package io.anonchat.app

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build
import android.util.Log
import androidx.core.content.ContextCompat

/**
 * Doze 深度休眠的逃生舱。
 *
 * 背景（真机实测）：灭屏进 Doze 后，系统暂停**所有应用**的网络访问——
 * 前台服务不豁免这条，电池白名单只把限制从"暂停网络"放宽到"维护窗口才联网"。
 * 白名单没授的手机（用户点掉弹窗、国产 ROM 静默收回，都常见），消息延迟没有上界。
 *
 * setExactAndAllowWhileIdle 是唯一不依赖白名单、Doze 下也按时触发的机制
 * （官方文档保证每个 app 在 idle 下至少每 9 分钟一个窗口）。9 分钟就是
 * 没白名单时的消息延迟上界；有白名单时窗口更频繁，闹钟只是兜底。
 *
 * 触发后做两件事：把前台服务拉起来（进程被杀的场景）；服务活着则拉起后
 * listenLoop 的下一轮 recv 会自然重建连接并补齐积压（sub 补投语义保证不丢）。
 */
class KeepaliveReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != ACTION_TICK) return
        // 进程活着、服务在跑：什么都不用做，下一轮 recv（≤30s）自己恢复。
        // 这里 launch 服务调用的意义只在"进程已被杀"的场合。
        if (MessagingService.isRunning) return
        if (!Repo.hasIdentity()) return
        Log.i("AnonChat", "keepalive tick: process was dead, restarting service")
        val i = Intent(context, MessagingService::class.java).apply {
            action = MessagingService.ACTION_START
        }
        runCatching {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                ContextCompat.startForegroundService(context, i)
            } else {
                context.startService(i)
            }
        }
    }

    companion object {
        const val ACTION_TICK = "io.anonchat.app.KEEPALIVE_TICK"

        /**
         * 安排下一个闹钟。服务每次 onStartCommand 调一次；每轮 tick 若服务
         * 意外死亡，下次 tick 再拉起——闹钟自己必须无限续期。
         * setExactAndAllowWhileIdle 在 Doze 下每包络期被系统限频到一次/9分钟，
         * 我们要的间隔恰好就是 9 分钟，这个限制不构成额外延迟。
         */
        fun schedule(ctx: Context) {
            val am = ctx.getSystemService(Context.ALARM_SERVICE) as android.app.AlarmManager
            val pi = android.app.PendingIntent.getBroadcast(
                ctx, 1001,
                Intent(ctx, KeepaliveReceiver::class.java).setAction(ACTION_TICK),
                android.app.PendingIntent.FLAG_IMMUTABLE or android.app.PendingIntent.FLAG_UPDATE_CURRENT
            )
            val next = System.currentTimeMillis() + INTERVAL_MS
            // 闹钟权限被特殊 ROM 收走时退到可设置的 inexact 版本，聊胜于无
            runCatching {
                am.setExactAndAllowWhileIdle(android.app.AlarmManager.RTC_WAKEUP, next, pi)
            }.onFailure {
                am.setAndAllowWhileIdle(android.app.AlarmManager.RTC_WAKEUP, next, pi)
            }
        }

        private const val INTERVAL_MS = 9 * 60 * 1000L
    }
}
