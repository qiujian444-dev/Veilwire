//! 客户端业务层（同步 API）
//!
//! CLI 和 Android 的 JNI 层都调这里，避免两套实现走偏。
//!
//! 为什么是**同步** API：Android 侧通过 JNI 调用，Kotlin 在 IO 线程上跑，
//! 同步阻塞接口最简单可靠。内部用一个全局 tokio runtime 驱动异步网络代码。
//!
//! 连接复用：`recv_once` 会在全局缓存里保留 WebSocket 连接，避免每次收消息都重连。
//! 这点对 Android 尤其重要——0.3 要测的就是"长连接能活多久"，频繁重连会污染测试。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use proto::{ClientFrame, ServerFrame, PROTOCOL_VERSION};
use secure::{Device, Payload, Wire, PAYLOAD_VERSION};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub mod call;

/// 通话信令的类型常量（转发 `secure::kind`，避免字符串在两处写死）
pub mod call_kind {
    pub use secure::kind::CALL_ANSWER as ANSWER;
    pub use secure::kind::CALL_END as END;
    pub use secure::kind::CALL_ICE as ICE;
    pub use secure::kind::CALL_OFFER as OFFER;
    pub use secure::kind::CALL_REJECT as REJECT;

    /// 这些 kind 都属于通话信令
    pub fn is_call(kind: &str) -> bool {
        matches!(kind, OFFER | ANSWER | REJECT | ICE | END)
    }
}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const SELF_FILE: &str = "self.json";
const CONTACTS_FILE: &str = "contacts.json";

fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("无法创建运行时"))
}

/// 收消息时的连接缓存：dir -> (qid, relay, conn)
fn cache() -> &'static Mutex<HashMap<String, (String, String, Conn)>> {
    static C: OnceLock<Mutex<HashMap<String, (String, String, Conn)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------- 数据模型

#[derive(Debug, Default, Serialize, Deserialize)]
struct SelfState {
    #[serde(default)]
    qid: Option<String>,
    #[serde(default)]
    relays: Vec<String>,
    #[serde(default)]
    name: Option<String>,
}

/// 自己用过的中继，随消息带给对方，让对方"回得回来"。
/// 空的就返回 None，免得在载荷里塞个空数组占地方。
fn self_relays(me: &SelfState) -> Option<Vec<String>> {
    if me.relays.is_empty() {
        None
    } else {
        Some(me.relays.clone())
    }
}

/// 合并"对方自报的中继"与"本地兜底中继"，去重保序，最多留 4 个。
///
/// **顺序即语义**：`send_kind` 只用 `relays.first()`，所以第一位必须是
/// *对方队列真正所在* 的中继，否则 `put` 会打到错误的中继上得到 `no_queue`。
///
/// 因此这里**只能**以对方自报的 `reply_relays` 为主；`fallback` 是本机当前中继，
/// 它只在本机与对方恰好共用同一中继时才有意义，所以永远排最后。
///
/// 历史教训：早期版本写成 `merge_relays(via, extra)` 并让 `via` 排第一位，
/// 那个推理是站在**发送方**立场（"消息刚从我这出去，所以这个中继可达"）。
/// 但接收方语境下 `via` 是**自己的**中继，对方的队列根本不在那儿——
/// 结果就是陌生人回信永远发不出去，而且双方界面上都看不出任何异常。
fn merge_relays(peer: Option<Vec<String>>, fallback: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in peer
        .unwrap_or_default()
        .into_iter()
        .chain(std::iter::once(fallback.to_string()))
    {
        let r = r.trim().to_string();
        if r.is_empty() || out.contains(&r) {
            continue;
        }
        out.push(r);
        if out.len() >= 4 {
            break;
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    pub name: String,
    pub pk: String,
    pub dh: String,
    pub qid: String,
    pub relays: Vec<String>,
    /// 本地备注名（只在本人设备上生效，不随协议传输）。
    /// 加了这个字段后，App 里显示的永远是 alias 优先——联系人自己改名不影响你的称呼。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

/// 收到的一条消息，序列化后交给上层
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incoming {
    pub from_name: Option<String>,
    pub from_id: String,
    pub text: String,
    pub ts: u64,
    pub dh: String,
    /// 消息类型，见 `secure::kind`。文本消息为 `"text"`。
    pub kind: String,
    /// 控制类消息的结构化内容（JSON 文本）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// 中继层的消息 ID。通话信令用它做去重，防止重投导致 UI 反复弹窗。
    #[serde(default)]
    pub msg_id: String,
    /// 发送方在载荷里带的消息 ID（`Payload.id`，从 v1 就有，只是 0.9.0 起才透出）。
    /// 撤回（recall）消息靠它引用原消息：它由发送方生成、随消息加密传输，
    /// 接收方落盘后，撤回时两边都能按它定位到同一条。
    #[serde(default)]
    pub rid: String,
    /// 对方自报的显示名。陌生人会话里这是唯一能显示给人看的称呼。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_name: Option<String>,
    /// 回复要用对方的队列 ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_qid: Option<String>,
    /// 回复要用的中继候选：投递这条消息的中继 + 对方自报的中继（去重、保序）
    #[serde(default)]
    pub reply_relays: Vec<String>,
}

impl Incoming {
    /// 这条消息里有没有足够的线索让我回过去。
    /// 老版本客户端不带 `reply_qid`，这时只能显示"加不了"而不是给个点了没反应的按钮。
    pub fn can_reply(&self) -> bool {
        self.reply_qid.as_deref().is_some_and(|q| !q.is_empty()) && !self.reply_relays.is_empty()
    }
}

impl Incoming {
    pub fn is_text(&self) -> bool {
        self.kind == secure::kind::TEXT
    }
}

/// 最近的告警（环形，最多 32 条）。
///
/// 为什么需要：Rust 侧的 `eprintln!` 在 Android 上**完全看不到**——
/// 进程的 stderr 被系统丢弃，logcat 里一个字都没有。
/// 于是"消息解密失败被静默丢弃"这种问题只能靠猜。
/// 这里把关键告警攒起来，由上层（Android 前台服务）定期取走并打到 logcat。
fn warnings() -> &'static Mutex<std::collections::VecDeque<String>> {
    static W: OnceLock<Mutex<std::collections::VecDeque<String>>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
}

pub fn warn(msg: impl Into<String>) {
    let msg = msg.into();
    eprintln!("[client] {msg}");
    let mut g = warnings().lock().expect("告警队列中毒");
    if g.len() >= 32 {
        g.pop_front();
    }
    g.push_back(msg);
}

/// 取走并清空告警队列
pub fn take_warnings() -> Vec<String> {
    let mut g = warnings().lock().expect("告警队列中毒");
    g.drain(..).collect()
}

/// 收消息时的异常计数（进程生命周期内累计，不落盘）。
///
/// 为什么要有：2026-10-22 排查中继里 84 条积压时，两侧日志都看不出
/// "到底丢了几条、有几条是重复"，只能人工重放中继日志才数清。
/// 有了这几个数，下次直接在日志里看：`dup_acked` 涨说明在自愈，
/// `bad_mac` / `no_session` 涨才是真出问题。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RxCounters {
    /// 重复消息，已 ack（重连后的正常自愈，不算故障）
    pub dup_acked: u64,
    /// 结构坏掉的消息，已 ack（确实读不出来了）
    pub poison_acked: u64,
    /// MAC 不符，**未** ack（留在队列里等对账）
    pub bad_mac: u64,
    /// 没有可用会话，**未** ack
    pub no_session: u64,
    /// 其他未归类失败，**未** ack
    pub unknown: u64,
    /// ack 发送失败次数（这些消息下次重连会被重投）
    pub ack_failed: u64,
}

fn rx_counters() -> &'static Mutex<RxCounters> {
    static C: OnceLock<Mutex<RxCounters>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(RxCounters::default()))
}

/// 读一份计数快照（给 UI / 诊断用，不清零）
pub fn rx_counters_snapshot() -> RxCounters {
    *rx_counters().lock().expect("计数锁中毒")
}

/// 记一次解密失败，并说明这条消息有没有被 ack 掉。
///
/// 同一类错误累计计数写进日志：一条刷屏时至少能看出"已经第 N 条了"。
/// `mt`（0=预共享 1=普通）单独打出来：MAC mismatch 到底出在建立会话阶段
/// 还是已有会话阶段，处置方式完全不同，之前两类混在一个计数里查不动。
fn note_decrypt_fail(kind: secure::FailKind, from: &str, mt: usize, detail: &str, acked: bool) {
    let mut c = rx_counters().lock().expect("计数锁中毒");
    let n = match kind {
        secure::FailKind::Duplicate => {
            c.dup_acked += 1;
            c.dup_acked
        }
        secure::FailKind::Poison => {
            c.poison_acked += 1;
            c.poison_acked
        }
        secure::FailKind::BadMac => {
            c.bad_mac += 1;
            c.bad_mac
        }
        secure::FailKind::NoSession => {
            c.no_session += 1;
            c.no_session
        }
        secure::FailKind::Unknown => {
            c.unknown += 1;
            c.unknown
        }
    };
    drop(c);
    let tail = if acked {
        "已确认并从中继队列删除"
    } else {
        "保留在队列中（未确认）"
    };
    // mt=1（既有会话的普通消息）解不开，最常见的原因是**对方重装/恢复了身份**：
    // 恢复短语只恢复 Ed25519 身份，X25519 会话密钥会重新生成，他自用的旧棘轮
    // 就再也对不上了。给用户一句可行动的提示，别让他对着技术术语干瞪眼。
    let hint = if mt == 1 && matches!(kind, secure::FailKind::BadMac | secure::FailKind::NoSession) {
        "。提示：对方可能重装或恢复过 App——恢复短语只恢复身份、不恢复加密会话；请让对方重新生成邀请码、双方重新互加即可恢复"
    } else {
        ""
    };
    warn(format!(
        "无法解密来自 {from} 的消息 [mt={mt} {}] {tail}，同类累计 {n} 条: {detail}{hint}",
        kind.as_str()
    ));
}

