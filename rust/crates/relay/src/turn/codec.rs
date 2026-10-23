//! STUN/TURN 报文编解码（RFC 5389 / RFC 5766）。
//!
//! 只覆盖我们用到的那部分属性：不实现 IPv6、ALTERNATE-SERVER、TCP 传输。
//! 常量与字段偏移都按 RFC 来，注释里标了出处章节，方便对照抓包。

use crc32fast::Hasher;

pub const MAGIC: u32 = 0x2112_A442;
pub const HEADER_LEN: usize = 20;
const FINGERPRINT_XOR: u32 = 0x5354_554E; // "STUN"

// ---------------------------------------------------------------- 方法 / 类

pub mod method {
    pub const BINDING: u16 = 0x001;
    pub const ALLOCATE: u16 = 0x003;
    pub const REFRESH: u16 = 0x004;
    pub const SEND: u16 = 0x006;
    pub const DATA: u16 = 0x007;
    pub const CREATE_PERMISSION: u16 = 0x008;
    pub const CHANNEL_BIND: u16 = 0x009;
}

pub mod class {
    pub const REQUEST: u8 = 0b00;
    pub const INDICATION: u8 = 0b01;
    pub const SUCCESS: u8 = 0b10;
    pub const ERROR: u8 = 0b11;
}

pub mod attr {
    pub const MAPPED_ADDRESS: u16 = 0x0001;
    pub const USERNAME: u16 = 0x0006;
    pub const MESSAGE_INTEGRITY: u16 = 0x0008;
    pub const ERROR_CODE: u16 = 0x0009;
    /// 保留：目前不返回 UNKNOWN-ATTRIBUTES，但排障时按名字查表有用
    #[allow(dead_code)]
    pub const UNKNOWN_ATTRIBUTES: u16 = 0x000A;
    pub const CHANNEL_NUMBER: u16 = 0x000C;
    pub const LIFETIME: u16 = 0x000D;
    pub const XOR_PEER_ADDRESS: u16 = 0x0012;
    pub const DATA: u16 = 0x0013;
    pub const REALM: u16 = 0x0014;
    pub const NONCE: u16 = 0x0015;
    pub const XOR_RELAYED_ADDRESS: u16 = 0x0016;
    /// 保留：RFC 6156 的地址族请求，我们只支持 IPv4，暂不处理
    #[allow(dead_code)]
    pub const REQUESTED_ADDRESS_FAMILY: u16 = 0x0017;
    pub const REQUESTED_TRANSPORT: u16 = 0x0019;
    pub const XOR_MAPPED_ADDRESS: u16 = 0x0020;
    pub const SOFTWARE: u16 = 0x8022;
    pub const FINGERPRINT: u16 = 0x8028;
}

/// 把 (method, class) 编码成 14 位消息类型（RFC 5389 §6 的位布局）
pub fn encode_type(method: u16, cls: u8) -> u16 {
    (method & 0x000F)
        | ((method & 0x0070) << 1)
        | ((method & 0x0F80) << 2)
        | (((cls as u16) & 0x02) << 7)
        | (((cls as u16) & 0x01) << 4)
}

/// 从消息类型里拆出 (method, class)
pub fn decode_type(t: u16) -> (u16, u8) {
    let method = (t & 0x000F) | ((t & 0x00E0) >> 1) | ((t & 0x3E00) >> 2);
    let cls = (((t >> 7) & 0x02) | ((t >> 4) & 0x01)) as u8;
    (method, cls)
}

// ---------------------------------------------------------------- 报文

#[derive(Clone, Debug)]
pub struct Msg {
    pub method: u16,
    pub class: u8,
    pub tid: [u8; 12],
    /// 属性按原始顺序保存（顺序会影响 MESSAGE-INTEGRITY 的字节）
    pub attrs: Vec<(u16, Vec<u8>)>,
    /// MESSAGE-INTEGRITY 在原始报文里的偏移与取值（校验时要重算）
    pub mi: Option<(usize, [u8; 20])>,
    /// 原始报文（校验 MESSAGE-INTEGRITY / FINGERPRINT 用）
    pub raw: Vec<u8>,
}

