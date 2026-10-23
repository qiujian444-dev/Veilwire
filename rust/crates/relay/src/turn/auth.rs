//! TURN 长凭证认证（RFC 5389 §10）。
//!
//! 机制：客户端第一次请求不带凭证 → 服务端回 401 + REALM + NONCE；
//! 客户端重发时带 USERNAME / REALM / NONCE / MESSAGE-INTEGRITY；
//! 服务端用 `MD5(username ":" realm ":" password)` 当 key 校验 HMAC-SHA1。
//!
//! nonce 是防重放与防离线爆破的粗糙手段：这里用"当前 + 上一个"两份，
//! 定时轮换（旧的那份在轮换后仍接受一个周期，避免临界时刻把正常请求打成 438）。

use md5::{Digest, Md5};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use super::codec::{attr, Msg};

/// 临时凭据表：`user -> (pass, 过期时刻 unix 秒)`。
///
/// 为什么用全局而不是塞进 `Creds`：`Creds` 在 TURN 事件循环里以 `&mut` 独占持有，
/// 而签发动作来自另一个任务（WS 连接）。一个进程只跑一个 TURN 实例，
/// 用全局表比把所有权在两层之间传来传去简单得多，也避免了长期持锁。
static EPHEMERAL: OnceLock<Mutex<HashMap<String, (String, u64)>>> = OnceLock::new();