/// 发 ack，失败要留痕并重试一次。
///
/// 以前是 `let _ = conn.ack(...)`：连接半开时 ack 全部静默消失，
/// 65 条已处理的消息一条都没从队列里删掉，下次重连全量重投、
/// 全部报"消息已被消费"（2026-10-22 实测）。ack 丢了的后果是队列无限膨胀，
/// 所以至少必须留痕。
///
/// 注意：半开连接上 write 会"成功"（数据进了内核缓冲就没了），这类失败
/// 重试也测不出来。真正兜底的是"重投时按重复消息处理并 ack"，
/// 见 `note_decrypt_fail` 的 Duplicate 分支——重投因此变成自愈而不是刷错误。
async fn ack_or_warn(conn: &mut Conn, qid: &str, mid: &str) {
    let ids = [mid.to_string()];
    for attempt in 1..=2u32 {
        match conn.ack(qid, &ids).await {
            Ok(()) => return,
            Err(e) if attempt == 1 => {
                warn(format!("发送 ack 失败，重试一次: {e:#}"));
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => {
                rx_counters().lock().expect("计数锁中毒").ack_failed += 1;
                warn(format!("发送 ack 失败（这条消息会在下次重连时重投）: {e:#}"));
            }
        }
    }
}

/// 设备级互斥锁。
///
/// 为什么需要：`recv_once` 会**长时间**等待（Android 上前台服务给 30 秒）。
/// 如果它在等待前就把 Device 读进内存、等完再写回，就会发生典型的丢失更新：
/// 用户在这 30 秒里加了联系人 / 发了消息，回写时被旧状态整个盖掉，
/// 表现为"刚加的人突然没了""刚发出去的消息对方收不到"。
///
/// 修法两条一起上：
/// 1. 等待期间**不持有** Device（收到消息后才 load）；
/// 2. 所有"读-改-写"都过同一把按目录分的锁，窗口缩到微秒级。
fn dev_locks() -> &'static Mutex<HashMap<String, std::sync::Arc<Mutex<()>>>> {
    static L: OnceLock<Mutex<HashMap<String, std::sync::Arc<Mutex<()>>>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 解密失败时尽量给出一点线索（不含任何密钥材料）
fn dh_probe(w: &Wire) -> String {
    format!("{}字节/类型{}", w.ct.len(), w.mt)
}

fn dev_lock(dir: &Path) -> std::sync::Arc<Mutex<()>> {
    let k = dir.display().to_string();
    let mut g = dev_locks().lock().expect("设备锁表中毒");
    g.entry(k).or_insert_with(|| std::sync::Arc::new(Mutex::new(()))).clone()
}

// ---------------------------------------------------------------- 本地状态

fn load_self(dir: &Path) -> Result<SelfState> {
    let p = dir.join(SELF_FILE);
    if !p.exists() {
        return Ok(SelfState::default());
    }
    Ok(serde_json::from_slice(&std::fs::read(&p)?)?)
}

fn save_self(dir: &Path, s: &SelfState) -> Result<()> {
    // tmp+rename：这两个文件有不持锁的读者（contacts()/self_queue/收消息路径），
    // 直接覆盖写到一半被读到就是半个 JSON——UI 瞬间"联系人全没了"、通话发不起。
    // 照抄 save_turn 的原子写法。
    let p = dir.join(SELF_FILE);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

fn load_contacts(dir: &Path) -> Result<Vec<Contact>> {
    let p = dir.join(CONTACTS_FILE);
    if !p.exists() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_slice(&std::fs::read(&p)?)?)
}

fn save_contacts(dir: &Path, c: &[Contact]) -> Result<()> {
    // 同 save_self：读者无锁，必须原子替换
    let p = dir.join(CONTACTS_FILE);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(c)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

fn find_contact(dir: &Path, who: &str) -> Result<Contact> {
    load_contacts(dir)?
        .into_iter()
        .find(|c| c.name == who || c.dh == who || c.pk == who)
        .ok_or_else(|| anyhow!("找不到联系人 {who}"))
}

// ---------------------------------------------------------------- 对外 API

/// 创建身份。
///
/// 身份从 BIP39 恢复短语派生（128 位熵 → 12 词 → PBKDF2 → 32 字节种子），
/// 熵加密存在设备目录里，用户随时可以在设置里查看恢复短语。
/// 这样"删掉 App = 身份没了"不再是死局。
pub fn init(dir: &str, name: Option<&str>) -> Result<String> {
    let dir = PathBuf::from(dir);
    if dir.join("device.key").exists() {
        return Err(anyhow!("这个目录里已经有一个身份了"));
    }
    std::fs::create_dir_all(&dir)?;
    let entropy = secure::mnemonic::new_entropy();
    let dev = Device::from_entropy(&entropy);
    dev.save(&dir)?;
    secure::Device::store_phrase_entropy(&dir, &entropy)?;
    let mut me = load_self(&dir)?;
    me.name = name.map(|s| s.to_string());
    save_self(&dir, &me)?;
    Ok(dev.id())
}

/// 查看恢复短语（12 个词，空格分隔）。
///
/// 调用方（UI）负责先警告用户"这串词就是身份本体，截图/粘贴到聊天里等于交出身份"。
pub fn backup_phrase(dir: &str) -> Result<String> {
    let dir = Path::new(dir);
    let e = secure::Device::read_phrase_entropy(dir)?;
    Ok(secure::mnemonic::entropy_to_mnemonic(&e))
}

/// 用恢复短语恢复身份到新目录。
///
/// 恢复的是**身份本身**（Ed25519 公钥，联系人认的就是它）。
/// 联系人列表、会话、队列都不在短语里——恢复后要重新生成邀请码互加，
/// 但对方那边能看到"同一个 ID 回来了"，不再是陌生人。
pub fn restore(dir: &str, phrase: &str, name: Option<&str>) -> Result<String> {
    let dir = PathBuf::from(dir);
    if dir.join("device.key").exists() {
        return Err(anyhow!("这个目录里已经有一个身份了，恢复请用空目录"));
    }
    // 归一：统一小写、压缩空白，容忍用户从记事本粘贴时带的换行
    let norm = phrase.split_whitespace().collect::<Vec<_>>().join(" ");
    let entropy = secure::mnemonic::mnemonic_to_entropy(&norm)?;
    let dev = Device::from_entropy(&entropy);
    std::fs::create_dir_all(&dir)?;
    dev.save(&dir)?;
    secure::Device::store_phrase_entropy(&dir, &entropy)?;
    let mut me = load_self(&dir)?;
    me.name = name.map(|s| s.to_string());
    save_self(&dir, &me)?;
    Ok(dev.id())
}

/// 从 Mainline DHT 自动发现中继地址（BEP5 `get_peers`）。
///
/// 返回形如 `ws://1.2.3.4:8443` 的候选列表，**不做可达性验证**——
/// 调用方应当拿到之后自己探测（`relay_probe`）再决定用哪个。
///
/// 几个设计上的取舍：
///
/// - **只发现、不决定**。这里不碰本机已有配置，纯粹返回候选。
///   是否采用、要不要切换中继，由上层决定（`set_relay` 才会动状态）。
/// - **节点表缓存在设备目录**。DHT 的 bootstrap 域名在国内普遍被污染
///   （实测 `router.utorrent.com` 会被解析到 Facebook 的 IP 段），
///   所以首轮成功后要把节点表存下来，下次不再依赖域名。
/// - **超时是硬约束**。这里给默认 25 秒：手机端冷启动可以接受，
///   再长用户会以为卡死。查不到就返回空，**绝不阻塞主流程**。
pub fn discover_relays(dir: &str, timeout_secs: u64) -> Result<Vec<String>> {
    let dir = PathBuf::from(dir);
    let cache = dir.join("dht-nodes.json");
    let target = dht::service_key();
    let dur = std::time::Duration::from_secs(timeout_secs.clamp(5, 90));
    let d = rt().block_on(async move { dht::discover(&cache, target, dur).await });

    // 一行日志交代清楚卡在哪：种子=0 是 DNS/网络，节点=0 是 UDP 出不去，
    // 都大于 0 但没有 peer 是时间不够或记录不在附近。
    // Rust 的 stderr 在 Android 上会被丢弃，所以走 warn() → 告警队列 → logcat。
    warn(format!(
        "DHT 发现：种子 {} 个，爬到 {} 个节点，命中 {} 个中继（预算 {}s）",
        d.seeds,
        d.nodes,
        d.peers.len(),
        dur.as_secs()
    ));
    if d.seeds == 0 {
        return Err(anyhow!("解析不到任何 DHT 引导节点（DNS 或网络不可用）"));
    }
    if d.nodes == 0 {
        return Err(anyhow!("DHT 节点全部无响应（UDP 出站可能被封）"));
    }
    let peers = d.peers;

    // 端口是对方公布的服务端口；relay 的 accept 不校验 path，连根路径即可。
    // 这里只给 ws://，wss:// 由上层根据是否配了 TLS 前缀再包一层。
    let mut out: Vec<String> = peers
        .into_iter()
        .map(|(ip, port)| match ip {
            std::net::IpAddr::V4(v4) => format!("ws://{v4}:{port}"),
            std::net::IpAddr::V6(v6) => format!("ws://[{v6}]:{port}"),
        })
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

// ---------------------------------------------------------------- TURN 凭据

/// 中继签发的临时 TURN 凭据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnCreds {
    /// 形如 `turn:1.2.3.4:3478`
    pub url: String,
    pub user: String,
    pub pass: String,
    /// 有效时长（秒）
    pub ttl: u64,
    /// 过期时刻（unix 秒）
    pub expires_at: u64,
}

impl TurnCreds {
    /// 剩余可用秒数；已过期返回 0
    pub fn remaining(&self) -> u64 {
        self.expires_at.saturating_sub(proto::now_secs())
    }
}

const TURN_FILE: &str = "turn.json";

fn load_turn(dir: &Path) -> Option<TurnCreds> {
    let txt = std::fs::read_to_string(dir.join(TURN_FILE)).ok()?;
    serde_json::from_str(&txt).ok()
}

fn save_turn(dir: &Path, c: &TurnCreds) -> Result<()> {
    let p = dir.join(TURN_FILE);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(c)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

// ---------------------------------------------------------------- 多中继

/// 按顺序挑第一个能连上的中继，返回连接和实际用到的地址。
///
/// 为什么要按顺序试、而不是轮流或随机：列表首位是"最近一次用成功的"（见
/// `promote`），命中率最高，正常情况只连一次；只有它挂了才往下走。
///
/// **只能用于"哪个实例都能干"的事**（投递、索取 TURN 凭据）。
/// 收消息不能用它——见 `a_recv_once` 里的说明。
async fn connect_first(relays: &[String]) -> Result<(Conn, String)> {
    let mut last: Option<anyhow::Error> = None;
    for r in relays {
        let addr = r.trim();
        if addr.is_empty() {
            continue;
        }
        match Conn::connect(addr).await {
            Ok(c) => return Ok((c, addr.to_string())),
            Err(e) => {
                warn(format!("中继连不上，换下一个: {addr}（{e:#}）"));
                last = Some(e);
            }
        }
    }
    Err(match last {
        Some(e) => e,
        None => anyhow!("没有可用的中继地址"),
    })
}

/// 把一串密文投进 `qid` 的队列，返回真正投成功的中继地址。
///
/// 注意这里**连接成功还要看 `put` 结果**：连上了但 `put` 回 `no_queue`，
/// 说明那个中继上根本没有他的队列（队列是单实例本地的，不跨机器共享），
/// 必须继续试下一个。这正是联系人记多个中继的用途。
async fn put_first(relays: &[String], qid: &str, wires: &[Wire]) -> Result<String> {
    let mut last: Option<anyhow::Error> = None;
    for r in relays {
        let addr = r.trim();
        if addr.is_empty() {
            continue;
        }
        let mut conn = match Conn::connect(addr).await {
            Ok(c) => c,
            Err(e) => {
                warn(format!("中继连不上，换下一个: {addr}（{e:#}）"));
                last = Some(e);
                continue;
            }
        };
        let mut delivered = true;
        for w in wires {
            if let Err(e) = conn.put(qid, w).await {
                warn(format!("投递失败，换下一个中继: {addr}（{e:#}）"));
                last = Some(e);
                delivered = false;
                break;
            }
        }
        if delivered {
            return Ok(addr.to_string());
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("没有可用的中继地址")))
}

/// 把实际投成功的中继提到联系人列表首位，返回是否改动过。
///
/// 自愈：某次撞上死掉的首位中继、退到备用成功之后，下次直接从能用的那个开始，
/// 不用每次都先撞一遍墙。只重排不增删——`relays` 的语义是
/// "他的队列可能在这几个地方"，多留一个是资产。
fn promote(relays: &mut Vec<String>, used: &str) -> bool {
    let Some(pos) = relays.iter().position(|r| r.trim() == used) else {
        return false;
    };
    if pos == 0 {
        return false;
    }
    let r = relays.remove(pos);
    relays.insert(0, r);
    true
}

/// 投递成功后把中继顺序落到磁盘。写盘失败只告警：消息已经投出去了，
/// 不该因为记不住"下次先试哪个"而让上层以为发送失败。
fn promote_contact_relay(dir: &Path, dh: &str, used: &str) {
    let Ok(mut cs) = load_contacts(dir) else {
        return;
    };
    let Some(c) = cs.iter_mut().find(|c| c.dh == dh) else {
        return;
    };
    if promote(&mut c.relays, used) {
        let order = c.relays.clone();
        if let Err(e) = save_contacts(dir, &cs) {
            warn(format!("记录中继顺序失败（不影响已投递的消息）: {e:#}"));
        } else {
            warn(format!("该联系人改用中继 {used}，新顺序 {order:?}"));
        }
    }
}

/// 向当前中继索取一组临时 TURN 凭据（不读缓存）。
///
/// 为什么要"按需索取"而不是写死在 App 里：
/// 把固定账号密码硬编码等于把 TURN 变成公开免费中继，必被滥用；
/// 而 DHT 的 BEP5 记录只有 6 字节（IP+端口），根本放不下凭据。
/// 走已经建立的 WS 连接，既能下发也能按账号限速 / 停用。
pub fn fetch_turn_creds(dir: &str) -> Result<TurnCreds> {
    let dir = PathBuf::from(dir);
    let me = load_self(&dir)?;
    if me.relays.is_empty() {
        return Err(anyhow!("还没有配置中继，无法索取 TURN 凭据"));
    }
    // 凭据是"中继自己的 TURN"签发的，连上任何一个都能拿到可用的，所以这里可以换着试。
    // 注意：不能把成功的中继提到 me.relays 首位——那里的首位有别的含义（见 a_recv_once）。
    let creds = rt().block_on(async {
        let (mut c, _relay) = connect_first(&me.relays).await?;
        c.get_turn().await
    })?;
    save_turn(&dir, &creds)?;
    Ok(creds)
}

/// 取一组可用的 TURN 凭据：缓存剩余时间不足 5 分钟就重新索取。
///
/// 网络失败但有旧缓存（哪怕快过期）时**仍然返回旧缓存**：
/// 通话前为了拿凭据而阻塞十几秒，比用一个快过期的凭据更糟。
pub fn turn_creds(dir: &str) -> Option<TurnCreds> {
    let dir = PathBuf::from(dir);
    let cached = load_turn(&dir);
    if let Some(c) = &cached {
        if c.remaining() > 300 {
            return Some(c.clone());
        }
    }
    match fetch_turn_creds(dir.to_str()?) {
        Ok(c) => Some(c),
        Err(e) => {
            warn(format!("索取 TURN 凭据失败，回退到缓存（如有）: {e:#}"));
            cached.filter(|c| c.remaining() > 0)
        }
    }
}

/// 探测一个中继：完成 WebSocket 握手并计时，返回毫秒数。
///
/// 用于首次启动的中继选择页：只验证"连得上"，不建队列不投递，
/// 探测对中继状态没有副作用。
pub fn relay_probe(url: &str) -> Result<u64> {
    let start = std::time::Instant::now();
    rt().block_on(async {
        // 必须有硬超时。`connect_async` 自己不带超时：探测一个不可达的地址
        // （防火墙丢 SYN）会一直挂着，直到操作系统放弃——安卓上约两分钟。
        // 表现是"自动发现"里三条结果一直显示 Probing…，界面像卡死。
        let (ws, _) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio_tungstenite::connect_async(url),
        )
        .await
        .map_err(|_| anyhow!("探测 {url} 超时（5 秒）"))?
        .with_context(|| format!("连不上中继 {url}"))?;
        // 连接留一句关闭握手就走，别占着中继的连接槽
        let mut ws = ws;
        let _ = ws.close(None).await;
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(start.elapsed().as_millis() as u64)
}

pub fn identity(dir: &str) -> Result<String> {
    Ok(Device::load(Path::new(dir))?.id())
}

/// 生成邀请码。第一次调用会先在中继上申请队列。
pub fn invite(dir: &str, relay: &str) -> Result<String> {
    let dir = PathBuf::from(dir);
    let lock = dev_lock(&dir);
    let _guard = lock.lock().expect("设备锁中毒");
    let mut dev = Device::load(&dir)?;
    let mut me = load_self(&dir)?;

    let qid = match me.qid.clone() {
        Some(q) => q,
        None => {
            let q = rt().block_on(async {
                let mut c = Conn::connect(relay).await?;
                c.new_queue().await
            })?;
            me.qid = Some(q.clone());
            if !me.relays.iter().any(|r| r == relay) {
                me.relays.push(relay.to_string());
            }
            save_self(&dir, &me)?;
            q
        }
    };

    let b = dev.bundle();
    // 一次性密钥被消耗，必须立刻落盘
    dev.save(&dir)?;

    let inv = identity::Invite::new(b.pk, b.dh, b.otk, qid, me.relays.clone())
        .map_err(|e| anyhow!(e))?;
    let inv = match &me.name {
        Some(n) => inv.with_name(n.clone()),
        None => inv,
    };
    Ok(inv.encode())
}

pub fn add(dir: &str, code: &str, alias: Option<&str>) -> Result<String> {
    let dir = PathBuf::from(dir);
    let inv = identity::Invite::decode(code).map_err(|e| anyhow!(e))?;
    let name = alias
        .map(|s| s.to_string())
        .or_else(|| inv.name.clone())
        .unwrap_or_else(|| inv.pk.chars().take(6).collect());

    let lock = dev_lock(&dir);
    let _guard = lock.lock().expect("设备锁中毒");
    let mut dev = Device::load(&dir)?;
    dev.start_session(&inv.dh, &inv.otk)?;
    dev.save(&dir)?;

    let mut cs = load_contacts(&dir)?;
    // 同一个人的判定：pk 相同**或** dh 相同。
    // pk 是 Ed25519 身份（12 词恢复后不变），dh 是 X25519 会话密钥（重装会换新）。
    // 只按 dh 去重的话，对方恢复身份后重新出码，这里会追加第二个条目，
    // 旧条目指向已废弃的队列——投进去的消息永远无人消费（实测踩过：
    // 同一个人分裂成新旧两个联系人，旧条目发消息静默丢进黑洞）。
    // 信任级别与首次添加相同：邀请码本身就是带外交付的一次性凭据。
    let old_alias = cs
        .iter()
        .find(|c| c.pk == inv.pk || c.dh == inv.dh)
        .and_then(|c| c.alias.clone())
        // 已有备注优先（重新扫码不该抹掉用户起的名字）；没有才用这次传入的。
        // 早先这里直接用 old_alias——首次添加带 alias 时会被静默丢弃。
        .or_else(|| alias.map(|s| s.to_string()));
    cs.retain(|c| c.pk != inv.pk && c.dh != inv.dh);
    // 重名加后缀：两个人都叫 "Bob" 时必须并存，否则 find_contact 按名命中
    // 第一个，发消息静默走错人（add_from_card 路径早有这套，这里补齐）。
    let mut final_name = name.clone();
    let mut i = 2;
    while cs.iter().any(|c| c.name == final_name) {
        final_name = format!("{name} {i}");
        i += 1;
    }
    cs.push(Contact {
        name: final_name.clone(),
        pk: inv.pk,
        dh: inv.dh,
        qid: inv.qid,
        relays: inv.relays,
        alias: old_alias,
    });
    save_contacts(&dir, &cs)?;
    Ok(final_name)
}

pub fn send(dir: &str, to: &str, text: &str) -> Result<String> {
    send_kind(dir, to, secure::kind::TEXT, text, None)
}

/// 发一条阅后即焚的文字消息。
///
/// 焚毁参数放在 body（`{"burn":秒}`）里随密文传输，kind 仍是 "text"：
/// 旧客户端不认识 body 就当普通消息显示，不会崩；中继看到的和普通文本没区别。
/// 计时由**接收端本地**负责（首次展示起算），不依赖发送方在线。
pub fn send_burn(dir: &str, to: &str, text: &str, burn_secs: u64) -> Result<String> {
    let body = format!("{{\"burn\":{burn_secs}}}");
    send_kind(dir, to, secure::kind::TEXT, text, Some(&body))
}

/// 确保本设备在中继上有一个**自己的**队列（收消息的唯一入口），返回队列 ID。
///
/// 为什么需要这个：队列是中继分配的，而此前只有 `invite` 会去申请。
/// 于是"只加了别人、自己没生成过邀请码"的设备根本没有收件地址——
/// 别人回不了他，他也收不到任何消息。这类设备在界面上看起来一切正常，
/// 实际是单向的哑终端。
///
/// 一旦从邀请码/引导流程里学到可用中继，就把队列补上。
/// 已经有了就只补充中继列表（多中继时互为备份）。
pub fn ensure_queue(dir: &str, relay: &str) -> Result<String> {
    let relay = relay.trim();
    if relay.is_empty() {
        return Err(anyhow!("没有中继地址，无法建立队列"));
    }
    let dir = PathBuf::from(dir);
    let lock = dev_lock(&dir);
    let _guard = lock.lock().expect("设备锁中毒");

    let mut me = load_self(&dir)?;
    if let Some(q) = me.qid.clone() {
        if !me.relays.iter().any(|r| r == relay) {
            me.relays.push(relay.to_string());
            save_self(&dir, &me)?;
        }
        return Ok(q);
    }

    let q = rt().block_on(async {
        let mut c = Conn::connect(relay).await?;
        c.new_queue().await
    })?;
    me.qid = Some(q.clone());
    if !me.relays.iter().any(|r| r == relay) {
        me.relays.push(relay.to_string());
    }
    save_self(&dir, &me)?;
    Ok(q)
}

/// 回邀：把"刚刚给我发过消息的对方"加为联系人，从而能回复他。
///
/// 和 [`add`] 的本质区别在**信任锚点**：
/// `add` 有邀请码，一次性密钥保证"只有拿到码的人才能加"；
/// 回邀没有这一环，所以只能认加密层已确认的 `dh`——
/// "能发来一条我解得开的加密消息"本身就是"这个 dh 是真的"的证明，
/// 而 `name` / `qid` / `relays` 全是从加密载荷里自报的，只能当路由线索：
/// 猜错了最多是投不出去，不可能把消息发给别人（加密目标由 `dh` 决定，不由这些字段决定）。
pub fn add_from_card(
    dir: &str,
    name: &str,
    dh: &str,
    pk: &str,
    qid: &str,
    relays: &[String],
) -> Result<String> {
    let dir = PathBuf::from(dir);
    let qid = qid.trim().to_string();
    if qid.is_empty() {
        return Err(anyhow!("对方没给回信地址（队列 ID），加不了"));
    }
    let relays: Vec<String> = relays
        .iter()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .collect();
    if relays.is_empty() {
        return Err(anyhow!("对方没给可用的中继地址，加不了"));
    }

    let lock = dev_lock(&dir);
    let _guard = lock.lock().expect("设备锁中毒");

    // 信任闸：没有会话就说明我从没解开过他任何一条消息
    let dev = Device::load(&dir)?;
    if !dev.has_session(dh) {
        return Err(anyhow!("还没有与这个身份建立加密会话，不能加为联系人"));
    }

    let mut cs = load_contacts(&dir)?;
    // 已经是联系人：只刷新路由，别改名（用户可能自己起过别名）。
    // 匹配放宽到 pk：对方重装恢复身份后 dh 会换，但 pk 不变——
    // 只按 dh 找会漏掉旧条目，让它继续指向死队列。
    if let Some(existing) = cs
        .iter_mut()
        .find(|c| c.dh == dh || (!pk.is_empty() && c.pk == pk))
    {
        existing.qid = qid;
        existing.relays = relays;
        if existing.pk.is_empty() && !pk.is_empty() {
            existing.pk = pk.to_string();
        }
        // 同身份换 dh（重装恢复）：把条目迁到新会话上，旧 dh 作废
        existing.dh = dh.to_string();
        let n = existing.name.clone();
        save_contacts(&dir, &cs)?;
        return Ok(n);
    }

    let base = if name.trim().is_empty() {
        dh.chars().take(8).collect::<String>()
    } else {
        name.trim().to_string()
    };
    // 重名不覆盖：两个人都叫"小明"时必须并存，否则会串会话
    let mut final_name = base.clone();
    let mut i = 2;
    while cs.iter().any(|c| c.name == final_name) {
        final_name = format!("{base} {i}");
        i += 1;
    }

    cs.push(Contact {
        name: final_name.clone(),
        pk: pk.to_string(),
        dh: dh.to_string(),
        qid,
        relays,
        alias: None,
    });
    save_contacts(&dir, &cs)?;
    Ok(final_name)
}

/// 发一条控制类消息（通话信令等）。`body` 是 JSON 文本。
///
/// 和文本走**同一个加密会话、同一个队列**，所以不需要为中继做任何额外配置。
pub fn send_control(dir: &str, to: &str, kind: &str, body: &str) -> Result<()> {
    send_kind(dir, to, kind, "", Some(body)).map(|_| ())
}

/// 批量发送多条同 kind 消息。`bodies` 每个元素就是一条消息的 body（JSON 文本）。
///
/// 为什么必须存在：文件分块传输每块都是一条独立消息，若逐块调 `send_control`，
/// 每块都要一次 WS 握手 + 设备锁 + 会话 load/save，大文件会慢到不可用。
/// 这里一次设备锁 + 一次连接把所有块顺序推过去。
///
/// 返回每条消息的载荷 ID（与 `bodies` 一一对应）。撤回引用（rid）用的是
/// 调用方自己放进 body 的业务 ID（如文件的 fid），不用这里的值。
pub fn send_batch(dir: &str, to: &str, kind: &str, bodies: &[String]) -> Result<Vec<String>> {
    if bodies.is_empty() {
        return Ok(Vec::new());
    }
    let dir = PathBuf::from(dir);
    let lock = dev_lock(&dir);
    let _guard = lock.lock().expect("设备锁中毒");
    let mut dev = Device::load(&dir)?;
    let mut me = load_self(&dir)?;
    let c = find_contact(&dir, to)?;

    // "每条消息都必须能回信"：块消息和普通消息一样要带 reply_qid。
    // send_kind 有这个兜底，这里漏了——新装设备直接发文件时，
    // 对端收到的所有块都无法回信/加好友（对端 UI 显示没有回信线索）。
    if me.qid.is_none() {
        let fb = c.relays.first().cloned().unwrap_or_default();
        ensure_own_queue(&dir, &mut me, &fb)?;
    }

    // 先全部加密（推进棘轮），再走网络——加密是纯 CPU，别占着连接时间
    let mut wires = Vec::with_capacity(bodies.len());
    let mut ids = Vec::with_capacity(bodies.len());
    for b in bodies {
        let mid = proto::new_message_id();
        ids.push(mid.clone());
        let w = dev.encrypt(
            &c.dh,
            &Payload {
                v: PAYLOAD_VERSION,
                id: mid,
                from: dev.id(),
                ts: proto::now_secs(),
                text: String::new(),
                reply_qid: me.qid.clone(),
                reply_relays: self_relays(&me),
                self_name: me.name.clone(),
                kind: kind.to_string(),
                body: Some(b.clone()),
            },
        )?;
        wires.push(w);
    }
    // 棘轮已在加密时推进，必须在任何密文离开本机之前落盘：
    // put_first 中途失败时前 k 块已进对端队列，若此刻磁盘还是旧棘轮，
    // 下次发送（哪怕只是普通文本）会用已被对端消费过的密钥加密新内容——
    // 对端要么判成重复静默吞掉（消息凭空消失），要么 MAC 失败无限重投。
    dev.save(&dir)?;

    if c.relays.is_empty() {
        return Err(anyhow!("联系人没有可用的中继地址"));
    }
    let used = rt().block_on(put_first(&c.relays, &c.qid, &wires))?;
    promote_contact_relay(&dir, &c.dh, &used);

    // 投递成功后的这次 save 是幂等冗余（棘轮在投递前已存），留着兜底
    // 将来有人在中间插入设备状态修改。
    dev.save(&dir)?;
    Ok(ids)
}

/// 补一个收件队列，并写回 `self.json`。
///
/// 调用方必须已经持有设备锁（`send_kind` / `ensure_queue` 都在锁内），
/// 所以这里不碰 `dev_lock`——它对同一目录不可重入。
///
/// 候选顺序：自己记过的中继 → 本次要投递的那台（`fallback`）。
///
/// 为什么要带上 `fallback`：消息正要往他那台发，说明那台**此刻是通的**；
/// 而自己记的可能是早就下线的旧地址（换过中继、或那台中继停了）。
/// 真机上就出现过这种情况：手机端配的中继失效，于是 `ensureQueue` 一直失败、
/// 队列迟迟建不起来，发出去的消息全都不带回信地址。
///
/// 全部候选都失败才报错——宁可让用户看到"发不出去"，也不要悄悄发一条对方回不了的。
fn ensure_own_queue(dir: &Path, me: &mut SelfState, fallback: &str) -> Result<String> {
    let mut cands: Vec<String> = me.relays.clone();
    let fb = fallback.trim();
    if !fb.is_empty() && !cands.iter().any(|r| r == fb) {
        cands.push(fb.to_string());
    }
    if cands.is_empty() {
        return Err(anyhow!("还没有配置中继，无法建立收件队列"));
    }

    let mut last: Option<anyhow::Error> = None;
    for relay in &cands {
        let made = rt().block_on(async {
            let mut c = Conn::connect(relay).await?;
            let q = c.new_queue().await?;
            c.sub(&q).await?;
            Ok::<String, anyhow::Error>(q)
        });
        match made {
            Ok(q) => {
                me.qid = Some(q.clone());
                if !me.relays.iter().any(|r| r == relay) {
                    me.relays.push(relay.clone());
                }
                save_self(dir, me)?;
                warn(format!("已补建收件队列 {q}（{relay}）"));
                return Ok(q);
            }
            Err(e) => {
                warn(format!("在中继 {relay} 上建队列失败: {e:#}"));
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("所有中继都建不了队列")))
}

fn send_kind(dir: &str, to: &str, kind: &str, text: &str, body: Option<&str>) -> Result<String> {
    let dir = PathBuf::from(dir);
    // 发送也要占设备锁：加密会推进棘轮，并发的收/发互相覆盖会让会话状态错乱
    let lock = dev_lock(&dir);
    let _guard = lock.lock().expect("设备锁中毒");
    let mut dev = Device::load(&dir)?;
    let mut me = load_self(&dir)?;
    let c = find_contact(&dir, to)?;

    // 发消息前先确保自己有队列。
    //
    // 为什么必须在这里兜：载荷里的 `reply_qid` 直接取自 `me.qid`，没有队列
    // 就等于告诉对方"我没法收信，别回了"——界面上显示成
    // "这条消息没有回信线索（可能来自旧版客户端）"，可实际上原因在我们这边。
    // 这个状态下对方无论做什么都回不了，只能干等，而且两边都不知道该修哪里。
    //
    // 什么情况下会没有队列：从老版本升上来（那时队列只在生成邀请码时才申请）、
    // 或在离线时完成首启/加好友（`ensureQueue` 失败后没人重试）。
    // 这些都是常态，不能靠"用户去设置里走一遍引导"来解决。
    if me.qid.is_none() {
        // 用"正要投递到的这台"当中继候选：它是此刻唯一确定可达的
        let fb = c.relays.first().cloned().unwrap_or_default();
        ensure_own_queue(&dir, &mut me, &fb)?;
    }

    // 载荷 ID 由发送方生成、随消息传输：接收方存下来后，撤回消息靠它
    // 引用原消息（两条设备各自的本地行号对不上，只有这个 id 两边一致）
    let mid = proto::new_message_id();
    let w = dev.encrypt(
        &c.dh,
        &Payload {
            v: PAYLOAD_VERSION,
            id: mid.clone(),
            from: dev.id(),
            ts: proto::now_secs(),
            text: text.to_string(),
            reply_qid: me.qid.clone(),
            reply_relays: self_relays(&me),
            self_name: me.name.clone(),
            kind: kind.to_string(),
            body: body.map(|b| b.to_string()),
        },
    )?;

    // 棘轮已在加密时推进，必须在密文离开本机之前落盘（与 send_batch 同一
    // 道理）：put_first 中途失败时这条消息可能已进对方队列，磁盘若还停在
    // 旧棘轮，下一条消息（哪怕只是普通文本）会被对端判重复吞掉或 MAC 永败。
    // send_kind 是单条主路径，0.15.6 只修了批量，这条漏到二轮审计才补上。
    dev.save(&dir)?;

    if c.relays.is_empty() {
        return Err(anyhow!("联系人没有可用的中继地址"));
    }
    let used = rt().block_on(put_first(&c.relays, &c.qid, &[w]))?;
    promote_contact_relay(&dir, &c.dh, &used);

    // 投递成功后的这次 save 是幂等冗余，兜底将来有人在中间插入设备状态修改
    dev.save(&dir)?;
    Ok(mid)
}

pub fn contacts(dir: &str) -> Result<Vec<Contact>> {
    load_contacts(Path::new(dir))
}

pub fn self_queue(dir: &str) -> Result<Option<String>> {
    Ok(load_self(Path::new(dir))?.qid)
}

pub fn local_name(dir: &str) -> Result<Option<String>> {
    Ok(load_self(Path::new(dir))?.name)
}

/// 安全码：双方身份公钥算出的 60 位数字，用户带外念一遍就能确认没被中间人。
///
/// 参数就是两个 Ed25519 身份 ID（= `Device::id()` = 邀请码里的 pk），顺序无关。
pub fn safety_code(a_pk: &str, b_pk: &str) -> Result<String> {
    if a_pk.trim().is_empty() || b_pk.trim().is_empty() {
        anyhow::bail!("缺少身份公钥，无法计算安全码");
    }
    Ok(secure::safety_number(a_pk.trim(), b_pk.trim()))
}

/// 收一条消息。超时返回 `Ok(None)`，让上层可以继续循环。
pub fn recv_once(dir: &str, timeout_secs: u64) -> Result<Option<Incoming>> {
    let dir = PathBuf::from(dir);
    rt().block_on(async move { a_recv_once(&dir, timeout_secs).await })
}

/// 盲 ack 清空自己的中继收件队列（紧急清除用），返回 ack 掉的条数。
///
/// **盲 ack**：对推来的每条消息直接 ack，**不做任何解密**——紧急清除的
/// 目标是"不留痕"，内容是什么不但不重要，读进来反而是负担。
///
/// 用**独立连接**，不复用收消息的缓存连接：收消息循环可能正拿着缓存
/// 连接在抢消息，两边抢会漏 ack。
///
/// 结束条件：连续 3 个窗口（各 2 秒）没消息，视为队列已空。
/// 中继零改动——ack 本来就是删队列的唯一方式。
pub fn purge_queue(dir: &str) -> Result<u64> {
    let dir = PathBuf::from(dir);
    rt().block_on(async {
        let me = load_self(&dir)?;
        let qid = me
            .qid
            .clone()
            .ok_or_else(|| anyhow!("还没有自己的队列，无法清除"))?;
        let relay = me
            .relays
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("还不知道连哪个中继"))?;

        let mut conn = Conn::connect(&relay).await?;
        // 订阅瞬间推来的积压第一条同样盲 ack（sub 的注释说明为什么不能吞）：
        // "不留痕"是这个函数的唯一目标，丢一条就等于留了一条。
        let mut acked: u64 = 0;
        if let Some((mid, _)) = conn.sub(&qid).await? {
            if conn.ack(&qid, &[mid]).await.is_ok() {
                acked += 1;
            }
        }
        let mut idle: u32 = 0;
        // 总量上限：正常会话队列撑死几千条；持续有人投递时（例如对面正在
        // 疯狂发消息）没有上限的话"紧急清除"会被无限拖住。
        let mut guard: u32 = 0;
        const PURGE_LIMIT: u32 = 20_000;
        while idle < 3 && guard < PURGE_LIMIT {
            match conn.recv_msg(Duration::from_secs(2)).await {
                Ok((mid, _body)) => {
                    // _body 整个丢弃：不解码、不进内存里的任何持久结构
                    idle = 0;
                    guard += 1;
                    let ids = [mid];
                    if conn.ack(&qid, &ids).await.is_ok() {
                        acked += 1;
                    }
                }
                Err(RecvErr::Timeout) => {
                    idle += 1;
                    // 保活探测，顺便确认连接没死（死连接上 ack 全是假的）。
                    // 等 Pong 期间中继可能恰好推来数据——那也是要清除的消息，
                    // 丢弃就违背了"不留痕"，必须 ack 掉。
                    match conn.probe().await {
                        Err(_) => break,
                        Ok(Some((mid, _))) => {
                            guard += 1;
                            if conn.ack(&qid, &[mid]).await.is_ok() {
                                acked += 1;
                            }
                        }
                        Ok(None) => {}
                    }
                }
                Err(RecvErr::Fatal(_)) => break,
            }
        }
        Ok(acked)
    })
}

// ---------------------------------------------------------------- 中继 / 联系人管理

/// 切换中继地址。
///
/// **为什么不能只在 App 设置里改个字符串**：中继地址在磁盘上有三份——
/// self.json 的 relays、contacts.json 里每个联系人的 relays、以及 App 的
/// SharedPreferences。只改 SP 的话，发消息仍按联系人里的旧地址投，表现为
/// "显示已保存但怎么都不通"（2026-10-22 实测踩到，最后只能清数据重装）。
///
/// 先连上新中继并 sub 自己的队列做连通性验证，通过后才落盘；失败直接报错、不改任何文件。
/// 注意：旧中继队列里未 ack 的消息**不会迁移**（中继之间没有互通协议），
/// 调用方需要在 UI 上明确告知用户。
/// 切换自己的中继：把**自己的收件队列**迁到新中继上。
///
/// 三条必须说清楚的语义（都是踩过的坑）：
///
/// 1. **只动自己的路由，不动联系人的**。
///    联系人的 `relays` 表示"他的队列在哪个中继"，与我在哪台中继毫无关系。
///    早期实现把所有联系人的 relays 全部改写成新地址，多中继场景下会让
///    所有对外发送都打到错误的中继上（`no_queue`），且界面没有任何报错。
///
/// 2. **必须在新的中继上重建队列**。
///    队列是中继实例本地的（不跨机器共享），旧 qid 在新中继上根本不存在；
///    而 `Sub` 对未知队列是静默成功的，不重建就会"看起来在线、实际收不到"。
///
/// 3. **重建后要主动通知联系人**。
///    对方记录的是我的旧 qid，不更新的话他们发的消息会堆在旧中继里没人取。
///    这里给每个联系人发一条 `route` 控制帧，把新 qid 推过去。
///
/// 返回值：通知到的联系人数（不是"改了他们的中继"）。
pub fn set_relay(dir: &str, url: &str) -> Result<usize> {
    let dir = PathBuf::from(dir);
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err(anyhow!("中继地址不能为空"));
    }
    let lock = dev_lock(&dir);
    let _guard = lock.lock().expect("设备锁中毒");
    let mut me = load_self(&dir)?;

    // 已经是这个中继就没必要重建队列（重建会换 qid，等于让所有联系人重新学一遍）
    if me.relays.first().map(|r| r == &url).unwrap_or(false) {
        return Ok(0);
    }

    // 还没有自己的队列（新装、从没生成过邀请码）：那就把"切中继"理解成
    // "在这台上建我的队列"。早先这里直接报错"请先完成首次引导"——
    // 而设置页的中继入口对新用户恰好是唯一能走的路，于是彻底死锁：
    // 不选中继就没队列，没队列就不能选中继。
    if me.qid.is_none() {
        let q = rt().block_on(async {
            let mut c = Conn::connect(&url).await?;
            let q = c.new_queue().await?;
            c.sub(&q).await?;
            Ok::<String, anyhow::Error>(q)
        })
        .map_err(|e| anyhow!("新中继不可用（{url}）: {e:#}"))?;
        me.qid = Some(q.clone());
        me.relays = vec![url.clone()];
        save_self(&dir, &me)?;
        warn(format!("已在 {url} 建立队列 {q}"));
        return Ok(0);
    }

    // 在新中继上建一个新队列：连通性验证 + 拿到新收件地址，一步完成。
    // 先建后改，失败就一个字节都不动（保持旧中继可用）。
    let new_qid = rt().block_on(async {
        let mut c = Conn::connect(&url).await?;
        let q = c.new_queue().await?;
        c.sub(&q).await?;
        Ok::<String, anyhow::Error>(q)
    })
    .map_err(|e| anyhow!("新中继不可用（{url}）: {e:#}"))?;

    let old_qid = me.qid.clone();
    me.qid = Some(new_qid.clone());
    me.relays = vec![url.clone()];
    save_self(&dir, &me)?;

    // 联系人路由一律不动！只把"我换了收件地址"这件事加密推给他们。
    //
    // 这里不能调 send_control：设备锁不可重入，而本函数已经持有它。
    // 所以就地用同一个 Device 加密；`route` 帧由接收端在 Rust 层直接消费
    // （不上浮到 UI，也不落聊天记录）。
    let mut dev = Device::load(&dir)?;
    let cs = load_contacts(&dir)?;
    let mut notified = 0usize;
    for c in &cs {
        // 没有中继记录的联系人推不到，跳过（不是错误）
        if c.relays.is_empty() {
            continue;
        }
        let body = serde_json::json!({ "qid": new_qid, "relays": [url] }).to_string();
        let mid = proto::new_message_id();
        let w = match dev.encrypt(
            &c.dh,
            &Payload {
                v: PAYLOAD_VERSION,
                id: mid,
                from: dev.id(),
                ts: proto::now_secs(),
                text: String::new(),
                reply_qid: me.qid.clone(),
                reply_relays: self_relays(&me),
                self_name: me.name.clone(),
                kind: "route".to_string(),
                body: Some(body),
            },
        ) {
            Ok(w) => w,
            // 某个联系人没有可用会话（比如刚被删过会话）：跳过，不阻断整体切换
            Err(_) => continue,
        };
        // 通知也要能容忍他那边中继挂了一个：只要有一个还认他的队列就够
        let ok = rt().block_on(put_first(&c.relays, &c.qid, &[w])).is_ok();
        if ok {
            notified += 1;
        }
    }
    dev.save(&dir)?;

    if let Some(o) = old_qid {
        warn(format!("已切换到新中继 {url}，新队列 {new_qid}（旧队列 {o} 已弃用）"));
    }
    Ok(notified)
}

/// 删除联系人：联系人列表 + Olm 会话状态一起清。
///
/// 会话状态必须删：留在 device 文件里的话，对方再来消息会被当成"已删除联系人"的消息，
/// 落在已被清空的会话里，用户完全看不到（静默丢失）。
/// 返回 (联系人是否存在, 清掉的会话条数)。
pub fn remove_contact(dir: &str, dh: &str) -> Result<(bool, usize)> {
    let dir = PathBuf::from(dir);
    let lock = dev_lock(&dir);
    let _guard = lock.lock().expect("设备锁中毒");

    let mut cs = load_contacts(&dir)?;
    let before = cs.len();
    cs.retain(|c| c.dh != dh);
    let existed = cs.len() != before;
    if existed {
        save_contacts(&dir, &cs)?;
    }

    let mut dev = Device::load(&dir)?;
    let dropped = dev.drop_sessions(dh);
    if dropped > 0 {
        dev.save(&dir)?;
    }
    Ok((existed, dropped))
}

/// 设置联系人备注名。传空串表示清除（回到对方自己的昵称）。
/// 备注只存在本机 contacts.json，不随协议传输。
pub fn set_contact_alias(dir: &str, dh: &str, alias: &str) -> Result<Option<String>> {
    let dir = PathBuf::from(dir);
    let lock = dev_lock(&dir);
    let _guard = lock.lock().expect("设备锁中毒");
    let mut cs = load_contacts(&dir)?;
    let c = cs
        .iter_mut()
        .find(|c| c.dh == dh)
        .ok_or_else(|| anyhow!("找不到联系人"))?;
    let a = alias.trim();
    c.alias = if a.is_empty() { None } else { Some(a.to_string()) };
    let out = c.alias.clone();
    save_contacts(&dir, &cs)?;
    Ok(out)
}

/// 取一条消息。**收消息只能连"我的队列所在的那个中继"，不能换着试。**
///
/// 为什么这里不像发送那样遍历 `me.relays`：
///
/// 1. `me.relays` 的首位就是"我的队列在哪台"。队列是各中继实例本地的（不跨机器
///    共享），`set_relay` 也是整表替换成 `[新地址]`，所以首位始终是队列所在处。
/// 2. 中继的 `Sub` 对**不存在的队列是静默成功的**（只回 Ok，不补投任何积压）。
///    于是"首位连不上就换下一个"会得到比失败更糟的结果：连接正常、订阅成功、
///    永远收不到消息——界面上看起来完全在线。
///
/// 真要在中继之间迁移，只能走 `set_relay`：在新中继重建队列，再把新地址加密推给
/// 所有联系人。那个动作会换 qid、会让对方重学路由，不能在一次网络抖动里自动做。
async fn a_recv_once(dir: &Path, timeout_secs: u64) -> Result<Option<Incoming>> {
    let me = load_self(dir)?;
    let qid = me
        .qid
        .clone()
        .ok_or_else(|| anyhow!("还没有自己的队列，先调用 invite"))?;
    let relay = me
        .relays
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("还不知道连哪个中继"))?;

    let key = dir.display().to_string();

    // 取出缓存的连接（锁必须在 await 之前释放）
    let cached = {
        let mut g = cache().lock().expect("缓存锁中毒");
        g.remove(&key)
    };
    let mut conn = match cached {
        Some((q, r, c)) if q == qid && r == relay => (c, None),
        _ => {
            // 连不上就说清楚"是哪台、为什么不能自动换"，否则用户只会看到一句没头没脑的
            // "连接失败"，而真正要做的是在设置里迁移中继。
            let mut c = Conn::connect(&relay).await.map_err(|e| {
                anyhow!(
                    "连不上你的中继 {relay}（收消息必须连这台，迁移中继会重建队列）: {e:#}"
                )
            })?;
            let pending = c.sub(&qid).await?;
            (c, pending)
        }
    };
    // 订阅瞬间推来的积压第一条：不能丢（见 sub 的注释）。放进本轮处理；
    // 没有才走正常的"等一条消息或空闲超时"。
    let got = match conn.1.take() {
        Some(v) => Some(v),
        None => match conn.0.recv_msg(Duration::from_secs(timeout_secs)).await {
            Ok(v) => Some(v),
            Err(RecvErr::Timeout) => match conn.0.probe().await {
                // 连接还活着：正常空闲超时
                Ok(None) => None,
                // 等心跳时恰好推来了数据：当作收到消息处理
                Ok(Some(v)) => Some(v),
                // 连接已死（半开）：不写回缓存，让下一轮重连
                Err(RecvErr::Fatal(e)) => {
                    return Err(anyhow!("收消息超时且心跳探测失败（丢弃坏连接）: {e:#}"))
                }
                Err(RecvErr::Timeout) => {
                    return Err(anyhow!("收消息超时且心跳也无响应（丢弃坏连接）"))
                }
            },
            Err(RecvErr::Fatal(e)) => {
                // 连接坏了：不写回缓存，下次重连
                return Err(e);
            }
        },
    };
    let mut conn = conn.0;

    let incoming = match got {
        Some((mid, body)) => {
            // 结构性坏帧（不是合法 Wire）按毒帧处理：ack 掉止住重投、连接回缓存。
            // 之前直接 `?` 返回：不 ack、连接也丢弃——每轮重连都收到同一条坏帧、
            // 再报错再丢连接，死循环，毒帧永远堵在队列前面（还拖慢每次重连）。
            let wire = match Wire::from_bytes(&body) {
                Ok(w) => w,
                Err(e) => {
                    warn(format!(
                        "信封不是合法的 Wire（已确认并从中继删除，来源未知）: {e:#}"
                    ));
                    ack_or_warn(&mut conn, &qid, &mid).await;
                    cache()
                        .lock()
                        .expect("缓存锁中毒")
                        .insert(key, (qid, relay, conn));
                    return Ok(None);
                }
            };
            // 到这里才读设备：等待消息的这段时间里用户可能已经改了状态
            // （加联系人、发消息都会推进棘轮），读早了就会写回旧状态把它盖掉。
            let lock = dev_lock(dir);
            let _guard = lock.lock().expect("设备锁中毒");
            let mut dev = Device::load(dir)?;
            let decrypted = dev.decrypt(None, &wire);
            // 先落盘再放锁；await 放到锁外，避免带着锁等 IO
            let saved = if decrypted.is_ok() { dev.save(dir) } else { Ok(()) };
            drop(_guard);
            match decrypted {
                Ok((dh, p)) => {
                    // 路由变更帧（对方换了中继/重建了队列）：在 Rust 层直接消费，
                    // 不产生任何聊天记录，也不上浮到 UI。
                    // 为什么必须在这里处理：否则对方换中继后我们会一直往他的旧队列发，
                    // 消息堆在旧中继里没人取，而且发送方看不到任何失败。
                    if p.kind == "route" {
                        let mut updated = false;
                        if let (Some(body), Ok(mut cs)) = (p.body.as_deref(), load_contacts(dir)) {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
                                let new_qid = v.get("qid").and_then(|x| x.as_str()).unwrap_or("");
                                let new_relays: Vec<String> = v
                                    .get("relays")
                                    .and_then(|x| x.as_array())
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|s| s.as_str())
                                            .filter(|s| !s.trim().is_empty())
                                            .map(|s| s.trim().to_string())
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                if !new_qid.is_empty() && !new_relays.is_empty() {
                                    if let Some(c) = cs.iter_mut().find(|c| c.dh == dh) {
                                        c.qid = new_qid.to_string();
                                        c.relays = new_relays;
                                        updated = save_contacts(dir, &cs).is_ok();
                                    }
                                }
                            }
                        }
                        ack_or_warn(&mut conn, &qid, &mid).await;
                        if updated {
                            warn(format!("对方 {dh} 更新了收件地址，路由已刷新"));
                        }
                        // 连接还健康：放回缓存再返回。之前直接 return 把好连接
                        // 扔了，每次 route 帧后下一轮 recv 都要白重连一次。
                        cache()
                            .lock()
                            .expect("缓存锁中毒")
                            .insert(key, (qid, relay, conn));
                        // 不递归（async fn 递归要装箱），直接当"这条不是聊天消息"返回；
                        // 上层服务是循环调用，下一轮马上继续取。
                        return Ok(None);
                    }

                    let from_name = load_contacts(dir)
                        .ok()
                        .and_then(|cs| cs.into_iter().find(|c| c.dh == dh))
                        .map(|c| c.name);
                    // clone 一份给 ack：mid 还要塞进 Incoming 供上层去做通话信令去重
                    ack_or_warn(&mut conn, &qid, &mid).await;
                    saved?;
                    Some(Incoming {
                        from_name,
                        from_id: p.from,
                        text: p.text,
                        ts: p.ts,
                        dh,
                        kind: p.kind,
                        body: p.body,
                        msg_id: mid,
                        rid: p.id,
                        self_name: p.self_name,
                        reply_qid: p.reply_qid,
                        reply_relays: merge_relays(p.reply_relays, &relay),
                    })
                }
                Err(e) => {
                    // 解不开的消息不该拖垮循环，但也绝不能悄悄丢：
                    // 最常见的两种原因是"对方重装后没有重新互加"和"一次性密钥已被用过"，
                    // 用户在界面上只会看到"对方发了消息但我没有"，所以必须留下痕迹。
                    //
                    // 能不能 ack 取决于错的类型（见 secure::FailKind）：
                    // - 重复消息：之前处理过、只是当时没 ack 掉 → **必须 ack**，
                    //   否则它永远留在中继队列（30 天 TTL），每次重连都白推一遍；
                    // - 结构坏了：永远解不开 → ack 掉止住重投；
                    // - MAC 不符 / 没会话：可能是真丢了消息 → 不 ack，留在队列里对账。
                    let kind = secure::fail_kind(&e);
                    let from = dh_probe(&wire);
                    if kind.can_ack() {
                        ack_or_warn(&mut conn, &qid, &mid).await;
                    }
                    note_decrypt_fail(kind, &from, wire.mt, &format!("{e:#}"), kind.can_ack());
                    None
                }
            }
        }
        None => None,
    };

    // 连接还活着就放回缓存，避免每次收消息都重连
    cache()
        .lock()
        .expect("缓存锁中毒")
        .insert(key, (qid, relay, conn));
    Ok(incoming)
}