impl Msg {
    pub fn parse(buf: &[u8]) -> Option<Msg> {
        if buf.len() < HEADER_LEN {
            return None;
        }
        let ty = u16::from_be_bytes([buf[0], buf[1]]);
        // 最高两位必须为 0，否则是 ChannelData（由上层单独分流）
        if ty & 0xC000 != 0 {
            return None;
        }
        let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        if buf.len() < HEADER_LEN + len || len % 4 != 0 {
            return None;
        }
        let magic = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        if magic != MAGIC {
            return None;
        }
        let (method, cls) = decode_type(ty);
        let mut tid = [0u8; 12];
        tid.copy_from_slice(&buf[8..20]);

        let mut attrs = Vec::new();
        let mut mi = None;
        let mut off = HEADER_LEN;
        let end = HEADER_LEN + len;
        while off + 4 <= end {
            let at = u16::from_be_bytes([buf[off], buf[off + 1]]);
            let al = u16::from_be_bytes([buf[off + 2], buf[off + 3]]) as usize;
            let vstart = off + 4;
            if vstart + al > end {
                return None; // 长度不合法，整条丢弃
            }
            let value = buf[vstart..vstart + al].to_vec();
            if at == attr::MESSAGE_INTEGRITY && al == 20 {
                let mut d = [0u8; 20];
                d.copy_from_slice(&value);
                mi = Some((off, d));
            }
            attrs.push((at, value));
            // 属性按 4 字节对齐，padding 不计入长度
            off = vstart + ((al + 3) & !3);
        }

        Some(Msg { method, class: cls, tid, attrs, mi, raw: buf[..end].to_vec() })
    }

    pub fn get(&self, ty: u16) -> Option<&[u8]> {
        self.attrs.iter().find(|(t, _)| *t == ty).map(|(_, v)| v.as_slice())
    }

    pub fn get_str(&self, ty: u16) -> Option<String> {
        self.get(ty).map(|v| String::from_utf8_lossy(v).to_string())
    }

    pub fn get_u32(&self, ty: u16) -> Option<u32> {
        self.get(ty).and_then(|v| {
            if v.len() >= 4 { Some(u32::from_be_bytes([v[0], v[1], v[2], v[3]])) } else { None }
        })
    }

    /// 解析 XOR-*-ADDRESS（RFC 5389 §15.2）。只支持 IPv4。
    pub fn get_xor_addr(&self, ty: u16) -> Option<std::net::SocketAddr> {
        let v = self.get(ty)?;
        if v.len() < 8 || v[1] != 0x01 {
            return None;
        }
        let port = u16::from_be_bytes([v[2], v[3]]) ^ ((MAGIC >> 16) as u16);
        let ip = u32::from_be_bytes([v[4], v[5], v[6], v[7]]) ^ MAGIC;
        Some(std::net::SocketAddr::from((std::net::Ipv4Addr::from(ip), port)))
    }

    /// 构造响应/请求报文。
    ///
    /// `key` 非 None 时在末尾追加 MESSAGE-INTEGRITY：
    /// 先把头部长度改成"含 MI"的值，再对 MI 之前的全部字节算 HMAC-SHA1（RFC 5389 §15.4）。
    /// `fingerprint` 为 true 时再加 FINGERPRINT（CRC32 ^ 0x5354554e）。
    pub fn encode(&self, key: Option<&[u8]>, fingerprint: bool) -> Vec<u8> {
        let ty = encode_type(self.method, self.class);
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&ty.to_be_bytes());
        out.extend_from_slice(&[0, 0]); // 长度先占位
        out.extend_from_slice(&MAGIC.to_be_bytes());
        out.extend_from_slice(&self.tid);
        for (t, v) in &self.attrs {
            if *t == attr::MESSAGE_INTEGRITY || *t == attr::FINGERPRINT {
                continue; // 这两个由下面统一算
            }
            out.extend_from_slice(&t.to_be_bytes());
            out.extend_from_slice(&(v.len() as u16).to_be_bytes());
            out.extend_from_slice(v);
            let pad = (4 - v.len() % 4) % 4;
            out.extend(std::iter::repeat(0).take(pad));
        }

