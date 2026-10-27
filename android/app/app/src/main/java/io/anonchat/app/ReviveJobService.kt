package io.anonchat.app

import android.app.job.JobInfo
import android.app.job.JobParameters
import android.app.job.JobScheduler
import android.app.job.JobService
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.util.Log
import androidx.core.content.ContextCompat

/**
 * 第二条复活通道：`JobScheduler`。
 *
 * 为什么还要一条：`KeepaliveReceiver` 靠的是 `setExactAndAllowWhileIdle` 闹钟，
 * 但部分国产 ROM（实测线索指向鸿蒙的"应用启动管理"）会同时拦掉两件事——
 * 闹钟投递、以及闹钟回调里的后台启动。闹钟被拦时"逃生舱"就完全不工作，
 * 进程死了就再也起不来，通话信令只能等用户手动打开 App。
 *
 * JobScheduler 是另一套调度机制：走 JobScheduler 服务而不是 AlarmManager，
 * 部分 ROM 拦闹钟不拦 job（反之亦然）。两条腿都断才真的没救。
 *
 * 注意它**不能**保证及时性：job 会被系统攒到维护窗口里批量跑，且 Doze 期间
 * 不执行。这里要的也不是"9 分钟必达"，而是"哪怕闹钟全被拦了，也总有一次机会"。
 * 及时性仍然由前台服务和精确闹钟负责。
 */
class ReviveJobService : JobService() {
    override fun onStartJob(params: JobParameters?): Boolean {
        // 用户在设置里主动停过：尊重意图，不拉起（与 KeepaliveReceiver 同一开关）
        val sp = getSharedPreferences("anon", Context.MODE_PRIVATE)
        if (sp.getBoolean(KeepaliveReceiver.PREF_USER_STOPPED, false)) return false
        if (MessagingService.isRunning) return false
        if (!Repo.hasIdentity()) return false

        Log.i(TAG, "job tick: process was dead, restarting service")
        runCatching {
            ContextCompat.startForegroundService(
                this,
                Intent(this, MessagingService::class.java).apply {
                    action = MessagingService.ACTION_START
                }
            )
        }.onFailure {
            // Android 12+ 后台启动 FGS 受限时可能抛 ForegroundServiceStartNotAllowed。
            // 不吞掉异常会让 job 被系统标记为失败并退避，把这条路也堵死。
            Log.w(TAG, "job restart failed: ${it.javaClass.simpleName}: ${it.message}")
        }
        // 一次性工作：periodic 的下一次由系统自行重排
        return false
    }

    override fun onStopJob(params: JobParameters?): Boolean = true

    companion object {
        private const val TAG = "AnonChat"
        private const val JOB_ID = 2001

        /**
         * 排定（或续期）周期任务。幂等：同 ID 重复 schedule 会覆盖旧定义，
         * 所以服务每次启动、每次闹钟 tick 都可以放心再调一次。
         */
        fun schedule(ctx: Context) {
            val js = ctx.getSystemService(Context.JOB_SCHEDULER_SERVICE) as? JobScheduler ?: return
            val info = JobInfo.Builder(
                JOB_ID,
                ComponentName(ctx, ReviveJobService::class.java)
            )
                // 15 分钟是 Android 允许的最小周期；比闹钟的 9 分钟粗，
                // 但这条路本来就是"闹钟失效时的兜底"
                .setPeriodic(INTERVAL_MS)
                // 重启后仍然有效（需要 RECEIVE_BOOT_COMPLETED，清单里已有）
                .setPersisted(true)
                .build()
            runCatching { js.schedule(info) }
                .onFailure { Log.w(TAG, "schedule revive job failed: ${it.message}") }
        }

        /** 用户主动停服务时取消，别让它在后台白醒 */
        fun cancel(ctx: Context) {
            val js = ctx.getSystemService(Context.JOB_SCHEDULER_SERVICE) as? JobScheduler ?: return
            runCatching { js.cancel(JOB_ID) }
        }

        private const val INTERVAL_MS = 15 * 60 * 1000L
    }
}
