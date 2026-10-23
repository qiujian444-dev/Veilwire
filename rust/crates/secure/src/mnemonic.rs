//! BIP39 助记词（12 词，英文词表）—— 身份备份与恢复。
//!
//! 为什么自己实现而不是引 `bip39` crate：
//! - 需求只覆盖「128 位熵 → 12 词 → 派生种子」这一条路，引入整个 crate（带 unicode
//!   规范化、多语言词表）属于过度依赖；
//! - 依赖全在 sha2/hmac 这两个已被 vodozemac 链路缓存的库里，`--offline` 能编译。
//!
//! 标准符合性：
//! - 词表来自 bitcoin/bips 官方 `bip-0039/english.txt`（构建期 include_str! 嵌入）；
//! - 校验位、索引计算、PBKDF2-HMAC-SHA512（2048 轮，盐 "mnemonic"+口令）全部按 BIP39；
//! - 有测试用官方测试向量（全零熵）守住，保证和钱包工具互通。
//!
//! 身份派生的约定（本项目自有约定，不是 BIP39 的一部分）：
//!   设备身份种子 32 字节 = BIP39 种子（64 字节）的前 32 字节。
//!   这样「同一段助记词」永远派生出同一个身份，恢复短语丢了词序或抄错会被校验位拦下。

use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha512};

/// BIP39 英文词表，一行一词，共 2048 行
const WORDLIST_RAW: &str = include_str!("bip39_english.txt");

/// 12 词对应 128 位熵
pub const ENTROPY_BYTES: usize = 16;
pub const WORD_COUNT: usize = 12;

fn wordlist() -> &'static [&'static str; 2048] {
    static LIST: std::sync::OnceLock<&'static [&'static str; 2048]> = std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        let mut words = Vec::with_capacity(2048);
        for line in WORDLIST_RAW.lines() {
            let w = line.trim();
            if !w.is_empty() {
                words.push(w);
            }
        }
        // 编译期约定：词表必须是 2048 个词，否则 BIP39 的 11 位索引无意义
        let boxed: Box<[&str]> = words.into_boxed_slice();
        let arr: &'static [&'static str; 2048] = Box::leak(boxed.try_into().expect("词表必须恰好 2048 词"));
        arr
    })
}

/// 生成 128 位新熵（系统熵源）
pub fn new_entropy() -> [u8; ENTROPY_BYTES] {
    let mut e = [0u8; ENTROPY_BYTES];
    getrandom::getrandom(&mut e).expect("系统熵源不可用");
    e
}

/// 熵 → 助记词（12 词，空格分隔）
pub fn entropy_to_mnemonic(entropy: &[u8; ENTROPY_BYTES]) -> String {
    let list = wordlist();
    // 校验位：熵的 SHA256 首字节的高 4 位（128/32 = 4）
    let checksum = sha256_first_byte(entropy);

    // 把 熵(128bit) + 校验位(4bit) 拼成比特流，每 11 位取一个词
    let mut bit_buf: u32 = 0;
    let mut bit_len = 0usize;
    let mut words: Vec<&str> = Vec::with_capacity(WORD_COUNT);

    for &b in entropy {
        bit_buf = (bit_buf << 8) | b as u32;
        bit_len += 8;
        while bit_len >= 11 {
            let idx = ((bit_buf >> (bit_len - 11)) & 0x7ff) as usize;
            words.push(list[idx]);
            bit_len -= 11;
        }
    }
    // 128 位熵消耗后 bit_len 应为 7（128 mod 11），加上 4 位校验正好凑出第 12 个词
    debug_assert_eq!(bit_len, 7);
    bit_buf = (bit_buf << 4) | ((checksum >> 4) & 0x0f) as u32;
    bit_len += 4;
    let idx = ((bit_buf >> (bit_len - 11)) & 0x7ff) as usize;
    words.push(list[idx]);

    words.join(" ")
}

/// 助记词 → 熵。校验失败（抄错词、词不在表里、词数不对）直接报错。
pub fn mnemonic_to_entropy(phrase: &str) -> Result<[u8; ENTROPY_BYTES]> {
    let list = wordlist();
    // 归一：词表全小写，容忍用户抄成大写或带多余空白
    let norm = phrase.to_lowercase();
    let words: Vec<&str> = norm.split_whitespace().collect();
    if words.len() != WORD_COUNT {
        return Err(anyhow!(
            "恢复短语必须是 {WORD_COUNT} 个词，当前 {} 个",
            words.len()
        ));
    }

    // 词 → 11 位索引
    let mut bits: Vec<bool> = Vec::with_capacity(WORD_COUNT * 11);
    for w in &words {
        let idx = list
            .iter()
            .position(|c| c == w)
            .ok_or_else(|| anyhow!("词「{w}」不在恢复词表里（目前只支持英文词表）"))?;
        for i in (0..11).rev() {
            bits.push((idx >> i) & 1 == 1);
        }
    }
    if bits.len() != ENTROPY_BYTES * 8 + 4 {
        return Err(anyhow!("短语位长不正确"));
    }

    let mut entropy = [0u8; ENTROPY_BYTES];
    for (i, b) in bits[..ENTROPY_BYTES * 8].iter().enumerate() {
        if *b {
            entropy[i / 8] |= 1 << (7 - i % 8);
        }
    }
    // 重算校验位比对
    let checksum = sha256_first_byte(&entropy);
    let expect = (checksum >> 4) & 0x0f;
    let mut got: u8 = 0;
    for b in &bits[ENTROPY_BYTES * 8..] {
        got = (got << 1) | (*b as u8);
    }
    if got != expect {
        return Err(anyhow!("恢复短语校验失败：有词抄错了（第 12 个词由前 11 个决定，改不了）"));
    }
    Ok(entropy)
}

