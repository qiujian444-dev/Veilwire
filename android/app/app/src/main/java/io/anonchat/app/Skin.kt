package io.anonchat.app

import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp

/**
 * 聊天页皮肤：一套"手机端 IM"的观感，而不是 Material 默认调色板。
 *
 * 为什么单独抽出来：之前 onCreate 直接用 `lightColorScheme()`，我方气泡取的是
 * primaryContainer —— M3 默认是浅紫 #EADDFF。聊天页里每个自己发的消息都是这个紫，
 * 密度极高，观感离用户每天用的微信/QQ 太远，而且改配色时要满文件找颜色。
 *
 * 这里只放颜色和尺寸常量，不放 Composable：皮肤要能被界面层直接读，
 * 而不是每处都靠 MaterialTheme.colorScheme.xxx 猜语义。
 */
data class SkinColors(
    /** 页面底色（消息区、聊天列表） */
    val bg: Color,
    /** 顶栏 / 输入栏底色。和 bg 同色是刻意的：中间只靠一条 0.5dp 细线分隔 */
    val bar: Color,
    val divider: Color,
    /** 对方气泡 */
    val incoming: Color,
    /** 我方气泡 */
    val outgoing: Color,
    val bubbleText: Color,
    /** 时间戳、副标题等弱化文字 */
    val subText: Color,
    val icon: Color,
    /** 强调色：发送按钮、已核对标记、进度条 */
    val accent: Color,
    val onAccent: Color,
    /** 输入框底色 */
    val field: Color,
    val placeholder: Color,
    /** 附件面板里的图标方块 */
    val chip: Color,
    /** 底部面板、弹窗的容器色 */
    val sheet: Color,
    val isDark: Boolean,
)

/**
 * 我方气泡绿 #7FD44F。
 *
 * 刻意不是微信的 #95EC69：同色系换一档，肉眼九成像，但不把别人的品牌色
 * 搬进一个同类 IM（外观设计/不正当竞争角度都更干净）。
 * 强调色比气泡深一档 —— 气泡上的黑字要能读，按钮上的白字也要能读，
 * 两者对明度的要求是相反的，只能拆成两个值。
 */
val SkinLight = SkinColors(
    bg = Color(0xFFEDEDED),
    bar = Color(0xFFEDEDED),
    divider = Color(0xFFDCDCDC),
    incoming = Color(0xFFFFFFFF),
    outgoing = Color(0xFF7FD44F),
    bubbleText = Color(0xFF191919),
    subText = Color(0xFFB2B2B2),
    icon = Color(0xFF191919),
    accent = Color(0xFF4FA845),
    onAccent = Color(0xFFFFFFFF),
    field = Color(0xFFFFFFFF),
    placeholder = Color(0xFFB2B2B2),
    chip = Color(0xFFF2F2F2),
    sheet = Color(0xFFF7F7F7),
    isDark = false,
)

/**
 * 深色：不照抄微信深色（#111 + #3EB575），但沿用同一套明暗关系，
 * 保证绿气泡上的浅色字仍然能读。
 */
val SkinDark = SkinColors(
    bg = Color(0xFF111111),
    bar = Color(0xFF1A1A1A),
    divider = Color(0xFF2A2A2A),
    incoming = Color(0xFF2C2C2C),
    outgoing = Color(0xFF3E9B39),
    bubbleText = Color(0xFFE8E8E8),
    subText = Color(0xFF8A8A8A),
    icon = Color(0xFFD8D8D8),
    accent = Color(0xFF4CAF50),
    onAccent = Color(0xFF0B1A08),
    field = Color(0xFF2C2C2C),
    placeholder = Color(0xFF7A7A7A),
    chip = Color(0xFF262626),
    sheet = Color(0xFF1F1F1F),
    isDark = true,
)

/** 默认给浅色，Provider 没包住的地方也不会崩 */
val LocalSkin = staticCompositionLocalOf { SkinLight }

/**
 * 尺寸。集中在对象里而不是散在各处：气泡圆角从 12dp 收到 6dp 是"像不像 IM"的
 * 关键一笔，写死在各分支里就没法一次改全（图片/文件/语音/文字四个分支都要一致）。
 */
object SkinDims {
    /** 文字/文件/语音气泡：微信是 6dp 量级，M3 默认 12dp 显得"胖" */
    val bubble = 6.dp
    /** 图片/视频气泡：裸图配 4dp，再大会露出灰边 */
    val image = 4.dp
    val barHeight = 48.dp
    val bubblePadH = 10.dp
    val bubblePadV = 8.dp
}

/**
 * 用自定义皮肤构造 Material3 的 colorScheme，让对话框、进度条、菜单一起跟着变。
 *
 * 这里把 surfaceContainer* 和 secondaryContainer 也一并接管：不接管的话，
 * Card / NavigationBar / 提示横幅仍然是 M3 默认的浅紫（它们的默认色不是 surface，
 * 而是 surfaceContainerLow / secondaryContainer），整个 App 会变成"灰底 + 紫色卡片"，
 * 比改之前更难看。
 */
fun skinColorScheme(skin: SkinColors) = if (skin.isDark) {
    darkColorScheme(
        primary = skin.accent,
        onPrimary = skin.onAccent,
        primaryContainer = skin.outgoing,
        onPrimaryContainer = skin.bubbleText,
        secondaryContainer = Color(0xFF23331F),
        onSecondaryContainer = Color(0xFFB7E0AA),
        background = skin.bg,
        onBackground = skin.bubbleText,
        surface = skin.bg,
        onSurface = skin.bubbleText,
        surfaceVariant = skin.incoming,
        onSurfaceVariant = skin.subText,
        surfaceContainerLowest = Color(0xFF0D0D0D),
        surfaceContainerLow = Color(0xFF1A1A1A),
        surfaceContainer = skin.sheet,
        surfaceContainerHigh = Color(0xFF242424),
        surfaceContainerHighest = skin.incoming,
        outline = skin.divider,
        outlineVariant = skin.divider,
    )
} else {
    lightColorScheme(
        primary = skin.accent,
        onPrimary = skin.onAccent,
        primaryContainer = skin.outgoing,
        onPrimaryContainer = skin.bubbleText,
        secondaryContainer = Color(0xFFE3F0DA),
        onSecondaryContainer = Color(0xFF2E5B22),
        background = skin.bg,
        onBackground = skin.bubbleText,
        surface = skin.bg,
        onSurface = skin.bubbleText,
        surfaceVariant = skin.incoming,
        onSurfaceVariant = skin.subText,
        surfaceContainerLowest = Color(0xFFFFFFFF),
        surfaceContainerLow = Color(0xFFFFFFFF),
        surfaceContainer = skin.sheet,
        surfaceContainerHigh = Color(0xFFF2F2F2),
        surfaceContainerHighest = Color(0xFFEBEBEB),
        outline = skin.divider,
        outlineVariant = skin.divider,
    )
}
