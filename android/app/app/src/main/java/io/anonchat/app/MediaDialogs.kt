package io.anonchat.app

import android.graphics.Bitmap
import android.net.Uri
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.gestures.rememberTransformableState
import androidx.compose.foundation.gestures.transformable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import androidx.media3.common.MediaItem
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.ui.PlayerView
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.File

/**
 * 按路径异步取缩略图。[video] = true 时抽首帧。
 *
 * 必须在后台线程做：解码一张 1600px 的图和抽视频帧都是几十毫秒起步，
 * 放在组合里同步做，列表一滑就是一串掉帧。
 */
@Composable
fun rememberMediaBitmap(path: String?, maxPx: Int, video: Boolean): Bitmap? {
    var bmp by remember(path, maxPx, video) { mutableStateOf<Bitmap?>(null) }
    LaunchedEffect(path, maxPx, video) {
        bmp = if (path == null) null
        else withContext(Dispatchers.IO) {
            if (video) Thumbs.video(path, maxPx) else Thumbs.image(path, maxPx)
        }
    }
    return bmp
}

/**
 * 全屏看图。
 *
 * 自己做而不是丢给系统相册：这是"App 内查看"的关键——一交出去，用户就离开了聊天，
 * 回来还得重新找会话。这里支持双指缩放、双击放大/还原，关闭后原位回到聊天。
 */
@Composable
fun ImageViewerDialog(path: String, onClose: () -> Unit) {
    Dialog(
        onDismissRequest = onClose,
        properties = DialogProperties(usePlatformDefaultWidth = false),
    ) {
        // 按屏幕宽解码：比屏幕还大的图白占内存，比屏幕小的又不清
        val cfg = LocalConfiguration.current
        val maxPx = with(LocalDensity.current) { cfg.screenWidthDp.dp.toPx().toInt() }
        val bmp = rememberMediaBitmap(path, maxPx, video = false)

        var scale by remember { mutableFloatStateOf(1f) }
        var offset by remember { mutableStateOf(Offset.Zero) }
        var box by remember { mutableStateOf(IntSize.Zero) }
        val state = rememberTransformableState { zoom, pan, _ ->
            val ns = (scale * zoom).coerceIn(1f, 5f)
            if (ns <= 1.01f) {
                offset = Offset.Zero
            } else {
                // 放大后允许拖动，但拖不出边界：拖到黑屏外面用户会以为图没了
                val maxX = box.width * (ns - 1f) / 2f
                val maxY = box.height * (ns - 1f) / 2f
                offset = Offset(
                    (offset.x + pan.x).coerceIn(-maxX, maxX),
                    (offset.y + pan.y).coerceIn(-maxY, maxY),
                )
            }
            scale = ns
        }

        Box(
            Modifier.fillMaxSize().background(Color.Black).onSizeChanged { box = it },
            contentAlignment = Alignment.Center,
        ) {
            when {
                bmp == null -> CircularProgressIndicator(color = Color.White)
                else -> Image(
                    bitmap = bmp.asImageBitmap(),
                    contentDescription = null,
                    contentScale = ContentScale.Fit,
                    modifier = Modifier
                        .fillMaxSize()
                        .graphicsLayer {
                            scaleX = scale
                            scaleY = scale
                            translationX = offset.x
                            translationY = offset.y
                        }
                        .pointerInput(Unit) {
                            detectTapGestures(
                                onDoubleTap = {
                                    if (scale > 1.01f) {
                                        scale = 1f; offset = Offset.Zero
                                    } else scale = 2.5f
                                },
                                onTap = { if (scale <= 1.01f) onClose() },
                            )
                        }
                        .transformable(state),
                )
            }
            TextButton(
                onClick = onClose,
                modifier = Modifier
                    .align(Alignment.TopEnd)
                    .statusBarsPadding()
                    .padding(10.dp)
                    // 黑色半透明底：图片本身可能是浅色的（白底截图、文档照片），
                    // 白字直接压上去会看不见——模拟器上就撞到过
                    .background(Color.Black.copy(alpha = 0.45f), RoundedCornerShape(20.dp)),
            ) { Text(stringResource(R.string.action_close), color = Color.White) }

            // 手势提示放在顶部而不是底部：底部要跟系统导航栏/手势条抢位置
            // （实测在 0.6.3 的模拟器上被压在导航栏下面，看不见）
            Text(
                stringResource(R.string.media_gesture_hint),
                color = Color.White.copy(alpha = 0.9f),
                fontSize = 12.sp,
                modifier = Modifier
                    .align(Alignment.TopStart)
                    .statusBarsPadding()
                    .padding(10.dp)
                    .background(Color.Black.copy(alpha = 0.45f), RoundedCornerShape(20.dp))
                    .padding(horizontal = 10.dp, vertical = 4.dp),
            )
        }
    }
}

