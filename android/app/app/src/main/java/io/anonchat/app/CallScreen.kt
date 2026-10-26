package io.anonchat.app

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.draw.clip
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import org.webrtc.RendererCommon
import org.webrtc.SurfaceViewRenderer
import kotlinx.coroutines.delay

/**
 * 通话界面。
 *
 * 覆盖在主页之上，不做导航跳转——因为通话可能在任何界面（甚至 App 在后台被通知拉起）发生，
 * 走导航栈会在返回时出现"退回哪个页面"的问题。
 */
@Composable
fun CallScreen(onClose: () -> Unit) {
    val ui = CallManager.ui
    if (!ui.visible) return

    var elapsed by remember { mutableStateOf(0L) }

    // 通话计时。放在这里而不是只靠 CallManager，是为了避免 UI 重建时计时归零。
    // 进入 active 的第一秒用 CallManager 的值兜底：早先 elapsed 初值是 0，
    // ticker 首格未到前会先显示一下 00:00（P3）。
    LaunchedEffect(ui.state) {
        elapsed = if (ui.elapsedSecs > 0) ui.elapsedSecs else 0L
        while (ui.state == "active") {
            delay(1_000)
            elapsed += 1
        }
    }

    Box(
        Modifier
            .fillMaxSize()
            .background(Color(0xFF101418))
    ) {
        // 视频通话时铺满远端画面
        if (ui.video) {
            RemoteVideo()
            // 本地预览在 outgoing 阶段也显示：主叫拨出后看不到自己的画面，
            // 与主流 IM 体验不一致（P3）。此时媒体已 attach（attachLocalMedia
            // 在 startCall 里做），渲染器挂上就能出图。
            LocalVideo()
        }

        Column(
            Modifier
                .fillMaxSize()
                .padding(24.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.SpaceBetween
        ) {
            // 顶部：对方 + 状态
            Column(
                Modifier.padding(top = if (ui.video) 16.dp else 80.dp),
                horizontalAlignment = Alignment.CenterHorizontally
            ) {
                Text(
                    ui.peerName.ifBlank { stringResource(R.string.call_unknown_contact) },
                    color = Color.White,
                    fontSize = 26.sp,
                    fontWeight = FontWeight.SemiBold
                )
                Spacer(Modifier.height(10.dp))
                Text(
                    statusText(ui, elapsed),
                    color = Color(0xFFB7C1CC),
                    fontSize = 15.sp,
                    textAlign = TextAlign.Center
                )
                ui.err?.let { e ->
                    Spacer(Modifier.height(8.dp))
                    val ctx = androidx.compose.ui.platform.LocalContext.current
                    Text(
                        errText(ctx, e),
                        color = Color(0xFFFF8A80), fontSize = 13.sp, textAlign = TextAlign.Center
                    )
                }
                if (ui.state == "active" && ui.iceState.isNotEmpty() &&
                    ui.iceState != "connected" && ui.iceState != "completed"
                ) {
                    Spacer(Modifier.height(6.dp))
                    Text(
                        stringResource(R.string.call_ice_connecting, ui.iceState),
                        color = Color(0xFF8FA3B5),
                        fontSize = 12.sp
                    )
                }
            }

            // 底部：按钮
            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                when (ui.state) {
                    "incoming" -> Row(horizontalArrangement = Arrangement.spacedBy(32.dp)) {
                        RoundButton(stringResource(R.string.call_reject), Color(0xFFE53935)) { CallManager.reject(); onClose() }
                        RoundButton(stringResource(R.string.call_accept), Color(0xFF2E7D32)) { CallManager.accept() }
                    }

                    "outgoing" -> Row(horizontalArrangement = Arrangement.spacedBy(32.dp)) {
                        RoundButton(stringResource(R.string.call_cancel), Color(0xFFE53935)) { CallManager.hangup(); onClose() }
                    }

                    "active" -> Column(horizontalAlignment = Alignment.CenterHorizontally) {
                        Row(horizontalArrangement = Arrangement.spacedBy(14.dp)) {
                            RoundButton(
                                if (ui.muted) stringResource(R.string.call_muted) else stringResource(R.string.call_mute),
                                if (ui.muted) Color(0xFF455A64) else Color(0xFF263238)
                            ) { CallManager.toggleMute() }
                            RoundButton(
                                if (ui.speakerOn) stringResource(R.string.call_speaker_on) else stringResource(R.string.call_speaker_off),
                                if (ui.speakerOn) Color(0xFF37474F) else Color(0xFF263238)
                            ) { CallManager.toggleSpeaker() }
                            // 自动检测不一定每次都准（比如 NAT 映射过期但网络没变），
                            // 留个人工兜底：点了就带 IceRestart 重新协商。
                            RoundButton(
                                if (ui.renegotiating) stringResource(R.string.call_reconnecting) else stringResource(R.string.call_reconnect),
                                if (ui.renegotiating) Color(0xFF4A3F2A) else Color(0xFF263238)
                            ) { CallManager.forceReconnect() }
                            // 前后摄像头切换：只有视频通话有本地采集才显示。
                            // 原地 switchCamera 不动 SDP，对端无感。
                            if (ui.video) {
                                RoundButton(
                                    stringResource(R.string.call_switch_camera),
                                    Color(0xFF263238)
                                ) { CallManager.switchCamera() }
                            }
                        }
                        Spacer(Modifier.height(24.dp))
                        RoundButton(stringResource(R.string.call_hangup), Color(0xFFE53935)) { CallManager.hangup(); onClose() }
                    }

                    "ended" -> Column(horizontalAlignment = Alignment.CenterHorizontally) {
                        Text(
                            endText(ui.reason),
                            color = Color(0xFFB7C1CC),
                            fontSize = 15.sp
                        )
                        Spacer(Modifier.height(20.dp))
                        OutlinedButton(
                            onClick = { CallManager.dismiss(); onClose() },
                            colors = ButtonDefaults.outlinedButtonColors(contentColor = Color.White)
                        ) { Text(stringResource(R.string.call_close)) }
                    }
                }
                Spacer(Modifier.height(40.dp))
            }
        }
    }
}

