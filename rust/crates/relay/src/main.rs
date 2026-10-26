//! 阶段 0 · 验证项 0.2：无标识队列中继 + 长连接稳定性
//!
//! 这个中继是**故意做成"傻"的**：
//! - 没有任何用户表、没有账号、不认识发送方；
//! - 队列 ID 由接收方生成，中继只按 ID 存消息；
//! - 消息投递后等 ACK 才删，超时自动清（避免无限堆积）。
//!
//! TLS 不在这里做：生产部署由 Caddy / nginx 在前面终止 TLS 并反代 WebSocket，
//! 中继只监听 127.0.0.1。这样证书续期、HTTP/2、伪装成普通 HTTPS 站点的
//! 工作全部交给成熟组件，中继代码保持最小攻击面。
//!
//! 用法：
//!   relay serve --addr 127.0.0.1:8080 [--data ./data] [--fsync] [--rate 120]
//!   relay load  --url ws://127.0.0.1:8080 --clients 20 --secs 60
//!
//! 压测输出可直接用于 0.2 的通过标准：掉线率 <1%/天。

use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::process::exit;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

mod storage;
mod turn;

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use proto::{ClientFrame, ServerFrame, MAX_BODY_LEN, PROTOCOL_VERSION, QUEUE_TTL_SECS};
use serde_json::json;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

use storage::Log;

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage();
        exit(2);
    }
    let r = match args[1].as_str() {
        "serve" => serve(&args[2..]).await,
        "load" => load(&args[2..]).await,
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
  relay serve --addr 127.0.0.1:8080 [--data ./data] [--fsync] [--rate 120]
              [--dht-announce [对外端口]] [--dht-interval 600]
              [--turn 0.0.0.0:3478 --turn-external-ip <公网IP>
               --turn-user <用户> --turn-pass <密码>
               --turn-realm anonchat --turn-port-min 49160 --turn-port-max 49200
               --turn-max-per-ip 8] [--turn-allow-private-peers]
  relay load  --url ws://127.0.0.1:8080 [--clients 20] [--secs 60] [--ping 5]

  --dht-announce 把本中继公布到 BitTorrent Mainline DHT，客户端可自动发现。
                  不跟端口时用 --addr 里的端口；反代场景请显式给对外端口（如 443）。"
    );
}

fn opt<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

// ------------------------------------------------------------------ 状态

struct StoredMsg {
    id: String,
    body: Vec<u8>,
    ts: u64,
}

struct Queue {
    msgs: Vec<StoredMsg>,
    expires_at: u64,
}

struct State {
    queues: HashMap<String, Queue>,
    /// 每个队列当前在线的订阅者。连接断开时必须清理，否则内存泄漏。
    subs: HashMap<String, Vec<mpsc::UnboundedSender<ServerFrame>>>,
    log: Log,
    /// 每连接每分钟最多投递多少条。0 = 不限制（仅测试用）
    rate_limit: u32,
    /// 对外公布的 TURN 地址（`turn:1.2.3.4:3478`）。None = 没开 TURN。
    ///
    /// 客户端通过 `GetTurn` 帧来取：DHT 里只能放 IP+端口，
    /// 账号密码放不进去，所以凭据必须走这条已建立的加密通道。
    turn_url: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            queues: HashMap::new(),
            subs: HashMap::new(),
            log: Log::memory(),
            rate_limit: 120,
            turn_url: None,
        }
    }
}

impl State {
    fn sweep(&mut self) -> usize {
        let now = proto::now_secs();
        let before = self.queues.len();
        self.queues.retain(|_, q| q.expires_at > now);
        // 顺手清掉没有订阅者且空的队列由 TTL 负责，这里只清断开的订阅者
        for v in self.subs.values_mut() {
            v.retain(|tx| !tx.is_closed());
        }
        self.subs.retain(|_, v| !v.is_empty());
        before - self.queues.len()
    }
}

// ------------------------------------------------------------------ 服务端

static LIVE_CONNS: AtomicU64 = AtomicU64::new(0);
static TOTAL_CONNS: AtomicU64 = AtomicU64::new(0);

