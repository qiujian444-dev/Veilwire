//! 内嵌 TURN 服务（STUN + TURN UDP）。
//!
//! 为什么要自己实现而不是拉一个 TURN crate：见 `docs/TURN内嵌中继设计.md`。
//! 简版理由：我们只要 RFC 5389/5766 的一个子集（UDP、长凭证、无 TLS/IPv6），
//! 依赖只有 sha1/hmac/crc32fast/md-5 四个小 crate（本地可离线构建）。

pub mod alloc;
pub mod auth;
pub mod codec;
pub mod server;

pub use server::{run, Config};
