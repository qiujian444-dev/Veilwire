//! 命令行客户端
//!
//! 目标：在 Android 端还没做出来之前，用最小成本把**整条链路**跑通并且**可长期使用**。
//! 会话状态会落盘，所以"关掉再打开还能接着聊"是成立的。
//!
//! 典型流程（两个终端各一个身份）：
//!   cli init   --dir ./alice --name Alice
//!   cli invite --dir ./alice --relay ws://127.0.0.1:8080     # 打印邀请码
//!   cli init   --dir ./bob   --name Bob
//!   cli invite --dir ./bob   --relay ws://127.0.0.1:8080
//!   cli add    --dir ./alice --invite <Bob 的邀请码>
//!   cli send   --dir ./alice --to Bob --text "在吗"
//!   cli recv   --dir ./bob                                    # 前台收消息
//!
//! 另有 `cli demo` 一键端到端自测（进程内模拟两台设备）。

use std::env;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use proto::{ClientFrame, ServerFrame, PROTOCOL_VERSION};
use secure::{Device, Payload, Wire, PAYLOAD_VERSION};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const SELF_FILE: &str = "self.json";
const CONTACTS_FILE: &str = "contacts.json";

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage();
        exit(2);
    }
    let a = &args[2..];
    let r = match args[1].as_str() {
        "init" => cmd_init(a),
        "invite" => cmd_invite(a).await,
        "add" => cmd_add(a),
        "send" => cmd_send(a).await,
        "filesend" => cmd_filesend(a),
        "recv" => cmd_recv(a).await,
        "contacts" => cmd_contacts(a),
        "safety" => cmd_safety(a),
        "demo" => demo(opt(a, "--relay").unwrap_or("ws://127.0.0.1:8080")).await,
        "calldemo" => cmd_calldemo(opt(a, "--relay").unwrap_or("ws://127.0.0.1:8080")),
        "calloffer" => cmd_calloffer(a),
        "callrecv" => cmd_callrecv(a),
        _ => {
            usage();
            exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("错误: {e:#}");
        exit(1);
    }
}

fn usage() {
    eprintln!(
        "用法:
  cli init     --dir <目录> [--name 昵称]
  cli invite   --dir <目录> --relay <ws://...>          生成邀请码（每个只能用一次）
  cli add      --dir <目录> --invite <邀请码|文件路径> [--as 备注名]
  cli send     --dir <目录> --to <联系人> --text \"内容\"
  cli filesend --dir <目录> --to <联系人> --path <文件>    分块发送文件（调试 App 接收端）
  cli recv     --dir <目录> [--once] [--json]            前台收消息（--json 打印完整载荷，含文件 body）
  cli contacts --dir <目录>
  cli safety   --dir <目录> --peer <联系人>              打印与对方的安全码（两端比对防中间人）
  cli demo     --relay <ws://...>                        进程内端到端自测
  cli calldemo --relay <ws://...>                        通话信令端到端自测（假 SDP）
  cli calloffer --dir <目录> --to <联系人> [--video]     给指定联系人发一个 offer（假 SDP，用于调试来电界面）
  cli callrecv  --dir <目录> [--secs 30]                 前台收信令并打印状态变化（调试用）"
    );
}

fn opt<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

fn dir_of(args: &[String]) -> Result<PathBuf> {
    opt(args, "--dir")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("缺少 --dir"))
}

// ---------------------------------------------------------------- 本地状态

#[derive(Debug, Default, Serialize, Deserialize)]
struct SelfState {
    #[serde(default)]
    qid: Option<String>,
    #[serde(default)]
    relays: Vec<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Contact {
    name: String,
    pk: String,
    dh: String,
    qid: String,
    relays: Vec<String>,
}

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
        .ok_or_else(|| anyhow!("找不到联系人 {who}，先用 `cli add` 添加"))
}

fn contact_name(dir: &Path, dh: &str) -> Option<String> {
    load_contacts(dir)
        .ok()?
        .into_iter()
        .find(|c| c.dh == dh)
        .map(|c| c.name)
}

// ---------------------------------------------------------------- 命令

