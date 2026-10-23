//! 回归：跨中继的「陌生人回信」路径，以及 `set_relay` 不能污染联系人路由。
//!
//! 这两个用例各自钉住一个已发现的真实缺陷：
//!
//! 1. `merge_relays(via, extra)` 在**接收方**语境里把 `via` 放在第一位。
//!    接收时 `via` 是**自己的**中继（我连自己的中继收自己的队列），
//!    把它当成"发给对方时该连的中继"是错的——对方的队列不在我的中继上。
//!    后果：陌生人回信时 `put` 打到错误中继 → `no_queue` → 回不了。
//!
//! 2. `set_relay` 把**所有联系人**的 relays 全部改写成新地址。
//!    联系人的中继是"他的队列所在处"，与我换不换中继无关。
//!    多中继场景下这会让所有对外发送全部发错地方。

use std::process::{Command, Stdio};
use std::time::Duration;

fn tmpdir(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("relay-cr2-{tag}-{nanos}"));
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
    for _ in 0..80 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    RelayProc {
        child,
        url: format!("ws://127.0.0.1:{port}"),
    }
}

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

/// 收一条指定文本的消息，返回完整 Incoming（保留回信线索）。
fn recv_incoming(dir: &str, want: &str, secs: u64) -> Option<client::Incoming> {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        match client::recv_once(dir, 2) {
            Ok(Some(inc)) => {
                if inc.text == want {
                    return Some(inc);
                }
            }
            Ok(None) => {}
            Err(e) => eprintln!("  recv err: {e:#}"),
        }
    }
    None
}

/// 缺陷 1：陌生人回信必须回到**对方的中继**，不能回到自己的。
#[test]
fn stranger_reply_goes_to_senders_relay() {
    let r1 = start_relay("a");
    let r2 = start_relay("b");

    let da = tmpdir("alice");
    let db = tmpdir("bob");
    client::init(&da, Some("Alice")).unwrap();
    client::init(&db, Some("Bob")).unwrap();

    // Alice 用自己的 R1，Bob 用自己的 R2
    client::ensure_queue(&da, &r1.url).unwrap();
    client::ensure_queue(&db, &r2.url).unwrap();

    // Bob 通过邀请码加了 Alice，于是 Bob 知道 Alice 在 R1
    let inv_a = client::invite(&da, &r1.url).unwrap();
    client::add(&db, &inv_a, None).unwrap();

    // Bob 发一条给 Alice —— Alice 此时还不认识 Bob（陌生人）
    let cb = client::contacts(&db).unwrap();
    client::send(&db, &cb[0].dh, "hi-from-stranger").unwrap();

    // 第一条 recv 会 ack 掉消息，所以必须从这一次拿到回信线索
    let inc = recv_incoming(&da, "hi-from-stranger", 15).expect("Alice 没收到 Bob 的消息");

    println!("reply_qid     = {:?}", inc.reply_qid);
    println!("reply_relays  = {:?}", inc.reply_relays);
    assert!(
        !inc.reply_relays.is_empty(),
        "回信线索里没有中继，陌生人永远回不了"
    );
    // 关键：发送只用 relays.first()，所以**顺序**决定成败。
    // 第一位必须是 Bob 真实所在的 R2，绝不能是 Alice 自己的 R1。
    assert_eq!(
        inc.reply_relays.first().map(|s| s.as_str()),
        Some(r2.url.as_str()),
        "reply_relays 首位应是 Bob 的 R2，实际 {:?}（首位是自己的中继就永远发不到）",
        inc.reply_relays
    );

    // 真的回一条，验证能投到
    client::add_from_card(
        &da,
        "Bob",
        &inc.dh,
        &inc.from_id,
        inc.reply_qid.as_deref().unwrap(),
        &inc.reply_relays,
    )
    .expect("回邀失败");
    let ca = client::contacts(&da).unwrap();
    client::send(&da, &ca[0].dh, "reply-to-stranger").expect("回信发送失败");
    assert!(recv_text(&db, "reply-to-stranger", 15), "Bob 没收到 Alice 的回信");
    println!("✓ 陌生人回信正确落到对方中继");
}

/// 缺陷 2：切换自己的中继，不能改写联系人的中继。
#[test]
fn set_relay_does_not_rewrite_contact_relays() {
    let r1 = start_relay("own1");
    let r2 = start_relay("peer");
    let r3 = start_relay("own2");

    let da = tmpdir("alice2");
    let db = tmpdir("bob2");
    client::init(&da, Some("Alice")).unwrap();
    client::init(&db, Some("Bob")).unwrap();

    client::ensure_queue(&da, &r1.url).unwrap();
    client::ensure_queue(&db, &r2.url).unwrap();

    let inv_a = client::invite(&da, &r1.url).unwrap();
    let inv_b = client::invite(&db, &r2.url).unwrap();
    client::add(&da, &inv_b, None).unwrap();
    client::add(&db, &inv_a, None).unwrap();

    let before = client::contacts(&da).unwrap();
    assert!(before[0].relays.iter().any(|r| r == &r2.url), "前置条件：应记住 Bob 在 R2");

    // Alice 把自己的中继从 R1 换到 R3
    let n = client::set_relay(&da, &r3.url).expect("切换中继失败");

    let after = client::contacts(&da).unwrap();
    println!("set_relay 返回 {n}；切换后 Bob.relays = {:?}", after[0].relays);
    assert!(
        after[0].relays.iter().any(|r| r == &r2.url),
        "切换自己的中继后，Bob 的中继被改成了 {:?}（应该是 R2 不变）",
        after[0].relays
    );

    // 而且切换后仍然能发给 Bob（走 Bob 的 R2）
    client::send(&da, &after[0].dh, "still-works").expect("切换中继后发送失败");
    assert!(recv_text(&db, "still-works", 15), "切换中继后 Bob 收不到");
    println!("✓ 切换自己的中继不影响联系人路由");
}
