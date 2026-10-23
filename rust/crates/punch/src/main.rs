//! 阶段 0 · 验证项 0.1：UDP 打洞成功率与 NAT 映射行为
//!
//! 为什么先测 NAT 映射行为，而不是直接打洞：
//! - 打洞能否成功，**80% 由 NAT 映射行为决定**。EIM（Endpoint-Independent Mapping）
//!   环境下几乎必然成功；APDM（Address-and-Port-Dependent Mapping，即对称型 NAT）
//!   下，只有"双方同时向同一个目标发包"才可能成功。
//! - 国内移动网络的 IPv6 通常没有 NAT，只要有状态防火墙，同时发包即可打洞；
//!   而 IPv4 侧普遍是 CGNAT，对称型占比高。
//! - 先拿到这批数据，才知道 TURN 要按多少比例预留带宽。
//!
//! 用法：
//!   punch serve --port 7000
//!   punch nat   --server 1.2.3.4:7000          # 单机测 NAT 映射行为
//!   punch client --server 1.2.3.4:7000 --id a --peer b --secs 20
//!
//! 两端各跑一个 client，id 互换。结果行可直接汇总：
//!   RUN id=a peer=b direct=true first_ms=312 recv=97 sent=100

use std::collections::HashMap;
use std::env;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::process::exit;
use std::thread;
use std::time::{Duration, Instant};

const MAX_DGRAM: usize = 2048;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage();
        exit(2);
    }
    let r = match args[1].as_str() {
        "serve" => cmd_serve(&args[2..]),
        "nat" => cmd_nat(&args[2..]),
        "client" => cmd_client(&args[2..]),
        "-h" | "--help" | "help" => {
            usage();
            Ok(())
        }
        other => {
            eprintln!("未知子命令: {other}");
            usage();
            exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("错误: {e}");
        exit(1);
    }
}

fn usage() {
    eprintln!(
        "用法:
  punch serve  --port <PORT>
  punch nat    --server <HOST:PORT>
  punch client --server <HOST:PORT> --id <ME> --peer <PEER> [--secs 20]"
    );
}

fn opt<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

// ---------------------------------------------------------------- 会合服务器

/// 极简会合（rendezvous）服务器：只交换双方的公网地址，不转发任何业务流量。
/// 真实产品里这层可以并入中继，或做成静态配置的多个实例。
fn cmd_serve(args: &[String]) -> io::Result<()> {
    let port: u16 = opt(args, "--port")
        .and_then(|s| s.parse().ok())
        .unwrap_or(7000);
    let sock = UdpSocket::bind(("0.0.0.0", port))?;
    println!("[serve] 监听 udp/0.0.0.0:{port}");

    // id -> (addr, 是否需要通知对端)
    let mut peers: HashMap<String, SocketAddr> = HashMap::new();
    let mut want: HashMap<String, String> = HashMap::new(); // id -> peer_id
    let mut buf = [0u8; MAX_DGRAM];

    loop {
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[serve] recv 出错: {e}");
                continue;
            }
        };
        let line = String::from_utf8_lossy(&buf[..n]).trim().to_string();
        let mut it = line.split_whitespace();
        let cmd = it.next().unwrap_or("");

        match cmd {
            // REG <id> [want_peer_id]
            "REG" => {
                let id = match it.next() {
                    Some(v) => v.to_string(),
                    None => continue,
                };
                let want_peer = it.next().map(|s| s.to_string());
                peers.insert(id.clone(), from);
                if let Some(p) = want_peer {
                    want.insert(id.clone(), p);
                }
                // 立刻回自己的外部地址（STUN-lite）
                let _ = sock.send_to(format!("EXT {from}\n").as_bytes(), from);
                println!("[serve] REG id={id} from={from}");

                // 双方都到齐了，互相通知
                if let Some(p) = want.get(&id).cloned() {
                    if let (Some(pa), Some(pb)) = (peers.get(&id).copied(), peers.get(&p).copied()) {
                        let _ = sock.send_to(format!("PEER {pb}\n").as_bytes(), pa);
                        let _ = sock.send_to(format!("PEER {pa}\n").as_bytes(), pb);
                        println!("[serve] 配对 {id}<->{p}: {pa} / {pb}");
                    }
                }
            }
            // MAP：仅回报外部地址，用于 NAT 映射行为探测
            "MAP" => {
                let _ = sock.send_to(format!("EXT {from}\n").as_bytes(), from);
            }
            "PING" => {
                let _ = sock.send_to(b"PONG\n", from);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------- NAT 探测

/// 用两个不同的本地端口向同一服务器发起，比较外部端口：
/// - 外部端口相同 => EIM（端点无关映射），打洞成功率最高
/// - 外部端口不同 => 依赖目标的映射（对称型倾向），需要 TURN 兜底
fn cmd_nat(args: &[String]) -> io::Result<()> {
    let server: SocketAddr = opt(args, "--server")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "缺少 --server"))?
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "--server 格式应为 host:port"))?;

    let ext1 = probe_external(server)?;
    let ext2 = probe_external(server)?;

    println!("外部地址 #1: {ext1}");
    println!("外部地址 #2: {ext2}");

    let eim = ext1.port() == ext2.port();
    let ip_same = ext1.ip() == ext2.ip();
    println!(
        "映射行为: {}",
        if eim {
            "EIM（端点无关）— 打洞成功率高"
        } else if ip_same {
            "APDM（依赖目标端口，偏对称型）— 需要同时发包 + TURN 兜底"
        } else {
            "ADM/AFDM（连外部 IP 都变）— 打洞基本不可用，必须 TURN"
        }
    );
    println!(
        "RESULT nat={} ext1={ext1} ext2={ext2}",
        if eim { "eim" } else { "symmetric_like" }
    );
    Ok(())
}

