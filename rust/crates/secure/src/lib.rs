//! 端到端加密会话
//!
//! 用 Olm（Matrix 的双棘轮实现，vodozemac）做 1v1 会话，不自己写密码学。
//!
//! 为什么是 Olm 而不是 libsignal：
//! - Olm 的 API 面小得多，且 vodozemac 是纯 Rust、独立审计过；
//! - libsignal 需要实现一整套 store trait，对一期是负担；
//! - Olm 本身就提供前向保密（每条消息新链密钥），满足阶段 0/P0 目标。
//!
//! 密钥分层：
//! - **Ed25519 身份**（`Identity`）：对外 ID、人类可读的身份。**不参与加密**。
//! - **X25519 身份 DH + OTK**（`Account`）：建立 3DH 共享密钥，之后走双棘轮。
//!
//! 会话按**对方 X25519 身份公钥**索引，因为收到预共享消息时只能拿到这个。
//! Ed25519 ID 只用于展示和对联系人建档。

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use identity::Identity;
use serde::{Deserialize, Serialize};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use vodozemac::olm::{
    Account, AccountPickle, DecryptionError, OlmMessage, Session, SessionConfig, SessionPickle,
};
use vodozemac::Curve25519PublicKey;

pub mod mnemonic;

/// 对外暴露的最小身份集合，用来塞进邀请码
#[derive(Debug, Clone)]
pub struct Bundle {
    /// Ed25519 身份公钥（base64url）
    pub pk: String,
    /// X25519 身份 DH 公钥（base64url）
    pub dh: String,
    /// 一次性预共享公钥（base64url），本次邀请专用
    pub otk: String,
}

/// 线上传输的密文信封。
///
/// 中继看到的只有这两样：消息类型和一个不可解的字节串。
/// 明文长度会被填充到 16 字节边界（见 `pad`），避免长度侧信道。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Wire {
    /// 0 = 预共享（首条消息），1 = 普通
    pub mt: usize,
    /// base64 密文
    pub ct: String,
}

impl Wire {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("信封序列化失败")
    }

    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        serde_json::from_slice(b).context("信封解析失败")
    }

    pub fn is_prekey(&self) -> bool {
        self.mt == 0
    }
}

/// 消息明文载荷。加密之后才出现这些字段，中继看不到。
///
/// **两条通道复用同一个队列**：文本和通话信令都走这里，靠 `kind` 区分。
/// 这样通话不需要额外端口、额外中继配置，也不产生新的元数据面。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payload {
    pub v: u16,
    /// 消息 ID，接收方用于去重（重放会命中这里）
    pub id: String,
    /// 发送方 Ed25519 ID，接收方借此知道"是谁发的"
    pub from: String,
    /// unix 秒
    pub ts: u64,
    /// 文本内容。控制类消息（`kind != "text"`）留空。
    pub text: String,
    /// 发送方自己的队列 ID，接收方据此知道往哪回复。
    /// 真实客户端里这由邀请码解决；阶段 0 直接塞在载荷里，少一次往返。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_qid: Option<String>,
    /// 发送方自己用的中继地址，配合 `reply_qid` 才构成"能回得去"的完整线索。
    ///
    /// 为什么必须有：接收方只知道"消息是从哪个中继推给我的"，
    /// 如果双方不在同一个中继上，光有 qid 依然投不进去。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_relays: Option<Vec<String>>,
    /// 发送方自报的显示名。陌生人会话靠它显示"谁想加你"，是唯一的身份线索。
    /// 注意：这是自报字段，**不能当身份凭据**（真正的身份凭据是 Olm 的 dh）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_name: Option<String>,
    /// 消息类型。缺省为 `"text"`，保证旧客户端/旧消息能正常解析。
    #[serde(default = "default_kind")]
    pub kind: String,
    /// 控制类消息的结构化内容（JSON 文本）。
    /// 用字符串而不是 `serde_json::Value`：跨 JNI/跨语言时字符串没有类型歧义。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

pub fn default_kind() -> String {
    "text".to_string()
}

/// 已知的消息类型常量。用点号分层，将来加 `file.*` 不会和 `call.*` 撞。
pub mod kind {
    /// 普通文本
    pub const TEXT: &str = "text";
    /// 主叫发起：{call_id, sdp, video:bool}
    pub const CALL_OFFER: &str = "call.offer";
    /// 被叫接受：{call_id, sdp}
    pub const CALL_ANSWER: &str = "call.answer";
    /// 被叫拒接：{call_id, reason}
    pub const CALL_REJECT: &str = "call.reject";
    /// 双方交换 ICE：{call_id, candidate, mid, mline}
    pub const CALL_ICE: &str = "call.ice";
    /// 任一方挂断：{call_id, reason}
    pub const CALL_END: &str = "call.end";
    /// 文件分块：body 为 JSON {fid, name, size, idx, total, data(base64)}
    pub const FILE: &str = "file";
}

impl Payload {
    /// 普通文本消息
    pub fn text_m(from: String, id: String, text: String) -> Self {
        Self {
            v: PAYLOAD_VERSION,
            id,
            from,
            ts: proto::now_secs(),
            text,
            reply_qid: None,
            reply_relays: None,
            self_name: None,
            kind: default_kind(),
            body: None,
        }
    }

    pub fn is_text(&self) -> bool {
        self.kind == kind::TEXT
    }
}

pub const PAYLOAD_VERSION: u16 = 1;

/// vodozemac 的密钥输出是**标准 base64**（含 + / 和填充），
/// 而我们的邀请码格式约定用 **base64url 无填充**。
/// 边界处不统一的话，邀请码会在校验阶段被拒（这个坑踩过一次）。
///
/// 注意 vodozemac 输出的是**无填充**的标准 base64（Matrix 的惯例），
/// 用带填充的引擎去解会失败——这个细节坑过一次。
fn norm_b64(s: &str) -> String {
    let trimmed = s.trim_end_matches('=');
    if let Ok(raw) = STANDARD_NO_PAD.decode(trimmed) {
        return URL_SAFE_NO_PAD.encode(raw);
    }
    if let Ok(raw) = URL_SAFE_NO_PAD.decode(trimmed) {
        return URL_SAFE_NO_PAD.encode(raw);
    }
    s.to_string()
}

/// 把 base64url 还原成 vodozemac 认识的标准 base64
fn to_vodo(s: &str) -> Result<String> {
    let trimmed = s.trim_end_matches('=');
    let raw = URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| STANDARD_NO_PAD.decode(trimmed))
        .map_err(|_| anyhow!("公钥不是合法的 base64/base64url"))?;
    // 交回 vodozemac 时用带填充的标准 base64
    Ok(STANDARD.encode(raw))
}

// ---------------------------------------------------------------- 解密失败的分类

/// 解密失败的分类。
///
/// 为什么需要分类：接收端必须决定"这条消息能不能 ack 掉"。
/// ack 一条重复消息的代价只是少一次重投；ack 一条解不开的真消息就等于把它删了。
/// 之前两种情况混成一个 `anyhow` 错误，接收端只能一律不 ack，
/// 结果是重复消息永远删不掉，在中继队列里挂满 30 天 TTL（2026-10-22 实测 19 条）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    /// 已经处理过：Olm 的消息密钥被消费过，或载荷 id 命中去重表
    Duplicate,
    /// 密文或载荷结构坏了，重试多少次都是这个结果
    Poison,
    /// MAC 校验不过：可能真的丢了消息，也可能双方状态错乱
    BadMac,
    /// 没有可用会话（对方还没建会话，或一次性密钥已被消耗）
    NoSession,
    /// 没归类（兜底）
    Unknown,
}

impl FailKind {
    /// 能否安全地 ack（= 从中继队列里删除）。
    ///
    /// 只有"确定已经处理过"和"结构坏了永远解不开"两种可以删。
    /// 其余一律不 ack：删了就真丢了，留在队列里至少还能对账。
    /// 代价是重投，而重投本身是幂等的（重复消息会被 ack 掉），可以接受。
    pub fn can_ack(self) -> bool {
        matches!(self, FailKind::Duplicate | FailKind::Poison)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FailKind::Duplicate => "duplicate",
            FailKind::Poison => "poison",
            FailKind::BadMac => "bad-mac",
            FailKind::NoSession => "no-session",
            FailKind::Unknown => "unknown",
        }
    }
}

