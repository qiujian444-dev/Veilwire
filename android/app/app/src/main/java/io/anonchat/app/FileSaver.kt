package io.anonchat.app

import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.webkit.MimeTypeMap
import java.io.File

/**
 * 把聊天里的文件导出到手机存储。
 *
 * 为什么需要这一层：收发过程中的文件都躺在 App 私有目录
 * （`files/inbox`、`files/sent`），那是 `/data/data/...` 下面，
 * 文件管理器看不见、卸载 App 就跟着没了。用户想「存一份到自己手机里」
 * 必须有出口，而这个出口不能靠「点一下用系统程序打开」来凑。
 *
 * 两条路：
 * - Android 10+（API 29）：走 MediaStore 直接写 `下载/AnonChat/`，一键完成、不要权限。
 * - Android 8/9（minSdk 26）：写公共下载目录要 WRITE_EXTERNAL_STORAGE 运行时权限，
 *   为一次保存去要存储全盘权限不值得，交给调用方走 SAF「另存为」。
 */
object FileSaver {

    /** 下载目录下的子目录名。不要带斜杠 */
    const val SUBDIR = "AnonChat"

    /** 下载目录里给人的可读路径前缀，例如「下载/AnonChat」。跟随系统语言 */
    fun displayDir(ctx: Context): String = ctx.getString(R.string.file_dir_download, SUBDIR)

    fun canAutoSave(): Boolean = Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q

    /**
     * 保存到 `下载/AnonChat/`，返回给人看的路径（"下载/AnonChat/xxx.pdf"）。
     *
     * 失败返回 null，调用方要按失败处理——别静默吞掉，用户会以为存好了。
     * API 29 以下直接返回 null（调用方改用 SAF）。
     */
    fun saveToDownloads(ctx: Context, name: String, src: File): String? {
        if (!canAutoSave() || !src.exists() || !src.isFile) return null
        val display = uniqueName(ctx, safeName(name))
        val rel = "${Environment.DIRECTORY_DOWNLOADS}/$SUBDIR/"
        val resolver = ctx.contentResolver
        val values = ContentValues().apply {
            put(MediaStore.Downloads.DISPLAY_NAME, display)
            put(MediaStore.Downloads.MIME_TYPE, mimeOf(display))
            put(MediaStore.Downloads.RELATIVE_PATH, rel)
            // IS_PENDING 期间别的 App 看不到这个未写完的文件
            put(MediaStore.Downloads.IS_PENDING, 1)
        }
        val uri = runCatching {
            resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
        }.getOrNull() ?: return null

        return runCatching {
            resolver.openOutputStream(uri)?.use { out ->
                src.inputStream().use { it.copyTo(out) }
            } ?: error("cannot open output stream")
            resolver.update(
                uri,
                ContentValues().apply { put(MediaStore.Downloads.IS_PENDING, 0) },
                null, null
            )
            "${displayDir(ctx)}/$display"
        }.getOrElse { t ->
            android.util.Log.w("AnonChat", "save to Downloads failed: ${t.message}")
            // 别留一个 0 字节的空壳在下载目录里
            runCatching { resolver.delete(uri, null, null) }
            null
        }
    }

    /**
     * 紧急清除：删掉本 App 写入「下载/AnonChat/」的全部 MediaStore 条目。
     *
     * 只删自己拥有的（写入方就是本包名，MediaStore 所有权校验天然放行），
     * API 29 以下没有这个机制（当时的文件本来就不自动落公共目录），返回 0。
     * 返回删除条数（尽力而为，失败逐条跳过）。
     */
    fun purgeDownloads(ctx: Context): Int {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return 0
        var n = 0
        runCatching {
            val resolver = ctx.contentResolver
            val uri = MediaStore.Downloads.EXTERNAL_CONTENT_URI
            val sel = "${MediaStore.Downloads.RELATIVE_PATH}=?"
            val args = arrayOf("${Environment.DIRECTORY_DOWNLOADS}/$SUBDIR/")
            resolver.query(uri, arrayOf(MediaStore.Downloads._ID), sel, args, null)?.use { c ->
                while (c.moveToNext()) {
                    val id = c.getLong(0)
                    val item = android.content.ContentUris.withAppendedId(uri, id)
                    if (runCatching { resolver.delete(item, null, null) }.getOrDefault(0) > 0) n++
                }
            }
        }
        return n
    }

    /** 复制到用户在系统文件选择器里挑的位置（SAF）。返回是否成功。 */
    fun copyTo(ctx: Context, src: File, target: Uri): Boolean = runCatching {
        ctx.contentResolver.openOutputStream(target)?.use { out ->
            src.inputStream().use { it.copyTo(out) }
        } ?: error("cannot open target file")
        true
    }.getOrElse { t ->
        android.util.Log.w("AnonChat", "save as failed: ${t.message}")
        false
    }

    /** 分享出去（ACTION_SEND），走 FileProvider */
    fun shareIntent(ctx: Context, src: File, name: String): Intent? = runCatching {
        val uri = androidx.core.content.FileProvider.getUriForFile(
            ctx, ctx.packageName + ".files", src
        )
        Intent.createChooser(
            Intent(Intent.ACTION_SEND)
                .setType(mimeOf(name))
                .putExtra(Intent.EXTRA_STREAM, uri)
                .putExtra(Intent.EXTRA_TITLE, name)
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION),
            ctx.getString(R.string.share_file_chooser)
        )
    }.getOrNull()

    fun safeName(name: String): String =
        name.replace(Regex("[\\\\/:*?\"<>|\\x00-\\x1f]"), "_").trim().ifBlank { "file" }

    fun mimeOf(name: String): String {
        val ext = name.substringAfterLast('.', "").lowercase()
        if (ext.isEmpty()) return "application/octet-stream"
        return MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext)
            ?: "application/octet-stream"
    }

    /**
     * 下载目录里已有同名文件时追加 (1)(2)…
     *
     * MediaStore 自己也会改名，但不同 ROM 表现不一致（有的直接覆盖同名），
     * 与其赌系统行为，不如自己保证不覆盖别人的文件。
     */
    private fun uniqueName(ctx: Context, name: String): String {
        if (!exists(ctx, name)) return name
        val dot = name.lastIndexOf('.')
        val base = if (dot > 0) name.substring(0, dot) else name
        val ext = if (dot > 0) name.substring(dot) else ""
        for (i in 1..999) {
            val cand = "$base ($i)$ext"
            if (!exists(ctx, cand)) return cand
        }
        return "$base-${System.currentTimeMillis()}$ext"
    }

    private fun exists(ctx: Context, displayName: String): Boolean {
        val rel = "${Environment.DIRECTORY_DOWNLOADS}/$SUBDIR/"
        val cur = runCatching {
            ctx.contentResolver.query(
                MediaStore.Downloads.EXTERNAL_CONTENT_URI,
                arrayOf(MediaStore.Downloads._ID),
                "${MediaStore.Downloads.DISPLAY_NAME}=? AND ${MediaStore.Downloads.RELATIVE_PATH}=?",
                arrayOf(displayName, rel),
                null
            )
        }.getOrNull() ?: return false
        return cur.use { it.count > 0 }
    }
}
