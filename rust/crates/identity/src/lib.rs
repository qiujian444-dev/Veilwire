//! 身份与邀请码
//!
//! 设计原则：
//! - **没有账号**。身份就是一对 Ed25519 密钥，公钥即 ID，不注册、不上报。
//! - **ID 不可搜索**。想加好友只能拿到对方给的邀请码/二维码，不存在"按 ID 找人"。
//! - 邀请码由**接收方**生成，里面带的是接收方自己的队列地址——所以加好友这个动作
//!   在网络上只表现为"发送方知道了往哪投"。
//!
//! 阶段 0 的信任模型是 TOFU（首次使用即信任）：邀请码里不带签名，因为它通常通过
//! 带外渠道（当面扫码、熟人转发）传递，被中间人篡改的窗口很小。
//! P1 要补的是：邀请码带签名 + 可选的安全码比对（和 Signal 的 safety number 一样）。

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

/// 身份公钥长度（Ed25519）
pub const PK_LEN: usize = 32;

/// 邀请码版本。加字段时递增，旧客户端遇到新版本要明确报错而不是静默忽略。
pub const INVITE_VERSION: u16 = 1;

/// 邀请码 URI 前缀。放在二维码里，扫码 App 能直接识别。
pub const INVITE_SCHEME: &str = "anonchat://invite/";

/// 一个身份 = 一对 Ed25519 密钥。
///
/// 注意：这里只存私钥种子，不存派生后的公钥缓存，避免两处不一致。
#[derive(Clone)]
pub struct Identity {
    signing: SigningKey,
}

impl Identity {
    /// 生成新身份。熵来自操作系统 CSPRNG（getrandom）。
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).expect("系统熵源不可用");
        Self {
            signing: SigningKey::from_bytes(&seed),
        }
    }

    /// 从恢复短语派生（P0 接 BIP39；阶段 0 先直接吃 32 字节种子）。
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(seed),
        }
    }

    /// 导出种子，用于生成恢复短语 / 本地备份。**这是明文私钥，调用方负责加密。**
    pub fn seed(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    /// 对外身份 ID：公钥的 base64url（43 字符，无填充）。
    ///
    /// 为什么不加前缀/校验位：ID 只是"往哪投"的路标，不是账号。
    /// 加前缀反而会让人以为它有语义，进而被用来做用户画像。
    pub fn id(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.signing.verifying_key().as_bytes())
    }

    pub fn sign(&self, msg: &[u8]) -> Signature {
        self.signing.sign(msg)
    }
}

impl std::fmt::Debug for Identity {
    /// 不要让人把私钥打进日志里
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Identity({})", self.id())
    }
}

/// 验签。对方发来的消息体要带签名，接收方用它确认"确实是他发的"。
pub fn verify(pk_b64: &str, msg: &[u8], sig: &Signature) -> bool {
    match decode_pk(pk_b64) {
        Ok(vk) => vk.verify_strict(msg, sig).is_ok(),
        Err(_) => false,
    }
}

pub fn decode_pk(pk_b64: &str) -> Result<VerifyingKey, &'static str> {
    let raw = URL_SAFE_NO_PAD
        .decode(pk_b64)
        .map_err(|_| "公钥不是合法的 base64url")?;
    let arr: [u8; PK_LEN] = raw.try_into().map_err(|_| "公钥长度不对")?;
    VerifyingKey::from_bytes(&arr).map_err(|_| "公钥不是合法的 Ed25519 点")
}

/// 校验一个 32 字节的 base64url 公钥（DH / OTK 用）。
pub fn decode_key32(s: &str) -> Result<[u8; PK_LEN], &'static str> {
    let raw = URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|_| "不是合法的 base64url")?;
    let arr: [u8; PK_LEN] = raw.try_into().map_err(|_| "长度不是 32 字节")?;
    Ok(arr)
}

