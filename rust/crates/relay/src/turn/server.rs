//! TURN 服务端事件循环（RFC 5766 子集）。
//!
//! 单任务模型：所有协议状态放在一个 `select!` 循环里，避免给分配表加锁。
//! 每个分配的 relay socket 由一个轻量任务读，收到包丢进 channel，由主循环统一处理
//! （它要查 permission/通道表，状态在那边）。规模是几十路并发，这个模型足够且不易出错。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{interval, timeout};

use super::alloc::{AllocErr, State, DEFAULT_LIFETIME, MAX_LIFETIME, MIN_LIFETIME};
use super::auth::{Creds, Outcome};
use super::codec::{attr, class, method, Msg};

pub struct Config {
    pub listen: SocketAddr,
    /// NAT 后的公网地址。XOR-RELAYED-ADDRESS 必须通告它，否则对端连不上。
    pub external_ip: IpAddr,
    pub realm: String,
    pub users: HashMap<String, String>,
    pub port_min: u16,
    pub port_max: u16,
    pub max_per_ip: usize,
    /// 是否允许把数据中转给回环/内网地址。
    /// 默认 false：TURN 是经典的 SSRF 跳板，开放后可以拿它探测部署方内网
    /// （coturn 默认也是拒绝的）。只有本机联调才打开。
    pub allow_private_peers: bool,
}

const SOFTWARE: &str = "anonchat-relay";

struct RelayPkt {
    client: SocketAddr,
    peer: SocketAddr,
    data: Vec<u8>,
}

pub async fn run(cfg: Config) -> Result<()> {
    let sock = Arc::new(
        UdpSocket::bind(cfg.listen)
            .await
            .with_context(|| format!("TURN 监听失败 {}", cfg.listen))?,
    );
    log_info(&format!(
        "TURN 已监听 {}（外网通告 {}，relay 端口 {}-{}）",
        cfg.listen, cfg.external_ip, cfg.port_min, cfg.port_max
    ));

    let mut state = State::new(cfg.external_ip, cfg.port_min, cfg.port_max, cfg.max_per_ip);
    let mut creds = Creds::new(&cfg.realm, cfg.users.clone());
    let (tx, mut rx) = mpsc::channel::<RelayPkt>(2048);

    let mut buf = vec![0u8; 65_535];
    let mut sweep = interval(Duration::from_secs(30));
    let mut rotate = interval(Duration::from_secs(600));

    loop {
        tokio::select! {
            r = sock.recv_from(&mut buf) => {
                // 瞬态读错误必须容忍：Windows 上向已关闭的客户端端口发包后
                // 收到 ICMP 会让 recv_from 返回 WSAECONNRESET，这是 UDP 服务
                // 的常态。早先直接 `?` 会用一包打死整个 TURN 服务。
                match r {
                    Ok((n, from)) => {
                        handle_client(&sock, &mut state, &mut creds, &cfg, &tx, &buf[..n], from).await;
                    }
                    Err(e) => {
                        log_warn(&format!("TURN recv 暂时性错误（忽略）: {e}"));
                    }
                }
            }
            Some(p) = rx.recv() => {
                send_to_client(&sock, &state, p).await;
            }
            _ = sweep.tick() => {
                let n = state.sweep();
                if n > 0 {
                    log_info(&format!("回收 {n} 个到期分配（当前 {} 路）", state.count()));
                }
            }
            _ = rotate.tick() => creds.rotate(),
        }
    }
}

// ---------------------------------------------------------------- 客户端 → 服务端