/**
 * App 内视频播放。
 *
 * 用 Media3 的 ExoPlayer + PlayerView：进度条、暂停、拖动全都有现成的，
 * 比自己拿 VideoView + MediaController 拼出来的可靠（MediaController 在
 * 全屏 Dialog 里定位经常错位）。
 *
 * 生命周期：Dialog 关闭时 release，否则每看一次视频就漏一个解码器，
 * 连续看十几个之后新视频会直接黑屏。
 */
@Composable
fun VideoPlayerDialog(path: String, title: String, onClose: () -> Unit) {
    Dialog(
        onDismissRequest = onClose,
        properties = DialogProperties(usePlatformDefaultWidth = false),
    ) {
        val ctx = LocalContext.current
        val exo = remember(path) { ExoPlayer.Builder(ctx).build() }
        // 播放器的进度条不能被系统导航栏压住。
        // 这里不用 navigationBarsPadding()：Dialog 窗口在 targetSdk 35 上边到边显示，
        // 但窗口本身不向 Compose 报告导航栏 inset（实测恒为 0，进度条正好压在三个导航键上）。
        // 退一步读系统资源里那条高度，再兜一个 48dp 的下限——三键导航正好是 48dp，
        // 手势导航下多出来的部分落在黑边上，看不出来。
        val navPx = remember(ctx) {
            val id = ctx.resources.getIdentifier("navigation_bar_height", "dimen", "android")
            if (id > 0) runCatching { ctx.resources.getDimensionPixelSize(id) }.getOrDefault(0) else 0
        }
        val navDp = with(LocalDensity.current) { navPx.toDp() }.coerceAtLeast(64.dp)

        DisposableEffect(exo) {
            onDispose { runCatching { exo.release() } }
        }
        LaunchedEffect(exo, path) {
            runCatching {
                exo.setMediaItem(MediaItem.fromUri(Uri.fromFile(File(path))))
                exo.prepare()
                exo.playWhenReady = true
            }
        }

        Box(Modifier.fillMaxSize().background(Color.Black)) {
            AndroidView(
                factory = { c ->
                    PlayerView(c).apply {
                        player = exo
                        useController = true
                        setShowNextButton(false)
                        setShowPreviousButton(false)
                        setShowBuffering(PlayerView.SHOW_BUFFERING_WHEN_PLAYING)
                        keepScreenOn = true
                    }
                },
                // 让出导航栏：不留的话进度条会被系统手势条压住，拖都拖不动
                modifier = Modifier.fillMaxSize().padding(bottom = navDp),
            )
            TextButton(
                onClick = onClose,
                modifier = Modifier
                    .align(Alignment.TopEnd)
                    .statusBarsPadding()
                    .padding(10.dp)
                    .background(Color.Black.copy(alpha = 0.45f), RoundedCornerShape(20.dp)),
            ) { Text(stringResource(R.string.action_close), color = Color.White) }
            Text(
                title,
                color = Color.White.copy(alpha = 0.8f),
                fontSize = 12.sp,
                modifier = Modifier
                    .align(Alignment.TopStart)
                    .statusBarsPadding()
                    .padding(start = 16.dp, top = 18.dp),
            )
        }
    }
}