fn cmd_init(args: &[String]) -> Result<()> {
    let dir = dir_of(args)?;
    if dir.join("device.key").exists() {
        return Err(anyhow!(
            "{} 里已经有一个身份了。要重新开始请换个目录，或先备份再删掉旧目录",
            dir.display()
        ));
    }
    std::fs::create_dir_all(&dir)?;
    let dev = Device::new();
    dev.save(&dir)?;

    let mut me = load_self(&dir)?;
    me.name = opt(args, "--name").map(|s| s.to_string());
    save_self(&dir, &me)?;

    println!("身份已创建：{}", dev.id());
    println!("目录：{}", dir.display());
    println!(
        "\n下一步：cli invite --dir {} --relay ws://你的中继",
        dir.display()
    );
    Ok(())
}

async fn cmd_invite(args: &[String]) -> Result<()> {
    let dir = dir_of(args)?;
    let relay = opt(args, "--relay").ok_or_else(|| anyhow!("缺少 --relay"))?;

    let mut dev = Device::load(&dir)?;
    let mut me = load_self(&dir)?;

    // 自己的队列：没有就向中继申请一个，之后复用
    let qid = match me.qid.clone() {
        Some(q) => q,
        None => {
            let mut c = Conn::connect(relay).await?;
            let q = c.new_queue().await?;
            me.qid = Some(q.clone());
            if !me.relays.iter().any(|r| r == relay) {
                me.relays.push(relay.to_string());
            }
            save_self(&dir, &me)?;
            println!("已在中继上创建队列：{q}");
            q
        }
    };

    let b = dev.bundle();
    // 一次性密钥被消耗了，必须立刻落盘；否则重启后会复用同一个 OTK，
    // "邀请码只用一次"这个承诺就破了。
    dev.save(&dir)?;

    let inv = identity::Invite::new(b.pk, b.dh, b.otk, qid.clone(), me.relays.clone())
        .map_err(|e| anyhow!(e))?;
    let inv = match &me.name {
        Some(n) => inv.with_name(n.clone()),
        None => inv,
    };

    println!("身份：{}", dev.id());
    println!("队列：{qid}");
    println!(
        "\n邀请码（只给对方扫一次，用后即失效）：\n{}",
        inv.encode()
    );
    Ok(())
}

fn cmd_add(args: &[String]) -> Result<()> {
    let dir = dir_of(args)?;
    let raw = opt(args, "--invite").ok_or_else(|| anyhow!("缺少 --invite"))?;

    // 允许传文件路径，方便把一长串邀请码存成文件再传
    let text = if Path::new(raw).exists() {
        std::fs::read_to_string(raw).context("读取邀请码文件失败")?
    } else {
        raw.to_string()
    };
    let inv = identity::Invite::decode(&text).map_err(|e| anyhow!(e))?;

    let name = opt(args, "--as")
        .map(|s| s.to_string())
        .or_else(|| inv.name.clone())
        .unwrap_or_else(|| inv.pk.chars().take(6).collect());

    let mut dev = Device::load(&dir)?;
    dev.start_session(&inv.dh, &inv.otk)?;
    dev.save(&dir)?;

    let mut cs = load_contacts(&dir)?;
    cs.retain(|c| c.dh != inv.dh);
    cs.push(Contact {
        name: name.clone(),
        pk: inv.pk.clone(),
        dh: inv.dh.clone(),
        qid: inv.qid.clone(),
        relays: inv.relays.clone(),
    });
    save_contacts(&dir, &cs)?;

    println!("已添加联系人：{name}");
    println!("身份：{}", inv.pk);
    Ok(())
}

