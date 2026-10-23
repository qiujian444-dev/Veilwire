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
use std::time::{SystemTime, UNIX_EPOCH};

use super::codec::{attr, Msg};

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
        let pass = self.users.get(user)?;
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