/// 带分类的解密错误。
///
/// 仍然用 `anyhow` 承载，是为了不打乱调用点上的 `?`；分类靠 [`fail_kind`] 取回来。
/// 内部用 `?` 逃出来的、没手动分类的错误会被 `decrypt` 兜成 `Unknown`（= 不 ack），
/// 所以"漏分类"的最坏后果是多投一次，不会是丢消息。
#[derive(Debug)]
pub struct DecryptError {
    pub kind: FailKind,
    pub detail: String,
}

impl std::fmt::Display for DecryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.detail)
    }
}

impl std::error::Error for DecryptError {}

fn decr_err(kind: FailKind, detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(DecryptError {
        kind,
        detail: detail.into(),
    })
}

/// 从解密错误里取分类。取不到就按 `Unknown`（不 ack）处理。
pub fn fail_kind(e: &anyhow::Error) -> FailKind {
    e.downcast_ref::<DecryptError>()
        .map(|d| d.kind)
        .unwrap_or(FailKind::Unknown)
}

/// 一个设备 = 身份 + 加密账户 + 所有活跃会话
pub struct Device {
    pub identity: Identity,
    account: Account,
    /// key: 对方 X25519 身份公钥 base64。
    /// **多会话存储**：同一对方可能同时存在多条 Olm 会话（重新扫码添加会建立新会话，
    /// 而旧会话的在途消息仍需可解）。向量尾部是最新的会话。
    /// 旧版是单键 HashMap<String, Session>，后建的会话直接覆盖前者，
    /// "聊过之后又重新扫码添加"会丢旧会话的在途消息（协议规范弱点表 P0）。
    sessions: HashMap<String, Vec<Session>>,
    /// 已消费的消息 ID，用于防重放（阶段 0 只留最近 1024 条）
    seen: Vec<String>,
}