async fn serve(args: &[String]) -> Result<()> {
    let addr: SocketAddr = opt(args, "--addr")
        .unwrap_or("127.0.0.1:8080")
        .parse()
        .map_err(|_| anyhow!("--addr 格式应为 ip:port"))?;

    // 持久化：不指定 --data 时用 ./data；队列重启不丢
    let data_dir = opt(args, "--data").unwrap_or("./data");
    let fsync = args.iter().any(|a| a == "--fsync");
    let rate_limit: u32 = opt(args, "--rate")
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    let (log, loaded) = Log::open(std::path::Path::new(data_dir), fsync)?;
    let mut st0 = State {
        queues: loaded.queues,
        subs: HashMap::new(),
        log,
        rate_limit,
        // TURN 还没配置，稍后若开启会补上
        turn_url: None,
    };
    let expired = st0.sweep();
    let n_queues = st0.queues.len();
    let n_msgs: usize = st0.queues.values().map(|q| q.msgs.len()).sum();
    let state = Arc::new(Mutex::new(st0));

    println!(
        "[relay] 数据目录 {data_dir}：恢复 {n_queues} 个队列 / {n_msgs} 条未确认消息，清理过期 {expired} 个，损坏行 {}",
        loaded.corrupted_lines
    );
    if fsync {
        println!("[relay] fsync 已开启（每条写入落盘，吞吐会明显下降）");
    }
    println!("[relay] 速率限制：每连接 {rate_limit} 条/分钟");

    let listener = TcpListener::bind(addr).await?;
    println!("[relay] 监听 ws://{addr}  (协议 v{PROTOCOL_VERSION})");
    println!("[relay] 提示：生产环境请在前面挂 Caddy/nginx 终止 TLS，中继只监听 127.0.0.1");

    // ---- 内嵌 TURN（可选）
    //
    // 不传 --turn 时行为与旧版完全一致（纯 WS 中继），方便回退。
    // 传了就必须给全：外网 IP（NAT 通告）、账号密码（不做匿名 TURN，否则等于开放代理）。
    let mut turn_port_for_dht: Option<u16> = None;
    let mut turn_url: Option<String> = None;
    if let Some(turn_addr) = opt(args, "--turn") {
        let listen: SocketAddr = turn_addr
            .parse()
            .map_err(|_| anyhow!("--turn 格式应为 ip:port"))?;
        turn_port_for_dht = Some(listen.port());
        let ext_ip: std::net::IpAddr = opt(args, "--turn-external-ip")
            .ok_or_else(|| anyhow!("--turn 需要同时给 --turn-external-ip（NAT 后的公网 IP）"))?
            .parse()
            .map_err(|_| anyhow!("--turn-external-ip 不是合法 IP"))?;
        let user = opt(args, "--turn-user").unwrap_or("").to_string();
        let pass = opt(args, "--turn-pass").unwrap_or("").to_string();
        if user.is_empty() || pass.is_empty() {
            return Err(anyhow!(
                "--turn 必须同时给 --turn-user 与 --turn-pass（不提供匿名 TURN）"
            ));
        }
        let realm = opt(args, "--turn-realm").unwrap_or("anonchat").to_string();
        let port_min: u16 = opt(args, "--turn-port-min")
            .and_then(|s| s.parse().ok())
            .unwrap_or(49160);
        let port_max: u16 = opt(args, "--turn-port-max")
            .and_then(|s| s.parse().ok())
            .unwrap_or(49200);
        if port_min > port_max {
            return Err(anyhow!("--turn-port-min 不能大于 --turn-port-max"));
        }
        let max_per_ip: usize = opt(args, "--turn-max-per-ip")
            .and_then(|s| s.parse().ok())
            .unwrap_or(8);
        // 默认拒绝把数据中转给回环/内网地址（TURN 是经典 SSRF 跳板）；本机联调才打开
        let allow_private_peers = args.iter().any(|a| a == "--turn-allow-private-peers");
        let mut users = HashMap::new();
        users.insert(user, pass);
        // 记下对外地址：客户端索取凭据时要连带告诉它连哪儿
        turn_url = Some(format!("turn:{ext_ip}:{}", listen.port()));
        let cfg = turn::Config {
            listen,
            external_ip: ext_ip,
            realm,
            users,
            port_min,
            port_max,
            max_per_ip,
            allow_private_peers,
        };
        tokio::spawn(async move {
            if let Err(e) = turn::run(cfg).await {
                eprintln!("[turn] 服务退出: {e:#}");
            }
        });
    }
    if let Some(u) = turn_url.clone() {
        // 客户端索取凭据时要连带告诉它连哪个地址
        state.lock().await.turn_url = Some(u.clone());
        println!("[turn] 客户端可通过 WS 索取临时凭据，对外地址 {u}（TTL 1 小时）");
    }

    // ---- Mainline DHT 自注册（可选，默认关闭）
    //
    // 开了之后中继会把自己公布到 BitTorrent DHT，客户端无需手输地址即可发现。
    // 默认关闭是刻意的：公布意味着地址公开可枚举，运营者应当自己决定。
    //
    // 公布用的端口 ≠ 监听端口是很常见的部署形态（Caddy 反代 443 -> 127.0.0.1:8080），
    // 所以 --dht-announce 可以带一个显式端口：`--dht-announce 443`。
    if let Some(dcfg) = dht::DhtConfig::from_args(
        args,
        std::path::Path::new(data_dir),
        addr.port(),
        turn_port_for_dht,
    ) {
        // 监听在回环地址时给个明确警告：公布出去的 IP 由 DHT 按来源判定，
        // 本机回环出去的来源是公网出口 IP，但端口若只监听 127.0.0.1 则外部连不上。
        if addr.ip().is_loopback() && opt(args, "--dht-announce").is_some() {
            println!(
                "[dht] 注意：中继只监听回环地址（{addr}）。请在前面用 Caddy/nginx 反代，\
                 并把 --dht-announce 设成**对外实际端口**（例如 --dht-announce 443）"
            );
        }
        println!(
            "[dht] 已启用：将公布 WS 端口 {}{}，每 {}s 一次",
            dcfg.ws_port,
            dcfg.turn_port.map(|p| format!(" / TURN 端口 {p}")).unwrap_or_default(),
            dcfg.interval.as_secs()
        );
        tokio::spawn(async move { dht::run(dcfg).await });
    }

    // TTL 清理
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let mut st = state.lock().await;
                let n = st.sweep();
                if n > 0 {
                    println!("[relay] TTL 清理 {n} 个过期队列");
                }
                let need_compact = st.log.should_compact(&st.queues);
                if need_compact {
                    // MutexGuard 的 Deref 让 `&mut st.log` 与 `&st.queues` 无法同时借，
                    // 所以先把日志临时取出来压缩，再放回去。
                    let mut log = std::mem::replace(&mut st.log, Log::memory());
                    let res = log.compact(&st.queues);
                    st.log = log;
                    match res {
                        Ok(_) => println!("[relay] 日志已压缩"),
                        Err(e) => eprintln!("[relay] 日志压缩失败: {e:#}"),
                    }
                }
            }
        });
    }

    accept_loop(listener, state).await
}

