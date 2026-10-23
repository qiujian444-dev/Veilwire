package io.anonchat.app

import android.annotation.SuppressLint
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.PowerManager
import android.provider.Settings

/**
 * 电池优化白名单。
 *
 * 这不是"优化建议"，是**前置条件**：Doze 期间系统会暂停所有应用的网络访问，
 * 前台服务也不例外。官方文档里唯一被文档化的豁免途径就是这里申请的电池优化白名单。
 * 不给这个权限，服务活着但发不出包，息屏后消息延迟可以到几十分钟。
 */
object BatteryOpt {

    fun isIgnoring(context: Context): Boolean {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) return true
        val pm = context.getSystemService(Context.POWER_SERVICE) as PowerManager
        return pm.isIgnoringBatteryOptimizations(context.packageName)
    }

    @SuppressLint("BatteryLife")
    fun requestIgnore(context: Context) {
        if (isIgnoring(context)) return
        val i = Intent(
            Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
            Uri.parse("package:${context.packageName}")
        )
        context.startActivity(i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
    }

    /** 国产 ROM 的自启动/后台运行设置入口，跳不过去就退到应用详情页 */
    fun openOemSettings(context: Context) {
        val candidates = listOf(
            ComponentName("com.miui.securitycenter", "com.miui.permcenter.autostart.AutoStartManagementActivity"),
            ComponentName("com.huawei.systemmanager", "com.huawei.systemmanager.startupmgr.ui.StartupNormalAppListActivity"),
            ComponentName("com.coloros.safecenter", "com.coloros.safecenter.permission.startup.StartupAppListActivity"),
            ComponentName("com.vivo.permissionmanager", "com.vivo.permissionmanager.activity.BgStartUpManagerActivity"),
        )
        for (c in candidates) {
            val i = Intent().setComponent(c).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            if (context.packageManager.resolveActivity(i, 0) != null) {
                runCatching { context.startActivity(i) }
                return
            }
        }
        context.startActivity(
            Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS)
                .setData(Uri.parse("package:${context.packageName}"))
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        )
    }
}