/// 分块发送一个文件或一段语音（调试 App 接收端）。协议：多条消息，
/// body = {fid,name,size,idx,total,dur,data(base64)}，一次连接批量推送。
///
/// `--kind voice --dur 7400` 可以伪造一条语音，用来验证 App 的语音气泡和播放——
/// 不用真拿两台手机对着说话。
fn cmd_filesend(args: &[String]) -> Result<()> {
    let dir = dir_of(args)?;
    let to = opt(args, "--to").ok_or_else(|| anyhow!("缺少 --to"))?;
    let path = opt(args, "--path").ok_or_else(|| anyhow!("缺少 --path"))?;
    let kind = opt(args, "--kind").unwrap_or("file").to_string();
    let dur: i64 = opt(args, "--dur")
        .map(|s| s.parse::<i64>())
        .transpose()
        .context("--dur 要传整数毫秒")?
        .unwrap_or(0);

    const CHUNK: usize = 24 * 1024;
    const BATCH: usize = 20;

    let bytes = std::fs::read(path).with_context(|| format!("读文件 {path} 失败"))?;
    if bytes.is_empty() {
        anyhow::bail!("空文件");
    }
    let name = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    let total = bytes.len().div_ceil(CHUNK);
    let fid = format!(
        "cli{}",
        &proto::new_message_id()[..8.min(proto::new_message_id().len())]
    );
    println!("发送 {name}（{} 字节，{total} 块）", bytes.len());

    use base64::Engine as _;
    let b64 = |d: &[u8]| base64::engine::general_purpose::STANDARD_NO_PAD.encode(d);

    let mut done = 0usize;
    let mut all: Vec<String> = Vec::with_capacity(total);
    for (i, piece) in bytes.chunks(CHUNK).enumerate() {
        all.push(
            serde_json::json!({
                "fid": fid, "name": name, "size": bytes.len(),
                "idx": i, "total": total, "dur": dur, "data": b64(piece),
            })
            .to_string(),
        );
    }
    for batch in all.chunks(BATCH) {
        // client::send_batch 内部 block_on；cli 的 main 已在 tokio 运行时里，
        // 必须挪到普通线程执行（老坑，cli demo 注释里记过）
        let d2 = dir.clone();
        let b2 = batch.to_vec();
        let to2 = to.to_string();
        let k2 = kind.clone();
        std::thread::spawn(move || {
            client::send_batch(
                d2.to_str().ok_or_else(|| anyhow!("目录路径不是 UTF-8"))?,
                &to2,
                &k2,
                &b2,
            )
        })
        .join()
        .unwrap()?;
        done += batch.len();
        println!("  已推 {done}/{total} 块");
    }
    println!("已发送：{name}（kind={kind}）");
    Ok(())
}

async fn cmd_send(args: &[String]) -> Result<()> {
    let dir = dir_of(args)?;
    let to = opt(args, "--to").ok_or_else(|| anyhow!("缺少 --to"))?;
    let text = opt(args, "--text").ok_or_else(|| anyhow!("缺少 --text"))?;

    let mut dev = Device::load(&dir)?;
    let me = load_self(&dir)?;
    let c = find_contact(&dir, to)?;

    let w = dev.encrypt(
        &c.dh,
        &Payload {
            v: PAYLOAD_VERSION,
            id: proto::new_message_id(),
            from: dev.id(),
            ts: proto::now_secs(),
            text: text.to_string(),
            reply_qid: me.qid.clone(),
            reply_relays: if me.relays.is_empty() { None } else { Some(me.relays.clone()) },
            self_name: me.name.clone(),
            kind: secure::default_kind(),
            body: None,
        },
    )?;

    // 默认用联系人邀请码里带的中继；--relay 可覆盖（跨网络环境测试时常用，
    // 比如模拟器邀请码里的 10.0.2.2 在宿主机上不可达）
    let relay = opt(args, "--relay")
        .map(|s| s.to_string())
        .or_else(|| c.relays.first().cloned())
        .ok_or_else(|| anyhow!("联系人没有可用的中继地址（可用 --relay 覆盖）"))?;
    let mut conn = Conn::connect(&relay).await?;
    conn.put(&c.qid, &w).await?;

    // 发送会推进棘轮，必须立刻落盘；否则密钥状态丢失，后续消息全解不开
    dev.save(&dir)?;
    println!("已发送给 {}（{} 字节密文）", c.name, w.ct.len());
    Ok(())
}

