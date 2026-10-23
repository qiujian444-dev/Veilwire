//! 端到端：客户端从 WS 通道索取 TURN 临时凭据。
//!
//! 这条链路的意义：DHT 只能公布 TURN 的 IP 和端口（BEP5 记录固定 6 字节），
//! 放不下账号密码；把固定凭据硬编码进 App 又等于把 TURN 变公开免费中继。
//! 所以凭据必须在已建立的 WS 通道里按需签发。

use std::process::{Command, Stdio};
use std::time::Duration;

fn tmpdir(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("relay-turn-{tag}-{nanos}"));
    d.to_str().unwrap().to_string()
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

struct Proc(std::process::Child);
impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 起一个带 TURN 的 relay，返回 (中继 ws 地址, 句柄)
fn start_relay_with_turn() -> (String, Proc) {
    let ws = free_port();
    let turn = free_port();
    let data = tmpdir("data");
    let child = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args([
            "serve",
            "--addr",
            &format!("127.0.0.1:{ws}"),
            "--data",
            &data,
            "--turn",
            &format!("127.0.0.1:{turn}"),
            "--turn-external-ip",
            "203.0.113.7",
            "--turn-user",
            "static-user",
            "--turn-pass",
            "static-pass",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 relay 失败");
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", ws)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    (format!("ws://127.0.0.1:{ws}"), Proc(child))
}

#[test]
fn client_fetches_ephemeral_turn_credentials() {
    let (relay, _guard) = start_relay_with_turn();
    let dir = tmpdir("dev");
    client::init(&dir, Some("probe")).unwrap();
    client::ensure_queue(&dir, &relay).unwrap();

    let c = client::fetch_turn_creds(&dir).expect("索取 TURN 凭据失败");
    println!("url={} user={} ttl={}", c.url, c.user, c.ttl);

    assert!(c.url.starts_with("turn:"), "url 应该是 turn: 开头，实际 {}", c.url);
    assert!(
        c.url.contains("203.0.113.7"),
        "对外地址应使用 --turn-external-ip，实际 {}",
        c.url
    );
    assert!(c.user.starts_with("eph-"), "应当是临时账号，实际 {}", c.user);
    assert_eq!(c.pass.len(), 32, "密码应当 32 位十六进制");
    assert!(c.ttl > 3000, "默认 TTL 应接近 1 小时，实际 {}", c.ttl);
    assert!(c.remaining() > 3000);

    // 再要一次：必须是不同的账号（一次性签发，不是固定账号）
    let c2 = client::fetch_turn_creds(&dir).expect("第二次索取失败");
    assert_ne!(c.user, c2.user, "每次签发都应该是新账号");

    // 缓存路径：剩余时间足够时应直接返回缓存，不再连中继
    let cached = client::turn_creds(&dir).expect("应当有可用缓存");
    assert_eq!(cached.user, c2.user, "缓存里应该是最后一次签发的账号");
}

/// 没开 TURN 的中继要明确拒绝，而不是让客户端拿到一个假的地址
#[test]
fn relay_without_turn_refuses_credentials() {
    let ws = free_port();
    let data = tmpdir("plain");
    let _guard = {
        let child = Command::new(env!("CARGO_BIN_EXE_relay"))
            .args(["serve", "--addr", &format!("127.0.0.1:{ws}"), "--data", &data])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("启动 relay 失败");
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", ws)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Proc(child)
    };

    let dir = tmpdir("dev2");
    client::init(&dir, Some("probe")).unwrap();
    client::ensure_queue(&dir, &format!("ws://127.0.0.1:{ws}")).unwrap();

    let e = client::fetch_turn_creds(&dir).expect_err("没开 TURN 时必须失败");
    println!("预期内的失败: {e:#}");
    assert!(format!("{e:#}").contains("no_turn"), "错误里应带 no_turn 便于上层区分");
}