/// BIP39 种子：PBKDF2-HMAC-SHA512，2048 轮。
/// 注意方向：**口令 = 助记词句子**，盐 = "mnemonic" + 口令短语（BIP39 规定，
/// 和一般"盐在前"的直觉相反，搞反了种子就对不上任何标准工具）。
pub fn seed_from_entropy(entropy: &[u8; ENTROPY_BYTES], passphrase: &str) -> [u8; 64] {
    let phrase = entropy_to_mnemonic(entropy);
    pbkdf2_sha512(phrase.as_bytes(), &format!("mnemonic{passphrase}"), 2048)
}

/// 由助记词直接得到设备身份种子（本项目约定：BIP39 种子前 32 字节）
pub fn device_seed_from_phrase(phrase: &str) -> Result<[u8; 32]> {
    let e = mnemonic_to_entropy(phrase)?;
    let seed = seed_from_entropy(&e, "");
    Ok(seed[..32].try_into().expect("64 字节取前 32"))
}

// ---------------------------------------------------------------- 内部实现

/// SHA256 摘要的首字节（本模块只用它的高 4 位做校验位）
fn sha256_first_byte(data: &[u8]) -> u8 {
    let mut h = sha2::Sha256::new();
    h.update(data);
    let out = h.finalize();
    out[0]
}

/// PBKDF2-HMAC-SHA512 单实现（标准算法，RFC 8018）
fn pbkdf2_sha512(password: &[u8], salt: &str, rounds: u32) -> [u8; 64] {
    let mut mac = Hmac::<Sha512>::new_from_slice(password).expect("HMAC 接受任意长度密钥");
    // 盐 + 大端 4 字节块序号（只有一个 64 字节块，序号恒为 1）
    let mut block = Vec::with_capacity(salt.len() + 4);
    block.extend_from_slice(salt.as_bytes());
    block.extend_from_slice(&1u32.to_be_bytes());

    mac.update(&block);
    let mut u = mac.finalize().into_bytes();
    let mut acc = u;
    for _ in 1..rounds {
        let mut m = Hmac::<Sha512>::new_from_slice(password).expect("HMAC 接受任意长度密钥");
        m.update(&u);
        u = m.finalize().into_bytes();
        for (a, b) in acc.iter_mut().zip(u.iter()) {
            *a ^= b;
        }
    }
    let mut out = [0u8; 64];
    out.copy_from_slice(&acc);
    out
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    /// 官方 BIP39 测试向量：全零 128 位熵
    const ZEROS_PHRASE: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn wordlist_is_2048_sorted_unique() {
        let list = wordlist();
        assert_eq!(list.len(), 2048);
        for w in list.windows(2) {
            assert!(w[0] < w[1], "词表必须按字典序，否则 binary 定位会错");
        }
    }

    #[test]
    fn matches_official_test_vector() {
        let e = [0u8; ENTROPY_BYTES];
        assert_eq!(entropy_to_mnemonic(&e), ZEROS_PHRASE);
        assert_eq!(mnemonic_to_entropy(ZEROS_PHRASE).unwrap(), e);
    }

    #[test]
    fn roundtrip_random_entropy() {
        for _ in 0..50 {
            let e = new_entropy();
            let p = entropy_to_mnemonic(&e);
            assert_eq!(p.split_whitespace().count(), WORD_COUNT);
            assert_eq!(mnemonic_to_entropy(&p).unwrap(), e);
        }
    }

    #[test]
    fn tampered_word_is_rejected() {
        // 把最后一个词换成别的合法词 → 校验位对不上
        let bad = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon zoo";
        assert!(mnemonic_to_entropy(bad).is_err());
        // 词数不对
        assert!(mnemonic_to_entropy("abandon about").is_err());
        // 不在词表里的词
        assert!(mnemonic_to_entropy("abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon 不存在的词").is_err());
    }

    #[test]
    fn seed_matches_official_test_vector() {
        // 官方向量：全零熵、空口令的 BIP39 种子
        let e = [0u8; ENTROPY_BYTES];
        let seed = seed_from_entropy(&e, "");
        assert_eq!(
            hex(&seed),
            "5eb00bbddcf069084889a8ab9155568165f5c453ccb85e70811aaed6f6da5fc19a5ac40b389cd370d086206dec8aa6c43daea6690f20ad3d8d48b2d2ce9e38e4"
        );
    }

    #[test]
    fn device_seed_is_stable_and_phrase_derived() {
        let p = ZEROS_PHRASE;
        let s1 = device_seed_from_phrase(p).unwrap();
        let s2 = device_seed_from_phrase("  ABANDON  abandon   abandon abandon abandon abandon abandon abandon abandon abandon abandon ABOUT ".trim()).unwrap();
        assert_eq!(s1, s2, "大小写/多余空白必须归一后等价");
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