        if let Some(k) = key {
            // 头部长度先指向"MI 属性末尾"，但 HMAC 只覆盖到 MI 属性**之前**
            // （不含 MI 属性头）。这是 2026-10-23 用 coturn 真实响应校准出来的，
            // 与 RFC 5389 §15.4 的文字一致：含头的算法会算出一个永远校验不过的值。
            let total = (out.len() + 24 - HEADER_LEN) as u16;
            out[2..4].copy_from_slice(&total.to_be_bytes());
            let mut mac = <hmac::Hmac<sha1::Sha1> as hmac::Mac>::new_from_slice(k)
                .expect("HMAC 接受任意长度 key");
            hmac::Mac::update(&mut mac, &out);
            let d = hmac::Mac::finalize(mac).into_bytes();
            out.extend_from_slice(&attr::MESSAGE_INTEGRITY.to_be_bytes());
            out.extend_from_slice(&20u16.to_be_bytes());
            out.extend_from_slice(&d);
        }

        if fingerprint {
            // 同理：CRC 覆盖到 FINGERPRINT 属性之前（不含属性头），再 XOR 0x5354554e
            let total = (out.len() + 8 - HEADER_LEN) as u16;
            out[2..4].copy_from_slice(&total.to_be_bytes());
            let mut h = Hasher::new();
            h.update(&out);
            let v = h.finalize() ^ FINGERPRINT_XOR;
            out.extend_from_slice(&attr::FINGERPRINT.to_be_bytes());
            out.extend_from_slice(&4u16.to_be_bytes());
            out.extend_from_slice(&v.to_be_bytes());
        }