impl Device {
    pub fn new() -> Self {
        let mut account = Account::new();
        // 一次性密钥池：邀请码会消耗一个，多备几个避免连续发邀请时枯竭
        account.generate_one_time_keys(4);
        Self {
            identity: Identity::generate(),
            account,
            sessions: HashMap::new(),
            seen: Vec::new(),
        }
    }

    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let mut account = Account::new();
        account.generate_one_time_keys(4);
        Self {
            identity: Identity::from_seed(seed),
            account,
            sessions: HashMap::new(),
            seen: Vec::new(),
        }
    }

    /// 从恢复短语的熵建身份。
    ///
    /// 身份种子 = BIP39 种子（PBKDF2 64 字节）的前 32 字节——
    /// 同一段助记词永远派生同一个身份，这是"换手机能恢复"的前提。
    pub fn from_entropy(entropy: &[u8; 16]) -> Self {
        let seed64 = mnemonic::seed_from_entropy(entropy, "");
        Self::from_seed(&seed64[..32].try_into().expect("64 字节取前 32"))
    }

    pub fn id(&self) -> String {
        self.identity.id()
    }

    /// 生成一份新的邀请材料。每调用一次消耗一个一次性密钥——
    /// 所以**一个邀请码只能用一次**，这是无中心分发的必然代价。
    pub fn bundle(&mut self) -> Bundle {
        let keys = self.account.one_time_keys();
        if keys.is_empty() {
            self.account.generate_one_time_keys(1);
        }
        let keys = self.account.one_time_keys();
        // 取任意一个；KeyId 不参与协议，只用于本地追踪
        let (_, otk) = keys
            .into_iter()
            .next()
            .expect("刚生成过一次性密钥，不可能为空");

        self.account.mark_keys_as_published();

        Bundle {
            pk: self.identity.id(),
            dh: norm_b64(&self.account.curve25519_key().to_base64()),
            otk: norm_b64(&otk.to_base64()),
        }
    }

    /// 用对方的邀请材料建立出站会话。首条消息会自动是预共享类型。
    pub fn start_session(&mut self, peer_dh: &str, peer_otk: &str) -> Result<()> {
        let dh = Curve25519PublicKey::from_base64(&to_vodo(peer_dh)?)
            .map_err(|e| anyhow!("对方 DH 公钥无效: {e}"))?;
        let otk = Curve25519PublicKey::from_base64(&to_vodo(peer_otk)?)
            .map_err(|e| anyhow!("对方一次性公钥无效: {e}"))?;

        let session = self.account.create_outbound_session(
            SessionConfig::default(),
            dh,
            otk,
        );
        // 追加而不是覆盖：旧会话的在途消息（对方已用旧会话加密、还没送达或没解开的）
        // 必须仍可解。重新扫码添加是用户会做的正常操作，不该导致消息丢失。
        self.sessions
            .entry(peer_dh.to_string())
            .or_default()
            .push(session);
        Ok(())
    }

    pub fn has_session(&self, peer_dh: &str) -> bool {
        self.sessions.contains_key(peer_dh)
    }

    /// 丢掉与某个联系人的全部 Olm 会话（删除联系人时调用）。
    ///
    /// 必须显式清理：会话状态存在 device 文件里，不删的话删了联系人还会留下
    /// "能解密但没人认领"的僵尸会话；对方再来消息会被当成已删除联系人的消息，
    /// 落在已清空的会话里，用户看不到（静默丢失）。
    ///
    /// 返回删掉的会话条数（0 表示本来就没有）。
    pub fn drop_sessions(&mut self, peer_dh: &str) -> usize {
        self.sessions.remove(peer_dh).map(|v| v.len()).unwrap_or(0)
    }

    /// 加密一条消息。用与该对方的**最新**会话（向量尾部）。
    /// 旧会话仍保留用于解密在途消息，但发新消息应该走最新的棘轮状态。
    pub fn encrypt(&mut self, peer_dh: &str, payload: &Payload) -> Result<Wire> {
        let session = self
            .sessions
            .get_mut(peer_dh)
            .and_then(|v| v.last_mut())
            .ok_or_else(|| anyhow!("尚未与该联系人建立会话，请先用邀请码添加"))?;

        let plain = serde_json::to_vec(payload).context("载荷序列化失败")?;
        let padded = pad(&plain);

        let msg = session.encrypt(&padded);
        let (mt, ct) = msg.to_parts();
        Ok(Wire {
            mt,
            ct: STANDARD_NO_PAD.encode(ct),
        })
    }

    /// 解密一条消息。会自动处理"首条消息"（预共享）的情况：
    /// 从信封里取出发送方身份公钥，建立入站会话，并把明文交给调用方。
    ///
    /// 返回 (对方 DH 公钥 base64, 载荷)
    ///
    /// 失败时错误最外层一定是 [`DecryptError`]，用 [`fail_kind`] 取分类。
    /// 这里绝不能在外面再套 `.context()`：anyhow 的 downcast 只看最外层类型，
    /// 套一层分类就丢了，接收端会一律当"不认识的错误"而不 ack。
    pub fn decrypt(&mut self, expected_dh: Option<&str>, wire: &Wire) -> Result<(String, Payload)> {
        match self.decrypt_inner(expected_dh, wire) {
            Ok(v) => Ok(v),
            Err(e) => Err(match e.downcast::<DecryptError>() {
                Ok(d) => anyhow::Error::new(d),
                Err(other) => anyhow::Error::new(DecryptError {
                    kind: FailKind::Unknown,
                    detail: format!("{other:#}"),
                }),
            }),
        }
    }

    fn decrypt_inner(
        &mut self,
        expected_dh: Option<&str>,
        wire: &Wire,
    ) -> Result<(String, Payload)> {
        let raw = STANDARD_NO_PAD.decode(&wire.ct).map_err(|e| {
            // 连 base64 都不是：这条消息永远解不开，留着只会无限重投
            decr_err(FailKind::Poison, format!("密文不是合法 base64: {e}"))
        })?;
        let msg = OlmMessage::from_parts(wire.mt, &raw).map_err(|e| {
            decr_err(FailKind::Poison, format!("密文格式不正确: {e}"))
        })?;

        // 预共享消息：可能是"第一条"，也可能是"对方还没收到我们回复前发来的后续消息"
        if let OlmMessage::PreKey(pre) = &msg {
            let sender = norm_b64(&pre.identity_key().to_base64());
            // 建立入站会话失败时要区分"重复"和"没会话"，先把已知事实记下来
            let known_sender = self.sessions.contains_key(&sender);

            // 先试该发送方已有的所有会话。Olm 在收到对端消息前会持续发预共享类型，
            // 但此时的 OTK 早已被消耗，直接走 create_inbound_session 会失败——
            // 不先试已有会话的话，对方在收到回复前发的消息就会丢。
            // 多会话下同一发送方可能有多条，逐条试解（Olm 先验 MAC，失败不改状态）。
            let mut tried_pre: Vec<String> = Vec::new();
            if let Some(list) = self.sessions.get_mut(&sender) {
                for session in list.iter_mut() {
                    match session.decrypt(&msg) {
                        Ok(plain) => {
                            let payload = self.accept_plaintext(&plain)?;
                            return Ok((sender, payload));
                        }
                        // 这里刻意不 return：预共享消息还有"新建会话"这条路可走，
                        // 失败分类留到下面统一判定
                        Err(_) => {
                            tried_pre.push(
                                session.session_id().chars().take(8).collect(),
                            );
                        }
                    }
                }
            }

            // 再尝试建立新的入站会话（真正的新会话）
            match self.account.create_inbound_session(pre.identity_key(), pre) {
                Ok(res) => {
                    let payload = self.accept_plaintext(&res.plaintext)?;
                    self.sessions
                        .entry(sender.clone())
                        .or_default()
                        .push(res.session);
                    Ok((sender, payload))
                }
                Err(e) => {
                    // 一次性密钥被消耗，且已有会话也解不开。原因基本只有一个：
                    // 这条消息之前处理过、当时没 ack 掉，现在被重投了。
                    // 判成 Duplicate 才能让它被 ack 掉——否则对方发来的第一条消息
                    // 会永远卡在队列里，每次重连都重投一次。
                    let kind = if known_sender {
                        FailKind::Duplicate
                    } else {
                        FailKind::NoSession
                    };
                    Err(decr_err(
                        kind,
                        format!(
                            "建立入站会话失败（邀请码可能已被使用过），已有会话试过 [{}]: {e}",
                            tried_pre.join(", ")
                        ),
                    ))
                }
            }
        } else {
            // 普通消息（非预共享）：Olm 的普通消息里**不带发送方身份**，
            // 所以接收方必须自己判断该用哪个会话解。
            //
            // 之前的实现是"只有一个会话才用兜底"，结果是用户只要有第二个联系人，
            // 所有普通消息就全部解不开——这个坑在单联系人测试里完全发现不了。
            //
            // 现在改为按会话逐个试解。可行性依据：Olm 的 decrypt 先校验 MAC 再推进棘轮，
            // 失败时不会改动会话状态，所以"试错"是安全的（有测试守着这一点）。
            // 代价是 O(会话数) 次尝试，1v1 场景下会话数是个位数，可以接受。
            // 多会话存储后，同一个对方键下也可能有多条会话，全部纳入候选。
            let candidates: Vec<String> = match expected_dh {
                Some(k) if self.sessions.contains_key(k) => vec![k.to_string()],
                _ => self.sessions.keys().cloned().collect(),
            };

            if candidates.is_empty() {
                return Err(decr_err(
                    FailKind::NoSession,
                    "还没有任何会话，无法解密（对方需要先通过邀请码建立会话）",
                ));
            }

            let mut key_consumed = false;
            let mut mac_failed = false;
            let mut bad_padding = false;
            let mut last_err: Option<String> = None;
            // 逐会话试解的指纹记录：排查 MAC mismatch 时没有它，
            // 就只能知道"没解开"，不知道试过哪些会话、各败在哪一步
            // （2026-10-22 的 64 条 MAC mismatch 事后无法定性，缺的就是这个）。
            let mut tried: Vec<String> = Vec::new();
            fn short_fp(s: &str) -> String {
                s.chars().take(8).collect()
            }
            for key in candidates {
                let Some(list) = self.sessions.get_mut(&key) else {
                    continue;
                };
                for session in list.iter_mut() {
                    match session.decrypt(&msg) {
                        Ok(plain) => {
                            let payload = self.accept_plaintext(&plain)?;
                            return Ok((key, payload));
                        }
                        Err(e) => {
                            // 错会话在 MAC 处就失败，对的会话才可能报"密钥已用过"。
                            // 所以"任意一条报密钥已消费"就足够判定为重复。
                            let why = match &e {
                                DecryptionError::MissingMessageKey(_) => {
                                    key_consumed = true;
                                    "key-gap"
                                }
                                DecryptionError::InvalidMAC(_)
                                | DecryptionError::InvalidMACLength(_, _) => {
                                    mac_failed = true;
                                    "bad-mac"
                                }
                                // MAC 过了但填充坏了：内容是真的，只是结构有问题
                                DecryptionError::InvalidPadding(_) => {
                                    bad_padding = true;
                                    "padding"
                                }
                                // 中间丢了 2000+ 条才会走到这里，属于真异常，不 ack
                                DecryptionError::TooBigMessageGap(_, _) => "too-big-gap",
                            };
                            tried.push(format!(
                                "{}/{}:{why}",
                                short_fp(&key),
                                short_fp(&session.session_id()),
                            ));
                            last_err = Some(format!("{e}"));
                        }
                    }
                }
            }

            // 优先级：MAC 已通过说明消息是真的（只坏在结构）→ Poison；
            // 其次是"密钥已消费"→ Duplicate；再次是 MAC 不匹配 → BadMac。
            let kind = if bad_padding {
                FailKind::Poison
            } else if key_consumed {
                FailKind::Duplicate
            } else if mac_failed {
                FailKind::BadMac
            } else {
                FailKind::Unknown
            };
            Err(decr_err(
                kind,
                format!(
                    "解密失败（可能是重放、乱序或密钥不匹配），尝试过 [{}]: {}",
                    tried.join(", "),
                    last_err.unwrap_or_default()
                ),
            ))
        }
    }

    /// 解填充 → 解析 → 查重 → 记入去重表。
    /// 所有解密路径都必须走这里，否则防重放会出现漏网。
    fn accept_plaintext(&mut self, padded: &[u8]) -> Result<Payload> {
        let p = self.open_payload(padded)?;
        if self.seen.contains(&p.id) {
            // 明文唯一 id 已经处理过 = 确认重复，可以放心 ack
            return Err(decr_err(
                FailKind::Duplicate,
                format!("重复消息（已处理过 id={}）", p.id),
            ));
        }
        self.mark_seen(&p.id);
        Ok(p)
    }

    fn open_payload(&self, padded: &[u8]) -> Result<Payload> {
        // 能走到这里说明 MAC 已经过了，内容是真品；解不开只能是结构问题，
        // 重试无意义 → Poison（允许 ack，免得它永远占着队列）
        let plain = unpad(padded)
            .map_err(|e| decr_err(FailKind::Poison, format!("解填充失败: {e:#}")))?;
        let p: Payload = serde_json::from_slice(plain)
            .map_err(|e| decr_err(FailKind::Poison, format!("载荷解析失败: {e:#}")))?;
        if p.v != PAYLOAD_VERSION {
            return Err(decr_err(
                FailKind::Poison,
                format!("载荷版本 {} 不受支持", p.v),
            ));
        }
        Ok(p)
    }

    fn mark_seen(&mut self, id: &str) {
        self.seen.push(id.to_string());
        if self.seen.len() > 1024 {
            self.seen.remove(0);
        }
    }
}

impl std::fmt::Debug for Device {
    /// 只打印身份和会话数，绝不把密钥打出来
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Device(id={}, sessions={})",
            self.identity.id(),
            self.session_count()
        )
    }
}

