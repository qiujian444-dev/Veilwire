package io.anonchat.app

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.sp

/**
 * 确定性头像：名字哈希 → 色相 + 首字，**零存储**。
 *
 * 为什么不做用户上传的头像：这个 App 里根本没有头像数据可传，
 * 也没有可以托管头像图片的中立服务。但会话列表一排灰底首字头像全一个色，
 * 两个人名字开头一样就完全分不清——给每个名字一个稳定的颜色，扫一眼就能对上号。
 *
 * 稳定性要求：同一个名字在任何一次启动、任何机型上都得到同一个颜色，
 * 否则用户会以为"头像变了 = 身份变了"。所以哈希自己实现（FNV-1a），
 * 不依赖 String.hashCode——它是稳定的，但把它当 API 依赖着心里不踏实。
 */
object AvatarLogic {
    /** FNV-1a（32 位）。逐 UTF-16 码元计算，中文英文行为一致 */
    fun hash(name: String): Long {
        var h = 2166136261L  // FNV offset basis
        for (ch in name) {
            h = (h xor ch.code.toLong()) * 0x01000193L and 0xFFFFFFFFL
        }
        return h
    }

    /** 名字 → 色相（0..360）。稳定的：同名永远同色 */
    fun hueFor(name: String): Float = (hash(name) % 360).toFloat()

    /**
     * 头像上显示的字：名字首字。空名字给 "#"，不至于画一个空圈。
     * "陌生人 xxx" 取"陌"是刻意的——它不是联系人，颜色和首字都该一眼区分开。
     */
    fun initial(name: String): String {
        val t = name.trim()
        if (t.isEmpty()) return "#"
        // 补充对代理对（生僻字 emoji 等）的基本处理：取第一个完整码点
        val cp = t.codePointAt(0)
        return String(Character.toChars(cp)).uppercase()
    }
}

/**
 * 头像圆。颜色由名字决定，明度/饱和度跟深浅色主题走。
 *
 * 取值依据：浅色下 L=0.60、S=0.52，白字能读、一排头像不刺眼；
 * 深色下降到 L=0.42 / S=0.45，否则黑底上一圈荧光色。
 */
@Composable
fun Avatar(name: String, size: Dp, fontSize: Dp = Dp.Unspecified) {
    val skin = LocalSkin.current
    val hue = AvatarLogic.hueFor(name)
    val bg = Color.hsl(
        hue = hue,
        saturation = if (skin.isDark) 0.45f else 0.52f,
        lightness = if (skin.isDark) 0.42f else 0.60f,
    )
    Box(
        Modifier
            .size(size)
            .background(bg, CircleShape),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            AvatarLogic.initial(name),
            color = Color.White,
            fontWeight = FontWeight.SemiBold,
            // 字号不指定时按圆径的比例来（0.42 是首字在圆里最舒服的比例）。
            // 不用 Dp.toSp()：那要拿 Density，这里直接用 dp 的数值换算成 sp 就够
            fontSize = if (fontSize != Dp.Unspecified) fontSize.value.sp else (size.value * 0.42f).sp,
            maxLines = 1,
        )
    }
}