        if key.is_none() && !fingerprint {
            let total = (out.len() - HEADER_LEN) as u16;
            out[2..4].copy_from_slice(&total.to_be_bytes());
        }
        out
    }

    /// 校验 MESSAGE-INTEGRITY。key 由 (user, realm, pass) 推出。
    /// 返回 false 表示签名不匹配（伪造/口令错/报文被改）。
    pub fn check_integrity(&self, key: &[u8]) -> bool {
        let Some((off, digest)) = self.mi else { return false };
        let mut buf = self.raw[..off].to_vec();
        // 计算 HMAC 时，头部长度必须当成"含 MI 属性"的值
        let total = (off + 24 - HEADER_LEN) as u16;
        if buf.len() < 4 {
            return false;
        }
        buf[2..4].copy_from_slice(&total.to_be_bytes());
        let mut mac = <hmac::Hmac<sha1::Sha1> as hmac::Mac>::new_from_slice(key)
            .expect("HMAC 接受任意长度 key");
        hmac::Mac::update(&mut mac, &buf);
        let expect = hmac::Mac::finalize(mac).into_bytes();
        // 定长比较，避免时序侧信道（这里是本地服务，但习惯保持一致）
        expect.len() == digest.len()
            && expect
                .iter()
                .zip(digest.iter())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    }

    // ------------------------------------------------------------ 构造辅助

    pub fn new(method: u16, cls: u8, tid: [u8; 12]) -> Msg {
        Msg { method, class: cls, tid, attrs: Vec::new(), mi: None, raw: Vec::new() }
    }

    pub fn add_str(&mut self, ty: u16, s: &str) {
        self.attrs.push((ty, s.as_bytes().to_vec()));
    }

    pub fn add_u32(&mut self, ty: u16, v: u32) {
        self.attrs.push((ty, v.to_be_bytes().to_vec()));
    }

    pub fn add_xor_addr(&mut self, ty: u16, addr: std::net::SocketAddr) {
        let std::net::SocketAddr::V4(v4) = addr else { return };
        let mut v = Vec::with_capacity(8);
        v.push(0);
        v.push(0x01); // IPv4
        v.extend_from_slice(&(addr.port() ^ (MAGIC >> 16) as u16).to_be_bytes());
        let ip = u32::from(*v4.ip()) ^ MAGIC;
        v.extend_from_slice(&ip.to_be_bytes());
        self.attrs.push((ty, v));
    }

    /// ERROR-CODE（RFC 5389 §15.6）：class 是 3 位数的高位（4/5/6），number 是低 8 位
    pub fn add_error(&mut self, code: u16, reason: &str) {
        let class = (code / 100) as u8 & 0x07;
        let number = (code % 100) as u8;
        let mut v = vec![0u8, 0u8, class, number];
        v.extend_from_slice(reason.as_bytes());
        self.attrs.push((attr::ERROR_CODE, v));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_roundtrip() {
        for &(m, c) in &[
            (method::BINDING, class::REQUEST),
            (method::BINDING, class::SUCCESS),
            (method::ALLOCATE, class::REQUEST),
            (method::ALLOCATE, class::ERROR),
            (method::CHANNEL_BIND, class::REQUEST),
            (method::DATA, class::INDICATION),
        ] {
            let t = encode_type(m, c);
            assert_eq!(decode_type(t), (m, c), "方法 {m:#x} 类 {c} 往返失败");
        }
    }

    #[test]
    fn known_wire_values() {
        // 抓包基准：Binding Request = 0x0001，Binding Success = 0x0101，
        // Allocate Request = 0x0003，Allocate Success = 0x0103，Allocate Error = 0x0113
        assert_eq!(encode_type(method::BINDING, class::REQUEST), 0x0001);
        assert_eq!(encode_type(method::BINDING, class::SUCCESS), 0x0101);
        assert_eq!(encode_type(method::ALLOCATE, class::REQUEST), 0x0003);
        assert_eq!(encode_type(method::ALLOCATE, class::SUCCESS), 0x0103);
        assert_eq!(encode_type(method::ALLOCATE, class::ERROR), 0x0113);
        assert_eq!(encode_type(method::CHANNEL_BIND, class::REQUEST), 0x0009);
    }

    #[test]
    fn parse_and_encode_binding() {
        let mut m = Msg::new(method::BINDING, class::REQUEST, [7u8; 12]);
        m.add_str(attr::SOFTWARE, "anonchat-test");
        let raw = m.encode(None, false);
        let back = Msg::parse(&raw).expect("能解析自己编的报文");
        assert_eq!(back.method, method::BINDING);
        assert_eq!(back.class, class::REQUEST);
        assert_eq!(back.tid, [7u8; 12]);
        assert_eq!(back.get_str(attr::SOFTWARE).as_deref(), Some("anonchat-test"));
    }

    #[test]
    fn xor_address_roundtrip() {
        let mut m = Msg::new(method::BINDING, class::SUCCESS, [1u8; 12]);
        let a: std::net::SocketAddr = "203.0.113.9:54321".parse().unwrap();
        m.add_xor_addr(attr::XOR_MAPPED_ADDRESS, a);
        let raw = m.encode(None, false);
        let back = Msg::parse(&raw).unwrap();
        assert_eq!(back.get_xor_addr(attr::XOR_MAPPED_ADDRESS), Some(a));
    }

    #[test]
    fn integrity_roundtrip_and_tamper() {
        let key = b"secret-key";
        let mut m = Msg::new(method::BINDING, class::REQUEST, [2u8; 12]);
        m.add_str(attr::USERNAME, "alice");
        let raw = m.encode(Some(key), false);
        let ok = Msg::parse(&raw).unwrap();
        assert!(ok.check_integrity(key), "正确 key 应当通过");
        assert!(!ok.check_integrity(b"wrong-key"), "错误 key 必须失败");

        // 篡改一个字节后签名必须失效
        let mut bad = raw.clone();
        let idx = HEADER_LEN + 4; // USERNAME 的值
        bad[idx] ^= 0xFF;
        let b = Msg::parse(&bad).unwrap();
        assert!(!b.check_integrity(key), "改过报文必须失败");
    }

    #[test]
    fn fingerprint_matches_rfc_formula() {
        let mut m = Msg::new(method::BINDING, class::REQUEST, [3u8; 12]);
        m.add_str(attr::SOFTWARE, "x");
        let raw = m.encode(None, true);
        // 最后一个属性是 FINGERPRINT：[type][len][crc]
        let fp = &raw[raw.len() - 8..];
        assert_eq!(u16::from_be_bytes([fp[0], fp[1]]), attr::FINGERPRINT);
        let claimed = u32::from_be_bytes([fp[4], fp[5], fp[6], fp[7]]);
        let mut h = Hasher::new();
        h.update(&raw[..raw.len() - 8]);
        assert_eq!(claimed, h.finalize() ^ FINGERPRINT_XOR);
    }

    #[test]
    fn rejects_bad_magic_and_truncated() {
        let mut m = Msg::new(method::BINDING, class::REQUEST, [4u8; 12]);
        m.add_str(attr::SOFTWARE, "y");
        let mut raw = m.encode(None, false);
        assert!(Msg::parse(&raw[..10]).is_none(), "截断报文不能解析");
        raw[4] ^= 0x01; // 破坏 magic cookie
        assert!(Msg::parse(&raw).is_none(), "magic 不对必须丢弃");
    }
}
