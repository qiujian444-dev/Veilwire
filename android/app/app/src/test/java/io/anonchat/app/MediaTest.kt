package io.anonchat.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * 图片/视频消息的纯逻辑用例。
 *
 * 这些判断一旦错了，用户看到的是"发了张竖图变成横的""气泡占满整屏""视频时长显示 0:00"，
 * 而它们全都不依赖 Android 框架，没有理由不测。
 */
class MediaTest {

    @Test
    fun extension_decides_kind() {
        assertEquals("image", Media.kindOfName("IMG_2031.JPG"))
        assertEquals("image", Media.kindOfName("photo.HEIC"))
        assertEquals("video", Media.kindOfName("VID_2026.mp4"))
        assertEquals("video", Media.kindOfName("clip.mov"))
        assertEquals("file", Media.kindOfName("report.pdf"))
        assertEquals("file", Media.kindOfName("noext"))
    }

    @Test
    fun compressed_name_is_jpg() {
        assertEquals("IMG_2031.jpg", Media.compressedName("IMG_2031.png"))
        assertEquals("photo.jpg", Media.compressedName("photo.HEIC"))
        assertEquals("loose.jpg", Media.compressedName("loose"))
    }

    @Test
    fun fit_within_never_upscales() {
        assertEquals(800 to 600, Media.fitWithin(800, 600, 1600))
        // 横图按长边缩
        assertEquals(1600 to 1200, Media.fitWithin(4032, 3024, 1600))
        // 竖图也按长边缩，短边跟着等比
        assertEquals(1200 to 1600, Media.fitWithin(3024, 4032, 1600))
        // 极端长图不会出现 0 像素
        val (w, h) = Media.fitWithin(100, 20000, 1600)
        assertEquals(1600, h)
        assertTrue(w >= 1)
    }

    @Test
    fun sample_size_keeps_enough_pixels() {
        // 4032 长边、目标 1600：2 倍采样后 2016，仍然 >= 1600，可以接受
        assertEquals(2, Media.inSampleSize(4032, 3024, 1600))
        // 刚好等于目标：不采样
        assertEquals(1, Media.inSampleSize(1600, 1200, 1600))
        // 超大的 8K 图要采到 4 倍以上，否则内存扛不住
        assertTrue(Media.inSampleSize(8000, 6000, 1600) >= 4)
    }

    @Test
    fun bubble_keeps_aspect_and_fits_box() {
        val maxW = 240f
        val maxH = 320f
        // 横图：宽度撑满
        val (w1, h1) = Media.bubbleDims(maxW, maxH, 1600, 900)
        assertEquals(240f, w1, 0.01f)
        assertTrue(h1 < maxH)
        // 竖图：高度受限，宽度按比例收窄
        val (w2, h2) = Media.bubbleDims(maxW, maxH, 900, 1600)
        assertEquals(320f, h2, 0.01f)
        assertTrue(w2 <= maxW)
        // 正方形
        val (w3, h3) = Media.bubbleDims(maxW, maxH, 1000, 1000)
        assertEquals(w3, h3, 0.01f)
        // 超宽全景不会变成"一根线"
        val (_, h4) = Media.bubbleDims(maxW, maxH, 8000, 400)
        assertTrue(h4 >= 72f)
        // 宽高未知（旧消息）也要给出可用尺寸
        val (w5, h5) = Media.bubbleDims(maxW, maxH, 0, 0)
        assertTrue(w5 > 0f && h5 > 0f && h5 <= maxH)
    }

    @Test
    fun duration_label() {
        assertEquals("0:07", Media.durLabel(7_400))
        assertEquals("1:03", Media.durLabel(63_000))
        assertEquals("0:00", Media.durLabel(0))
        assertEquals("0:00", Media.durLabel(-5))
    }