/// 抽出来是为了让测试能在临时端口上起一个同样的服务
async fn accept_loop(listener: TcpListener, state: Arc<Mutex<State>>) -> Result<()> {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[relay] accept 出错: {e}");
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            TOTAL_CONNS.fetch_add(1, Ordering::Relaxed);
            LIVE_CONNS.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = handle_conn(stream, state, peer).await {
                eprintln!("[relay] 连接 {peer} 结束: {e:#}");
            }
            LIVE_CONNS.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

async fn handle_conn(stream: TcpStream, state: Arc<Mutex<State>>, peer: SocketAddr) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut src) = ws.split();

    // 出站队列：回复、订阅推送都走这里，避免在持锁时 await 网络
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerFrame>();

    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let text = match serde_json::to_string(&frame) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if sink.send(Message::text(text)).await.is_err() {
                break;
            }
        }
    });

    // 本连接订阅过的队列，断开时统一清理
    let mut my_subs: Vec<String> = Vec::new();

    // 速率限制：每连接一个固定窗口计数器。
    // 没有这个，任何人写个脚本就能把中继磁盘写满——这是当前最大的现实风险。
    let rate_limit = state.lock().await.rate_limit;
    let mut window_start = std::time::Instant::now();
    let mut puts_in_window: u32 = 0;

    while let Some(msg) = src.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[relay] ws 读错误（{peer}）: {e}");
                break;
            }
        };
        if msg.is_close() {
            break;
        }
        let text = match msg.to_text() {
            Ok(t) => t,
            Err(_) => {
                // 二进制帧阶段 0 不支持
                let _ = tx.send(ServerFrame::err("bad_frame", "仅支持文本 JSON 帧"));
                continue;
            }
        };

        let frame: ClientFrame = match serde_json::from_str(text) {
            Ok(f) => f,
            Err(e) => {
                let _ = tx.send(ServerFrame::err("bad_json", e.to_string()));
                continue;
            }
        };

        // 字段长度统一设防：body 有 64KiB 上限，但 qid/id 是任意长 String——
        // 一条 Put 携带 16MB 的 id 就能在"合法速率"内灌盘灌内存（二轮审计 P2-3）。
        // qid 恒为 32 hex、id 恒为 32 hex（proto 生成），超长的一律 bad_frame。
        if !frame.check_sizes() {
            let _ = tx.send(ServerFrame::err("bad_frame", "字段长度不合法"));
            continue;
        }

        match frame {
            ClientFrame::New { v } => {
                if v != PROTOCOL_VERSION {
                    let _ = tx.send(ServerFrame::err("bad_version", format!("需要 v{PROTOCOL_VERSION}")));
                    continue;
                }
                // 建队列与投递共用同一速率窗口：每个 New 都会写一行 30 天 TTL 的
                // 日志，不限速的话匿名中继上一个脚本就能把磁盘灌满（put 有
                // 限制而 new 没有，等于只锁了前门）。
                if rate_limit > 0 {
                    if window_start.elapsed() >= Duration::from_secs(60) {
                        window_start = std::time::Instant::now();
                        puts_in_window = 0;
                    }
                    if puts_in_window >= rate_limit {
                        let _ = tx.send(ServerFrame::err(
                            "rate_limited",
                            format!("超过速率限制（{rate_limit} 条/分钟），请稍后重试"),
                        ));
                        continue;
                    }
                    puts_in_window += 1;
                }
                let qid = proto::new_queue_id();
                let exp = proto::now_secs() + QUEUE_TTL_SECS;
                let mut st = state.lock().await;
                st.queues.insert(
                    qid.clone(),
                    Queue {
                        msgs: Vec::new(),
                        expires_at: exp,
                    },
                );
                // 落盘失败不阻断服务：内存状态仍是对的，客户端至少能正常收发。
                // 但必须吵闹地记录——静默丢持久化比直接报错更危险。
                if let Err(e) = st.log.append_new(&qid, exp) {
                    eprintln!("[relay] 持久化失败（建队列 {qid}）: {e:#}");
                }
                drop(st);
                let _ = tx.send(ServerFrame::NewOk {
                    qid,
                    ttl: QUEUE_TTL_SECS,
                });
            }

            ClientFrame::Sub { qid } => {
                let mut st = state.lock().await;
                // 队列必须存在才注册：对任意字符串无脑注册的话，一个连接
                // 高速发海量 Sub 就能让 subs 表内存膨胀（sweep 只清 closed
                // 通道，不清没有对应队列的条目）。合法流程 New 必然先于 Sub。
                if !st.queues.contains_key(&qid) {
                    drop(st);
                    let _ = tx.send(ServerFrame::err("no_queue", "队列不存在或已过期"));
                    continue;
                }
                // 同连接对同队列重复 Sub：只注册一次。重复注册的话之后每条
                // 消息这个连接会收到 N 份重复推送（客户端去重能兜，但白费带宽）。
                let already = st.subs.get(&qid).map(|v| v.iter().any(|x| x.same_channel(&tx)));
                if already != Some(true) {
                    st.subs.entry(qid.clone()).or_default().push(tx.clone());
                    my_subs.push(qid.clone());
                }
                // 先回确认，再补投积压。
                // 顺序不能反：积压不为空时若跳过 OK 直接推 Msg，客户端在等
                // "订阅确认"时会把第一条消息当成确认帧吞掉，那条消息就永久丢了。
                let _ = tx.send(ServerFrame::Ok {
                    ref_id: format!("sub:{qid}"),
                });
                if let Some(q) = st.queues.get(&qid) {
                    for m in &q.msgs {
                        let _ = tx.send(ServerFrame::Msg {
                            qid: qid.clone(),
                            id: m.id.clone(),
                            body: m.body.clone(),
                            ts: m.ts,
                        });
                    }
                }
                drop(st);
            }

            ClientFrame::Put { qid, id, body } => {
                if rate_limit > 0 {
                    if window_start.elapsed() >= Duration::from_secs(60) {
                        window_start = std::time::Instant::now();
                        puts_in_window = 0;
                    }
                    if puts_in_window >= rate_limit {
                        let _ = tx.send(ServerFrame::err(
                            "rate_limited",
                            format!("超过速率限制（{rate_limit} 条/分钟），请稍后重试"),
                        ));
                        continue;
                    }
                    puts_in_window += 1;
                }
                if body.len() > MAX_BODY_LEN {
                    let _ = tx.send(ServerFrame::err("too_large", "消息体超过 64KiB"));
                    continue;
                }
                let mut st = state.lock().await;
                if !st.queues.contains_key(&qid) {
                    drop(st);
                    let _ = tx.send(ServerFrame::err("no_queue", "队列不存在或已过期"));
                    continue;
                }

                // 幂等：同 id 重复投递既不重复入队，**也不重复推送**。
                // 只去重入队而不拦推送的话，对端重放同一个 id 就能无限轰炸接收方。
                let ts = proto::now_secs();
                let mut inserted = false;
                if let Some(q) = st.queues.get_mut(&qid) {
                    if !q.msgs.iter().any(|m| m.id == id) {
                        q.msgs.push(StoredMsg {
                            id: id.clone(),
                            body: body.clone(),
                            ts,
                        });
                        inserted = true;
                    }
                }

                if inserted {
                    if let Err(e) = st.log.append_put(&qid, &id, &body, ts) {
                        eprintln!("[relay] 持久化失败（投递 {id}）: {e:#}");
                    }
                }

                if inserted {
                    let msg = ServerFrame::Msg {
                        qid: qid.clone(),
                        id: id.clone(),
                        body: body.clone(),
                        ts,
                    };
                    if let Some(subs) = st.subs.get(&qid) {
                        for s in subs {
                            let _ = s.send(msg.clone());
                        }
                    }
                }
                drop(st);
                let _ = tx.send(ServerFrame::Ok { ref_id: id });
            }

            ClientFrame::Get { qid } => {
                let st = state.lock().await;
                match st.queues.get(&qid) {
                    Some(q) => {
                        for m in &q.msgs {
                            let _ = tx.send(ServerFrame::Msg {
                                qid: qid.clone(),
                                id: m.id.clone(),
                                body: m.body.clone(),
                                ts: m.ts,
                            });
                        }
                    }
                    None => {
                        let _ = tx.send(ServerFrame::err("no_queue", "队列不存在或已过期"));
                    }
                }
            }

            ClientFrame::Ack { qid, ids } => {
                // Ack 与 put/new/get_turn 共享速率窗口：ids 数组无上限、队列
                // 不存在也照写日志的话，一个匿名连接就能把第一轮想堵的磁盘
                // DoS 从侧门重新打开（二轮审计 P1-2）。
                if rate_limit > 0 {
                    if window_start.elapsed() >= Duration::from_secs(60) {
                        window_start = std::time::Instant::now();
                        puts_in_window = 0;
                    }
                    if puts_in_window >= rate_limit {
                        let _ = tx.send(ServerFrame::err(
                            "rate_limited",
                            format!("超过速率限制（{rate_limit} 条/分钟），请稍后重试"),
                        ));
                        continue;
                    }
                    puts_in_window += 1;
                }
                // ids 条数上限：一条 ack 里带几万个 id 既是放大写盘的载体，
                // 也让下面的 retain 退化为 O(消息数×ids数)。64 远超正常客户端
                // 的单批确认量（分批 ack 是客户端既有行为）。
                if ids.len() > 64 {
                    let _ = tx.send(ServerFrame::err("bad_frame", "ack 一次最多确认 64 条"));
                    continue;
                }
                let mut st = state.lock().await;
                let exists = st.queues.contains_key(&qid);
                if exists {
                    if let Some(q) = st.queues.get_mut(&qid) {
                        q.msgs.retain(|m| !ids.contains(&m.id));
                    }
                    if let Err(e) = st.log.append_ack(&qid, &ids) {
                        eprintln!("[relay] 持久化失败（ACK）: {e:#}");
                    }
                }
                drop(st);
                let _ = tx.send(ServerFrame::Ok { ref_id: "ack".into() });
            }

            ClientFrame::Ping => {
                let _ = tx.send(ServerFrame::Pong);
            }

            // 通话前索取 TURN 临时凭据。
            //
            // 每次请求都现签发一组新的：客户端换网络重连时会再来要，
            // 旧的自然过期，不需要吊销逻辑。
            ClientFrame::GetTurn => {
                // 与 put/new 共享速率窗口：凭据表虽有过期清理，但不限速的话
                // 一个脚本就能让服务端不停做哈希与表插入（轻微 DoS）。
                if rate_limit > 0 {
                    if window_start.elapsed() >= Duration::from_secs(60) {
                        window_start = std::time::Instant::now();
                        puts_in_window = 0;
                    }
                    if puts_in_window >= rate_limit {
                        let _ = tx.send(ServerFrame::err(
                            "rate_limited",
                            format!("超过速率限制（{rate_limit} 条/分钟），请稍后重试"),
                        ));
                        continue;
                    }
                    puts_in_window += 1;
                }
                let url = {
                    let st = state.lock().await;
                    st.turn_url.clone()
                };
                match url {
                    Some(url) => {
                        let (user, pass, expires) = turn::auth::issue_ephemeral(3600);
                        let ttl = expires.saturating_sub(proto::now_secs());
                        let _ = tx.send(ServerFrame::TurnCreds {
                            url,
                            user,
                            pass,
                            ttl,
                        });
                    }
                    // 没开 TURN：明确告诉客户端"别指望我"，让它回退到自填配置
                    None => {
                        let _ = tx.send(ServerFrame::err(
                            "no_turn",
                            "这个中继没有开启 TURN",
                        ));
                    }
                }
            }
        }
    }

    // 清理订阅，防止向已断开的连接持续推送
    {
        let mut st = state.lock().await;
        for qid in my_subs {
            if let Some(v) = st.subs.get_mut(&qid) {
                v.retain(|x| !x.is_closed());
                if v.is_empty() {
                    st.subs.remove(&qid);
                }
            }
        }
    }
    writer.abort();
    Ok(())
}