// ---------------------------------------------------------------- 通话

/// 每个设备目录一个状态机。Android 上同一时刻只有一个进程在用，够用。
fn calls() -> &'static Mutex<HashMap<String, call::Call>> {
    static C: OnceLock<Mutex<HashMap<String, call::Call>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 交回给上层的通话事件。
///
/// **只包含 UI 需要的信息**：信令已经在 Rust 侧发走了，上层不用管。
#[derive(Debug, Default, Serialize)]
pub struct CallEvent {
    pub state: String,
    pub call_id: String,
    pub peer_dh: String,
    pub video: bool,
    pub reason: String,
    /// 收到 offer 时带出对方的 SDP，上层要拿它 setRemoteDescription
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_sdp: Option<String>,
    /// 收到 ICE 时带出候选，上层要 addIceCandidate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ice: Option<call::IceBody>,
    /// true = 正在重新协商（对端换网络）。上层要 setRemoteDescription 后 createAnswer。
    pub renegotiating: bool,
}

fn now() -> u64 {
    proto::now_secs()
}

/// 执行状态机产出的 `Send` 动作（真正把信令发出去），返回最后一个状态事件。
///
/// 注意：**必须在锁外调用**。发送会做文件 IO 和网络 IO，持锁会拖住整个通话流程。
fn flush_actions(dir: &str, acts: Vec<call::Action>) -> Result<CallEvent, String> {
    let mut last = CallEvent::default();
    for a in acts {
        match a {
            call::Action::Send { to_dh, kind, body } => {
                if let Err(e) = send_control(dir, &to_dh, &kind, &body) {
                    // 信令发不出去不能静默：对方会一直等着
                    let msg = format!("发送 {kind} 失败：{e:#}");
                    warn(msg.clone());
                    return Err(msg);
                }
            }
            call::Action::State { state, call_id, peer_dh, video, reason, renegotiating } => {
                last = CallEvent {
                    state,
                    call_id,
                    peer_dh,
                    video,
                    reason,
                    renegotiating,
                    ..Default::default()
                };
            }
        }
    }
    Ok(last)
}

