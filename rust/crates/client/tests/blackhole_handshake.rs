//! 回归：黑洞服务器下的握手必须被打断，不能永久挂起。
//!
//! 背景（2026-10-20 生产事故）：`Conn::connect` 的 WS 握手原来没有超时，
//! adb reverse 半开连接 / NAT 老化会让 `connect_async` 永久挂起，
//! 表现为前台服务活着、但永远收不到消息且无任何日志。
//! 修复：握手 10 秒硬超时。本测试用"接受 TCP 连接但绝不回应"的黑洞服务器复现。
//!
//! 修复前的代码跑这个测试会**永久挂起**——挂起本身就是失败信号。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// 接受连接、读走握手请求、然后装死：不回 101、也不关连接。
fn spawn_blackhole() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定黑洞端口");
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            // 收下连接，把客户端发来的 HTTP 升级请求读掉（模拟收到但不处理），
            // 然后保持连接、永不回应
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = s.flush();
                std::mem::forget(s); // 故意泄漏：连接保持打开
            });
        }
    });
    addr
}

fn tmpdir(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("client-bh-{tag}-{nanos}"));
    d.to_str().unwrap().to_string()
}

#[test]
fn blackhole_handshake_must_timeout() {
    let addr = spawn_blackhole();
    let dir = tmpdir("invite");
    client::init(&dir, None).expect("init 应成功（不联网）");

    // invite/recv_once 都要先连中继：黑洞下必须在有限时间内报错。
    // 修复后握手超时 10 秒；给 25 秒余量覆盖 CI 慢机器。
    let t0 = Instant::now();
    let r = client::invite(&dir, &format!("ws://{addr}"));
    let elapsed = t0.elapsed();

    assert!(r.is_err(), "对黑洞服务器 invite 不该成功");
    assert!(
        elapsed < Duration::from_secs(25),
        "握手挂起未被超时打断：耗时 {elapsed:?}（这就是当年的静默卡死）"
    );

    // recv_once 同样必须被打断（生产里是收消息循环卡死的入口）
    let dir2 = tmpdir("recv");
    client::init(&dir2, None).expect("init 应成功");
    // 直接对 recv_once 没法测（没队列会先报"先调用 invite"），
    // 但 invite 与 recv_once 共用同一个 Conn::connect，上面已经覆盖。
    let _ = dir2;

    std::fs::remove_dir_all(&dir).ok();
}
