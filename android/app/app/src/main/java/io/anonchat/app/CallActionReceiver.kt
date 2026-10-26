package io.anonchat.app

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/**
 * 来电通知上的"接听 / 拒接"按钮。
 *
 * 为什么需要：CallStyle.forIncomingCall 要求两个 action PendingIntent，
 * 没有它们通知栏就只有一条横幅，用户必须点进 App 才能接——而"锁屏来电"
 * 场景下这正是最慢的路径。
 *
 * 直接调 CallManager：它是进程内单例，来电状态下 pc/状态机都已就绪；
 * 若进程已被回收（极端情况），接听会静默失败，来电本身也会因超时结束。
 *
 * 接听必须经过 MainActivity：麦克风/相机是运行时权限，第一次来电时很可能还没授。
 * 早先这里直接 CallManager.accept()，权限缺失就只打一行日志——用户点了接听
 * 什么都不会发生（二轮审计 P2-4 只做了检测、没做申请）。现在改为把主界面拉起来，
 * 由它补权限后再真正接听。
 */
class CallActionReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        Repo.init(context)
        CallManager.init(context)
        when (intent.action) {
            ACTION_ANSWER -> runCatching {
                if (CallManager.missingCallPermissions().isEmpty()) {
                    CallManager.accept()
                } else {
                    context.startActivity(
                        Intent(context, MainActivity::class.java).apply {
                            addFlags(
                                Intent.FLAG_ACTIVITY_NEW_TASK or
                                    Intent.FLAG_ACTIVITY_SINGLE_TOP
                            )
                            putExtra(MainActivity.EXTRA_ANSWER_CALL, true)
                        }
                    )
                }
            }
            ACTION_DECLINE -> runCatching { CallManager.reject() }
        }
    }

    companion object {
        const val ACTION_ANSWER = "io.anonchat.app.CALL_ANSWER"
        const val ACTION_DECLINE = "io.anonchat.app.CALL_DECLINE"
    }
}
