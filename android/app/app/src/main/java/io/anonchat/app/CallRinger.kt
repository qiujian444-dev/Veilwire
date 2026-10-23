package io.anonchat.app

import android.content.Context
import android.media.AudioAttributes
import android.media.MediaPlayer
import android.media.RingtoneManager
import android.os.Build
import android.os.VibrationEffect
import android.os.Vibrator
import android.os.VibratorManager

/**
 * 来电振铃：铃声循环 + 震动。
 *
 * 为什么不用 NotificationChannel.setSound：来电界面是 fullScreenIntent 直接顶上来的
 * Activity，通知本身（CallNotifier）只是后台入口。界面上放着"接听/挂断"却不响，
 * 观感等于漏接——所以振铃必须由 UI 状态机自己驱动（进 incoming 播、离开停），
 * 这也是微信的做法。
 *
 * MediaPlayer 而不是 Ringtone：Ringtone.play() 不带循环，来一遍就停；
 * MediaPlayer.isLooping 才能一直响到用户接听或对方超时。
 *
 * 为什么单独抽 object：MediaPlayer / Vibrator 在 JVM 单测里都不可用，
 * 收在这里让 CallManager 不用关心资源生命周期。
 */
object CallRinger {
    private const val TAG = "AnonChat"
    private var player: MediaPlayer? = null
    private var vibrator: Vibrator? = null

    /** 是否正在振铃（给状态机做兜底判断用） */
    val isRinging: Boolean get() = player != null || vibrator != null

    fun start(ctx: Context?) {
        if (ctx == null) {
            android.util.Log.w(TAG, "start: null context, cannot ring")
            return
        }
        stop() // 防御：连进来两个来电时别叠两个播放器
        try {
            val uri = RingtoneManager.getDefaultUri(RingtoneManager.TYPE_RINGTONE)
            android.util.Log.i(TAG, "start: uri=$uri")
            val p = MediaPlayer()
            p.setDataSource(ctx, uri)
            p.setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_NOTIFICATION_RINGTONE)
                    .setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
                    .build()
            )
            p.isLooping = true
            p.prepare()
            p.start()
            player = p
            android.util.Log.i(TAG, "start: ringer already running")
        } catch (t: Throwable) {
            // 之前这里静默吞掉异常，导致"铃声不响"无从排查（0.11.7 实测踩到）
            android.util.Log.w(TAG, "start: ring playback failed", t)
        }
        runCatching {
            val v: Vibrator = if (Build.VERSION.SDK_INT >= 31) {
                val vm = ctx.getSystemService(Context.VIBRATOR_MANAGER_SERVICE) as VibratorManager
                vm.defaultVibrator
            } else {
                @Suppress("DEPRECATION")
                ctx.getSystemService(Context.VIBRATOR_SERVICE) as Vibrator
            }
            // 600ms 震 + 400ms 停，无限重复
            v.vibrate(VibrationEffect.createWaveform(longArrayOf(600, 400), 0))
            vibrator = v
        }
    }

    fun stop() {
        // 空闲时直接返回：ticker 每秒都会调一次做兜底，没必要每次都走一遍 runCatching
        if (player == null && vibrator == null) return
        android.util.Log.i(TAG, "stop: ringing stopped")
        runCatching { player?.stop() }
        runCatching { player?.release() }
        player = null
        runCatching { vibrator?.cancel() }
        vibrator = null
    }
}