/// 邀请码内容。
///
/// 它由**接收方**生成并交给对方，所以里面全是接收方的信息。
/// 除了身份公钥，还要带两把用于建立会话的密钥（对应 Signal 的 prekey 机制）：
/// - `pk`  ：Ed25519 身份公钥（ID、验签）
/// - `dh`  ：X25519 身份 DH 公钥（3DH 第一把）
/// - `otk` ：一次性预共享公钥（3DH 第二把），**用一次就作废**
///
/// 为什么要带 OTK：我们没有中心服务器来分发预共享密钥，只能把它塞进邀请码。
/// 代价是邀请码**只能用一次**——第二次用它建立会话会失败（对应 OTK 已被消耗）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Invite {
    pub v: u16,
    /// 接收方 Ed25519 身份公钥（base64url）
    pub pk: String,
    /// 接收方 X25519 身份 DH 公钥（base64url）
    pub dh: String,
    /// 一次性预共享公钥（base64url），用一次即作废
    pub otk: String,
    /// 接收方队列 ID
    pub qid: String,
    /// 中继地址，形如 ws://host:port 或 wss://host
    pub relays: Vec<String>,
    /// 接收方想显示的昵称（可选，纯展示，不参与任何认证）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Invite {
    pub fn new(
        pk: String,
        dh: String,
        otk: String,
        qid: String,
        relays: Vec<String>,
    ) -> Result<Self, String> {
        let inv = Invite {
            v: INVITE_VERSION,
            pk,
            dh,
            otk,
            qid,
            relays,
            name: None,
        };
        inv.validate()?;
        Ok(inv)
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// 校验：宁可把错误说清楚，也不要收下一个看起来能用实则连不上的邀请码。
    pub fn validate(&self) -> Result<(), String> {
        if self.v != INVITE_VERSION {
            return Err(format!(
                "邀请码版本 {} 不受支持（当前支持 v{INVITE_VERSION}）",
                self.v
            ));
        }
        decode_pk(&self.pk).map_err(|e| format!("邀请码里的身份公钥无效：{e}"))?;
        decode_key32(&self.dh).map_err(|e| format!("邀请码里的 DH 公钥无效：{e}"))?;
        decode_key32(&self.otk).map_err(|e| format!("邀请码里的一次性公钥无效：{e}"))?;
        if self.qid.len() != 32 || !self.qid.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("队列 ID 应为 32 位十六进制".into());
        }
        if self.relays.is_empty() {
            return Err("邀请码里至少要有一个中继地址".into());
        }
        if self.relays.len() > 8 {
            return Err("中继地址过多（上限 8 个）".into());
        }
        for r in &self.relays {
            if !(r.starts_with("ws://") || r.starts_with("wss://")) {
                return Err(format!("中继地址必须以 ws:// 或 wss:// 开头：{r}"));
            }
        }
        Ok(())
    }

    /// 编码成二维码/可粘贴的字符串
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("邀请码序列化失败");
        format!("{INVITE_SCHEME}{}", URL_SAFE_NO_PAD.encode(json))
    }

    /// 解析邀请码。容忍用户从聊天软件里复制时带上的空白。
    pub fn decode(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let body = s.strip_prefix(INVITE_SCHEME).unwrap_or(s);
        let raw = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| "邀请码不是合法的 base64url")?;
        let inv: Invite = serde_json::from_slice(&raw).map_err(|e| format!("邀请码解析失败：{e}"))?;
        inv.validate()?;
        Ok(inv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 随机生成一个 32 字节 base64url 公钥（DH / OTK 占位）
    fn k32() -> String {
        let mut b = [0u8; 32];
        getrandom::getrandom(&mut b).unwrap();
        URL_SAFE_NO_PAD.encode(b)
    }

    #[test]
    fn identity_id_is_43_chars() {
        let id = Identity::generate();
        assert_eq!(id.id().len(), 43, "Ed25519 公钥 base64url 应为 43 字符");
    }

    #[test]
    fn identity_rejects_duplicate_entropy() {
        // 同一份种子必须派生出同一身份，否则恢复短语就白给了
        let seed = [7u8; 32];
        assert_eq!(Identity::from_seed(&seed).id(), Identity::from_seed(&seed).id());
        // 不同种子必须不同
        assert_ne!(Identity::from_seed(&[1u8; 32]).id(), Identity::from_seed(&[2u8; 32]).id());
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let id = Identity::generate();
        let msg = b"hello";
        let sig = id.sign(msg);
        assert!(verify(&id.id(), msg, &sig));
        // 改一个字节就该验不过
        assert!(!verify(&id.id(), b"hellO", &sig));
    }

    #[test]
    fn verify_rejects_garbage_pk() {
        let sig = Identity::generate().sign(b"x");
        assert!(!verify("not-a-key", b"x", &sig));
        assert!(!verify("", b"x", &sig));
    }

    #[test]
    fn invite_roundtrip() {
        let id = Identity::generate();
        let inv = Invite::new(
            id.id(),
            k32(),
            k32(),
            proto::new_queue_id(),
            vec!["wss://relay.example.com".into()],
        )
        .unwrap()
        .with_name("阿甲");

        let code = inv.encode();
        assert!(code.starts_with(INVITE_SCHEME));
        let back = Invite::decode(&code).unwrap();
        assert_eq!(inv, back);
        assert_eq!(back.name.as_deref(), Some("阿甲"));
    }

    #[test]
    fn invite_tolerates_whitespace() {
        let id = Identity::generate();
        let inv = Invite::new(id.id(), k32(), k32(), proto::new_queue_id(), vec!["ws://127.0.0.1:8080".into()])
            .unwrap();
        let padded = format!("  {}\n", inv.encode());
        assert!(Invite::decode(&padded).is_ok());
    }

    #[test]
    fn invite_rejects_bad_relay_scheme() {
        let id = Identity::generate();
        let e = Invite::new(
            id.id(),
            k32(),
            k32(),
            proto::new_queue_id(),
            vec!["http://relay.example.com".into()],
        );
        assert!(e.is_err());
    }

    #[test]
    fn invite_rejects_wrong_version() {
        let id = Identity::generate();
        let mut inv = Invite::new(id.id(), k32(), k32(), proto::new_queue_id(), vec!["wss://a.b".into()]).unwrap();
        inv.v = 99;
        let code = {
            let json = serde_json::to_vec(&inv).unwrap();
            format!("{INVITE_SCHEME}{}", URL_SAFE_NO_PAD.encode(json))
        };
        let err = Invite::decode(&code).unwrap_err();
        assert!(err.contains("版本"), "实际错误：{err}");
    }

    #[test]
    fn invite_rejects_bad_dh_key() {
        let id = Identity::generate();
        assert!(Invite::new(
            id.id(),
            "short".into(),
            k32(),
            proto::new_queue_id(),
            vec!["wss://a.b".into()]
        )
        .is_err());
    }

    #[test]
    fn invite_rejects_empty_relays() {
        let id = Identity::generate();
        assert!(Invite::new(id.id(), k32(), k32(), proto::new_queue_id(), vec![]).is_err());
    }

    #[test]
    fn invite_tampered_base64_fails() {
        let id = Identity::generate();
        let inv = Invite::new(id.id(), k32(), k32(), proto::new_queue_id(), vec!["wss://a.b".into()]).unwrap();
        let mut code = inv.encode();
        // 翻转最后一个字符，模拟传输损坏
        code.pop();
        code.push(if code.ends_with('A') { 'B' } else { 'A' });
        assert!(Invite::decode(&code).is_err());
    }
}