// ------------------------------------------------------------------ 压测客户端

/// 模拟"真实用户前台服务里的那条长连接"：连上 → 建队列 → 定时 ping。
/// 断线立即指数退避重连，统计掉线次数和在线时长占比。
async fn load(args: &[String]) -> Result<()> {
    let url = opt(args, "--url")
        .unwrap_or("ws://127.0.0.1:8080")
        .to_string();
    let clients: usize = opt(args, "--clients").and_then(|s| s.parse().ok()).unwrap_or(10);
    let secs: u64 = opt(args, "--secs").and_then(|s| s.parse().ok()).unwrap_or(60);
    let ping: u64 = opt(args, "--ping").and_then(|s| s.parse().ok()).unwrap_or(5);

    println!("[load] {clients} 条连接 × {secs}s，ping 间隔 {ping}s → {url}");

    let drops = Arc::new(AtomicU64::new(0));
    let online_ms = Arc::new(AtomicU64::new(0));
    let pings = Arc::new(AtomicU64::new(0));
    let rtt_sum = Arc::new(AtomicU64::new(0));
    let conn_fail = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for i in 0..clients {
        let url = url.clone();
        let drops = drops.clone();
        let online_ms = online_ms.clone();
        let pings = pings.clone();
        let rtt_sum = rtt_sum.clone();
        let conn_fail = conn_fail.clone();
        handles.push(tokio::spawn(async move {
            one_client(
                i, url, secs, ping, drops, online_ms, pings, rtt_sum, conn_fail,
            )
            .await;
        }));
    }
    for h in handles {
        let _ = h.await;
    }

    let total_ms = (clients as u64) * secs * 1000;
    let online = online_ms.load(Ordering::Relaxed);
    let d = drops.load(Ordering::Relaxed);
    let p = pings.load(Ordering::Relaxed);
    let rtt = rtt_sum.load(Ordering::Relaxed);
    let fails = conn_fail.load(Ordering::Relaxed);
    // 单条连接的实际存活时间可能略超 secs（tick 边界），分子会略大于分母，封顶到 1.0
    let ratio = if total_ms > 0 {
        (online as f64 / total_ms as f64).min(1.0)
    } else {
        0.0
    };
    // 按在线时长比例外推到 24 小时
    let est_per_day = if online > 0 {
        d as f64 / (online as f64 / 86_400_000.0)
    } else {
        f64::INFINITY
    };

    println!(
        "SUMMARY clients={clients} secs={secs} conn_fail={fails} drops={d} online_ratio={ratio:.4} ping_avg_ms={:.1} est_drops_per_day={est_per_day:.2}",
        if p > 0 { rtt as f64 / p as f64 / 1000.0 } else { 0.0 }
    );

    // 注意：这里必须先判"一个连接都没成功"的情况。
    // 否则 online=0 会让 est_per_day 算成 0，把一个彻底连不上的配置误报成 PASS。
    let verdict = if online == 0 {
        "FAIL（全程未能建立任何连接：检查地址/端口/TLS/防火墙，或中继是否在运行）"
    } else if est_per_day < clients as f64 {
        "PASS（估算日掉线 < 每连接 1 次）"
    } else {
        "FAIL（掉线过于频繁，需要排查链路或心跳参数）"
    };
    println!("VERDICT {verdict}");

    let _ = json!({}); // 保持 serde_json 依赖显式可见
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn one_client(
    idx: usize,
    url: String,
    secs: u64,
    ping: u64,
    drops: Arc<AtomicU64>,
    online_ms: Arc<AtomicU64>,
    pings: Arc<AtomicU64>,
    rtt_sum: Arc<AtomicU64>,
    conn_fail: Arc<AtomicU64>,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut backoff = 1u64;

    while tokio::time::Instant::now() < deadline {
        let conn = tokio_tungstenite::connect_async(&url).await;
        let (mut ws, _) = match conn {
            Ok(v) => v,
            Err(e) => {
                conn_fail.fetch_add(1, Ordering::Relaxed);
                if idx == 0 && conn_fail.load(Ordering::Relaxed) == 1 {
                    eprintln!("[load] 连接失败：{e}");
                }
                drops.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
                continue;
            }
        };
        backoff = 1;

        // 建一个队列并订阅，模拟真实连接在做的两件事
        let new = serde_json::to_string(&ClientFrame::New { v: PROTOCOL_VERSION }).unwrap();
        if ws.send(Message::text(new)).await.is_err() {
            drops.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let qid = match ws.next().await {
            Some(Ok(Message::Text(t))) => serde_json::from_str::<ServerFrame>(&t)
                .ok()
                .and_then(|f| match f {
                    ServerFrame::NewOk { qid, .. } => Some(qid),
                    _ => None,
                })
                .unwrap_or_default(),
            _ => {
                drops.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let sub = serde_json::to_string(&ClientFrame::Sub { qid }).unwrap();
        let _ = ws.send(Message::text(sub)).await;

        let connected_at = tokio::time::Instant::now();
        let mut tick = tokio::time::interval(Duration::from_secs(ping.max(1)));
        let mut alive = true;

        while tokio::time::Instant::now() < deadline {
            tick.tick().await;
            let sent_at = tokio::time::Instant::now();
            let p = serde_json::to_string(&ClientFrame::Ping).unwrap();
            if ws.send(Message::text(p)).await.is_err() || !wait_pong(&mut ws).await {
                alive = false;
                break;
            }
            // 用微秒累加：本机/同城链路的 RTT 常常不足 1ms，按毫秒取整会全变 0
            rtt_sum.fetch_add(sent_at.elapsed().as_micros() as u64, Ordering::Relaxed);
            pings.fetch_add(1, Ordering::Relaxed);
        }

        online_ms.fetch_add(connected_at.elapsed().as_millis() as u64, Ordering::Relaxed);
        if !alive {
            drops.fetch_add(1, Ordering::Relaxed);
            if idx == 0 {
                println!("[load] 连接 #{idx} 断开，进入退避重连");
            }
        }
    }
}

async fn wait_pong<S>(ws: &mut S) -> bool
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let t = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    if let Ok(ServerFrame::Pong) = serde_json::from_str::<ServerFrame>(&t) {
                        return true;
                    }
                    // 其他帧（比如推送）跳过
                }
                Some(Ok(_)) => continue,
                _ => return false,
            }
        }
    })
    .await;
    t.unwrap_or(false)
}

