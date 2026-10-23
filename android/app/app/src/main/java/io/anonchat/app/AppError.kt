package io.anonchat.app

/**
 * 应用层错误的"码 + 参数"。
 *
 * 为什么不给现成字符串：错误文案要显示在界面上，必须随系统语言变。
 * 数据层只说清"错在哪、参数是什么"，措辞交给 UI 层（见 MainActivity.errText）。
 * 和 IceProbe / Preview 是同一个模式。
 */
enum class ErrKind {
    // ---- 中继 / 发送 ----
    RELAY_NOT_SET,
    RELAY_OR_IDENTITY_MISSING,

    // ---- 重发 / 撤回 ----
    RESEND_TOO_OLD,
    RECALL_NOT_OUTGOING,
    RECALL_NO_RID,
    RECALL_WINDOW_PASSED,
    RECALL_FAILED,

    // ---- 转发 ----
    FORWARD_NO_FILE,

    // ---- 文件 ----
    FILE_EMPTY,
    FILE_TOO_BIG,
    FILE_SEND_FAILED,

    // ---- 图片 ----
    IMG_SIZE_UNKNOWN,
    IMG_UNREADABLE,
    IMG_PIXELS_TOO_MANY,
    IMG_DECODE_FAILED,
    IMG_COMPRESS_FAILED,
    IMG_STILL_TOO_BIG,

    // ---- 视频 ----
    VIDEO_SIZE_UNKNOWN,
    VIDEO_UNREADABLE,

    // ---- 录音 ----
    REC_ALREADY_RECORDING,
    REC_OPEN_FAILED,

    // ---- 加好友 ----
    ADD_NO_CLUE,
    ADD_FAILED,

    // ---- 通话 ----
    CALL_NOT_INIT,
    CALL_START_FAILED,
    CALL_PC_FAILED,
    CALL_MIC_FAILED,
    CALL_BUILD_FAILED,
    CALL_OFFER_FAILED,
    CALL_ANSWER_FAILED,
    CALL_RENEGOTIATE_FAILED,
    CALL_RECONNECT_FAILED,
    CALL_NEGOTIATE_FAILED,
    CALL_NETWORK_DOWN,

    // ---- 通用 ----

    /** Rust 核心转发的失败。[AppError.detail] 是核心层的技术信息 */
    CORE_FAILURE,

    /** 核心返回了无法解析的结果 */
    CORE_PARSE_FAILURE,

    /** 兜底 */
    UNKNOWN,
}

/** @param detail 可选的技术细节（异常消息等），UI 决定要不要展示 */
data class AppError(val kind: ErrKind, val detail: String? = null)
