package io.anonchat.app

import android.app.Activity
import android.graphics.Color
import android.graphics.drawable.GradientDrawable
import android.os.Bundle
import android.view.Gravity
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.TextView
import com.journeyapps.barcodescanner.CaptureActivity

/**
 * 扫码页：把 zxing 自带的 CaptureActivity 收窄成「只能竖屏 + 有明确的关闭按钮」。
 *
 * 为什么要自己继承一层（这两个都是库的默认行为，不是我们的代码写错）：
 * 1. 库的 AndroidManifest 把 CaptureActivity 声明成 `screenOrientation="sensorLandscape"`，
 *    所以一进扫码界面手机就横过来——而扫码这个动作本身跟屏幕方向无关，
 *    用户竖着拿手机、界面突然横了，取景框还得歪着对，体验很差；
 * 2. 库用的是 `Theme.Holo.NoActionBar.Fullscreen`，全屏且没有标题栏，
 *    界面上**一个可点的退出控件都没有**。手势导航的手机没有实体返回键，
 *    不少人就卡在取景界面不知道怎么出去。
 *
 * 处理方式是：在清单里给本类锁 `portrait`，再往内容视图上叠一个左上角 ✕。
 * 相机预览、对焦、闪光灯、权限申请这些全部沿用库的实现——重写相机预览风险远大于收益。
 * 另外调用方要传 `setOrientationLocked(false)`：库会按"当前物理旋转"去
 * setRequestedOrientation，横着拿手机进来时会把我们刚锁好的竖屏又改回横向。
 */
class QrScanActivity : CaptureActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        addCloseButton()
    }

    /**
     * 左上角 ✕。库的主题是全屏，所以这里不依赖任何标题栏，直接把按钮叠在取景层上面。
     * 用文字字形而不是图片资源：省掉一套多倍图，也不会因为换皮肤失效。
     */
    private fun addCloseButton() {
        val root = findViewById<ViewGroup>(android.R.id.content) ?: return
        val density = resources.displayMetrics.density
        fun dp(v: Int) = (v * density).toInt()

        val button = TextView(this).apply {
            text = "✕"
            textSize = 22f
            setTextColor(Color.WHITE)
            gravity = Gravity.CENTER
            contentDescription = getString(R.string.qr_close)
            isClickable = true
            isFocusable = true
            background = GradientDrawable().apply {
                shape = GradientDrawable.OVAL
                // 半透明黑：取景画面无论明暗都能看清这个按钮
                setColor(0x80000000.toInt())
            }
            setOnClickListener {
                // 取消 = 结果里没有内容，调用方（添加联系人对话框）原样停着，不报错
                setResult(Activity.RESULT_CANCELED)
                finish()
            }
        }

        val lp = FrameLayout.LayoutParams(dp(44), dp(44)).apply {
            gravity = Gravity.TOP or Gravity.START
            leftMargin = dp(16)
            topMargin = dp(16)
        }
        root.addView(button, lp)

        // 刘海屏/挖孔屏上别被额头挡住：主题是全屏，systemBars 通常是 0，
        // 真正要躲的是 displayCutout
        androidx.core.view.ViewCompat.setOnApplyWindowInsetsListener(root) { _, insets ->
            val top = insets.getInsets(
                androidx.core.view.WindowInsetsCompat.Type.displayCutout() or
                    androidx.core.view.WindowInsetsCompat.Type.systemBars()
            ).top
            lp.topMargin = dp(16) + top
            button.layoutParams = lp
            insets
        }
    }
}