async fn handle_client(
    sock: &Arc<UdpSocket>,
    state: &mut State,
    creds: &mut Creds,
    cfg: &Config,
    tx: &mpsc::Sender<RelayPkt>,
    data: &[u8],
    from: SocketAddr,
) {
    if data.is_empty() {
        return;
    }
    // ChannelData 的最高两位是 01（RFC 5766 §11.4），不是 STUN 报文
    if data[0] & 0xC0 == 0x40 {
        if data.len() < 4 {
            return;
        }
        let num = u16::from_be_bytes([data[0], data[1]]);
        let len = u16::from_be_bytes([data[2], data[3]]) as usize;
        if data.len() < 4 + len {
            return;
        }
        let payload = &data[4..4 + len];
        let Some(a) = state.get(&from) else { return };
        let Some(peer) = a.chan_to_peer.get(&num).copied() else { return };
        let relay = a.relay.clone();
        if let Err(e) = relay.send_to(payload, peer).await {
            log_warn(&format!("通道转发失败 {num:#x} → {peer}: {e}"));
        }
        return;
    }

    let Some(msg) = Msg::parse(data) else { return };
    match (msg.method, msg.class) {
        (method::BINDING, class::REQUEST) => {
            let mut resp = Msg::new(method::BINDING, class::SUCCESS, msg.tid);
            resp.add_xor_addr(attr::XOR_MAPPED_ADDRESS, from);
            resp.add_xor_addr(attr::MAPPED_ADDRESS, from); // 老客户端兼容
            resp.add_str(attr::SOFTWARE, SOFTWARE);
            let _ = sock.send_to(&resp.encode(None, true), from).await;
        }
        // Send 指示不需要认证：它只能用已授权的 permission 发包
        (method::SEND, class::INDICATION) => {
            let Some(peer) = msg.get_xor_addr(attr::XOR_PEER_ADDRESS) else { return };
            let Some(payload) = msg.get(attr::DATA) else { return };
            if !peer_allowed(peer.ip(), cfg.allow_private_peers) {
                log_warn(&format!("拒绝向受限地址转发（SSRF 防护）：{peer}"));
                return;
            }
            let Some(a) = state.get(&from) else { return };
            if !a.perms.contains(&peer.ip()) {
                log_warn(&format!("丢弃未授权 Send：{from} → {peer}"));
                return;
            }
            let relay = a.relay.clone();
            if let Err(e) = relay.send_to(payload, peer).await {
                log_warn(&format!("relay 发送失败 → {peer}: {e}"));
            }
        }
        (m, class::REQUEST)
            if m == method::ALLOCATE
                || m == method::REFRESH
                || m == method::CREATE_PERMISSION
                || m == method::CHANNEL_BIND =>
        {
            // 认证：Challenge/Stale/Denied 都在这里拦掉
            let (key, user) = match creds.authenticate(&msg) {
                Outcome::Ok { user, key } => (key, user),
                Outcome::Challenge => {
                    let resp = challenge(&msg, creds, 401, "Unauthorized");
                    let _ = sock.send_to(&resp, from).await;
                    return;
                }
                Outcome::Stale => {
                    let resp = challenge(&msg, creds, 438, "Stale Nonce");
                    let _ = sock.send_to(&resp, from).await;
                    return;
                }
                Outcome::Denied => {
                    let resp = challenge(&msg, creds, 401, "Unauthorized");
                    let _ = sock.send_to(&resp, from).await;
                    return;
                }
            };

            match m {
                method::ALLOCATE => {
                    handle_allocate(sock, state, tx, &msg, from, &key, &user).await
                }
                method::REFRESH => handle_refresh(sock, state, &msg, from, &key).await,
                method::CREATE_PERMISSION => {
                    handle_permission(sock, state, cfg.allow_private_peers, &msg, from, &key).await
                }
                method::CHANNEL_BIND => {
                    handle_channel_bind(sock, state, cfg.allow_private_peers, &msg, from, &key).await
                }
                _ => {}
            }
        }
        _ => {
            // 未知方法：回 400，别静默丢弃（排查时能看出来是谁在打）
            let mut resp = Msg::new(msg.method, class::ERROR, msg.tid);
            resp.add_error(400, "Bad Request");
            resp.add_str(attr::SOFTWARE, SOFTWARE);
            let _ = sock.send_to(&resp.encode(None, false), from).await;
        }
    }
}

/// 对端地址是否允许（默认拒绝回环/私网/链路本地，防 SSRF）
fn peer_allowed(ip: IpAddr, allow_private: bool) -> bool {
    if allow_private {
        return true;
    }
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.octets()[0] == 0)
        }
        IpAddr::V6(v6) => !(v6.is_loopback() || v6.is_unspecified()),
    }
}

fn challenge(req: &Msg, creds: &Creds, code: u16, reason: &str) -> Vec<u8> {
    let mut resp = Msg::new(req.method, class::ERROR, req.tid);
    resp.add_error(code, reason);
    resp.add_str(attr::REALM, creds.realm());
    resp.add_str(attr::NONCE, creds.nonce());
    resp.add_str(attr::SOFTWARE, SOFTWARE);
    resp.encode(None, true)
}

async fn respond_ok(
    sock: &Arc<UdpSocket>,
    req: &Msg,
    from: SocketAddr,
    key: &[u8],
    attrs: impl FnOnce(&mut Msg),
) {
    let mut resp = Msg::new(req.method, class::SUCCESS, req.tid);
    attrs(&mut resp);
    resp.add_str(attr::SOFTWARE, SOFTWARE);
    let _ = sock.send_to(&resp.encode(Some(key), true), from).await;
}