/// 当前通话状态
pub fn call_state(dir: &str) -> CallEvent {
    let g = calls().lock().expect("通话锁中毒");
    match g.get(dir) {
        Some(c) => CallEvent {
            state: c.state().as_str().to_string(),
            call_id: c.call_id().to_string(),
            peer_dh: c.peer_dh().to_string(),
            video: c.video(),
            reason: c.reason().to_string(),
            renegotiating: c.renegotiating(),
            ..Default::default()
        },
        None => CallEvent { state: "idle".into(), ..Default::default() },
    }
}

fn with_call<T>(dir: &str, f: impl FnOnce(&mut call::Call) -> T) -> T {
    let mut g = calls().lock().expect("通话锁中毒");
    let c = g.entry(dir.to_string()).or_insert_with(call::Call::new);
    f(c)
}

/// 发起通话。返回 call_id，上层据此创建 PeerConnection。
pub fn call_start(dir: &str, to: &str, video: bool) -> Result<String> {
    // 联系人必须存在，否则信令根本发不出去
    let c = find_contact(Path::new(dir), to)?;
    let call_id = proto::new_queue_id();
    let acts = with_call(dir, |s| s.start_outgoing(&call_id, &c.dh, video, now()))
        .map_err(|e| anyhow!(e))?;
    flush_actions(dir, acts).map_err(|e| anyhow!(e))?;
    Ok(call_id)
}

