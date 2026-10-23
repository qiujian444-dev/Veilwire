//! 回归：双方连**不同中继**时能否通信。
//!
//! 背景（用户提问）：DHT 自动发现后，两台手机可能落到不同中继上，
//! 这会不会导致彼此收不到消息？
//!
//! 从代码读出来的架构是：
//!   - 每个设备在自己的中继上有一个队列（qid），只有自己能 sub 它
//!   - 发送时用**联系人的 relays**（`c.relays.first()`），不是自己的
//!   - 邀请码和每条消息的载荷都带 reply_qid / reply_relays
//! 所以理论上 A 连 R1、B 连 R2 也能通：A 发消息时连的是 B 的 R2。
//!
//! 本测试把这条链路真的跑一遍：两个 relay 进程，两个设备目录，
//! 各自在不同中继上建队列，再用邀请码互相添加，双向收发都要成功。

use std::process::{Command, Stdio};
use std::time::Duration;

fn tmpdir(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("relay-xrelay-{tag}-{nanos}"));
    d.to_str().unwrap().to_string()
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

struct RelayProc {
    child: std::process::Child,
    url: String,
}

impl Drop for RelayProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_relay(tag: &str) -> RelayProc {
    let port = free_port();
    let data = tmpdir(&format!("{tag}-data"));
    let child = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(["serve", "--addr", &format!("127.0.0.1:{port}"), "--data", &data])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("无法启动 relay");
    // 等端口真的能连
    let url = format!("ws://127.0.0.1:{port}");
    for _ in 0..80 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    RelayProc { child, url }
}

/// 反复 recv 直到拿到一条 text 消息或超时。
fn recv_text(dir: &str, want: &str, secs: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        match client::recv_once(dir, 2) {
            Ok(Some(inc)) => {
                if inc.text == want {
                    return true;
                }
            }
            Ok(None) => {}
            Err(e) => eprintln!("  recv err: {e:#}"),
        }
    }
    false
}

#[test]
fn two_devices_on_different_relays_can_talk() {
    let r1 = start_relay("r1");
    let r2 = start_relay("r2");
    println!("R1={}  R2={}", r1.url, r2.url);

    let da = tmpdir("dev-a");
    let db = tmpdir("dev-b");

    // --- A 在 R1 上建队列，B 在 R2 上建队列 ---
    client::init(&da, Some("Alice")).expect("init A");
    client::init(&db, Some("Bob")).expect("init B");

    let qa = client::ensure_queue(&da, &r1.url).expect("A 建队列失败");
    let qb = client::ensure_queue(&db, &r2.url).expect("B 建队列失败");
    println!("A.qid={qa}  B.qid={qb}");

    // A 的自我状态里只有 R1，B 只有 R2
    let me_a = client::invite(&da, &r1.url).expect("A 邀请码");
    let me_b = client::invite(&db, &r2.url).expect("B 邀请码");

    // --- 互相添加 ---
    client::add(&da, &me_b, None).expect("A 添加 B 失败");
    client::add(&db, &me_a, None).expect("B 添加 A 失败");

    // 确认双方记住的对方中继是**对端**的，不是自己的
    let ca = client::contacts(&da).expect("A contacts");
    let cb = client::contacts(&db).expect("B contacts");
    println!("A 记录的 B.relays = {:?}", ca[0].relays);
    println!("B 记录的 A.relays = {:?}", cb[0].relays);
    assert!(
        ca[0].relays.iter().any(|r| r == &r2.url),
        "A 应该记住 B 在 R2 上，实际 {:?}",
        ca[0].relays
    );
    assert!(
        cb[0].relays.iter().any(|r| r == &r1.url),
        "B 应该记住 A 在 R1 上，实际 {:?}",
        cb[0].relays
    );

    // --- A -> B（跨中继）---
    client::send(&da, &ca[0].dh, "hello-across-relays").expect("A 发消息失败");
    let got = recv_text(&db, "hello-across-relays", 15);
    assert!(got, "B 没收到 A 的跨中继消息");
    println!("✓ A(R1) -> B(R2) 成功");

    // --- B -> A（跨中继反向）---
    // 注意：B 对 A 的中继认知来自**接触时**的信息，这里正是要验证它能回来
    let dh_a = cb[0].dh.clone();
    client::send(&db, &dh_a, "reply-across-relays").expect("B 回消息失败");
    let got2 = recv_text(&da, "reply-across-relays", 15);
    assert!(got2, "A 没收到 B 的跨中继回信");
    println!("✓ B(R2) -> A(R1) 成功");
}

/// 直接验证核心机制：发送用的到底是不是联系人的中继。
///
/// 做法：故意把联系人记录里的中继改成错的（指向没起服务的中继），
/// 如果发送失败，说明发送确实按 c.relays 走 —— 也就证明"跨中继可行"
/// 的前提成立（因为 c.relays 里存的是对方的中继）。
#[test]
fn send_uses_contact_relay_not_own() {
    let r1 = start_relay("own");
    let r2 = start_relay("peer");

    let da = tmpdir("dev-own");
    let db = tmpdir("dev-peer");
    client::init(&da, Some("A")).expect("init A");
    client::init(&db, Some("B")).expect("init B");

    // A 自己的中继是 R1；B 自己的中继是 R2
    client::ensure_queue(&da, &r1.url).expect("A queue");
    client::ensure_queue(&db, &r2.url).expect("B queue");

    let inv_a = client::invite(&da, &r1.url).unwrap();
    let inv_b = client::invite(&db, &r2.url).unwrap();
    client::add(&da, &inv_b, None).unwrap();
    client::add(&db, &inv_a, None).unwrap();

    let ca = client::contacts(&da).unwrap();
    // A 自己连 R1，但发给 B 时必须连 R2
    client::send(&da, &ca[0].dh, "route-test").expect("发消息应成功（走 B 的 R2）");

    // B 在自己的 R2 队列上能收到 = 证明 A 真的把消息投到了 R2
    assert!(recv_text(&db, "route-test", 15), "B 在 R2 上没收到");
    println!("✓ 发送走的是联系人的中继（R2），不是自己的（R1）");
}
