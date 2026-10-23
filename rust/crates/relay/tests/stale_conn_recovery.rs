//! 回归：半开连接必须被心跳探测剔除，不能被缓存无限复用。
//!
//! 背景（2026-10-21 模拟器实测）：连接空闲几分钟后 TCP 半开，recv 安静超时，
//! 旧代码把"看起来还活着"的连接放回缓存——之后每一轮收消息都安静超时、
//! 无错误日志、新消息永远收不到，直到进程重启。修复：超时后发 Ping 探测，
//! 探测失败就丢弃缓存连接，下一轮重连拿积压。
//!
//! 复现手法：真 relay + 一个可控的 TCP 代理。转发开关关闭时代理把两个方向的
//! 数据全部**吞掉**（TCP 保持打开 = 应用层黑洞 = 半开连接的精确等价物）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, SocketAddr};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 启动转发代理：client ↔ proxy ↔ relay。
/// fwd=false 时代理照常读两端但丢弃数据（黑洞模式），TCP 连接保持打开。
fn spawn_proxy(
    listen: TcpListener,
    target: SocketAddr,
    fwd: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        for stream in listen.incoming() {
            let Ok(client) = stream else { continue };
            let Ok(relay) = TcpStream::connect(target) else { continue };
            let fwd2 = fwd.clone();
            std::thread::spawn(move || {
                let Ok(mut cr) = client.try_clone() else { return };
                let Ok(mut cw) = client.try_clone() else { return };
                let Ok(mut rr) = relay.try_clone() else { return };
                let Ok(mut rw) = relay.try_clone() else { return };
                // client → relay
                let f = fwd2.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    loop {
                        match cr.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if f.load(Ordering::Relaxed)
                                    && rw.write_all(&buf[..n]).is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                });
                // relay → client
                let mut buf = [0u8; 4096];
                loop {
                    match rr.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if fwd2.load(Ordering::Relaxed)
                                && cw.write_all(&buf[..n]).is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
}

fn tmpdir(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("relay-stale-{tag}-{nanos}"));
    d.to_str().unwrap().to_string()
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

fn wait_port(port: u16) -> bool {
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
fn halfopen_connection_is_dropped_and_recovers() {
    // ---- 真 relay ----
    let relay_port = free_port();
    let data = tmpdir("relay-data");
    let mut relay = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args([
            "serve",
            "--addr",
            &format!("127.0.0.1:{relay_port}"),
            "--data",
            &data,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("拉起 relay");
    assert!(wait_port(relay_port), "relay 没起来");

    // ---- 代理（初始开启转发）----
    let proxy_port = free_port();
    let fwd = Arc::new(AtomicBool::new(true));
    let listener = TcpListener::bind(("127.0.0.1", proxy_port)).unwrap();
    spawn_proxy(listener, ([127, 0, 0, 1], relay_port).into(), fwd.clone());

    // Alice 经代理建队列
    let a = tmpdir("alice");
    client::init(&a, Some("Alice")).unwrap();
    let code = client::invite(&a, &format!("ws://127.0.0.1:{proxy_port}")).expect("invite");

    // Bob 直连真 relay 添加并发第一条
    let b = tmpdir("bob");
    client::init(&b, Some("Bob")).unwrap();
    client::add(&b, &code, None).unwrap();
    // 邀请码里只有代理地址；把 Bob 的联系人 relays 改成直连真 relay，
    // 这样黑洞期间 Bob 照常能发消息（黑洞只影响 Alice 这条连接）
    let cf = format!("{}/contacts.json", b);
    let txt = std::fs::read_to_string(&cf).expect("读 contacts");
    let patched = txt.replace(
        &format!("ws://127.0.0.1:{proxy_port}"),
        &format!("ws://127.0.0.1:{relay_port}"),
    );
    std::fs::write(&cf, patched).expect("写 contacts");
    client::send(&b, "Alice", "黑洞前第一条").expect("send 1");

    // Alice 收到第一条（此刻 her 连接是缓存里的活连接）
    let m1 = client::recv_once(&a, 15).expect("recv 1").expect("应收到第一条");
    assert_eq!(m1.text, "黑洞前第一条");

    // ---- 模拟半开：关闭代理转发（TCP 打开、应用层黑洞）----
    // Bob 直连 relay 发第二条：消息落进 relay 队列，relay 往 Alice 连接的
    // 推送会被黑洞代理吞掉——这正是"半开连接收不到推送"的等价场景。
    fwd.store(false, Ordering::Relaxed);
    client::send(&b, "Alice", "黑洞期间第二条").expect("send 2");

    // Alice 下一次收：用缓存的黑洞连接 → 超时 → Ping 也被吞。
    // 修复后的行为：报错并丢弃连接（Ok(None) + 回缓存 = 旧代码，会让下面的重连
    // 永远卡死，本测试超时失败）。
    let r = client::recv_once(&a, 3);
    assert!(
        r.is_err(),
        "黑洞连接上的收消息必须报错丢弃，而不是静默超时（旧代码行为）"
    );

    // ---- 恢复转发：下一轮必须重连并拿到积压 ----
    fwd.store(true, Ordering::Relaxed);
    // 未 ACK 的第一条会被补投（幂等由上层 msgId 去重），所以循环等第二条
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut got_second = false;
    while std::time::Instant::now() < deadline {
        match client::recv_once(&a, 10) {
            Ok(Some(m)) if m.text == "黑洞期间第二条" => {
                got_second = true;
                break;
            }
            Ok(Some(_)) => continue, // 补投的旧消息，跳过
            Ok(None) => continue,
            Err(_) => continue, // 重连可能需要一两轮
        }
    }
    assert!(got_second, "剔除坏连接后必须能重连并收到积压消息");

    relay.kill().ok();
    std::fs::remove_dir_all(&a).ok();
    std::fs::remove_dir_all(&b).ok();
    std::fs::remove_dir_all(&data).ok();
}