// ------------------------------------------------------------------ 集成测试

#[cfg(test)]
mod tests {
    use super::*;

    type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

    async fn connect(addr: SocketAddr) -> Ws {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .expect("连不上中继");
        ws
    }

    async fn send(ws: &mut Ws, f: &ClientFrame) {
        let s = serde_json::to_string(f).unwrap();
        ws.send(Message::text(s)).await.unwrap();
    }

    /// 读取下一帧并反序列化（跳过非文本帧）
    async fn recv(ws: &mut Ws) -> ServerFrame {
        loop {
            let m = tokio::time::timeout(Duration::from_secs(3), ws.next())
                .await
                .expect("等帧超时")
                .expect("连接关闭")
                .expect("ws 错误");
            if let Ok(t) = m.to_text() {
                return serde_json::from_str(t).unwrap();
            }
        }
    }

    async fn start_test_server() -> SocketAddr {
        start_test_server_with_rate(0).await
    }

    async fn start_test_server_with_rate(rate: u32) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut st = State::default();
        st.rate_limit = rate;
        let state = Arc::new(Mutex::new(st));
        tokio::spawn(accept_loop(listener, state));
        addr
    }

    /// 完整走一遍：建队列 → 订阅 → 他人投递 → 实时收到 → 幂等 → ACK 后清空
    #[tokio::test]
    async fn queue_lifecycle() {
        let addr = start_test_server().await;

        // 接收方（Alice）建队列并订阅
        let mut alice = connect(addr).await;
        send(&mut alice, &ClientFrame::New { v: PROTOCOL_VERSION }).await;
        let qid = match recv(&mut alice).await {
            ServerFrame::NewOk { qid, .. } => qid,
            other => panic!("期望 NewOk，实际 {other:?}"),
        };
        assert_eq!(qid.len(), 32, "队列 ID 应为 32 位 hex");

        send(&mut alice, &ClientFrame::Sub { qid: qid.clone() }).await;
        assert!(matches!(recv(&mut alice).await, ServerFrame::Ok { .. }));

        // 发送方（Bob）投递，完全不知道 Alice 是谁，只知道队列 ID
        let mut bob = connect(addr).await;
        send(
            &mut bob,
            &ClientFrame::Put {
                qid: qid.clone(),
                id: "m1".into(),
                body: b"hello".to_vec(),
            },
        )
        .await;
        assert!(matches!(recv(&mut bob).await, ServerFrame::Ok { .. }));

        // Alice 应当被主动推送
        match recv(&mut alice).await {
            ServerFrame::Msg { body, id, .. } => {
                assert_eq!(body, b"hello");
                assert_eq!(id, "m1");
            }
            other => panic!("期望 Msg，实际 {other:?}"),
        }

        // 幂等：同 id 重复投递不应产生第二条推送
        send(
            &mut bob,
            &ClientFrame::Put {
                qid: qid.clone(),
                id: "m1".into(),
                body: b"hello".to_vec(),
            },
        )
        .await;
        assert!(matches!(recv(&mut bob).await, ServerFrame::Ok { .. }));
        let dup = tokio::time::timeout(Duration::from_millis(400), alice.next()).await;
        assert!(dup.is_err(), "重复投递不应再推送");

        // ACK 之后队列应清空，Get 拿不到东西
        send(
            &mut alice,
            &ClientFrame::Ack {
                qid: qid.clone(),
                ids: vec!["m1".into()],
            },
        )
        .await;
        assert!(matches!(recv(&mut alice).await, ServerFrame::Ok { .. }));

        send(&mut alice, &ClientFrame::Get { qid: qid.clone() }).await;
        let after = tokio::time::timeout(Duration::from_millis(400), alice.next()).await;
        assert!(after.is_err(), "ACK 后不应还能拉到消息");
    }

    /// 投递到不存在的队列要明确报错，不能静默丢
    #[tokio::test]
    async fn put_to_unknown_queue_fails() {
        let addr = start_test_server().await;
        let mut c = connect(addr).await;
        send(
            &mut c,
            &ClientFrame::Put {
                qid: "deadbeef".into(),
                id: "x".into(),
                body: b"lost".to_vec(),
            },
        )
        .await;
        match recv(&mut c).await {
            ServerFrame::Err { code, .. } => assert_eq!(code, "no_queue"),
            other => panic!("期望 Err(no_queue)，实际 {other:?}"),
        }
    }

    /// 版本不匹配必须拒绝，避免新旧客户端混跑出诡异问题
    #[tokio::test]
    async fn rejects_wrong_version() {
        let addr = start_test_server().await;
        let mut c = connect(addr).await;
        send(&mut c, &ClientFrame::New { v: 999 }).await;
        match recv(&mut c).await {
            ServerFrame::Err { code, .. } => assert_eq!(code, "bad_version"),
            other => panic!("期望 Err(bad_version)，实际 {other:?}"),
        }
    }

    /// 速率限制必须生效：没有它，一个脚本就能把中继磁盘写满
    #[tokio::test]
    async fn rate_limit_blocks_flood() {
        let addr = start_test_server_with_rate(3).await;
        let mut c = connect(addr).await;
        // New 也占一条额度（建队列同样写日志行，不做限速就是磁盘 DoS 向量），
        // 所以本连接剩余额度 = 3 - 1 = 2 条 put。
        send(&mut c, &ClientFrame::New { v: PROTOCOL_VERSION }).await;
        let qid = match recv(&mut c).await {
            ServerFrame::NewOk { qid, .. } => qid,
            other => panic!("期望 NewOk，实际 {other:?}"),
        };

        let mut limited = 0;
        let mut ok = 0;
        for i in 0..6 {
            send(
                &mut c,
                &ClientFrame::Put {
                    qid: qid.clone(),
                    id: format!("m{i}"),
                    body: b"x".to_vec(),
                },
            )
            .await;
            match recv(&mut c).await {
                ServerFrame::Err { code, .. } if code == "rate_limited" => limited += 1,
                ServerFrame::Ok { .. } => ok += 1,
                other => panic!("意外帧: {other:?}"),
            }
        }
        assert_eq!(ok, 2, "New 用掉 1 条后，剩余 2 条额度应放行 2 条 put");
        assert_eq!(limited, 4, "额度用完后其余应被限流");
    }

    /// 超过 64KiB 的消息体必须拒绝（阶段 0 的上限，大文件走另一条通道）
    #[tokio::test]
    async fn rejects_oversized_body() {
        let addr = start_test_server().await;
        let mut c = connect(addr).await;
        send(&mut c, &ClientFrame::New { v: PROTOCOL_VERSION }).await;
        let qid = match recv(&mut c).await {
            ServerFrame::NewOk { qid, .. } => qid,
            other => panic!("期望 NewOk，实际 {other:?}"),
        };
        send(
            &mut c,
            &ClientFrame::Put {
                qid,
                id: "big".into(),
                body: vec![0u8; MAX_BODY_LEN + 1],
            },
        )
        .await;
        match recv(&mut c).await {
            ServerFrame::Err { code, .. } => assert_eq!(code, "too_large"),
            other => panic!("期望 Err(too_large)，实际 {other:?}"),
        }
    }
}
