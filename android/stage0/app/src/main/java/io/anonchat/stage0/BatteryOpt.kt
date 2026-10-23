package io.anonchat.stage0

import android.annotation.SuppressLint
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.PowerManager
import android.provider.Settings

/**
 * 电池优化白名单。
 *
 * 为什么这是刚需（不是"优化建议"）：
 * Doze 模式下系统会**暂停所有应用的网络访问，且不为前台服务提供豁免**。
 * 官方文档里唯一被文档化的豁免途径就是这里申请的电池优化白名单。
 * 不给这个权限，前台服务活着但发不出包，用户息屏后消息延迟可以到几十分钟。
 *
 * 注意 Google Play 政策：REQUEST_IGNORE_BATTERY_OPTIMIZATIONS 属于敏感权限，
 * 只允许在"核心功能确实受影响"时申请。IM 属于合理用途，但要在商店 listing 里说明。
 */
object BatteryOpt {

    fun isIgnoring(context: Context): Boolean {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) return true
        val pm = context.getSystemService(Context.POWER_SERVICE) as PowerManager
        return pm.isIgnoringBatteryOptimizations(context.packageName)
    }

    /** 弹出系统级"忽略电池优化"对话框（需在 Activity 中调用） */
    @SuppressLint("BatteryLife")
    fun requestIgnore(context: Context) {
        if (isIgnoring(context)) return
        val intent = Intent(
            Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
            Uri.parse("package:${context.packageName}")
        )
        context.startActivity(intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
    }

    /** 打不开系统对话框时的兜底：跳到电池优化列表让用户手动找 */
    fun openSettings(context: Context) {
        val intent = Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS)
        context.startActivity(intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
    }

    /**
     * 国产 ROM 的二次限制：AOSP 白名单之外，各家还有自己的"自启动 / 后台运行"开关。
     * 这里列的是各家设置的跳转入口，跳不过去就退回到应用详情页。
     * 实测清单见 docs/阶段0-验证手册.md 的"逐机型操作表"。
     */
    fun openOemSettings(context: Context) {
        val candidates = listOf(
            // 小米 / Redmi
            Intent().setComponent(
                android.content.ComponentName(
                    "com.miui.securitycenter",
                    "com.miui.permcenter.autostart.AutoStartManagementActivity"
                )
            ),
            // 华为 / 荣耀
            Intent().setComponent(
                android.content.ComponentName(
                    "com.huawei.systemmanager",
                    "com.huawei.systemmanager.startupmgr.ui.StartupNormalAppListActivity"
                )
            ),
            // OPPO / 一加 / realme
            Intent().setComponent(
                android.content.ComponentName(
                    "com.coloros.safecenter",
                    "com.coloros.safecenter.permission.startup.StartupAppListActivity"
                )
            ),
            // vivo / iQOO
            Intent().setComponent(
                android.content.ComponentName(
                    "com.vivo.permissionmanager",
                    "com.vivo.permissionmanager.activity.BgStartUpManagerActivity"
                )
            ),
        )
        for (i in candidates) {
            i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            if (context.packageManager.resolveActivity(i, 0) != null) {
                runCatching { context.startActivity(i) }
                return
            }
        }
        // 都跳不过去就退到应用详情
        val detail = Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS)
            .setData(Uri.parse("package:${context.packageName}"))
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        context.startActivity(detail)
    }
}
