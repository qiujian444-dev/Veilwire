package io.anonchat.app

import android.content.Context

/**
 * UI 层的错误文案生成。
 *
 * 数据层给的是"码 + 参数"（[AppError]），这里按当前语言拼成句子。
 * 和 IceProbe / Preview 是同一个模式：接口只给类型和参数，界面负责措辞。
 */
internal fun errText(ctx: Context, e: AppError): String = when (e.kind) {
    // ---- 中继 / 发送 ----
    ErrKind.RELAY_NOT_SET -> ctx.getString(R.string.err_relay_not_set)
    ErrKind.RELAY_OR_IDENTITY_MISSING -> ctx.getString(R.string.err_relay_or_identity_missing)

    // ---- 重发 / 撤回 ----
    ErrKind.RESEND_TOO_OLD -> ctx.getString(R.string.err_resend_too_old)
    ErrKind.RECALL_NOT_OUTGOING -> ctx.getString(R.string.err_recall_not_outgoing)
    ErrKind.RECALL_NO_RID -> ctx.getString(R.string.err_recall_no_rid)
    ErrKind.RECALL_WINDOW_PASSED -> ctx.getString(R.string.err_recall_window_passed)
    ErrKind.RECALL_FAILED -> ctx.getString(R.string.err_recall_failed)

    // ---- 转发 ----
    ErrKind.FORWARD_NO_FILE -> ctx.getString(R.string.err_forward_no_file)

    // ---- 文件 ----
    ErrKind.FILE_EMPTY -> ctx.getString(R.string.err_file_empty)
    ErrKind.FILE_TOO_BIG ->
        ctx.getString(R.string.err_file_too_big, e.detail?.toIntOrNull() ?: 0)
    ErrKind.FILE_SEND_FAILED -> ctx.getString(R.string.err_file_send_failed, e.detail ?: "")

    // ---- 图片 ----
    ErrKind.IMG_SIZE_UNKNOWN -> ctx.getString(R.string.err_img_size_unknown)
    ErrKind.IMG_UNREADABLE -> ctx.getString(R.string.err_img_unreadable)
    ErrKind.IMG_PIXELS_TOO_MANY -> ctx.getString(R.string.err_img_pixels_too_many, e.detail ?: "")
    ErrKind.IMG_DECODE_FAILED -> ctx.getString(R.string.err_img_decode_failed)
    ErrKind.IMG_COMPRESS_FAILED -> ctx.getString(R.string.err_img_compress_failed)
    ErrKind.IMG_STILL_TOO_BIG -> ctx.getString(R.string.err_img_still_too_big, e.detail ?: "")

    // ---- 视频 ----
    ErrKind.VIDEO_SIZE_UNKNOWN -> ctx.getString(R.string.err_video_size_unknown)
    ErrKind.VIDEO_UNREADABLE -> ctx.getString(R.string.err_video_unreadable)

    // ---- 录音 ----
    ErrKind.REC_ALREADY_RECORDING -> ctx.getString(R.string.err_rec_already)
    ErrKind.REC_OPEN_FAILED -> ctx.getString(R.string.err_rec_open_failed, e.detail ?: "")

    // ---- 加好友 ----
    ErrKind.ADD_NO_CLUE -> ctx.getString(R.string.err_add_no_clue)
    ErrKind.ADD_FAILED -> ctx.getString(R.string.err_add_failed)

    // ---- 通话 ----
    ErrKind.CALL_NOT_INIT -> ctx.getString(R.string.err_core)
    ErrKind.CALL_START_FAILED -> ctx.getString(R.string.err_core)
    ErrKind.CALL_PC_FAILED -> ctx.getString(R.string.err_core)
    ErrKind.CALL_MIC_FAILED -> ctx.getString(R.string.err_call_mic_failed)
    ErrKind.CALL_BUILD_FAILED -> ctx.getString(R.string.err_call_build_failed)
    ErrKind.CALL_OFFER_FAILED -> ctx.getString(R.string.err_call_step_failed, e.detail ?: "")
    ErrKind.CALL_ANSWER_FAILED -> ctx.getString(R.string.err_call_step_failed, e.detail ?: "")
    ErrKind.CALL_RENEGOTIATE_FAILED -> ctx.getString(R.string.err_call_renegotiate_failed, e.detail ?: "")
    ErrKind.CALL_RECONNECT_FAILED -> ctx.getString(R.string.err_call_reconnect_failed, e.detail ?: "")
    ErrKind.CALL_NEGOTIATE_FAILED -> ctx.getString(R.string.err_call_negotiate_failed, e.detail ?: "")
    ErrKind.CALL_NETWORK_DOWN -> ctx.getString(R.string.err_call_network_down)

    // ---- 通用 ----
    ErrKind.CORE_FAILURE, ErrKind.CORE_PARSE_FAILURE ->
        if (e.detail.isNullOrBlank()) ctx.getString(R.string.err_core)
        else ctx.getString(R.string.err_core_detail, e.detail)
    ErrKind.UNKNOWN -> ctx.getString(R.string.err_unknown)
}

/**
 * 从 [Res] 里取给人看的错误文案。
 *
 * 优先用结构化的 [Res.err]（应用校验失败）；否则原样显示 [Res.error]
 * （Rust 核心转发的技术信息，本来就是语言中性的）；两者都没有给通用兜底。
 */
internal fun errText(ctx: Context, r: Res<*>): String = when {
    r.err != null -> errText(ctx, r.err)
    r.error != null -> r.error
    else -> ctx.getString(R.string.err_unknown)
}