impl Default for Device {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------- 填充

/// 把明文补到 16 字节边界（PKCS#7 风格）。
///
/// 目的不是加密（Olm 已经做了），而是**抹掉长度特征**：
/// 不填充的话，"嗯"和"我今晚不回家了"的密文长度差得很明显，中继能猜出内容量级。
///
/// 注意：这是粗粒度防护。真正要做到不泄露，需要固定分桶（比如按 256B 向上取整），
/// 代价是带宽浪费。P0 再权衡。
fn pad(data: &[u8]) -> Vec<u8> {
    let block = 16;
    let pad_len = block - (data.len() % block);
    let mut out = Vec::with_capacity(data.len() + pad_len);
    out.extend_from_slice(data);
    out.extend(std::iter::repeat(pad_len as u8).take(pad_len));
    out
}

fn unpad(data: &[u8]) -> Result<&[u8]> {
    if data.is_empty() {
        return Err(anyhow!("空载荷"));
    }
    let pad_len = *data.last().unwrap() as usize;
    if pad_len == 0 || pad_len > 16 || pad_len > data.len() {
        return Err(anyhow!("填充长度非法"));
    }
    // 校验填充字节全部一致，防篡改
    if !data[data.len() - pad_len..].iter().all(|b| *b as usize == pad_len) {
        return Err(anyhow!("填充内容不一致"));
    }
    Ok(&data[..data.len() - pad_len])
}


// ---------------------------------------------------------------- 持久化
//
// 磁盘布局（一个目录 = 一个设备）：
//   device.key        32 字节主密钥。**没有它，其余文件都解不开**
//   identity.enc      ChaCha20-Poly1305 加密的 Ed25519 种子
//   account.pickle    vodozemac 加密的账户（含一次性密钥池状态）
//   sessions.json     { 对方DH公钥: 加密的 SessionPickle }
//   seen.json         去重表（跨重启防重放）
//
// 安全说明：
// - 所有敏感文件都加密，密钥在 device.key 里。unix 下会把权限设成 600。
// - **这不是口令保护**：拿到整个目录的人依然能解密。真正的口令方案（Argon2id + BIP39
//   恢复短语）排在 P0；Android 上改成把主密钥放进 Keystore。
// - account 的 pickle 必须存：里面有一次密钥池的状态，不存的话重启后会重复使用
//   同一个 OTK，邀请码就失去"一次性"这个性质了。

const KEY_FILE: &str = "device.key";
const IDENTITY_FILE: &str = "identity.enc";
const ACCOUNT_FILE: &str = "account.pickle";
const SESSIONS_FILE: &str = "sessions.json";
const SEEN_FILE: &str = "seen.json";
/// 恢复短语的熵（16 字节，加密存）。备份短语随时能从这里重新生成。
const PHRASE_FILE: &str = "phrase.enc";

fn seal(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher =
        ChaCha20Poly1305::new_from_slice(key).map_err(|_| anyhow!("主密钥长度不对"))?;
    let mut nonce = [0u8; 12];
    getrandom::getrandom(&mut nonce).context("系统熵源不可用")?;
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext)
        .map_err(|_| anyhow!("加密失败"))?;
    let mut out = Vec::with_capacity(12 + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn unseal(key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < 12 + 16 {
        return Err(anyhow!("密文长度异常，文件可能被截断"));
    }
    let (nonce, ct) = blob.split_at(12);
    let cipher =
        ChaCha20Poly1305::new_from_slice(key).map_err(|_| anyhow!("主密钥长度不对"))?;
    cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| anyhow!("解密失败：主密钥不匹配，或文件被篡改"))
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)
        .with_context(|| format!("写入 {} 失败", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("替换 {} 失败", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn restrict(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_p: &Path) {
    // Windows 上没法简单设 600。生产环境应改用 DPAPI / Keystore 保护主密钥。
}

impl Device {
    /// 把整个设备状态写到目录。已存在则覆盖（仅覆盖内容，不覆盖主密钥）。
    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("无法创建目录 {}", dir.display()))?;
        let key = Self::load_or_create_key(dir)?;

        atomic_write(&dir.join(IDENTITY_FILE), &seal(&key, &self.identity.seed())?)?;
        atomic_write(
            &dir.join(ACCOUNT_FILE),
            self.account.pickle().encrypt(&key).as_bytes(),
        )?;

        let mut map = serde_json::Map::new();
        for (dh, list) in &self.sessions {
            // 兼容策略：只有一条会话时写**旧格式**（单个 pickle 字符串）。
            // 旧版程序读不了数组——一旦读到新格式文件会把整个会话表当成空的，
            // 用户如果把 App 换回旧版本（或测试环境新旧混装），所有会话就静默丢失。
            // 单会话是绝大多数用户的常态，这条路径写旧格式能让新旧版本互通。
            if list.len() == 1 {
                map.insert(
                    dh.clone(),
                    serde_json::Value::String(list[0].pickle().encrypt(&key)),
                );
            } else {
                let arr: Vec<serde_json::Value> = list
                    .iter()
                    .map(|s| serde_json::Value::String(s.pickle().encrypt(&key)))
                    .collect();
                map.insert(dh.clone(), serde_json::Value::Array(arr));
            }
        }
        atomic_write(
            &dir.join(SESSIONS_FILE),
            &serde_json::to_vec(&map).context("会话序列化失败")?,
        )?;
        atomic_write(
            &dir.join(SEEN_FILE),
            &serde_json::to_vec(&self.seen).context("去重表序列化失败")?,
        )?;
        Ok(())
    }

    /// 从目录恢复。**目录不存在或缺少密钥时直接报错，绝不悄悄生成新身份**——
    /// 那会让用户以为"聊天记录没了但账号还在"，实际上身份已经换了一个。
    pub fn load(dir: &Path) -> Result<Self> {
        let key_path = dir.join(KEY_FILE);
        let key_raw = std::fs::read(&key_path).with_context(|| {
            format!(
                "找不到设备密钥 {}。如果不是路径写错，说明这个目录里没有可恢复的设备",
                key_path.display()
            )
        })?;
        let key: [u8; 32] = key_raw
            .try_into()
            .map_err(|_| anyhow!("设备密钥长度不对（应为 32 字节）"))?;

        let seed_raw = std::fs::read(dir.join(IDENTITY_FILE))
            .context("缺少身份文件")
            .and_then(|b| unseal(&key, &b))?;
        let seed: [u8; 32] = seed_raw
            .try_into()
            .map_err(|_| anyhow!("身份种子长度不对"))?;

        let acc_txt = std::fs::read_to_string(dir.join(ACCOUNT_FILE))
            .context("缺少账户文件")?;
        let acc_pickle = AccountPickle::from_encrypted(&acc_txt, &key)
            .map_err(|e| anyhow!("账户 pickle 解密失败: {e}"))?;

        let sess_raw = std::fs::read(dir.join(SESSIONS_FILE)).unwrap_or_else(|_| b"{}".to_vec());
        let sess_map: serde_json::Map<String, serde_json::Value> = match serde_json::from_slice(&sess_raw)
        {
            Ok(m) => m,
            Err(e) => {
                // 文件级损坏必须吵闹：静默当空表 = 所有会话无声消失，用户只会
                // 看到"联系人都在但消息解不开了"。保留坏文件供人工抢救。
                let bad = dir.join(format!("{SESSIONS_FILE}.corrupt"));
                let _ = std::fs::rename(dir.join(SESSIONS_FILE), &bad);
                eprintln!(
                    "[secure] sessions.json 损坏（{e}），已改名为 {}。\
                     所有 Olm 会话丢失，需要对方重新互加才能恢复通信",
                    bad.display()
                );
                serde_json::Map::new()
            }
        };
        let mut sessions: HashMap<String, Vec<Session>> = HashMap::new();
        for (dh, v) in sess_map {
            // 新格式：[pickle, ...]；旧格式：单个 pickle 字符串。都接受，
            // 老用户升级后第一次收发消息就会把文件重写成新格式。
            let pickles: Vec<&str> = match &v {
                serde_json::Value::String(s) => vec![s.as_str()],
                serde_json::Value::Array(a) => {
                    a.iter().filter_map(|x| x.as_str()).collect()
                }
                _ => continue,
            };
            let mut list = Vec::new();
            for txt in pickles {
                match SessionPickle::from_encrypted(txt, &key) {
                    Ok(p) => list.push(Session::from_pickle(p)),
                    // 单个会话坏了不该连累整个设备启动
                    Err(e) => eprintln!("[secure] 会话 {dh} 解密失败，已跳过: {e}"),
                }
            }
            if !list.is_empty() {
                sessions.insert(dh, list);
            }
        }

        let seen: Vec<String> = std::fs::read(dir.join(SEEN_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();

        Ok(Self {
            identity: Identity::from_seed(&seed),
            account: Account::from_pickle(acc_pickle),
            sessions,
            seen,
        })
    }

    fn load_or_create_key(dir: &Path) -> Result<[u8; 32]> {
        let p = dir.join(KEY_FILE);
        if p.exists() {
            let raw = std::fs::read(&p)?;
            return raw
                .try_into()
                .map_err(|_| anyhow!("设备密钥长度不对（应为 32 字节）"));
        }
        let mut k = [0u8; 32];
        getrandom::getrandom(&mut k).context("系统熵源不可用")?;
        atomic_write(&p, &k)?;
        restrict(&p);
        Ok(k)
    }

    /// 是否已经与某人建立过会话
    pub fn session_count(&self) -> usize {
        self.sessions.values().map(Vec::len).sum()
    }

    /// 保存恢复短语对应的熵（用设备主密钥加密）。
    /// 存熵而不是存词句：词句可由熵随时重算，熵更小且不暴露口令派生路径。
    pub fn store_phrase_entropy(dir: &Path, entropy: &[u8; 16]) -> Result<()> {
        let key = Self::load_existing_key(dir)?;
        atomic_write(&dir.join(PHRASE_FILE), &seal(&key, entropy)?)?;
        Ok(())
    }

    /// 读回恢复短语的熵。设备目录损坏/被篡改时报错，不静默。
    pub fn read_phrase_entropy(dir: &Path) -> Result<[u8; 16]> {
        let key = Self::load_existing_key(dir)?;
        let raw = std::fs::read(dir.join(PHRASE_FILE))
            .map_err(|_| anyhow!("没有找到备份短语。这个身份创建于旧版本，无法生成恢复短语"))?;
        let e: [u8; 16] = unseal(&key, &raw)?
            .try_into()
            .map_err(|_| anyhow!("备份短语数据长度不对"))?;
        Ok(e)
    }

    /// 只读取已有密钥，不创建（区别于 `load_or_create_key`）
    fn load_existing_key(dir: &Path) -> Result<[u8; 32]> {
        let raw = std::fs::read(dir.join(KEY_FILE))
            .map_err(|_| anyhow!("找不到设备密钥，身份不存在"))?;
        raw.try_into().map_err(|_| anyhow!("设备密钥长度不对"))
    }
}

/// 安全码：两个人各自算一遍，数字一样就说明中间没有第三方。
///
/// 算法：把双方身份公钥（Ed25519 ID，43 字符 base64url）**按字典序排**后拼接，
/// SHA-256 打散，再逐字节展开成 60 位十进制数字（12 组 × 5 位）。
///
/// 为什么排序：双方手里"我的公钥/对方公钥"的位置是相反的，不排序就永远算不出同一个数。
/// 为什么只用公钥不用 Olm 会话密钥：会话是会重新建立的（重新扫码添加），
/// 而身份公钥终身不变——安全码跟着人会变的话，用户没法长期记住"上次核对过"。
/// 代价是：**它证明的是"这个身份没被换过"，而不是"这条会话没被劫持"**。
/// 中间人若能让两端各建一条独立会话，两个安全码必然不同，比对时立刻暴露。
pub fn safety_number(a_pk: &str, b_pk: &str) -> String {
    let (x, y) = if a_pk <= b_pk { (a_pk, b_pk) } else { (b_pk, a_pk) };
    let mut seed = {
        use sha2::{Digest, Sha256};
        Sha256::digest(format!("anonchat-safety-v1|{x}|{y}").as_bytes()).to_vec()
    };
    let mut out = String::with_capacity(60);
    while out.len() < 60 {
        // 32 字节按 5 字节一组，每组取模 10^5 → 一次 hash 出 30 位，两轮凑满 60 位
        for chunk in seed.chunks(5) {
            if out.len() >= 60 {
                break;
            }
            let mut v: u64 = 0;
            for (i, b) in chunk.iter().enumerate() {
                v |= (*b as u64) << (8 * i);
            }
            out.push_str(&format!("{:05}", v % 100_000));
        }
        seed = {
            use sha2::{Digest, Sha256};
            Sha256::digest(&seed).to_vec()
        };
    }
    out.truncate(60);
    out
}

/// 把 60 位数字切成 12 组，供界面显示（3 行 × 4 组）
pub fn safety_groups(code: &str) -> Vec<String> {
    code.as_bytes()
        .chunks(5)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safety_number_is_order_independent_and_stable() {
        let a = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let b = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        let ab = safety_number(a, b);
        // 双方拿到的"我的/对方"顺序相反，必须算出同一个数，否则比对毫无意义
        assert_eq!(ab, safety_number(b, a));
        // 确定性：同输入永远同输出（这也是能跨端核对的前提）
        assert_eq!(ab, safety_number(a, b));
        assert_eq!(ab.len(), 60);
        assert!(ab.chars().all(|c| c.is_ascii_digit()), "必须是纯数字，否则用户没法念出来比对");
    }

    #[test]
    fn safety_number_differs_for_different_peers() {
        let me = "ME_ME_ME_ME_ME_ME_ME_ME_ME_ME_ME_ME_ME_ME_ME";
        let p1 = safety_number(me, "peer-one");
        let p2 = safety_number(me, "peer-two");
        assert_ne!(p1, p2);
        // 只改一个字符也必须全变（雪崩）
        let p3 = safety_number(me, "peer-onf");
        assert_ne!(p1, p3);
    }

    #[test]
    fn safety_groups_split_into_twelve() {
        let code = safety_number("a", "b");
        let g = safety_groups(&code);
        assert_eq!(g.len(), 12);
        assert!(g.iter().all(|s| s.len() == 5));
        assert_eq!(g.concat(), code);
    }

    fn payload(text: &str) -> Payload {
        Payload {
            v: PAYLOAD_VERSION,
            id: proto::new_message_id(),
            from: "x".into(),
            ts: proto::now_secs(),
            text: text.into(),
            reply_qid: None,
            reply_relays: None,
            self_name: None,
            kind: default_kind(),
            body: None,
        }
    }

    /// 恢复短语 → 身份：同一段短语必须派生出同一个身份，这是备份的全部意义
    #[test]
    fn phrase_restore_yields_same_identity() {
        let e = [7u8; 16];
        let d1 = Device::from_entropy(&e);
        let d2 = Device::from_entropy(&e);
        assert_eq!(d1.id(), d2.id());

        // 和 from_phrase 路径（mnemonic → entropy → device）保持一致
        let phrase = mnemonic::entropy_to_mnemonic(&e);
        let seed = mnemonic::device_seed_from_phrase(&phrase).unwrap();
        let d3 = Device::from_seed(&seed);
        assert_eq!(d1.id(), d3.id());
    }

    #[test]
    fn phrase_entropy_roundtrip_through_disk() {
        let dir = std::env::temp_dir().join(format!("secure-phrase-{}", proto::new_queue_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dev = Device::from_entropy(&[3u8; 16]);
        dev.save(&dir).unwrap();

        Device::store_phrase_entropy(&dir, &[9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 1, 2, 3, 4, 5, 6]).unwrap();
        let back = Device::read_phrase_entropy(&dir).unwrap();
        assert_eq!(back, [9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 1, 2, 3, 4, 5, 6]);

        // 文件被篡改必须被发现，不能返回错误的熵
        let p = dir.join("phrase.enc");
        let mut raw = std::fs::read(&p).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        std::fs::write(&p, &raw).unwrap();
        assert!(Device::read_phrase_entropy(&dir).is_err(), "被篡改的短语必须报错");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 最关键的用例：两个设备通过邀请码建立会话，双向收发
    #[test]
    fn full_session_roundtrip() {
        let mut alice = Device::new();
        let mut bob = Device::new();

        let b = bob.bundle();
        alice.start_session(&b.dh, &b.otk).unwrap();
        assert!(alice.has_session(&b.dh));

        // Alice → Bob 首条（预共享）
        let w = alice.encrypt(&b.dh, &payload("你好")).unwrap();
        assert!(w.is_prekey(), "首条消息必须是预共享类型");

        let (sender, p) = bob.decrypt(None, &w).unwrap();
        // 收到的会话以**发送方**的 DH 公钥为键，不是自己的
        assert_ne!(sender, b.dh, "sender 应是 Alice 的 DH 公钥");
        assert_eq!(p.text, "你好");
        assert!(bob.has_session(&sender), "入站会话应已建立");

        // Bob → Alice 回一条普通消息（用 sender 作为会话键）
        let w2 = bob.encrypt(&sender, &payload("收到")).unwrap();
        assert!(!w2.is_prekey(), "后续消息应是普通类型");
        let (_, p2) = alice.decrypt(Some(&b.dh), &w2).unwrap();
        assert_eq!(p2.text, "收到");

        // 再多聊几轮，确认棘轮持续前进
        for i in 0..6 {
            let m = format!("第{i}条");
            let w = alice.encrypt(&b.dh, &payload(&m)).unwrap();
            let (_, got) = bob.decrypt(Some(&sender), &w).unwrap();
            assert_eq!(got.text, m);
            // 回一句，形成完整的往返
            let r = format!("回{i}");
            let wr = bob.encrypt(&sender, &payload(&r)).unwrap();
            let (_, gotr) = alice.decrypt(Some(&b.dh), &wr).unwrap();
            assert_eq!(gotr.text, r);
        }
    }

    /// bundle 的输出必须能直接塞进邀请码并通过校验。
    /// CLI 端到端跑的时候就是这里挂的：两边 base64 变体不一致。
    #[test]
    fn bundle_fits_invite_format() {
        let mut d = Device::new();
        let b = d.bundle();
        let inv = identity::Invite::new(
            b.pk.clone(),
            b.dh.clone(),
            b.otk.clone(),
            proto::new_queue_id(),
            vec!["wss://relay.example.com".into()],
        );
        assert!(inv.is_ok(), "邀请码校验失败: {:?}", inv.err());
    }

    /// 对方在收到回复前发来的第二条消息，仍然是预共享类型，
    /// 但 OTK 已经被第一条消耗掉了。必须能用已有会话解出来，否则消息会丢。
    #[test]
    fn second_prekey_before_reply_is_decryptable() {
        let mut a = Device::new();
        let mut b = Device::new();
        let bun = b.bundle();
        a.start_session(&bun.dh, &bun.otk).unwrap();

        let w1 = a.encrypt(&bun.dh, &payload("第一条")).unwrap();
        assert!(w1.is_prekey());
        let (sender, p1) = b.decrypt(None, &w1).unwrap();
        assert_eq!(p1.text, "第一条");

        // b 还没回复，a 又发一条：Olm 仍会标成预共享类型
        let w2 = a.encrypt(&bun.dh, &payload("第二条")).unwrap();
        assert!(w2.is_prekey(), "收到回复前应仍为预共享类型");
        let (_, p2) = b.decrypt(Some(&sender), &w2).unwrap();
        assert_eq!(p2.text, "第二条", "第二条不能因为 OTK 已消耗而丢失");
    }

    /// 重新扫码添加（同一对方身份、新一次性密钥建立第二条会话）时，
    /// 旧会话的在途消息必须仍然可解。旧版单键存储会直接覆盖旧会话，
    /// 这条测试在旧实现下必挂——它是"会话多键存储"的守门测试。
    #[test]
    fn rescan_keeps_old_session_inflight_decryptable() {
        let mut alice = Device::new();
        let mut bob = Device::new();

        // 第一条会话（第一次扫码）
        let bun1 = bob.bundle();
        alice.start_session(&bun1.dh, &bun1.otk).unwrap();
        let w1 = alice.encrypt(&bun1.dh, &payload("第一次聊")).unwrap();
        let (a_dh, _) = bob.decrypt(None, &w1).unwrap();

        // Bob 用第一条会话加密一条**在途消息**（Alice 还没收到）
        let inflight = bob.encrypt(&a_dh, &payload("在途消息")).unwrap();

        // 第二次扫码：同一个 Bob 身份、新一次性密钥 → Alice 建第二条会话
        let bun2 = bob.bundle();
        assert_eq!(bun1.dh, bun2.dh, "同一身份的 DH 公钥不应变化");
        alice.start_session(&bun2.dh, &bun2.otk).unwrap();

        // 旧会话的在途消息仍要解得开（旧版在这里挂：会话已被覆盖）
        let (_, p_old) = alice.decrypt(Some(&bun1.dh), &inflight).unwrap();
        assert_eq!(p_old.text, "在途消息", "重新扫码不能丢旧会话的在途消息");

        // 新会话照常双向收发
        let w2 = alice.encrypt(&bun2.dh, &payload("第二次聊")).unwrap();
        let (_, p2) = bob.decrypt(None, &w2).unwrap();
        assert_eq!(p2.text, "第二次聊");
        let wr = bob.encrypt(&a_dh, &payload("新的回复")).unwrap();
        let (_, pr) = alice.decrypt(Some(&bun2.dh), &wr).unwrap();
        assert_eq!(pr.text, "新的回复");

        // 同一对方下确实存在两条会话
        assert_eq!(alice.session_count(), 2, "两条会话必须并存");
    }

    /// 多会话状态必须完整过盘：保存 → 重新加载后，新旧会话的消息都解得开
    #[test]
    fn multi_session_survives_disk_roundtrip() {
        let dir = std::env::temp_dir().join(format!("secure-multi-{}", proto::new_queue_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut alice = Device::from_entropy(&[5u8; 16]);
        let mut bob = Device::new();

        let bun1 = bob.bundle();
        alice.start_session(&bun1.dh, &bun1.otk).unwrap();
        let w1 = alice.encrypt(&bun1.dh, &payload("会话一")).unwrap();
        let (a_dh, _) = bob.decrypt(None, &w1).unwrap();
        let inflight = bob.encrypt(&a_dh, &payload("过盘在途")).unwrap();

        let bun2 = bob.bundle();
        alice.start_session(&bun2.dh, &bun2.otk).unwrap();

        alice.save(&dir).unwrap();
        let mut alice2 = Device::load(&dir).unwrap();

        let (_, p_old) = alice2.decrypt(Some(&bun1.dh), &inflight).unwrap();
        assert_eq!(p_old.text, "过盘在途", "旧会话过盘后必须仍可解");

        let w2 = alice2.encrypt(&bun2.dh, &payload("会话二")).unwrap();
        let (_, p2) = bob.decrypt(None, &w2).unwrap();
        assert_eq!(p2.text, "会话二");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 旧版 sessions.json 是"一个对方键 = 一个 pickle 字符串"。
    /// 升级用户的数据必须能原样读进来，不能要求清数据重装。
    #[test]
    fn legacy_single_session_file_still_loads() {
        let dir = std::env::temp_dir().join(format!("secure-legacy-{}", proto::new_queue_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut alice = Device::from_entropy(&[6u8; 16]);
        let mut bob = Device::new();
        let bun = bob.bundle();
        alice.start_session(&bun.dh, &bun.otk).unwrap();
        let w1 = alice.encrypt(&bun.dh, &payload("旧格式时代")).unwrap();
        let (a_dh, _) = bob.decrypt(None, &w1).unwrap();
        let inflight = bob.encrypt(&a_dh, &payload("旧格式在途")).unwrap();

        alice.save(&dir).unwrap();

        // 单会话保存出来本就是旧格式（字符串）；为覆盖"文件里是旧格式"的场景，
        // 直接把值归一成字符串（如果是数组就取第一个元素）
        let path = dir.join(SESSIONS_FILE);
        let raw: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut legacy = serde_json::Map::new();
        for (k, v) in raw {
            let s = match v {
                serde_json::Value::String(s) => s,
                serde_json::Value::Array(a) => {
                    a.into_iter().next().and_then(|x| x.as_str().map(String::from)).unwrap()
                }
                other => panic!("意外的格式: {other}"),
            };
            legacy.insert(k, serde_json::Value::String(s));
        }
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let mut alice2 = Device::load(&dir).unwrap();
        let (_, p) = alice2.decrypt(Some(&bun.dh), &inflight).unwrap();
        assert_eq!(p.text, "旧格式在途", "旧格式 sessions.json 必须能读");

        // 单会话保存回**仍是旧格式**（新旧版本互通的兼容策略，见 save 的注释）
        alice2.save(&dir).unwrap();
        let raw2: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            raw2.values().all(|v| v.is_string()),
            "单会话必须写回旧格式（字符串），否则旧版程序读新文件会丢全部会话"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 多会话（同一对方两条）保存时才写新格式数组；这是多会话落盘的正向验证
    #[test]
    fn multi_session_saves_as_array() {
        let dir = std::env::temp_dir().join(format!("secure-mfmt-{}", proto::new_queue_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut alice = Device::from_entropy(&[8u8; 16]);
        let mut bob = Device::new();
        let bun1 = bob.bundle();
        alice.start_session(&bun1.dh, &bun1.otk).unwrap();
        let w1 = alice.encrypt(&bun1.dh, &payload("一")).unwrap();
        let (a_dh, _) = bob.decrypt(None, &w1).unwrap();
        let bun2 = bob.bundle();
        alice.start_session(&bun2.dh, &bun2.otk).unwrap();
        alice.save(&dir).unwrap();

        let raw: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&std::fs::read(dir.join(SESSIONS_FILE)).unwrap()).unwrap();
        assert!(
            raw.values().all(|v| v.is_array() && v.as_array().unwrap().len() == 2),
            "同一对方两条会话必须存成数组"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ciphertext_differs_even_for_same_text() {
        // 同样的明文，两次加密的密文必须不同（否则中继能看出"说了两遍同一句话"）
        let mut a = Device::new();
        let mut b = Device::new();
        let bun = b.bundle();
        a.start_session(&bun.dh, &bun.otk).unwrap();

        let p1 = payload("相同");
        let p2 = payload("相同");
        let c1 = a.encrypt(&bun.dh, &p1).unwrap();
        let c2 = a.encrypt(&bun.dh, &p2).unwrap();
        assert_ne!(c1.ct, c2.ct);
    }

    #[test]
    fn replay_is_rejected() {
        let mut a = Device::new();
        let mut b = Device::new();
        let bun = b.bundle();
        a.start_session(&bun.dh, &bun.otk).unwrap();

        let p = payload("只说一次");
        let w = a.encrypt(&bun.dh, &p).unwrap();
        assert!(b.decrypt(None, &w).is_ok());
        // 原样重放：Olm 的棘轮会拒绝，即使不拒绝，去重表也会拦
        assert!(b.decrypt(None, &w.clone()).is_err(), "重放必须失败");
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let mut a = Device::new();
        let mut b = Device::new();
        let bun = b.bundle();
        a.start_session(&bun.dh, &bun.otk).unwrap();

        let mut w = a.encrypt(&bun.dh, &payload("原话")).unwrap();
        // 改一个字节
        let mut raw = STANDARD_NO_PAD.decode(&w.ct).unwrap();
        let n = raw.len();
        raw[n - 1] ^= 0x01;
        w.ct = STANDARD_NO_PAD.encode(raw);
        assert!(b.decrypt(None, &w).is_err(), "篡改必须被发现");
    }

    #[test]
    fn invite_otk_is_single_use() {
        // 同一个邀请码不能被两个人用：第二次建立会话必须失败。
        // 这是"无中心分发预共享密钥"的固有代价，必须在产品上说清楚。
        let mut bob = Device::new();
        let bun = bob.bundle();

        let mut a1 = Device::new();
        let mut a2 = Device::new();
        let w1 = {
            a1.start_session(&bun.dh, &bun.otk).unwrap();
            a1.encrypt(&bun.dh, &payload("甲")).unwrap()
        };
        assert!(bob.decrypt(None, &w1).is_ok());

        a2.start_session(&bun.dh, &bun.otk).unwrap();
        let w2 = a2.encrypt(&bun.dh, &payload("乙")).unwrap();
        assert!(
            bob.decrypt(None, &w2).is_err(),
            "同一 OTK 的第二次入站必须失败"
        );
    }

    #[test]
    fn padding_roundtrip() {
        for len in [0usize, 1, 15, 16, 17, 100] {
            let data = vec![42u8; len];
            let p = pad(&data);
            assert_eq!(p.len() % 16, 0, "填充后必须是 16 的倍数");
            assert_eq!(unpad(&p).unwrap(), &data[..]);
        }
    }

    #[test]
    fn unpad_rejects_garbage() {
        assert!(unpad(&[]).is_err());
        assert!(unpad(&[0u8]).is_err(), "填充长度 0 非法");
        assert!(unpad(&[1u8, 2, 3, 2]).is_err(), "填充内容不一致");
    }

    #[test]
    fn wire_roundtrip() {
        let w = Wire {
            mt: 1,
            ct: STANDARD_NO_PAD.encode(b"abc"),
        };
        let b = w.to_bytes();
        let back = Wire::from_bytes(&b).unwrap();
        assert_eq!(back.mt, 1);
        assert_eq!(STANDARD_NO_PAD.decode(back.ct).unwrap(), b"abc");
    }

    #[test]
    fn encrypt_without_session_fails_clearly() {
        let mut a = Device::new();
        let err = a.encrypt("unopened", &payload("x")).unwrap_err().to_string();
        assert!(err.contains("邀请码"), "错误信息要能指导用户，实际：{err}");
    }
    // ---------------------------------------------------------- 持久化

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "secure-store-{}-{}-{}",
            tag,
            std::process::id(),
            proto::new_queue_id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn save_load_preserves_identity() {
        let dir = tmpdir("id");
        let d = Device::new();
        let id = d.id();
        d.save(&dir).unwrap();
        drop(d);
        let back = Device::load(&dir).unwrap();
        assert_eq!(back.id(), id, "身份必须完全一致");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 最关键的一条：会话的棘轮状态必须跨重启延续，
    /// 否则"昨天聊到一半，今天打开就发不出去了"。
    #[test]
    fn session_survives_restart() {
        let dir = tmpdir("sess");
        let mut alice = Device::new();
        let mut bob = Device::new();
        let bun = bob.bundle();
        alice.start_session(&bun.dh, &bun.otk).unwrap();

        let w = alice.encrypt(&bun.dh, &payload("第一条")).unwrap();
        let (sender, p) = bob.decrypt(None, &w).unwrap();
        assert_eq!(p.text, "第一条");

        bob.save(&dir).unwrap();
        drop(bob);
        let mut bob2 = Device::load(&dir).unwrap();
        assert_eq!(bob2.session_count(), 1, "会话应被恢复");

        // Alice 继续用同一个会话发，重启后的 bob 必须能解
        let w2 = alice.encrypt(&bun.dh, &payload("重启后")).unwrap();
        let (_, p2) = bob2.decrypt(Some(&sender), &w2).unwrap();
        assert_eq!(p2.text, "重启后", "棘轮状态必须跨重启延续");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replay_is_rejected_after_restart() {
        let dir = tmpdir("replay");
        let mut a = Device::new();
        let mut b = Device::new();
        let bun = b.bundle();
        a.start_session(&bun.dh, &bun.otk).unwrap();
        let w = a.encrypt(&bun.dh, &payload("只发一次")).unwrap();
        assert!(b.decrypt(None, &w).is_ok());

        b.save(&dir).unwrap();
        drop(b);
        let mut b2 = Device::load(&dir).unwrap();
        assert!(b2.decrypt(None, &w).is_err(), "重启后重放仍必须被拒");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 目录不存在时必须报错，不能悄悄造一个新身份——
    /// 否则用户会以为"账号还在，只是记录没了"，实际上人已经换了。
    #[test]
    fn load_without_key_does_not_silently_create_identity() {
        let dir = tmpdir("nokey");
        let err = Device::load(&dir).unwrap_err().to_string();
        assert!(
            err.contains("找不到设备密钥"),
            "错误信息应指明原因，实际：{err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 一次性密钥池的状态必须持久化，否则重启后邀请码会复用同一个 OTK，
    /// "邀请码一次性"这个承诺就破了。
    #[test]
    fn otk_not_reused_after_reload() {
        let dir = tmpdir("otk");
        let mut d = Device::new();
        let first = d.bundle();
        d.save(&dir).unwrap();
        drop(d);
        let mut d2 = Device::load(&dir).unwrap();
        let second = d2.bundle();
        assert_ne!(first.otk, second.otk, "重启后不能复用同一个一次性密钥");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tampered_identity_file_is_rejected() {
        let dir = tmpdir("tamper");
        let d = Device::new();
        d.save(&dir).unwrap();
        drop(d);

        // 改一个字节，AEAD 必须报错而不是解出垃圾
        let p = dir.join("identity.enc");
        let mut raw = std::fs::read(&p).unwrap();
        let n = raw.len();
        raw[n - 1] ^= 0x01;
        std::fs::write(&p, raw).unwrap();

        assert!(Device::load(&dir).is_err(), "身份文件被篡改必须被发现");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 用户有两个以上联系人时，普通消息必须能正确归类到对应会话。
    ///
    /// 旧实现是"只有一个会话才敢兜底"，于是加了第二个联系人之后
    /// **所有普通消息全部解密失败**——单联系人测试永远发现不了。
    #[test]
    fn decrypt_picks_right_session_among_many() {
        let mut bob = Device::new();
        let mut alice = Device::new();
        let mut carol = Device::new();

        // 每人一个独立的一次性密钥（同一个 OTK 只能用一次）
        let b1 = bob.bundle();
        let b2 = bob.bundle();
        alice.start_session(&b1.dh, &b1.otk).unwrap();
        carol.start_session(&b2.dh, &b2.otk).unwrap();

        let wa = alice.encrypt(&b1.dh, &payload("来自 Alice")).unwrap();
        let (da, pa) = bob.decrypt(None, &wa).unwrap();
        assert_eq!(pa.text, "来自 Alice");

        let wc = carol.encrypt(&b2.dh, &payload("来自 Carol")).unwrap();
        let (dc, pc) = bob.decrypt(None, &wc).unwrap();
        assert_eq!(pc.text, "来自 Carol");

        assert_ne!(da, dc, "两个发送方必须是不同会话");
        assert_eq!(bob.session_count(), 2);

        // 各回一句：Olm 在收到回复之前会一直发预共享类型，
        // 只有收到回复之后才转成普通消息（这是上一轮踩过的坑）
        let r1 = bob.encrypt(&da, &payload("收到 A")).unwrap();
        alice.decrypt(Some(&b1.dh), &r1).unwrap();
        let r2 = bob.encrypt(&dc, &payload("收到 C")).unwrap();
        carol.decrypt(Some(&b2.dh), &r2).unwrap();

        // 之后全是普通消息：既要解得开，也要归到正确的人。
        // 这里同时验证了"试错解不会破坏会话状态"。
        for i in 0..3 {
            let m = format!("Alice 第{i}条");
            let w = alice.encrypt(&b1.dh, &payload(&m)).unwrap();
            assert!(!w.is_prekey(), "后续消息应是普通类型");
            let (who, p) = bob.decrypt(None, &w).unwrap();
            assert_eq!(who, da, "必须归到 Alice 的会话");
            assert_eq!(p.text, m);
        }
        for i in 0..3 {
            let m = format!("Carol 第{i}条");
            let w = carol.encrypt(&b2.dh, &payload(&m)).unwrap();
            let (who, p) = bob.decrypt(None, &w).unwrap();
            assert_eq!(who, dc, "必须归到 Carol 的会话");
            assert_eq!(p.text, m);
        }
    }

    /// 完全没有会话时必须给出可读的错误，而不是空转。
    /// 真实场景：对方换了设备/重装，会话没了，但还在用旧会话发普通消息。
    #[test]
    fn decrypt_without_any_session_reports_clearly() {
        let mut bob = Device::new();
        let mut alice = Device::new();
        let b = bob.bundle();
        alice.start_session(&b.dh, &b.otk).unwrap();

        // 先正常往返一轮，让 alice 的会话转成普通消息模式
        let w1 = alice.encrypt(&b.dh, &payload("hi")).unwrap();
        let (da, _) = bob.decrypt(None, &w1).unwrap();
        let r = bob.encrypt(&da, &payload("yo")).unwrap();
        alice.decrypt(Some(&b.dh), &r).unwrap();

        let w2 = alice.encrypt(&b.dh, &payload("normal")).unwrap();
        assert!(!w2.is_prekey(), "此时应是普通消息");

        // 一个全新的设备收到这条普通消息：没有任何会话可用
        let mut fresh = Device::new();
        let e = fresh.decrypt(None, &w2).unwrap_err().to_string();
        assert!(e.contains("还没有任何会话"), "错误信息应说明原因，实际：{e}");
    }

    // -------------------------------------------------- 失败分类（决定能不能 ack）

    #[test]
    fn fail_kind_ack_policy() {
        // 只有"已处理过"和"结构坏了"可以 ack。
        // 其余 ack 掉就等于把消息删了，宁可让它留在队列里重投。
        assert!(FailKind::Duplicate.can_ack());
        assert!(FailKind::Poison.can_ack());
        assert!(!FailKind::BadMac.can_ack());
        assert!(!FailKind::NoSession.can_ack());
        assert!(!FailKind::Unknown.can_ack());
        // 日志里要能一眼分辨，别都打成同一串
        assert_eq!(FailKind::BadMac.as_str(), "bad-mac");
    }

    /// 重放一条已经解过的消息 → Duplicate（可以 ack）。
    /// 这是 2026-10-22 那 19 条积压消息的形态：处理过、当时没 ack 掉、重连被重投。
    #[test]
    fn replayed_message_is_classified_duplicate() {
        let mut a = Device::new();
        let mut b = Device::new();
        let bun = b.bundle();
        a.start_session(&bun.dh, &bun.otk).unwrap();

        let w = a.encrypt(&bun.dh, &payload("只发一次")).unwrap();
        b.decrypt(None, &w).unwrap();
        let e = b.decrypt(None, &w).unwrap_err();
        assert_eq!(
            fail_kind(&e),
            FailKind::Duplicate,
            "重放必须判成 duplicate 才 ack 得掉，实际：{e:#}"
        );
    }

    /// 篡改密文 → BadMac（**不能** ack，可能真的丢了消息）。
    #[test]
    fn tampered_message_is_classified_bad_mac() {
        let mut a = Device::new();
        let mut b = Device::new();
        let bun = b.bundle();
        a.start_session(&bun.dh, &bun.otk).unwrap();
        // 先往返一轮，把消息转成普通消息（不走 prekey 分支）
        let w1 = a.encrypt(&bun.dh, &payload("hi")).unwrap();
        let (da, _) = b.decrypt(None, &w1).unwrap();
        let r = b.encrypt(&da, &payload("yo")).unwrap();
        a.decrypt(Some(&bun.dh), &r).unwrap();

        let mut w = a.encrypt(&bun.dh, &payload("原话")).unwrap();
        assert!(!w.is_prekey());
        let mut raw = STANDARD_NO_PAD.decode(&w.ct).unwrap();
        let n = raw.len();
        raw[n - 1] ^= 0x01;
        w.ct = STANDARD_NO_PAD.encode(raw);

        let e = b.decrypt(None, &w).unwrap_err();
        assert_eq!(
            fail_kind(&e),
            FailKind::BadMac,
            "篡改必须归成 bad-mac（不 ack），实际：{e:#}"
        );
    }

    /// 密文不是合法 base64 → Poison（永远解不开，可以 ack 掉止住重投）。
    #[test]
    fn garbage_envelope_is_classified_poison() {
        let mut b = Device::new();
        let w = Wire {
            mt: 1,
            ct: "这不是 base64!!".into(),
        };
        let e = b.decrypt(None, &w).unwrap_err();
        assert_eq!(fail_kind(&e), FailKind::Poison, "实际：{e:#}");
    }

    /// 没有会话时收到普通消息 → NoSession（**不能** ack，可能是真丢的第一条）。
    #[test]
    fn no_session_is_not_ackable() {
        let mut bob = Device::new();
        let mut alice = Device::new();
        let b = bob.bundle();
        alice.start_session(&b.dh, &b.otk).unwrap();
        let w1 = alice.encrypt(&b.dh, &payload("hi")).unwrap();
        let (da, _) = bob.decrypt(None, &w1).unwrap();
        let r = bob.encrypt(&da, &payload("yo")).unwrap();
        alice.decrypt(Some(&b.dh), &r).unwrap();
        let w2 = alice.encrypt(&b.dh, &payload("normal")).unwrap();

        let mut fresh = Device::new();
        let e = fresh.decrypt(None, &w2).unwrap_err();
        assert_eq!(fail_kind(&e), FailKind::NoSession);
        assert!(!fail_kind(&e).can_ack());
    }

    /// 首条预共享消息处理过之后再重投：OTK 已消耗，但已有会话 →
    /// 必须判 Duplicate（可 ack），否则对方发来的第一条消息会永远卡在队列里。
    #[test]
    fn replayed_first_message_is_classified_duplicate() {
        let mut a = Device::new();
        let mut b = Device::new();
        let bun = b.bundle();
        a.start_session(&bun.dh, &bun.otk).unwrap();

        // 第一条一定是 prekey
        let w = a.encrypt(&bun.dh, &payload("第一条")).unwrap();
        assert!(w.is_prekey());
        b.decrypt(None, &w).unwrap();

        let e = b.decrypt(None, &w).unwrap_err();
        assert_eq!(
            fail_kind(&e),
            FailKind::Duplicate,
            "首条消息重投必须能 ack，实际：{e:#}"
        );
    }
}