/// 主叫：本地 SDP 就绪后，把 offer 发出去
pub fn call_set_offer(dir: &str, sdp: &str) -> Result<CallEvent> {
    let acts = with_call(dir, |s| s.make_offer_actions(sdp));
    if acts.is_empty() {
        return Err(anyhow!("当前不在主叫状态，无法发送 offer"));
    }
    flush_actions(dir, acts).map_err(|e| anyhow!(e))
}

/// 通话中换网络后发起重协商（ICE restart）。
///
/// 上层要用 `IceRestart=true` 重新 createOffer，再把本地 SDP 传进来。
/// 这里只负责把带 `restart` 标志的 offer 发出去并置标记——**通话状态不变**。
pub fn call_renegotiate(dir: &str, sdp: &str) -> Result<CallEvent> {
    let acts = with_call(dir, |s| s.make_restart_offer_actions(sdp));
    if acts.is_empty() {
        return Err(anyhow!("当前不在通话中，无法重新协商"));
    }
    flush_actions(dir, acts).map_err(|e| anyhow!(e))
}

/// 被叫：接听。上层需先 setRemoteDescription(offer) 再 createAnswer，然后把 SDP 传进来。
pub fn call_accept(dir: &str, sdp: &str) -> Result<CallEvent> {
    let acts = with_call(dir, |s| s.accept(sdp, now()));
    if acts.is_empty() {
        return Err(anyhow!("当前没有可接听的通话"));
    }
    flush_actions(dir, acts).map_err(|e| anyhow!(e))
}

