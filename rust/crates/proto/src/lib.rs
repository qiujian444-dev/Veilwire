//! 阶段 0 协议骨架（SMP 风格无标识队列模型）
//!
//! 设计要点：
//! 1. 队列 ID 由**接收方**生成，与任何身份标识无关，中继不知道队列属于谁。
//! 2. 中继只做"存-取-删"，不做路由、不存用户表、不认识发送方。
//! 3. 一条消息可同时投递到 N 个队列（N 个不同中继），取首个成功。
//!
//! 阶段 0 用明文 JSON 传输（便于抓包调试）；进入 P0 后：
//! - 帧内 payload 换成 CBOR/MessagePack
//! - 队列 ID 与消息体全部走 TLS 之上，中继侧仍不可解
//! - 队列 ID 生成改为 CSPRNG（当前已用 getrandom，符合要求）

use serde::{Deserialize, Serialize};

/// 协议版本。中继按此字段拒绝不兼容客户端。
pub const PROTOCOL_VERSION: u16 = 1;

/// 队列默认存活时间：30 天。与文档第 5.2 节一致。
pub const QUEUE_TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// 单条消息体上限 64 KiB（阶段 0 限制，大文件走 DataChannel）。
pub const MAX_BODY_LEN: usize = 64 * 1024;

/// 客户端 → 中继
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientFrame {
    /// 创建新队列（接收方在加好友前调用，把 qid 写进邀请码）
    New { v: u16 },
    /// 订阅队列：建立长连接后立即发送，此后中继主动推送新消息
    Sub { qid: String },
    /// 投递消息（发送方调用）
    Put {
        qid: String,
        /// 客户端生成的消息 UUID，用于幂等去重
        id: String,
        /// 消息体（阶段 0 为明文 JSON 的字节串；P0 起为密文）
        body: Vec<u8>,
    },
    /// 拉取离线消息（重连后补齐）
    Get { qid: String },
    /// 确认已消费，中继删除
    Ack { qid: String, ids: Vec<String> },
    /// 心跳
    Ping,
    /// 索取 TURN 临时凭据（通话前用）。
    ///
    /// 为什么不把凭据写进 DHT：BEP5 的每条记录固定 6 字节（IP + 端口），
    /// 根本没有地方放账号密码；而把固定凭据硬编码进 App 等于把 TURN
    /// 变成公开免费中继。所以做成"连上 WS 之后按需索取"。
    GetTurn,
}

/// 中继 → 客户端
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerFrame {
    NewOk { qid: String, ttl: u64 },
    Ok { ref_id: String },
    /// 主动推送 / Get 的返回
    Msg {
        qid: String,
        id: String,
        body: Vec<u8>,
        /// 中继收到的时间（unix 秒），仅用于客户端展示，不作排序依据
        ts: u64,
    },
    Pong,
    /// TURN 临时凭据。`url` 形如 `turn:1.2.3.4:3478`，`ttl` 单位秒。
    ///
    /// 凭据是中继现签发的一次性账号，过期自动作废：这样即使泄露，
    /// 影响窗口也是有限的，而且能按账号限速 / 封禁。
    TurnCreds {
        url: String,
        user: String,
        pass: String,
        ttl: u64,
    },
    Err { code: String, msg: String },
}

impl ServerFrame {
    pub fn err(code: &str, msg: impl Into<String>) -> Self {
        ServerFrame::Err {
            code: code.to_string(),
            msg: msg.into(),
        }
    }
}

/// 生成 128 位随机队列 ID，hex 编码（32 字符）。
///
/// 注意：这个 ID 就是"地址"，拿到它的人可以往里投递（但读不了别人的密文）。
/// 生产环境需要配合"队列轮换"——每次重连可换新队列，避免长期地址被画像。
pub fn new_queue_id() -> String {
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf).expect("系统熵源不可用");
    hex(&buf)
}

/// 生成 128 位随机消息 ID。
pub fn new_message_id() -> String {
    new_queue_id()
}

pub fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_id_is_32_hex_chars() {
        let id = new_queue_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn queue_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            assert!(seen.insert(new_queue_id()), "队列 ID 出现碰撞");
        }
    }

    #[test]
    fn client_frame_roundtrip() {
        let f = ClientFrame::Put {
            qid: "ab".into(),
            id: "cd".into(),
            body: vec![1, 2, 3],
        };
        let s = serde_json::to_string(&f).unwrap();
        let back: ClientFrame = serde_json::from_str(&s).unwrap();
        match back {
            ClientFrame::Put { body, .. } => assert_eq!(body, vec![1, 2, 3]),
            _ => panic!("类型丢失"),
        }
    }

    #[test]
    fn server_frame_tag_is_snake_case() {
        let s = serde_json::to_string(&ServerFrame::NewOk {
            qid: "x".into(),
            ttl: 10,
        })
        .unwrap();
        assert!(s.contains("\"t\":\"new_ok\""), "实际: {s}");
    }
}