fn probe_external(server: SocketAddr) -> io::Result<SocketAddr> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(Duration::from_secs(3)))?;
    // 重试三次，容忍首个包丢在 NAT 上
    for _ in 0..3 {
        sock.send_to(b"MAP\n", server)?;
        let mut buf = [0u8; MAX_DGRAM];
        if let Ok((n, _)) = sock.recv_from(&mut buf) {
            let s = String::from_utf8_lossy(&buf[..n]);
            if let Some(addr) = s.trim().strip_prefix("EXT ") {
                if let Ok(a) = addr.parse() {
                    return Ok(a);
                }
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "服务器没有回应（检查地址、端口与防火墙）",
    ))
}

// ---------------------------------------------------------------- 打洞客户端

fn cmd_client(args: &[String]) -> io::Result<()> {
    let server: SocketAddr = opt(args, "--server")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "缺少 --server"))?
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "--server 格式应为 host:port"))?;
    let me = opt(args, "--id").unwrap_or("a").to_string();
    let peer = opt(args, "--peer").unwrap_or("b").to_string();
    let secs: u64 = opt(args, "--secs")
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    // 必须与注册用同一个 socket，否则 NAT 映射不是同一条
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    sock.send_to(format!("REG {me} {peer}\n").as_bytes(), server)?;
    println!("[client] id={me} 注册到 {server}，等待 {peer}");

    let mut peer_addr: Option<SocketAddr> = None;
    let mut buf = [0u8; MAX_DGRAM];
    let deadline = Instant::now() + Duration::from_secs(secs.max(3));

    // 等对端地址
    while Instant::now() < deadline && peer_addr.is_none() {
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                let s = String::from_utf8_lossy(&buf[..n]);
                if let Some(a) = s.trim().strip_prefix("PEER ") {
                    if let Ok(addr) = a.parse() {
                        peer_addr = Some(addr);
                        println!("[client] 拿到对端地址 {addr}");
                    }
                }
            }
            Err(_) => continue,
        }
    }

    let peer_addr = match peer_addr {
        Some(a) => a,
        None => {
            println!("RUN id={me} peer={peer} direct=false reason=no_peer_registered");
            return Ok(());
        }
    };

    // 同时发包打洞：双方都往对方外部地址发，穿越各自 NAT
    let start = Instant::now();
    let mut sent = 0u32;
    let mut recv = 0u32;
    let mut first_ok: Option<u128> = None;

    while start.elapsed() < Duration::from_secs(secs) {
        let payload = format!("PUNCH {me} {}\n", sent);
        let _ = sock.send_to(payload.as_bytes(), peer_addr);
        sent += 1;

        // 连收几轮，缩短首包发现时间
        for _ in 0..4 {
            match sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    let s = String::from_utf8_lossy(&buf[..n]);
                    if s.starts_with("PUNCH ") {
                        recv += 1;
                        if first_ok.is_none() {
                            first_ok = Some(start.elapsed().as_millis());
                            println!("[client] 打通！首包来自 {from}，用时 {}ms", start.elapsed().as_millis());
                        }
                    }
                }
                Err(_) => break,
            }
        }
        thread::sleep(Duration::from_millis(200));
    }

    let direct = recv > 0;
    println!(
        "RUN id={me} peer={peer} direct={direct} first_ms={} recv={recv} sent={sent}",
        first_ok.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
    );
    Ok(())
}