/// 被叫：拒接
pub fn call_reject(dir: &str, reason: &str) -> Result<CallEvent> {
    let acts = with_call(dir, |s| s.reject(reason, now()));
    flush_actions(dir, acts).map_err(|e| anyhow!(e))
}

/// 本端挂断
pub fn call_hangup(dir: &str, reason: &str) -> Result<CallEvent> {
    let acts = with_call(dir, |s| s.hangup(reason, now()));
    flush_actions(dir, acts).map_err(|e| anyhow!(e))
}

/// 本端产生了一个 ICE candidate，要发给对方
pub fn call_ice(dir: &str, candidate: &str, mid: &str, mline: i32) -> Result<CallEvent> {
    let acts = with_call(dir, |s| s.candidate_action(candidate, mid, mline));
    flush_actions(dir, acts).map_err(|e| anyhow!(e))
}

/// 定时驱动（振铃超时）。上层每秒调一次。
pub fn call_tick(dir: &str) -> Result<CallEvent> {
    let acts = with_call(dir, |s| s.tick(now()));
    flush_actions(dir, acts).map_err(|e| anyhow!(e))
}

/// 结束态回到空闲（UI 关掉通话界面时调）
pub fn call_reset(dir: &str) {
    with_call(dir, |s| s.reset());
}