async fn respond_err(
    sock: &Arc<UdpSocket>,
    req: &Msg,
    from: SocketAddr,
    key: Option<&[u8]>,
    code: u16,
    reason: &str,
) {
    let mut resp = Msg::new(req.method, class::ERROR, req.tid);
    resp.add_error(code, reason);
    resp.add_str(attr::SOFTWARE, SOFTWARE);
    let _ = sock.send_to(&resp.encode(key, true), from).await;
}

#[allow(clippy::too_many_arguments)]
async fn handle_allocate(
    sock: &Arc<UdpSocket>,
    state: &mut State,
    tx: &mpsc::Sender<RelayPkt>,
    msg: &Msg,
    from: SocketAddr,
    key: &[u8],
    user: &str,
) {
    if state.exists(&from) {
        respond_err(sock, msg, from, Some(key), 437, "Allocation Mismatch").await;
        return;
    }
    // REQUESTED-TRANSPORT：值首字节 17 = UDP（RFC 5766 §14.7）
    let proto = msg.get(attr::REQUESTED_TRANSPORT).and_then(|v| v.first().copied());
    if proto != Some(17) {
        respond_err(
            sock,
            msg,
            from,
            Some(key),
            442,
            "Unsupported Transport Protocol",
        )
        .await;
        return;
    }
    let lifetime = msg.get_u32(attr::LIFETIME).unwrap_or(DEFAULT_LIFETIME);
    let port = match state.allocate(from, user, lifetime).await {
        Ok(p) => p,
        Err(AllocErr::Quota) => {
            respond_err(sock, msg, from, Some(key), 486, "Allocation Quota Reached").await;
            return;
        }
    };
    let secs = lifetime.clamp(MIN_LIFETIME, MAX_LIFETIME);
    // 用 State 里的 external_ip（唯一来源），避免配置两处漂移
    let relayed = SocketAddr::new(state.external_ip, port);

    // 起一个读任务：relay socket 上收到的对端包，交给主循环回给客户端。
    // alive 标志在 State::remove 里置 false，读任务据此退出（否则每路分配都会漏一个任务）
    let (relay_sock, client, alive, uname) = match state.get(&from) {
        Some(a) => (a.relay.clone(), a.client, a.alive.clone(), a.username.clone()),
        None => return,
    };
    spawn_relay_reader(relay_sock, client, tx.clone(), alive);
    log_info(&format!(
        "分配 {from}（{uname}）→ relay {relayed}，空闲端口 {}",
        state.free_ports()
    ));

    respond_ok(sock, msg, from, key, |r| {
        r.add_xor_addr(attr::XOR_RELAYED_ADDRESS, relayed);
        r.add_xor_addr(attr::XOR_MAPPED_ADDRESS, from);
        r.add_u32(attr::LIFETIME, secs);
    })
    .await;
}

async fn handle_refresh(
    sock: &Arc<UdpSocket>,
    state: &mut State,
    msg: &Msg,
    from: SocketAddr,
    key: &[u8],
) {
    let lifetime = msg.get_u32(attr::LIFETIME).unwrap_or(DEFAULT_LIFETIME);
    if lifetime == 0 {
        if state.remove(&from).is_some() {
            log_info(&format!("释放分配 {from}"));
        }
        respond_ok(sock, msg, from, key, |r| r.add_u32(attr::LIFETIME, 0)).await;
        return;
    }
    match state.refresh(&from, lifetime) {
        Some(secs) => {
            respond_ok(sock, msg, from, key, |r| r.add_u32(attr::LIFETIME, secs)).await
        }
        None => respond_err(sock, msg, from, Some(key), 437, "Allocation Mismatch").await,
    }
}

async fn handle_permission(
    sock: &Arc<UdpSocket>,
    state: &mut State,
    allow_private: bool,
    msg: &Msg,
    from: SocketAddr,
    key: &[u8],
) {
    if !state.exists(&from) {
        respond_err(sock, msg, from, Some(key), 437, "Allocation Mismatch").await;
        return;
    }
    // 一次请求可以带多个 XOR-PEER-ADDRESS，逐个解析（Msg::get_xor_addr 只取第一个）
    let mut count = 0;
    for (t, v) in msg.attrs.iter() {
        if *t != attr::XOR_PEER_ADDRESS || v.len() < 8 || v[1] != 0x01 {
            continue;
        }
        let port = u16::from_be_bytes([v[2], v[3]]) ^ ((super::codec::MAGIC >> 16) as u16);
        let ip = u32::from_be_bytes([v[4], v[5], v[6], v[7]]) ^ super::codec::MAGIC;
        let peer = SocketAddr::new(IpAddr::from(std::net::Ipv4Addr::from(ip)), port);
        if !peer_allowed(peer.ip(), allow_private) {
            log_warn(&format!("拒绝为受限地址建权限（SSRF 防护）：{peer}"));
            continue;
        }
        if state.add_permission(&from, peer.ip()) {
            count += 1;
        }
    }
    if count == 0 {
        respond_err(sock, msg, from, Some(key), 400, "Bad Request").await;
        return;
    }
    respond_ok(sock, msg, from, key, |_| {}).await;
}