    @Test
    fun summary_roundtrip_keeps_media_fields() {
        val json = Media.summary("ab12cd34", "IMG_1.jpg", 312_000, "/data/x.jpg", null, 1600, 1200)
        val m = Media.parse(json.toString())
        assertNotNull(m)
        assertEquals("ab12cd34", m!!.fid)
        assertEquals("IMG_1.jpg", m.name)
        assertEquals(312_000L, m.size)
        assertEquals("/data/x.jpg", m.path)
        assertEquals(1600, m.w)
        assertEquals(1200, m.h)
        assertEquals(0L, m.durMs)
        assertNull(m.saved)
    }

    @Test
    fun summary_keeps_video_duration() {
        val json = Media.summary("v1", "clip.mp4", 9_000_000, null, "下载/AnonChat/clip.mp4", 1920, 1080, 12_000)
        val m = Media.parse(json.toString())!!
        assertEquals(12_000L, m.durMs)
        assertEquals(1920, m.w)
        assertEquals("下载/AnonChat/clip.mp4", m.saved)
        assertNull(m.path)
    }

    @Test
    fun plain_file_summary_has_no_media_fields() {
        // 普通文件不该多写 dur/w/h，老版本 App 读到不认识字段虽然不炸，但没必要污染
        val o = org.json.JSONObject(
            Media.summary("f1", "a.pdf", 100, "/p/a.pdf", null, 0, 0).toString()
        )
        assertTrue(!o.has("dur"))
        assertTrue(!o.has("w"))
        assertTrue(o.has("path"))
    }

    @Test
    fun broken_json_returns_null() {
        assertNull(Media.parse("{\"fid\":\"x\""))
        assertNull(Media.parse("not json"))
        // 空对象不该崩，只是字段都是空值
        val m = Media.parse("{}")
        assertNotNull(m)
        // 名字留空，由 UI 层按当前语言兜底（数据层不写死语言）
        assertEquals("", m!!.name)
    }

    @Test
    fun too_big_info_tells_size_and_limit() {
        // 提示的句子在 UI 层资源里；这里测的是喂给它的两个数字
        val (label, limitMb) = Media.tooBigInfo(87L * 1024 * 1024, 20L * 1024 * 1024)
        assertEquals("87.0 MB", label)
        assertEquals(20, limitMb)
    }

    // ---------------------------------------------------------- 免压缩直传

    private val NORMAL = Media.ORIENT_NORMAL

    @Test
    fun jpeg_small_and_upright_passes_through() {
        // 实测会膨胀的那张：1200×1600、252KB、方向正常 —— 就该原样发
        assertTrue(Media.canPassThrough("image/jpeg", "picsum.jpg", 1200, 1600, 252_048, NORMAL))
        // mime 拿不到但扩展名是 jpg，也认
        assertTrue(Media.canPassThrough("", "IMG_2031.JPG", 1080, 1440, 300_000, NORMAL))
    }

    @Test
    fun oversized_jpeg_still_gets_compressed() {
        // 长边超 1600：必须缩
        assertTrue(!Media.canPassThrough("image/jpeg", "big.jpg", 4032, 3024, 400_000, NORMAL))
        // 尺寸没问题但体积超门槛：压一下能小
        assertTrue(!Media.canPassThrough("image/jpeg", "fat.jpg", 1600, 1200, 900 * 1024, NORMAL))
    }

    @Test
    fun rotated_jpeg_still_gets_compressed() {
        // 方向标记非 1 时必须重编码，否则竖拍的照片在气泡里会躺着
        assertTrue(!Media.canPassThrough("image/jpeg", "rot.jpg", 1600, 1200, 200_000, 6))
    }

    @Test
    fun non_jpeg_never_passes_through() {
        assertTrue(!Media.canPassThrough("image/png", "shot.png", 1080, 2340, 200_000, NORMAL))
        assertTrue(!Media.canPassThrough("image/gif", "a.gif", 300, 300, 50_000, NORMAL))
        // 未知大小（provider 没给 SIZE）按不安全处理，走压缩
        assertTrue(!Media.canPassThrough("image/jpeg", "x.jpg", 1200, 1600, 0, NORMAL))
        // 尺寸读不出来同理
        assertTrue(!Media.canPassThrough("image/jpeg", "x.jpg", 0, 0, 100_000, NORMAL))
    }
}