/// 收到一条通话信令，驱动状态机，并自动回应该回的（忙线拒接等）。
///
/// `msg_id` 用加密层的消息 ID 做去重，防止中继重投导致 UI 反复弹窗。
pub fn call_inbound(
    dir: &str,
    from_dh: &str,
    kind: &str,
    body: &str,
    msg_id: &str,
) -> Result<CallEvent> {
    let t = now();
    let mut extra = CallEvent::default();

    let acts = match kind {
        call_kind::OFFER => {
            let b: call::OfferBody = serde_json::from_str(body)
                .map_err(|e| anyhow!("offer 格式不对：{e}"))?;
            // 只有被状态机接受的 offer 才把 SDP 透出给 UI（仿 ICE 分支的写法）。
            // 之前无条件透出：被忙线拒接的第三方 offer、被代际守卫丢弃的迟到
            // 重协商，其 SDP 照样挂在 CallEvent 上——UI 按契约 setRemoteDescription
            // 会污染正在进行的通话（状态机防了、API 层漏了的那一刀）。
            let (acts, accepted) = with_call(dir, |s| s.on_offer_ex(from_dh, &b, msg_id, t));
            if accepted {
                extra.remote_sdp = Some(b.sdp.clone());
                extra.video = b.video;
            }
            acts
        }
        call_kind::ANSWER => {
            let b: call::AnswerBody = serde_json::from_str(body)
                .map_err(|e| anyhow!("answer 格式不对：{e}"))?;
            extra.remote_sdp = Some(b.sdp.clone());
            with_call(dir, |s| s.on_answer(from_dh, &b, t))
        }
        call_kind::REJECT | call_kind::END => {
            let b: call::EndBody = serde_json::from_str(body)
                .map_err(|e| anyhow!("end 格式不对：{e}"))?;
            with_call(dir, |s| s.on_end(from_dh, &b, t))
        }
        call_kind::ICE => {
            let b: call::IceBody = serde_json::from_str(body)
                .map_err(|e| anyhow!("ice 格式不对：{e}"))?;
            let accepted = with_call(dir, |s| s.on_ice(from_dh, &b));
            extra.ice = accepted;
            Vec::new()
        }
        other => {
            return Err(anyhow!("不是通话信令：{other}"));
        }
    };

    // 自动回复（忙线拒接等）发不出去时不能让整个流程失败：
    // 本地状态已经迁移，UI 仍要正确显示
    let mut ev = match flush_actions(dir, acts) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[client] 通话信令发送失败: {e}");
            call_state(dir)
        }
    };
    // flush 只在有状态变化时填 ev；没有变化时用当前状态兜底
    if ev.state.is_empty() {
        ev = call_state(dir);
    }
    // 补充 offer/ice 附带的信息
    if extra.remote_sdp.is_some() {
        ev.remote_sdp = extra.remote_sdp;
    }
    if extra.ice.is_some() {
        ev.ice = extra.ice;
    }
    if ev.video == false && extra.video {
        ev.video = true;
    }
    Ok(ev)
}

// ---------------------------------------------------------------- 连接

struct Conn {
    ws: Ws,
}

enum RecvErr {
    Timeout,
    Fatal(anyhow::Error),
}