async fn handle_channel_bind(
    sock: &Arc<UdpSocket>,
    state: &mut State,
    allow_private: bool,
    msg: &Msg,
    from: SocketAddr,
    key: &[u8],
) {
    if !state.exists(&from) {
        respond_err(sock, msg, from, Some(key), 437, "Allocation Mismatch").await;
        return;
    }
    let num = msg.get(attr::CHANNEL_NUMBER).and_then(|v| {
        if v.len() >= 2 { Some(u16::from_be_bytes([v[0], v[1]])) } else { None }
    });
    let peer = msg.get_xor_addr(attr::XOR_PEER_ADDRESS);
    let (Some(num), Some(peer)) = (num, peer) else {
        respond_err(sock, msg, from, Some(key), 400, "Bad Request").await;
        return;
    };
    if !(0x4000..=0x7FFF).contains(&num) {
        respond_err(sock, msg, from, Some(key), 400, "Bad Request").await;
        return;
    }
    if !peer_allowed(peer.ip(), allow_private) {
        log_warn(&format!("拒绝把通道绑到受限地址（SSRF 防护）：{peer}"));
        respond_err(sock, msg, from, Some(key), 403, "Forbidden").await;
        return;
    }
    if !state.bind_channel(&from, num, peer) {
        respond_err(sock, msg, from, Some(key), 437, "Allocation Mismatch").await;
        return;
    }
    respond_ok(sock, msg, from, key, |_| {}).await;
}

// ---------------------------------------------------------------- relay → 客户端

fn spawn_relay_reader(
    relay: Arc<UdpSocket>,
    client: SocketAddr,
    tx: mpsc::Sender<RelayPkt>,
    alive: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65_535];
        loop {
            // alive 每轮循环都查，**不是只在超时路径查**：早先只在 5 秒超时
            // 分支看标志，只要对端媒体包持续到达（通话在继续），recv_from
            // 永远成功、reader 永不退出、socket 永不关闭——而 remove() 已经
            // 把端口放回池子，新分配弹出这个端口 bind 必然失败（被映射成
            // 486），等于一路过期通话能级联打死整个 TURN（二轮审计 P1-3）。
            if !alive.load(Ordering::Relaxed) {
                break;
            }
            match timeout(Duration::from_secs(5), relay.recv_from(&mut buf)).await {
                Ok(Ok((n, peer))) => {
                    let pkt = RelayPkt { client, peer, data: buf[..n].to_vec() };
                    if tx.send(pkt).await.is_err() {
                        break; // 主循环没了，任务退出
                    }
                }
                // 瞬态读错误（如 ICMP 触发的 ECONNRESET）不应打死单路数据面：
                // 记录并继续，分配本身还活着。
                Ok(Err(_)) => continue,
                Err(_) => {
                    if !alive.load(Ordering::Relaxed) {
                        break;
                    }
                }
            }
        }
    });
}

async fn send_to_client(sock: &Arc<UdpSocket>, state: &State, p: RelayPkt) {
    let Some(a) = state.get(&p.client) else { return };
    if let Some(num) = a.peer_to_chan.get(&p.peer).copied() {
        // 已绑通道：走 ChannelData，省 36 字节头（实时媒体每秒几十个包）
        let mut out = Vec::with_capacity(4 + p.data.len() + 3);
        out.extend_from_slice(&num.to_be_bytes());
        out.extend_from_slice(&(p.data.len() as u16).to_be_bytes());
        out.extend_from_slice(&p.data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        let _ = sock.send_to(&out, p.client).await;
    } else {
        let mut ind = Msg::new(method::DATA, class::INDICATION, rand_tid());
        ind.add_xor_addr(attr::XOR_PEER_ADDRESS, p.peer);
        ind.attrs.push((attr::DATA, p.data));
        let _ = sock.send_to(&ind.encode(None, false), p.client).await;
    }
}

fn rand_tid() -> [u8; 12] {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut t = [0u8; 12];
    t[..8].copy_from_slice(&n.to_be_bytes());
    t[8..].copy_from_slice(&(std::process::id() as u32).to_be_bytes());
    t
}

fn log_info(s: &str) {
    println!("[turn] {s}");
}

fn log_warn(s: &str) {
    eprintln!("[turn] 警告: {s}");
}
