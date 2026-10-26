//! 回归：没有队列的设备发消息，必须仍然给对方留下回信线索。
//!
//! 场景来自真机：老版本升级上来的设备只生成过身份、从没生成过邀请码，
//! 于是 `self.qid` 是 `None`。用户扫码加了别人、发了消息，对方那边显示的却是
//! "这条消息没有回信线索（可能来自旧版客户端），暂时无法回复"——
//! 因为 `reply_qid` 直接取了 `me.qid`，没有就去拿不到。
//!
//! 这属于"设备状态不全"被误当成"对方是老客户端"，两条完全不同的原因
//! 在界面上长得一模一样，用户的下一步动作却相反（一个等对方重发，一个该修自己）。
//!
//! 正确行为：发消息时若还没有队列，就先在**自己当前的中继**上建一个，
//! 让这条消息带上可回复的地址。

use std::process::{Command, Stdio};
use std::time::Duration;

fn tmpdir(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("relay-clue-{tag}-{nanos}"));
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

/// 直接改写 self.json，构造"有中继、没队列"这个真实状态。
fn set_self(dir: &str, v: serde_json::Value) {
    let p = std::path::Path::new(dir).join("self.json");
    std::fs::write(&p, serde_json::to_vec_pretty(&v).unwrap()).expect("写 self.json 失败");
}
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

/// 只有身份、没有队列的设备发消息时，必须自动补队列并带上线索。
/// 这些用例各自会真起 relay 进程，而 `free_port()` 是"绑一下再放开"的，
/// 并发跑时两个用例可能抢到同一个端口（TOCTOU），表现为随机失败。
/// 用进程内互斥把它们串起来——比引入 `serial_test` 依赖轻。
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn sender_without_queue_still_leaves_reply_clue() {
    let _serial = serial();
    let r1 = start_relay("a");
    let da = tmpdir("alice"); // 有队列的正常设备
    let db = tmpdir("bob"); // 只有身份，从不生成邀请码

    client::init(&da, Some("Alice")).unwrap();
    client::init(&db, Some("Bob")).unwrap();
    client::ensure_queue(&da, &r1.url).unwrap();
    // Bob 刻意不建队列：模拟"老版本升级上来"的设备

    // Alice 出邀请码，Bob 扫码加她
    let inv = client::invite(&da, &r1.url).unwrap();
    client::add(&db, &inv, None).unwrap();

    // 关键：Bob 配了中继但**没有队列**。
    // 真实成因通常是"配中继时正好离线"或"从老版本升上来，只生成过身份"——
    // `createIdentity` / `add` 里的 ensureQueue 都会因离线失败，之后没人补。
    // 这里直接写 self.json 构造出这个状态，不依赖断网时机。
    set_self(&db, serde_json::json!({ "qid": null, "relays": [r1.url], "name": "Bob" }));
    assert!(
        client::self_queue(&db).unwrap().is_none(),
        "前提：Bob 此刻没有队列"
    );

    let c = client::contacts(&db).unwrap();
    client::send(&db, &c[0].dh, "hi-with-clue").expect("发送失败");

    let inc = recv_incoming(&da, "hi-with-clue", 15).expect("Alice 没收到消息");
    println!("reply_qid    = {:?}", inc.reply_qid);
    println!("reply_relays = {:?}", inc.reply_relays);
    assert!(
        !inc.reply_qid.as_deref().unwrap_or("").is_empty(),
        "对方收到消息但没有回信地址（reply_qid 为空）——\
         这正是界面上显示\"没有回信线索\"的原因"
    );
    assert!(
        !inc.reply_relays.is_empty(),
        "对方收到消息但没有回信中继，回不了"
    );

    // 真的回一条，确认整条链路通
    let name = client::add_from_card(
        &da,
        "Bob",
        &inc.dh,
        &inc.from_id,
        inc.reply_qid.as_deref().unwrap(),
        &inc.reply_relays,
    )
    .expect("回邀失败");
    client::send(&da, &inc.dh, "pong-with-clue").expect("回信失败");
    assert!(
        recv_incoming(&db, "pong-with-clue", 15).is_some(),
        "Bob 没收到回信（{name}）"
    );
    println!("✓ 无队列设备发出的消息可以正常回复");
}

/// 自己记的中继已经死了、但对方那台活着：必须退到对方的中继上建队列。
///
/// 这正是真机上的形态——手机换过中继、或原来那台停了，`self.relays`
/// 里是个连不上的地址；而消息能发出去，说明对方的中继此刻是通的。
#[test]
fn dead_own_relay_falls_back_to_peers_relay() {
    let _serial = serial();
    let r1 = start_relay("a");
    let da = tmpdir("alice");
    let db = tmpdir("bob");
    client::init(&da, Some("Alice")).unwrap();
    client::init(&db, Some("Bob")).unwrap();
    client::ensure_queue(&da, &r1.url).unwrap();

    let inv = client::invite(&da, &r1.url).unwrap();
    client::add(&db, &inv, None).unwrap();

    // Bob 记着一个死掉的中继、且没有队列
    let dead = format!("ws://127.0.0.1:{}", free_port());
    set_self(&db, serde_json::json!({ "qid": null, "relays": [dead], "name": "Bob" }));

    let c = client::contacts(&db).unwrap();
    client::send(&db, &c[0].dh, "hi-over-fallback").expect("应当退到对方中继上发出去");

    let inc = recv_incoming(&da, "hi-over-fallback", 15).expect("Alice 没收到");
    assert!(
        !inc.reply_qid.as_deref().unwrap_or("").is_empty(),
        "退到备用中继后仍然没带回信地址"
    );
    println!("reply_qid = {:?}", inc.reply_qid);
    println!("✓ 死中继下仍能补建队列并带上线索");
}