fn ephemeral() -> &'static Mutex<HashMap<String, (String, u64)>> {
    EPHEMERAL.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    let _ = getrandom::getrandom(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 签发一组临时 TURN 凭据，返回 `(user, pass, expires_at)`。
///
/// 用户名带 `eph-` 前缀便于在日志里和静态账号区分。`ttl_secs` 给 0 时按 1 小时。
pub fn issue_ephemeral(ttl_secs: u64) -> (String, String, u64) {
    let ttl = if ttl_secs == 0 { 3600 } else { ttl_secs };
    let expires = now_secs() + ttl;
    let user = format!("eph-{}", random_hex(8));
    let pass = random_hex(16);
    let mut m = ephemeral().lock().expect("临时凭据表中毒");
    // 顺手清理过期项，避免长期运行后无限增长
    let now = now_secs();
    m.retain(|_, (_, exp)| *exp > now);
    m.insert(user.clone(), (pass.clone(), expires));
    (user, pass, expires)
}

/// 查临时凭据，过期返回 None（并顺手删除）。
fn ephemeral_pass(user: &str) -> Option<String> {
    let mut m = ephemeral().lock().expect("临时凭据表中毒");
    let now = now_secs();
    match m.get(user) {
        Some((pass, exp)) if *exp > now => Some(pass.clone()),
        Some(_) => {
            m.remove(user);
            None
        }
        None => None,
    }
}

pub struct Creds {
    users: HashMap<String, String>,
    realm: String,
    nonce: String,
    prev_nonce: Option<String>,
}

pub enum Outcome {
    Ok { user: String, key: Vec<u8> },
    /// 没带凭证（或凭证不全）：回 401，带 realm + nonce
    Challenge,
    /// nonce 过期：回 438（Stale Nonce），带新 nonce
    Stale,
    /// 用户不存在 / 签名不对
    Denied,
}

impl Creds {
    pub fn new(realm: &str, users: HashMap<String, String>) -> Creds {
        let mut c = Creds {
            users,
            realm: realm.to_string(),
            nonce: new_nonce(),
            prev_nonce: None,
        };
        c.rotate();
        c
    }

    pub fn realm(&self) -> &str {
        &self.realm
    }

    pub fn nonce(&self) -> &str {
        &self.nonce
    }

    /// 轮换 nonce。保留上一份一个周期，避免轮换瞬间的正常请求被判 438。
    pub fn rotate(&mut self) {
        self.prev_nonce = Some(std::mem::replace(&mut self.nonce, new_nonce()));
    }

    /// 长凭证的 key = MD5(username:realm:password)
    pub fn key_for(&self, user: &str) -> Option<Vec<u8>> {
        // 先查静态账号（部署时配置的），再查临时签发的
        let pass = match self.users.get(user) {
            Some(p) => p.clone(),
            None => ephemeral_pass(user)?,
        };
        let mut h = Md5::new();
        h.update(format!("{user}:{}:{pass}", self.realm).as_bytes());
        Some(h.finalize().to_vec())
    }

    pub fn authenticate(&self, msg: &Msg) -> Outcome {
        let user = msg.get_str(attr::USERNAME).unwrap_or_default();
        let realm = msg.get_str(attr::REALM).unwrap_or_default();
        let nonce = msg.get_str(attr::NONCE).unwrap_or_default();
        if user.is_empty() || msg.mi.is_none() {
            return Outcome::Challenge;
        }
        if realm != self.realm {
            return Outcome::Denied;
        }
        let nonce_ok = nonce == self.nonce || Some(&nonce) == self.prev_nonce.as_ref();
        if !nonce_ok {
            return Outcome::Stale;
        }
        let Some(key) = self.key_for(&user) else {
            return Outcome::Denied;
        };
        if msg.check_integrity(&key) {
            Outcome::Ok { user, key }
        } else {
            Outcome::Denied
        }
    }
}

/// 进程内 nonce：时间 + 计数 + 地址熵，经 SHA-1 折叠成 32 位十六进制。
/// 不追求密码学强度——它只是"服务端签发过的凭证"，真正的安全性来自 HMAC。
fn new_nonce() -> String {
    use sha1::{Digest as _, Sha1};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let stack = 0u8;
    let addr = &stack as *const u8 as usize;
    let mut h = Sha1::new();
    h.update(nanos.to_le_bytes());
    h.update(n.to_le_bytes());
    h.update(addr.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    let d = h.finalize();
    d.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::codec::{class, method};

    fn creds() -> Creds {
        let mut u = HashMap::new();
        u.insert("alice".to_string(), "s3cret".to_string());
        Creds::new("anonchat", u)
    }

    #[test]
    fn key_is_md5_of_triplet() {
        let c = creds();
        let key = c.key_for("alice").unwrap();
        let mut h = Md5::new();
        h.update(b"alice:anonchat:s3cret");
        assert_eq!(key, h.finalize().to_vec());
    }

    #[test]
    fn challenge_then_accept() {
        let c = creds();
        // 第一次：不带凭证 → Challenge
        let bare = Msg::new(method::ALLOCATE, class::REQUEST, [0u8; 12]);
        assert!(matches!(c.authenticate(&bare), Outcome::Challenge));

        // 第二次：带 username/realm/nonce + MI → Ok
        let mut m = Msg::new(method::ALLOCATE, class::REQUEST, [1u8; 12]);
        m.add_str(attr::USERNAME, "alice");
        m.add_str(attr::REALM, "anonchat");
        m.add_str(attr::NONCE, c.nonce());
        let key = c.key_for("alice").unwrap();
        let raw = m.encode(Some(&key), false);
        let parsed = Msg::parse(&raw).unwrap();
        assert!(matches!(c.authenticate(&parsed), Outcome::Ok { .. }));
    }

    #[test]
    fn wrong_password_is_denied() {
        let c = creds();
        let mut m = Msg::new(method::ALLOCATE, class::REQUEST, [2u8; 12]);
        m.add_str(attr::USERNAME, "alice");
        m.add_str(attr::REALM, "anonchat");
        m.add_str(attr::NONCE, c.nonce());
        let raw = m.encode(Some(b"wrong-pass-key"), false);
        let parsed = Msg::parse(&raw).unwrap();
        assert!(matches!(c.authenticate(&parsed), Outcome::Denied));
    }

    #[test]
    fn unknown_nonce_is_stale() {
        let c = creds();
        let mut m = Msg::new(method::ALLOCATE, class::REQUEST, [3u8; 12]);
        m.add_str(attr::USERNAME, "alice");
        m.add_str(attr::REALM, "anonchat");
        m.add_str(attr::NONCE, "deadbeef");
        let raw = m.encode(Some(&c.key_for("alice").unwrap()), false);
        let parsed = Msg::parse(&raw).unwrap();
        assert!(matches!(c.authenticate(&parsed), Outcome::Stale));
    }

    #[test]
    fn previous_nonce_still_accepted() {
        let mut c = creds();
        let old = c.nonce().to_string();
        c.rotate();
        let mut m = Msg::new(method::ALLOCATE, class::REQUEST, [4u8; 12]);
        m.add_str(attr::USERNAME, "alice");
        m.add_str(attr::REALM, "anonchat");
        m.add_str(attr::NONCE, &old);
        let raw = m.encode(Some(&c.key_for("alice").unwrap()), false);
        let parsed = Msg::parse(&raw).unwrap();
        assert!(matches!(c.authenticate(&parsed), Outcome::Ok { .. }));
    }

    /// 临时签发的凭据必须能被正常校验（这是"客户端自动拿到 TURN 账号"的前提）
    #[test]
    fn ephemeral_credential_authenticates() {
        let c = creds();
        let (user, pass, expires) = issue_ephemeral(3600);
        assert!(user.starts_with("eph-"), "用户名要有前缀便于排查: {user}");
        assert_eq!(pass.len(), 32);
        assert!(expires > now_secs());

        let key = c.key_for(&user).expect("临时账号应当能算出 key");
        let mut h = Md5::new();
        h.update(format!("{user}:anonchat:{pass}").as_bytes());
        assert_eq!(key, h.finalize().to_vec(), "key 必须是 MD5(user:realm:pass)");

        let mut m = Msg::new(method::ALLOCATE, class::REQUEST, [7u8; 12]);
        m.add_str(attr::USERNAME, &user);
        m.add_str(attr::REALM, "anonchat");
        m.add_str(attr::NONCE, c.nonce());
        let raw = m.encode(Some(&key), false);
        let parsed = Msg::parse(&raw).unwrap();
        assert!(matches!(c.authenticate(&parsed), Outcome::Ok { .. }));
    }

    /// 过期凭据必须拒绝，否则等于永久账号
    #[test]
    fn expired_ephemeral_is_rejected() {
        let (user, pass, _) = issue_ephemeral(1);
        // 直接把表里的过期时间改成过去
        {
            let mut m = ephemeral().lock().unwrap();
            m.insert(user.clone(), (pass, now_secs() - 1));
        }
        let c = creds();
        assert!(c.key_for(&user).is_none(), "过期后不能再算出 key");
    }

    #[test]
    fn unknown_user_denied() {
        let c = creds();
        let mut m = Msg::new(method::ALLOCATE, class::REQUEST, [5u8; 12]);
        m.add_str(attr::USERNAME, "bob");
        m.add_str(attr::REALM, "anonchat");
        m.add_str(attr::NONCE, c.nonce());
        let raw = m.encode(Some(b"whatever"), false);
        let parsed = Msg::parse(&raw).unwrap();
        assert!(matches!(c.authenticate(&parsed), Outcome::Denied));
    }
}