@Composable
private fun statusText(ui: CallManager.UiState, elapsed: Long): String = when (ui.state) {
    "incoming" ->
        if (ui.video) stringResource(R.string.call_state_incoming_video)
        else stringResource(R.string.call_state_incoming_voice)
    "outgoing" ->
        if (ui.video) stringResource(R.string.call_state_outgoing_video)
        else stringResource(R.string.call_state_outgoing_voice)
    "active" -> when {
        ui.renegotiating -> stringResource(R.string.call_state_resuming)
        ui.iceState == "connected" || ui.iceState == "completed" -> formatDuration(elapsed)
        else -> stringResource(R.string.call_state_connected)
    }
    "ended" -> stringResource(R.string.call_state_ended)
    else -> ""
}

@Composable
private fun endText(reason: String): String = when (reason) {
    "declined" -> stringResource(R.string.call_end_declined)
    "busy" -> stringResource(R.string.call_end_busy)
    "no_answer" -> stringResource(R.string.call_end_no_answer)
    "missed" -> stringResource(R.string.call_end_missed)
    "local_ended" -> stringResource(R.string.call_end_local_ended)
    "peer_ended" -> stringResource(R.string.call_end_peer_ended)
    "" -> stringResource(R.string.call_state_ended)
    else -> stringResource(R.string.call_end_other, reason)
}

private fun formatDuration(secs: Long): String {
    val m = secs / 60
    val s = secs % 60
    return "%02d:%02d".format(m, s)
}

@Composable
private fun RoundButton(label: String, color: Color, onClick: () -> Unit) {
    Box(
        contentAlignment = Alignment.Center,
        modifier = Modifier
            .size(84.dp)
            .clip(CircleShape)
            .background(color)
            .clickable(onClick = onClick)
    ) {
        Text(label, color = Color.White, fontSize = 15.sp, fontWeight = FontWeight.Medium)
    }
}

@Composable
private fun RemoteVideo() {
    AndroidView(
        modifier = Modifier.fillMaxSize(),
        factory = { ctx ->
            SurfaceViewRenderer(ctx).apply {
                init(CallManager.eglContext, null)
                setScalingType(RendererCommon.ScalingType.SCALE_ASPECT_FILL)
                setEnableHardwareScaler(true)
                CallManager.bindRenderers(this, CallManager.localRenderer)
            }
        },
        onRelease = { r ->
            // 先摘 sink 再 release：SurfaceViewRenderer 持有 native EGL surface，
            // detach 不等于释放——早先从不调用 release，每通视频通话泄漏一个
            // （二轮审计 P2）。
            CallManager.bindRenderers(null, CallManager.localRenderer)
            runCatching { r.release() }
        }
    )
}

@Composable
private fun LocalVideo() {
    Box(
        Modifier
            .fillMaxSize()
            .padding(top = 150.dp, end = 16.dp),
        contentAlignment = Alignment.TopEnd
    ) {
        AndroidView(
            modifier = Modifier.size(96.dp, 140.dp),
            factory = { ctx ->
                SurfaceViewRenderer(ctx).apply {
                    init(CallManager.eglContext, null)
                    setScalingType(RendererCommon.ScalingType.SCALE_ASPECT_FILL)
                    setMirror(true)
                    CallManager.bindRenderers(CallManager.remoteRenderer, this)
                }
            },
            onRelease = { r ->
                CallManager.bindRenderers(CallManager.remoteRenderer, null)
                runCatching { r.release() }
            }
        )
    }
}
