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

/// 合并"投递这条消息的中继"和"对方自报的中继"，去重保序，最多留 4 个。
///
/// 投递中继排第一位：它一定可达（消息刚从我这儿出去），
/// 对方自报的只是候选，可能已经下线。
fn merge_relays(via: &str, extra: Option<Vec<String>>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in std::iter::once(via.to_string()).chain(extra.unwrap_or_default()) {
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
    warn(format!(
        "无法解密来自 {from} 的消息 [mt={mt} {}] {tail}，同类累计 {n} 条: {detail}",
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
    std::fs::write(dir.join(SELF_FILE), serde_json::to_vec_pretty(s)?)?;
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
    std::fs::write(dir.join(CONTACTS_FILE), serde_json::to_vec_pretty(c)?)?;
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

/// 探测一个中继：完成 WebSocket 握手并计时，返回毫秒数。
///
/// 用于首次启动的中继选择页：只验证"连得上"，不建队列不投递，
/// 探测对中继状态没有副作用。
pub fn relay_probe(url: &str) -> Result<u64> {
    let start = std::time::Instant::now();
    rt().block_on(async {
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
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
    // 重复加同一个好友时保留原有备注：重新扫码不该把用户起的名字抹掉
    let old_alias = cs.iter().find(|c| c.dh == inv.dh).and_then(|c| c.alias.clone());
    cs.retain(|c| c.dh != inv.dh);
    cs.push(Contact {
        name: name.clone(),
        pk: inv.pk,
        dh: inv.dh,
        qid: inv.qid,
        relays: inv.relays,
        alias: old_alias,
    });
    save_contacts(&dir, &cs)?;
    Ok(name)
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
    // 已经是联系人：只刷新路由，别改名（用户可能自己起过别名）
    if let Some(existing) = cs.iter_mut().find(|c| c.dh == dh) {
        existing.qid = qid;
        existing.relays = relays;
        if existing.pk.is_empty() {
            existing.pk = pk.to_string();
        }
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
    let me = load_self(&dir)?;
    let c = find_contact(&dir, to)?;

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

    let relay = c
        .relays
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("联系人没有可用的中继地址"))?;

    rt().block_on(async {
        let mut conn = Conn::connect(&relay).await?;
        for w in &wires {
            conn.put(&c.qid, w).await?;
        }
        Ok::<(), anyhow::Error>(())
    })?;

    dev.save(&dir)?;
    Ok(ids)
}

fn send_kind(dir: &str, to: &str, kind: &str, text: &str, body: Option<&str>) -> Result<String> {
    let dir = PathBuf::from(dir);
    // 发送也要占设备锁：加密会推进棘轮，并发的收/发互相覆盖会让会话状态错乱
    let lock = dev_lock(&dir);
    let _guard = lock.lock().expect("设备锁中毒");
    let mut dev = Device::load(&dir)?;
    let me = load_self(&dir)?;
    let c = find_contact(&dir, to)?;

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

    let relay = c
        .relays
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("联系人没有可用的中继地址"))?;

    rt().block_on(async {
        let mut conn = Conn::connect(&relay).await?;
        conn.put(&c.qid, &w).await?;
        Ok::<(), anyhow::Error>(())
    })?;

    // 发送推进了棘轮，必须落盘
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
        conn.sub(&qid).await?;
        let mut acked: u64 = 0;
        let mut idle: u32 = 0;
        while idle < 3 {
            match conn.recv_msg(Duration::from_secs(2)).await {
                Ok((mid, _body)) => {
                    // _body 整个丢弃：不解码、不进内存里的任何持久结构
                    idle = 0;
                    let ids = [mid];
                    if conn.ack(&qid, &ids).await.is_ok() {
                        acked += 1;
                    }
                }
                Err(RecvErr::Timeout) => {
                    idle += 1;
                    // 保活探测，顺便确认连接没死（死连接上 ack 全是假的）
                    if conn.probe().await.is_err() {
                        break;
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
pub fn set_relay(dir: &str, url: &str) -> Result<usize> {
    let dir = PathBuf::from(dir);
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err(anyhow!("中继地址不能为空"));
    }
    let lock = dev_lock(&dir);
    let _guard = lock.lock().expect("设备锁中毒");
    let mut me = load_self(&dir)?;
    let qid = me
        .qid
        .clone()
        .ok_or_else(|| anyhow!("还没有自己的队列，请先完成首次引导"))?;

    // 连通性验证：连不上、sub 不上都算失败，一个字节都不改
    rt().block_on(async {
        let mut c = Conn::connect(&url).await?;
        c.sub(&qid).await?;
        Ok::<(), anyhow::Error>(())
    })
    .map_err(|e| anyhow!("新中继不可用（{url}）: {e:#}"))?;

    me.relays = vec![url.clone()];
    save_self(&dir, &me)?;

    let mut cs = load_contacts(&dir)?;
    for c in cs.iter_mut() {
        c.relays = vec![url.clone()];
    }
    let n = cs.len();
    save_contacts(&dir, &cs)?;
    Ok(n)
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

async fn a_recv_once(dir: &Path, timeout_secs: u64) -> Result<Option<Incoming>> {    let me = load_self(dir)?;
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
        Some((q, r, c)) if q == qid && r == relay => c,
        _ => {
            let mut c = Conn::connect(&relay).await?;
            c.sub(&qid).await?;
            c
        }
    };

    let got = match conn.recv_msg(Duration::from_secs(timeout_secs)).await {
        Ok(v) => Some(v),
        Err(RecvErr::Timeout) => match conn.probe().await {
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
    };

    let incoming = match got {
        Some((mid, body)) => {
            let wire = Wire::from_bytes(&body)?;
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
                        reply_relays: merge_relays(&relay, p.reply_relays),
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
            extra.remote_sdp = Some(b.sdp.clone());
            extra.video = b.video;
            with_call(dir, |s| s.on_offer(from_dh, &b, msg_id, t))
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
        self.ws.send(Message::text(s)).await.context("发送失败")?;
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

    async fn sub(&mut self, qid: &str) -> Result<()> {
        self.send(&ClientFrame::Sub {
            qid: qid.to_string(),
        })
        .await?;
        match self.recv_frame(Duration::from_secs(10)).await {
            Ok(ServerFrame::Ok { .. }) | Ok(ServerFrame::Msg { .. }) => Ok(()),
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