async fn cmd_recv(args: &[String]) -> Result<()> {
    let dir = dir_of(args)?;
    let once = args.iter().any(|a| a == "--once");

    let mut dev = Device::load(&dir)?;
    let me = load_self(&dir)?;
    let qid = me
        .qid
        .clone()
        .ok_or_else(|| anyhow!("还没有自己的队列，先跑 `cli invite`"))?;
    let relay = opt(args, "--relay")
        .map(|s| s.to_string())
        .or_else(|| me.relays.first().cloned())
        .ok_or_else(|| anyhow!("还不知道连哪个中继（可用 --relay 覆盖）"))?;

    let mut conn = Conn::connect(&relay).await?;
    conn.sub(&qid).await?;
    println!("正在监听 {qid} @ {relay}（Ctrl+C 退出）");

    loop {
        let (mid, body) = match conn.recv_msg(Duration::from_secs(3600)).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("连接中断: {e:#}");
                break;
            }
        };
        let wire = match Wire::from_bytes(&body) {
            Ok(w) => w,
            Err(e) => {
                println!("收到无法解析的信封: {e:#}");
                continue;
            }
        };

        // 会话键交给 Device 自己判断：首条预共享消息会自动建会话，
        // 单联系人场景下普通消息也能落到唯一会话上。
        match dev.decrypt(None, &wire) {
            Ok((dh, p)) => {
                let from =
                    contact_name(&dir, &dh).unwrap_or_else(|| p.from.chars().take(8).collect());
                // --json 输出完整载荷：文件传输调试时 text 是空的，
                // 真正的内容在 body 里，人类可读格式看不到，得靠这个抓出来校验。
                if args.iter().any(|a| a == "--json") {
                    println!(
                        "{}",
                        serde_json::json!({
                            "kind": p.kind, "from": from, "ts": p.ts,
                            "text": p.text, "body": p.body,
                        })
                    );
                } else {
                    println!("[{}] {}: {}", fmt_time(p.ts), from, p.text);
                }

                // 不 ACK 的话中继会一直存着这条消息
                ack_or_warn(&mut conn, &qid, &mid).await;

                // 棘轮状态 + 去重表都变了，必须落盘
                if let Err(e) = dev.save(&dir) {
                    eprintln!("保存状态失败（消息已收到，但棘轮状态没存）: {e:#}");
                }
                if once {
                    break;
                }
            }
            Err(e) => {
                // 同一套策略与 App 侧一致（见 client::note_decrypt_fail）：
                // 重复 / 结构坏掉 → ack 掉，否则它会永远留在队列里重投；
                // MAC 不符 / 没会话 → 不 ack，留着对账。
                let kind = secure::fail_kind(&e);
                if kind.can_ack() {
                    ack_or_warn(&mut conn, &qid, &mid).await;
                    println!(
                        "无法解密 [{}] 已确认并删除（重复或结构损坏）: {e:#}",
                        kind.as_str()
                    );
                } else {
                    println!(
                        "无法解密 [{}] 保留在队列中（未确认，可能是真丢了消息）: {e:#}",
                        kind.as_str()
                    );
                }
            }
        }
    }
    Ok(())
}