impl Conn {
    async fn connect(url: &str) -> Result<Self> {
        // WS 握手必须有硬超时。connect_async 内部的 TCP/HTTP 升级没有任何超时，
        // 遇到半开连接（adb reverse、NAT 老化）会永久挂起——
        // 表现为 recv_once 永不返回、Kotlin 收消息循环静默卡死。
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async(url),
        )
        .await
        .map_err(|_| anyhow!("连接中继 {url} 超时（10 秒）"))?
        .with_context(|| format!("连不上中继 {url}"))?;
        Ok(Self { ws })
    }

    async fn send(&mut self, f: &ClientFrame) -> Result<()> {
        let s = serde_json::to_string(f)?;
        // 必须有写超时：TCP 零窗口/半开路径上 `SinkExt::send` 会无限 pending，
        // 而所有发送 API 都在设备锁内同步 block_on 到这里——锁被无限持有后，
        // 收消息、加好友、切中继会全部卡死且无任何日志。超时按 Fatal 语义
        // 处理（调用方丢弃连接，下一轮重连）。
        tokio::time::timeout(Duration::from_secs(10), self.ws.send(Message::text(s)))
            .await
            .map_err(|_| anyhow!("发送超时（连接疑似半开，丢弃）"))?
            .context("发送失败")?;
        Ok(())
    }

    async fn recv_frame(&mut self, timeout: Duration) -> std::result::Result<ServerFrame, RecvErr> {
        loop {
            let r = tokio::time::timeout(timeout, self.ws.next())
                .await
                .map_err(|_| RecvErr::Timeout)?;
            let msg = match r {
                Some(Ok(m)) => m,
                Some(Err(e)) => return Err(RecvErr::Fatal(e.into())),
                None => return Err(RecvErr::Fatal(anyhow!("连接被关闭"))),
            };
            if let Ok(t) = msg.to_text() {
                return Ok(serde_json::from_str(t).map_err(|e| RecvErr::Fatal(e.into()))?);
            }
        }
    }

    async fn new_queue(&mut self) -> Result<String> {
        self.send(&ClientFrame::New { v: PROTOCOL_VERSION }).await?;
        match self.recv_frame(Duration::from_secs(10)).await {
            Ok(ServerFrame::NewOk { qid, .. }) => Ok(qid),
            Ok(ServerFrame::Err { code, msg }) => Err(anyhow!("建队列失败 [{code}] {msg}")),
            Ok(other) => Err(anyhow!("建队列收到意外帧: {other:?}")),
            Err(RecvErr::Timeout) => Err(anyhow!("建队列超时")),
            Err(RecvErr::Fatal(e)) => Err(e),
        }
    }

    /// 索取 TURN 临时凭据。中继没开 TURN 时返回 Err（上层回退到用户自填配置）。
    async fn get_turn(&mut self) -> Result<TurnCreds> {
        self.send(&ClientFrame::GetTurn).await?;
        match self.recv_frame(Duration::from_secs(10)).await {
            Ok(ServerFrame::TurnCreds { url, user, pass, ttl }) => Ok(TurnCreds {
                url,
                user,
                pass,
                ttl,
                expires_at: proto::now_secs() + ttl,
            }),
            Ok(ServerFrame::Err { code, msg }) => Err(anyhow!("中继不给 TURN [{}] {}", code, msg)),
            Ok(other) => Err(anyhow!("索取 TURN 收到意外帧: {other:?}")),
            Err(RecvErr::Timeout) => Err(anyhow!("索取 TURN 超时")),
            Err(RecvErr::Fatal(e)) => Err(e),
        }
    }

    /// 订阅。返回 `Some((id, body))` = 订阅确认前中继就推来了积压的第一条
    /// （队列非空时真实会发生）——这条消息绝不能吞：早先只读一帧就返回，
    /// Msg 帧被直接丢掉也不 ack，消息只能等下一次重连重投，若中继只在
    /// 本连接内投递，它就永远出不来。调用方必须按正常消息处理它。
    async fn sub(&mut self, qid: &str) -> Result<Option<(String, Vec<u8>)>> {
        self.send(&ClientFrame::Sub {
            qid: qid.to_string(),
        })
        .await?;
        match self.recv_frame(Duration::from_secs(10)).await {
            Ok(ServerFrame::Ok { .. }) => Ok(None),
            Ok(ServerFrame::Msg { id, body, .. }) => Ok(Some((id, body))),
            Ok(ServerFrame::Err { code, msg }) => Err(anyhow!("订阅失败 [{code}] {msg}")),
            Ok(other) => Err(anyhow!("订阅收到意外帧: {other:?}")),
            Err(RecvErr::Timeout) => Err(anyhow!("订阅超时")),
            Err(RecvErr::Fatal(e)) => Err(e),
        }
    }

    async fn put(&mut self, qid: &str, wire: &Wire) -> Result<String> {
        let id = proto::new_message_id();
        self.send(&ClientFrame::Put {
            qid: qid.to_string(),
            id: id.clone(),
            body: wire.to_bytes(),
        })
        .await?;
        match self.recv_frame(Duration::from_secs(10)).await {
            Ok(ServerFrame::Ok { .. }) => Ok(id),
            Ok(ServerFrame::Err { code, msg }) => Err(anyhow!("投递失败 [{code}] {msg}")),
            Ok(other) => Err(anyhow!("投递收到意外帧: {other:?}")),
            Err(RecvErr::Timeout) => Err(anyhow!("投递超时")),
            Err(RecvErr::Fatal(e)) => Err(e),
        }
    }

    async fn ack(&mut self, qid: &str, ids: &[String]) -> Result<()> {
        self.send(&ClientFrame::Ack {
            qid: qid.to_string(),
            ids: ids.to_vec(),
        })
        .await
    }

    /// 收消息超时后的活性探测：发 Ping 等 Pong（5 秒）。
    ///
    /// 超时本身区分不了"真空闲"和"半开连接"（TCP 半开后 recv 同样是安静超时）。
    /// 半开连接如果不剔除，会被放回缓存无限复用——每轮收消息都安静超时、
    /// 无错误日志、新消息永远收不到（2026-10-21 模拟器实测复现的形态）。
    /// 等 Pong 期间如果中继推送了数据帧，直接返回给上层处理，不丢消息。
    async fn probe(&mut self) -> std::result::Result<Option<(String, Vec<u8>)>, RecvErr> {
        self.send(&ClientFrame::Ping)
            .await
            .map_err(RecvErr::Fatal)?;
        loop {
            match self.recv_frame(Duration::from_secs(5)).await? {
                ServerFrame::Pong | ServerFrame::Ok { .. } => return Ok(None),
                ServerFrame::Msg { id, body, .. } => return Ok(Some((id, body))),
                ServerFrame::Err { code, msg } => {
                    return Err(RecvErr::Fatal(anyhow!("中继返回错误 [{code}] {msg}")))
                }
                other => return Err(RecvErr::Fatal(anyhow!("意外帧: {other:?}"))),
            }
        }
    }

    async fn recv_msg(
        &mut self,
        timeout: Duration,
    ) -> std::result::Result<(String, Vec<u8>), RecvErr> {
        // 超时必须是"总等待时长"的 deadline 语义，而不是"每帧间隔"。
        // 每帧重置的话，只要对端以小于 timeout 的间隔持续发帧（心跳、补投、错误帧），
        // 就永远等不到超时，上层循环会卡死在这一轮。
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remain.is_zero() {
                return Err(RecvErr::Timeout);
            }
            match self.recv_frame(remain).await? {
                ServerFrame::Msg { id, body, .. } => return Ok((id, body)),
                ServerFrame::Ok { .. } | ServerFrame::Pong => continue,
                ServerFrame::Err { code, msg } => {
                    return Err(RecvErr::Fatal(anyhow!("中继返回错误 [{code}] {msg}")))
                }
                other => return Err(RecvErr::Fatal(anyhow!("意外帧: {other:?}"))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> String {
        let p = std::env::temp_dir().join(format!(
            "client-test-{}-{}-{}",
            tag,
            std::process::id(),
            proto::new_queue_id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p.display().to_string()
    }

    #[test]
    fn init_creates_identity_and_refuses_overwrite() {
        let d = tmpdir("init");
        let id = init(&d, Some("Alice")).unwrap();
        assert_eq!(id.len(), 43);
        assert_eq!(identity(&d).unwrap(), id);
        assert_eq!(local_name(&d).unwrap().as_deref(), Some("Alice"));
        // 第二次必须拒绝，否则会覆盖身份
        assert!(init(&d, None).is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn add_then_find_contact() {
        let da = tmpdir("ca");
        let db = tmpdir("cb");
        init(&da, Some("Alice")).unwrap();
        init(&db, Some("Bob")).unwrap();

        // 手工造一个邀请（不联网），走 add 的解析与建档逻辑
        let mut bob = Device::load(Path::new(&db)).unwrap();
        let b = bob.bundle();
        let inv = identity::Invite::new(
            b.pk,
            b.dh,
            b.otk,
            proto::new_queue_id(),
            vec!["ws://127.0.0.1:1".into()],
        )
        .unwrap()
        .with_name("Bob");

        let name = add(&da, &inv.encode(), None).unwrap();
        assert_eq!(name, "Bob");

        let cs = contacts(&da).unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].name, "Bob");
        std::fs::remove_dir_all(&da).ok();
        std::fs::remove_dir_all(&db).ok();
    }

    #[test]
    fn send_without_contact_fails_clearly() {
        let d = tmpdir("send");
        init(&d, None).unwrap();
        let e = send(&d, "Nobody", "hi").unwrap_err().to_string();
        assert!(e.contains("找不到联系人"), "实际: {e}");
        std::fs::remove_dir_all(&d).ok();
    }

    /// 对方重装恢复身份：pk 不变、dh 换新。重新用他的新码添加必须**合并**
    /// 而不是追加——旧条目指向已废弃的队列，留着就是死信箱。
    #[test]
    fn add_merges_same_pk_with_new_dh() {
        let da = tmpdir("mg");
        let db = tmpdir("mg2");
        init(&da, Some("Alice")).unwrap();
        init(&db, Some("Bob")).unwrap();

        // 第一次：正常添加 Bob
        let mut bob = Device::load(Path::new(&db)).unwrap();
        let b1 = bob.bundle();
        let inv1 = identity::Invite::new(
            b1.pk.clone(),
            b1.dh.clone(),
            b1.otk.clone(),
            proto::new_queue_id(),
            vec!["ws://127.0.0.1:1".into()],
        )
        .unwrap()
        .with_name("Bob");
        add(&da, &inv1.encode(), Some("老王备注")).unwrap();
        // 用户自己起的别名必须在合并后保留
        assert_eq!(contacts(&da).unwrap()[0].alias.as_deref(), Some("老王备注"));

        // 第二次：Bob 重装恢复（pk 相同、dh/otk/qid 全新）后出的新码
        let mut bob2 = Device::load(Path::new(&db)).unwrap();
        let b2 = bob2.bundle();
        let new_dh = b2.dh.clone();
        let inv2 = identity::Invite::new(
            b1.pk, // 身份没变
            b2.dh, // dh 是新的
            b2.otk,
            proto::new_queue_id(),
            vec!["ws://127.0.0.1:2".into()],
        )
        .unwrap()
        .with_name("BobNew");
        add(&da, &inv2.encode(), None).unwrap();

        let cs = contacts(&da).unwrap();
        assert_eq!(cs.len(), 1, "同 pk 必须合并成一条，实际: {cs:?}");
        assert_eq!(cs[0].dh, new_dh, "路由必须指向新 dh");
        assert_eq!(cs[0].name, "BobNew");
        assert_eq!(cs[0].alias.as_deref(), Some("老王备注"), "用户备注不能被重装抹掉");
        assert_eq!(cs[0].relays, vec!["ws://127.0.0.1:2".to_string()]);

        std::fs::remove_dir_all(&da).ok();
        std::fs::remove_dir_all(&db).ok();
    }

    /// 回邀路径同样要合并：同 pk 换 dh 时旧条目必须迁到新 dh 上。
    #[test]
    fn add_from_card_merges_same_pk_with_new_dh() {
        let da = tmpdir("mc");
        let db = tmpdir("mc2");
        init(&da, Some("Alice")).unwrap();
        init(&db, Some("Bob")).unwrap();

        let mut bob = Device::load(Path::new(&db)).unwrap();
        let b1 = bob.bundle();
        let inv1 = identity::Invite::new(
            b1.pk.clone(),
            b1.dh.clone(),
            b1.otk.clone(),
            proto::new_queue_id(),
            vec!["ws://127.0.0.1:1".into()],
        )
        .unwrap()
        .with_name("Bob");
        add(&da, &inv1.encode(), None).unwrap();

        // Bob 重装恢复后从陌生人消息回邀：dh 是新会话的，pk 不变
        let mut bob2 = Device::load(Path::new(&db)).unwrap();
        let b2 = bob2.bundle();
        let new_dh = b2.dh.clone();
        // Alice 这边要先有与新 dh 的会话（信任闸：has_session）
        let mut alice = Device::load(Path::new(&da)).unwrap();
        alice.start_session(&b2.dh, &b2.otk).unwrap();
        alice.save(Path::new(&da)).unwrap();

        let name = add_from_card(
            &da, "BobNew", &b2.dh, &b1.pk, &proto::new_queue_id(),
            &["ws://127.0.0.1:2".to_string()],
        )
        .unwrap();

        let cs = contacts(&da).unwrap();
        assert_eq!(cs.len(), 1, "回邀同 pk 也必须合并，实际: {cs:?}");
        assert_eq!(cs[0].dh, new_dh);
        assert_eq!(name, "Bob", "合并时保留旧条目的名字，不采用自报新名");
        assert_eq!(cs[0].relays, vec!["ws://127.0.0.1:2".to_string()]);

        std::fs::remove_dir_all(&da).ok();
        std::fs::remove_dir_all(&db).ok();
    }

    /// 备份 → 清空 → 恢复：身份必须一模一样。这是恢复短语存在的全部意义。
    #[test]
    fn backup_then_restore_yields_same_identity() {
        let d = tmpdir("bak");
        let id = init(&d, Some("Alice")).unwrap();

        let phrase = backup_phrase(&d).unwrap();
        assert_eq!(phrase.split_whitespace().count(), 12, "必须是 12 个词: {phrase}");

        // 恢复到全新目录（模拟换手机）
        let d2 = tmpdir("bak2");
        let id2 = restore(&d2, &phrase, Some("Alice")).unwrap();
        assert_eq!(id, id2, "恢复出的身份必须与原身份一致");

        // 恢复出的设备同样能再备份出同样的短语（循环闭合）
        assert_eq!(backup_phrase(&d2).unwrap(), phrase);

        // 抄错一个词必须被校验位拦下
        let mut words: Vec<&str> = phrase.split_whitespace().collect();
        words[5] = "zoo";
        let bad = words.join(" ");
        assert!(restore(&tmpdir("bak3"), &bad, None).is_err());

        // 已有身份的目录必须拒绝恢复（覆盖身份是最危险的误操作）
        assert!(restore(&d, &phrase, None).is_err());

        std::fs::remove_dir_all(&d).ok();
        std::fs::remove_dir_all(&d2).ok();
    }
}
