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
    data: String,
}

impl Drop for RelayProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_relay(tag: &str) -> RelayProc {
    start_relay_on(free_port(), tag)
}

fn start_relay_on(port: u16, tag: &str) -> RelayProc {
    start_relay_with(port, &tmpdir(&format!("{tag}-data")))
}

/// 用同一个数据目录重启中继（模拟"中继维护完恢复"）：
/// 队列在磁盘上，重启后必须还在——换个新目录等于换了一台新机器。
fn start_relay_with(port: u16, data: &str) -> RelayProc {
    let data = data.to_string();
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
        data,
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
/// 这些用例各自会真起 relay 进程，而 `free_port()` 是"绑一下再放开"的，
/// 并发跑时两个用例可能抢到同一个端口（TOCTOU），表现为随机失败。
/// 用进程内互斥把它们串起来——比引入 `serial_test` 依赖轻。
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn stranger_reply_goes_to_senders_relay() {
    let _serial = serial();
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
    let _serial = serial();
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

/// 换中继后，对方必须能学到新地址（`route` 通知），并且**双向**都能继续通。
///
/// 这是"迁移引导"的核心承诺：我换了信箱，联系人把我的新地址记下来，
/// 而不是继续往废弃队列发（那种情况下消息堆在旧中继上没人取，
/// 双方界面上还都显示"已发送"）。
#[test]
fn relay_switch_notifies_contact_and_traffic_resumes() {
    let _serial = serial();
    let r1 = start_relay("m1");
    let r2 = start_relay("m2");
    let r3 = start_relay("m3");

    let da = tmpdir("alice-mv");
    let db = tmpdir("bob-mv");
    client::init(&da, Some("Alice")).unwrap();
    client::init(&db, Some("Bob")).unwrap();

    // 各自的中继：Alice@R1，Bob@R2
    client::ensure_queue(&da, &r1.url).unwrap();
    client::ensure_queue(&db, &r2.url).unwrap();

    // 互加（走真实邀请码）
    let inv_a = client::invite(&da, &r1.url).unwrap();
    client::add(&db, &inv_a, None).unwrap();
    let inv_b = client::invite(&db, &r2.url).unwrap();
    client::add(&da, &inv_b, None).unwrap();

    // 先确认基线能通
    let cb = client::contacts(&db).unwrap();
    client::send(&db, &cb[0].dh, "before-move").unwrap();
    assert!(recv_text(&da, "before-move", 20), "换中继前基线不通");

    // Alice 把信箱从 R1 搬到 R3
    let (notified, total) = client::set_relay_report(&da, &r3.url).expect("切换中继失败");
    assert_eq!(total, 1, "应当有 1 个联系人需要通知");
    assert_eq!(notified, 1, "在线联系人的通知必须发出去");

    // Bob 仍然把消息发到 Alice 的新队列：他要先消费掉那条 route 帧
    // （recv_once 在 Rust 层直接消费，不上浮）。
    for _ in 0..5 {
        let _ = client::recv_once(&db, 2);
    }
    let cb = client::contacts(&db).unwrap();
    client::send(&db, &cb[0].dh, "after-move").unwrap();
    assert!(
        recv_text(&da, "after-move", 25),
        "换中继后对方没学到新地址（消息应该投到 R3 的新队列）"
    );

    // 反方向也要通：Alice 发回 Bob，仍然走 Bob 的 R2
    let ca = client::contacts(&da).unwrap();
    client::send(&da, &ca[0].dh, "reply-after-move").unwrap();
    assert!(recv_text(&db, "reply-after-move", 25), "换中继后回信不通");

    // 并且 Alice 没有污染 Bob 的中继记录（仍然在 R2）
    assert!(
        ca[0].relays.iter().any(|r| r == &r2.url),
        "换自己的中继不该改联系人所在中继: {:?}",
        ca[0].relays
    );
}

/// 对方中继此刻不可达时，换中继通知必须**排队重试**，而不是悄悄丢掉。
///
/// 丢了会怎样：对方永远不知道我的新地址，之后一直往废弃队列发，
/// 消息堆在旧中继上没人取，而发送方界面显示"已发送"，双方都看不出问题。
#[test]
fn failed_route_notice_is_retried_until_delivered() {
    let _serial = serial();
    let r1 = start_relay("p1");
    let r2_port = free_port();
    let mut r2 = start_relay_on(r2_port, "p2");
    let r3 = start_relay("p3");
    let r2_url = r2.url.clone();
    let r2_data = r2.data.clone();

    let da = tmpdir("alice-pending");
    let db = tmpdir("bob-pending");
    client::init(&da, Some("Alice")).unwrap();
    client::init(&db, Some("Bob")).unwrap();
    client::ensure_queue(&da, &r1.url).unwrap();
    client::ensure_queue(&db, &r2_url).unwrap();

    let inv_a = client::invite(&da, &r1.url).unwrap();
    client::add(&db, &inv_a, None).unwrap();
    let inv_b = client::invite(&db, &r2_url).unwrap();
    client::add(&da, &inv_b, None).unwrap();

    // 把 Bob 的中继停掉：此时 Alice 的换中继通知发不出去
    drop(r2);
    std::thread::sleep(Duration::from_millis(300));

    let (notified, total) = client::set_relay_report(&da, &r3.url).expect("切换中继失败");
    assert_eq!(total, 1);
    assert_eq!(notified, 0, "对方中继已死，不该报告成功");
    assert!(
        std::path::Path::new(&da).join("pending_routes.json").exists(),
        "推失败的通知必须落盘排队"
    );

    // 对方中继恢复（同一个数据目录：队列必须还在）
    let _r2b = start_relay_with(r2_port, &r2_data);
    std::thread::sleep(Duration::from_millis(500));

    let n = client::flush_pending_routes(&da).expect("补发失败");
    assert_eq!(n, 1, "对方中继恢复后应当补发成功");
    assert!(
        !std::path::Path::new(&da).join("pending_routes.json").exists(),
        "补发成功后不该留下待办文件"
    );

    // Bob 消费掉 route 帧后，消息应当投到 Alice 在 R3 的新队列
    for _ in 0..5 {
        let _ = client::recv_once(&db, 2);
    }
    let cb = client::contacts(&db).unwrap();
    client::send(&db, &cb[0].dh, "after-retry").unwrap();
    assert!(recv_text(&da, "after-retry", 25), "补发后对方仍没用上新地址");
}

/// 送达回执闭环：收到文字消息后回一条 `rcpt`，发送方据此点亮双勾。
#[test]
fn delivery_receipt_reaches_sender() {
    let _serial = serial();
    let r1 = start_relay("d1");
    let da = tmpdir("alice-rcpt");
    let db = tmpdir("bob-rcpt");
    client::init(&da, Some("Alice")).unwrap();
    client::init(&db, Some("Bob")).unwrap();
    client::ensure_queue(&da, &r1.url).unwrap();
    client::ensure_queue(&db, &r1.url).unwrap();

    let inv_a = client::invite(&da, &r1.url).unwrap();
    client::add(&db, &inv_a, None).unwrap();
    let inv_b = client::invite(&db, &r1.url).unwrap();
    client::add(&da, &inv_b, None).unwrap();

    let ca = client::contacts(&da).unwrap();
    let rid = client::send(&da, &ca[0].dh, "need-receipt").unwrap();

    // Bob 收到后必须产生一条待发回执
    let inc = recv_incoming(&db, "need-receipt", 20).expect("Bob 没收到");
    assert_eq!(inc.rid, rid, "回执要引用发送方的消息 id");

    let n = client::flush_receipts(&db).expect("发回执失败");
    assert_eq!(n, 1, "应当发出 1 条回执");

    // Alice 收到 rcpt 帧，body 里带的就是她那条的 id
    let mut got = None;
    for _ in 0..6 {
        if let Ok(Some(x)) = client::recv_once(&da, 2) {
            if x.kind == "rcpt" {
                got = Some(x);
                break;
            }
        }
    }
    let rcpt = got.expect("Alice 没收到回执");
    let body = rcpt.body.unwrap_or_default();
    assert!(body.contains(&rid), "回执里应当包含消息 id: {body}");
}