/// 发 ack：失败要留痕并重试一次。
///
/// 以前这里是 `let _ = conn.ack(...)`。连接半开时 ack 会静默消失，
/// 已处理的消息全留在队列里，下次重连全量重投、全部报"已被消费"
/// （2026-10-22 实测 65 条）。CLI 是排查问题的唯一观察窗口，
/// 这类失败再也不能吞掉。
async fn ack_or_warn(conn: &mut Conn, qid: &str, mid: &str) {
    let ids = [mid.to_string()];
    for attempt in 1..=2u32 {
        match conn.ack(qid, &ids).await {
            Ok(()) => return,
            Err(e) if attempt == 1 => {
                eprintln!("发送 ack 失败，重试一次: {e:#}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => eprintln!("发送 ack 失败（这条消息会在下次重连时重投）: {e:#}"),
        }
    }
}

fn cmd_contacts(args: &[String]) -> Result<()> {
    let dir = dir_of(args)?;
    let dev = Device::load(&dir)?;
    let me = load_self(&dir)?;
    println!("本机身份：{}", dev.id());
    if let Some(q) = &me.qid {
        println!("本机队列：{q}");
    }
    let cs = load_contacts(&dir)?;
    if cs.is_empty() {
        println!("还没有联系人");
        return Ok(());
    }
    println!("\n{:<12} {:<14} {}", "名称", "身份(前12)", "队列");
    for c in cs {
        println!(
            "{:<12} {:<14} {}",
            c.name,
            c.pk.chars().take(12).collect::<String>(),
            c.qid
        );
    }
    Ok(())
}

/// 打印与某个联系人的安全码。两个终端各跑一次，数字一样就说明没有中间人。
fn cmd_safety(args: &[String]) -> Result<()> {
    let dir = dir_of(args)?;
    let peer = opt(args, "--peer").ok_or_else(|| anyhow!("缺少 --peer"))?;
    let dev = Device::load(&dir)?;
    let cs = load_contacts(&dir)?;
    let c = cs
        .iter()
        .find(|c| c.name == peer)
        .ok_or_else(|| anyhow!("没有这个联系人：{peer}"))?;
    let code = secure::safety_number(&dev.id(), &c.pk);
    let grouped = code
        .chars()
        .collect::<Vec<_>>()
        .chunks(5)
        .map(|g| g.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(" ");
    println!("我　　：{}", dev.id());
    println!("对方　：{}", c.pk);
    println!("安全码：{grouped}");
    Ok(())
}

fn fmt_time(ts: u64) -> String {
    let now = proto::now_secs();
    if ts == 0 || ts > now {
        return "刚刚".into();
    }
    match now - ts {
        0..=59 => "刚刚".into(),
        s @ 60..=3599 => format!("{} 分钟前", s / 60),
        s @ 3600..=86399 => format!("{} 小时前", s / 3600),
        s => format!("{} 天前", s / 86400),
    }
}

// ---------------------------------------------------------------- 连接封装

struct Conn {
    ws: Ws,
}

impl Conn {
    async fn connect(url: &str) -> Result<Self> {
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .with_context(|| format!("连不上中继 {url}"))?;
        Ok(Self { ws })
    }

    async fn send(&mut self, f: &ClientFrame) -> Result<()> {
        let s = serde_json::to_string(f)?;
        self.ws.send(Message::text(s)).await.context("发送失败")?;
        Ok(())
    }

    async fn recv(&mut self, timeout: Duration) -> Result<ServerFrame> {
        loop {
            let m = tokio::time::timeout(timeout, self.ws.next())
                .await
                .context("等待中继响应超时")?
                .context("连接被关闭")?
                .context("ws 错误")?;
            if let Ok(t) = m.to_text() {
                return Ok(serde_json::from_str(t).context("中继返回了无法解析的帧")?);
            }
        }
    }

    async fn new_queue(&mut self) -> Result<String> {
        self.send(&ClientFrame::New { v: PROTOCOL_VERSION }).await?;
        match self.recv(Duration::from_secs(10)).await? {
            ServerFrame::NewOk { qid, .. } => Ok(qid),
            ServerFrame::Err { code, msg } => Err(anyhow!("建队列失败 [{code}] {msg}")),
            other => Err(anyhow!("建队列收到意外帧: {other:?}")),
        }
    }

    async fn sub(&mut self, qid: &str) -> Result<()> {
        self.send(&ClientFrame::Sub {
            qid: qid.to_string(),
        })
        .await?;
        match self.recv(Duration::from_secs(10)).await? {
            ServerFrame::Ok { .. } => Ok(()),
            // 订阅时可能有积压消息先到，属于正常情况
            ServerFrame::Msg { .. } => Ok(()),
            ServerFrame::Err { code, msg } => Err(anyhow!("订阅失败 [{code}] {msg}")),
            other => Err(anyhow!("订阅收到意外帧: {other:?}")),
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
        match self.recv(Duration::from_secs(10)).await? {
            ServerFrame::Ok { .. } => Ok(id),
            ServerFrame::Err { code, msg } => Err(anyhow!("投递失败 [{code}] {msg}")),
            other => Err(anyhow!("投递收到意外帧: {other:?}")),
        }
    }

    async fn ack(&mut self, qid: &str, ids: &[String]) -> Result<()> {
        // 不等回复，避免阻塞收消息
        self.send(&ClientFrame::Ack {
            qid: qid.to_string(),
            ids: ids.to_vec(),
        })
        .await
    }

    async fn recv_msg(&mut self, timeout: Duration) -> Result<(String, Vec<u8>)> {
        loop {
            match self.recv(timeout).await? {
                ServerFrame::Msg { id, body, .. } => return Ok((id, body)),
                ServerFrame::Ok { .. } | ServerFrame::Pong => continue,
                ServerFrame::Err { code, msg } => {
                    return Err(anyhow!("等消息时收到错误 [{code}] {msg}"))
                }
                other => return Err(anyhow!("等消息时收到意外帧: {other:?}")),
            }
        }
    }
}

// ---------------------------------------------------------------- demo

async fn demo(relay: &str) -> Result<()> {
    println!("中继: {relay}");

    let mut ca = Conn::connect(relay).await?;
    let qid_a = ca.new_queue().await?;
    ca.sub(&qid_a).await?;

    let mut cb = Conn::connect(relay).await?;
    let qid_b = cb.new_queue().await?;
    cb.sub(&qid_b).await?;

    println!("Alice 队列: {qid_a}");
    println!("Bob   队列: {qid_b}");

    let mut alice = Device::new();
    let mut bob = Device::new();
    let bob_bundle = bob.bundle();

    let inv = identity::Invite::new(
        bob_bundle.pk.clone(),
        bob_bundle.dh.clone(),
        bob_bundle.otk.clone(),
        qid_b.clone(),
        vec![relay.to_string()],
    )
    .map_err(|e| anyhow!(e))?
    .with_name("Bob");
    println!(
        "\nBob 的邀请码（真实客户端会把它做成二维码）：\n  {}\n",
        inv.encode()
    );

    alice.start_session(&bob_bundle.dh, &bob_bundle.otk)?;

    let text1 = "你好，我是 Alice";
    let w1 = alice.encrypt(
        &bob_bundle.dh,
        &Payload {
            v: PAYLOAD_VERSION,
            id: proto::new_message_id(),
            from: alice.id(),
            ts: proto::now_secs(),
            text: text1.into(),
            reply_qid: Some(qid_a.clone()),
            reply_relays: None,
            self_name: None,
            kind: secure::default_kind(),
            body: None,
        },
    )?;
    println!("[密文] Alice→Bob: {} 字节（中继只能看到这个）", w1.ct.len());

    ca.put(&qid_b, &w1).await?;

    let (_mid, body) = cb.recv_msg(Duration::from_secs(10)).await?;
    let wire = Wire::from_bytes(&body)?;
    let (peer_dh, p1) = bob.decrypt(None, &wire)?;
    println!("[明文] Bob  收到: {}", p1.text);
    if p1.text != text1 {
        return Err(anyhow!("Bob 收到的内容不对"));
    }
    println!("      发送方身份: {}", p1.from);

    let reply_qid = p1
        .reply_qid
        .ok_or_else(|| anyhow!("首条消息没带回复地址，无法回信"))?;
    let text2 = "收到，我是 Bob";
    let w2 = bob.encrypt(
        &peer_dh,
        &Payload {
            v: PAYLOAD_VERSION,
            id: proto::new_message_id(),
            from: bob.id(),
            ts: proto::now_secs(),
            text: text2.into(),
            reply_qid: Some(qid_b.clone()),
            reply_relays: None,
            self_name: None,
            kind: secure::default_kind(),
            body: None,
        },
    )?;
    cb.put(&reply_qid, &w2).await?;

    let (_id, body2) = ca.recv_msg(Duration::from_secs(10)).await?;
    let wire2 = Wire::from_bytes(&body2)?;
    let (_, p2) = alice.decrypt(Some(&bob_bundle.dh), &wire2)?;
    println!("[明文] Alice 收到: {}", p2.text);
    if p2.text != text2 {
        return Err(anyhow!("Alice 收到的内容不对"));
    }

    println!("\nOK 端到端链路打通：中继只经手密文，两端各自解密。");
    Ok(())
}

// ---------------------------------------------------------------- 通话信令自测
//
// 只跑信令，不跑媒体：SDP 是假的。目的是验证"信令能不能穿过真实加密会话和真实中继"，
// 以及状态机在两端是否一致。媒体层（WebRTC）只能在真机/模拟器上验。
//
// 必须在独立线程里跑：client 内部用自己的 tokio runtime + block_on，
// 在已有的 runtime 里再 block_on 会直接 panic。
fn cmd_calldemo(relay: &str) -> Result<()> {
    let relay = relay.to_string();
    std::thread::spawn(move || calldemo_inner(&relay))
        .join()
        .map_err(|_| anyhow!("自测线程崩溃"))?
}

fn calldemo_inner(relay: &str) -> Result<()> {
    let base = std::env::temp_dir().join(format!(
        "anonchat-calldemo-{}-{}",
        std::process::id(),
        proto::new_queue_id()
    ));
    let da = base.join("alice");
    let db = base.join("bob");
    std::fs::create_dir_all(&da)?;
    std::fs::create_dir_all(&db)?;
    let da = da.display().to_string();
    let db = db.display().to_string();

    println!("=== 1. 建立两个身份并互加 ===");
    client::init(&da, Some("Alice"))?;
    let inv_a = client::invite(&da, relay)?;
    client::init(&db, Some("Bob"))?;
    let inv_b = client::invite(&db, relay)?;
    client::add(&da, &inv_b, None)?;
    client::add(&db, &inv_a, None)?;
    println!("    Alice / Bob 已互加，各自队列已在中继上建立");

    println!("=== 2. Alice 发起通话 ===");
    let call_id = client::call_start(&da, "Bob", false)?;
    println!("    call_id = {call_id}");
    client::call_set_offer(&da, "v=0\r\ns=FakeOffer\r\n")?;
    println!("    offer 已发出（真实加密 + 真实中继）");

    println!("=== 3. Bob 收到 offer ===");
    let (kind, ev) = recv_call(&db)?;
    println!("    kind={} state={} video={}", kind, ev.state, ev.video);
    if ev.state != "incoming" {
        return Err(anyhow!("Bob 应进入 incoming，实际 {}", ev.state));
    }
    println!("    对端 SDP 已带出：{:?}", ev.remote_sdp);

    println!("=== 4. Bob 接听 ===");
    let ev = client::call_accept(&db, "v=0\r\ns=FakeAnswer\r\n")?;
    println!("    Bob 本地状态 = {}", ev.state);

    println!("=== 5. Alice 收到 answer ===");
    let (_inc, ev) = recv_call(&da)?;
    println!("    Alice 本地状态 = {}", ev.state);
    if ev.state != "active" {
        return Err(anyhow!("Alice 应进入 active，实际 {}", ev.state));
    }

    println!("=== 6. 双向 ICE 交换 ===");
    client::call_ice(&da, "candidate:1 1 UDP 2130706431 10.0.0.2 5000 typ host", "audio", 0)?;
    let (_inc, ev) = recv_call(&db)?;
    let cand = ev.ice.clone().map(|i| i.candidate).unwrap_or_default();
    println!("    Bob 收到候选: {}", &cand[..cand.len().min(48)]);
    if cand.is_empty() {
        return Err(anyhow!("Bob 没拿到 ICE 候选"));
    }

    client::call_ice(&db, "candidate:1 1 UDP 2130706431 10.0.0.3 6000 typ host", "audio", 0)?;
    let (_inc, ev) = recv_call(&da)?;
    if ev.ice.is_none() {
        return Err(anyhow!("Alice 没拿到反向 ICE 候选"));
    }
    println!("    Alice 收到反向候选，双向通");

    println!("=== 6.5 换网络重协商（ICE restart）===");
    client::call_renegotiate(&da, "v=0\r\ns=RestartOffer\r\n")?;
    let (kind, ev) = recv_call(&db)?;
    println!(
        "    Bob 收到 {kind} → state={} renegotiating={}",
        ev.state, ev.renegotiating
    );
    if ev.state != "active" || !ev.renegotiating {
        return Err(anyhow!(
            "重协商应保持 active 且置 renegotiating，实际 {} / {}",
            ev.state,
            ev.renegotiating
        ));
    }
    let ev = client::call_accept(&db, "v=0\r\ns=RestartAnswer\r\n")?;
    if ev.state != "active" {
        return Err(anyhow!("重协商应答后仍是 active，实际 {}", ev.state));
    }
    let (_kind, ev) = recv_call(&da)?;
    println!("    Alice 收到应答 → state={} renegotiating={}", ev.state, ev.renegotiating);
    if ev.renegotiating {
        return Err(anyhow!("应答到达后应清掉重协商标记"));
    }

    println!("=== 7. 忙线保护：第三人打进来 ===");
    let dc = base.join("carol");
    std::fs::create_dir_all(&dc)?;
    let dc = dc.display().to_string();
    client::init(&dc, Some("Carol"))?;
    let inv_c = client::invite(&dc, relay)?;
    client::add(&db, &inv_c, None)?;   // Carol 成为 Bob 的联系人
    client::add(&dc, &{
        // Carol 也需要 Bob 的邀请（新生成一次，OTK 一次性）
        client::invite(&db, relay)?
    }, None)?;
    let _ = client::call_start(&dc, "Bob", false)?;
    client::call_set_offer(&dc, "v=0\r\ns=CarolOffer\r\n")?;

    // Bob 通话中，应当自动拒接
    let (inc, ev) = match client::recv_once(&db, 15)? {
        Some(i) => {
            let e = client::call_inbound(&db, &i.dh, &i.kind, i.body.as_deref().unwrap_or(""), &i.msg_id)?;
            (i, e)
        }
        None => return Err(anyhow!("Bob 没收到 Carol 的 offer")),
    };
    println!("    来电 kind={} → Bob 状态仍是 {}", inc.kind, ev.state);
    if ev.state != "active" {
        return Err(anyhow!("忙线时不能改变当前通话，实际 {}", ev.state));
    }

    // Carol 应当收到 reject(busy)
    match client::recv_once(&dc, 15)? {
        Some(i) => {
            let e = client::call_inbound(&dc, &i.dh, &i.kind, i.body.as_deref().unwrap_or(""), &i.msg_id)?;
            println!("    Carol 收到 {} → state={} reason={}", i.kind, e.state, e.reason);
            if e.reason != "busy" {
                return Err(anyhow!("Carol 应收到 busy，实际 {}", e.reason));
            }
        }
        None => return Err(anyhow!("Carol 没收到拒接")),
    }

    println!("=== 8. Alice 挂断 ===");
    client::call_hangup(&da, "normal")?;
    let (_inc, ev) = recv_call(&db)?;
    println!("    Bob 状态 = {} reason={}", ev.state, ev.reason);
    if ev.state != "ended" {
        return Err(anyhow!("Bob 应收到 ended，实际 {}", ev.state));
    }

    std::fs::remove_dir_all(&base).ok();
    println!("\n全部通过：信令在真实加密会话与真实中继上完整走通");
    Ok(())
}

/// 收一条通话信令并交给状态机
fn recv_call(dir: &str) -> Result<(String, client::CallEvent)> {
    let inc = client::recv_once(dir, 15)?
        .ok_or_else(|| anyhow!("{dir} 等信令超时"))?;
    if !client::call_kind::is_call(&inc.kind) {
        return Err(anyhow!("期望通话信令，收到 {}", inc.kind));
    }
    let ev = client::call_inbound(
        dir,
        &inc.dh,
        &inc.kind,
        inc.body.as_deref().unwrap_or(""),
        &inc.msg_id,
    )?;
    Ok((inc.kind, ev))
}

/// 给某个联系人发一个 offer。SDP 是假的，只用来驱动对端的来电界面。
/// 真机/模拟器联调时，主机侧用它把 App 叫起来，省去手点。
fn cmd_calloffer(args: &[String]) -> Result<()> {
    let dir = opt(args, "--dir").ok_or_else(|| anyhow!("缺少 --dir"))?.to_string();
    let to = opt(args, "--to").ok_or_else(|| anyhow!("缺少 --to"))?.to_string();
    let video = args.iter().any(|a| a == "--video");
    // 同 calldemo：client 内部自己 block_on，必须在独立线程里跑，
    // 否则在 tokio 运行时里再 block_on 会直接 panic
    std::thread::spawn(move || calloffer_inner(&dir, &to, video))
        .join()
        .map_err(|_| anyhow!("发送线程崩溃"))?
}

fn calloffer_inner(dir: &str, to: &str, video: bool) -> Result<()> {
    let (dir, to) = (dir.to_string(), to.to_string());
    let call_id = client::call_start(&dir, &to, video)?;
    println!("call_id = {call_id}");
    // 一个足够像样的假 SDP：某些实现会检查 v= / m= 行是否存在
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=FakeCall\r\nt=0 0\r\n\
               m=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=mid:audio\r\n\
               a=sendrecv\r\n";
    client::call_set_offer(&dir, sdp)?;
    println!("offer 已发送给 {to}（video={video}）");
    Ok(())
}

/// 前台收电话信令并打印状态迁移。用来验证对端（手机）到底回了什么。
fn cmd_callrecv(args: &[String]) -> Result<()> {
    let dir = opt(args, "--dir").ok_or_else(|| anyhow!("缺少 --dir"))?.to_string();
    let secs: u64 = opt(args, "--secs").and_then(|s| s.parse().ok()).unwrap_or(30);
    std::thread::spawn(move || callrecv_inner(&dir, secs))
        .join()
        .map_err(|_| anyhow!("接收线程崩溃"))?
}

fn callrecv_inner(dir: &str, secs: u64) -> Result<()> {
    let dir = dir.to_string();
    let start = std::time::Instant::now();
    println!("监听 {secs} 秒…");
    while start.elapsed().as_secs() < secs {
        match client::recv_once(&dir, 5) {
            Ok(Some(inc)) => {
                if client::call_kind::is_call(&inc.kind) {
                    let ev = client::call_inbound(
                        &dir,
                        &inc.dh,
                        &inc.kind,
                        inc.body.as_deref().unwrap_or(""),
                        &inc.msg_id,
                    )?;
                    println!(
                        "[{}] {} → state={} reason={} sdp_len={}",
                        inc.kind,
                        if inc.from_name.is_some() { inc.from_name.clone().unwrap() } else { inc.dh.chars().take(8).collect() },
                        ev.state,
                        ev.reason,
                        ev.remote_sdp.map(|s| s.len()).unwrap_or(0),
                    );
                } else {
                    println!("[文本] {}", inc.text);
                }
            }
            Ok(None) => {}
            Err(e) => println!("[错误] {e:#}"),
        }
    }
    Ok(())
}
