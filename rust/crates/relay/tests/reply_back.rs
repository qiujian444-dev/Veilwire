//! 回归：**回邀闭环** —— 收到陌生人的消息后，必须能把他加为联系人并回过去。
//!
//! 背景：邀请码是单向的。A 拿码加了 B，B 收到 A 的消息时能解密（入站会话已建立），
//! 但 B 的通讯录里没有 A，`find_contact` 找不到 → UI 只能"看不能回"，
//! 要回复得让 A 重新生成一次邀请码。这是产品级的沟通断点。
//!
//! 修复思路：发送方在加密载荷里带上回信线索（`reply_qid` / `reply_relays` / `self_name`），
//! 接收方据此把对方写进通讯录。信任锚点仍然是 Olm 的 `dh`（会话已存在才允许添加）。
//!
//! 本测试守三件事：
//! 1. 陌生人消息里必须带齐回信线索（老客户端不带，`can_reply()` 为 false）；
//! 2. 加为联系人之后，**真的能发回去并且对方能收到**（不是"加了但发不出"）；
//! 3. 没通过信的身份不能被加进通讯录（否则通讯录变成任人写入的公开表）。

use std::process::{Command, Stdio};
use std::time::Duration;

fn tmpdir(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("relay-rb-{tag}-{nanos}"));
    d.to_str().unwrap().to_string()
}

#[test]
fn stranger_message_can_be_replied_via_card() {
    // ---- 起真 relay ----
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let data = tmpdir("relay-data");
    let mut relay = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args([
            "serve",
            "--addr",
            &format!("127.0.0.1:{port}"),
            "--data",
            &data,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("拉起 relay 进程");

    let mut up = false;
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            up = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(up, "relay 没能在 10 秒内监听");
    let relay_url = format!("ws://127.0.0.1:{port}");

    // ---- Bob 出邀请码；Alice 加 Bob（单向：Bob 此时不认识 Alice）----
    let b = tmpdir("bob");
    client::init(&b, Some("Bob")).unwrap();
    let code_b = client::invite(&b, &relay_url).expect("Bob invite");

    let a = tmpdir("alice");
    client::init(&a, Some("Alice")).unwrap();
    let added = client::add(&a, &code_b, None).expect("Alice 加 Bob");
    assert_eq!(added, "Bob");

    // 真实 App 在加完好友后会补建自己的队列（App 侧 Repo 同步调用 ensureQueue）。
    // 不补的话 Alice 只是个哑终端：收不到消息，也没人回得了她。
    let a_qid = client::ensure_queue(&a, &relay_url).expect("Alice 应能建立自己的队列");

    client::send(&a, "Bob", "在吗？我是刚加你的").expect("Alice 发第一条");

    // ---- Bob 收到陌生人消息：回信线索必须齐全 ----
    let inc = client::recv_once(&b, 15)
        .expect("recv_once 不应报错")
        .expect("Bob 必须能收到 Alice 的消息");
    assert_eq!(inc.text, "在吗？我是刚加你的");
    assert!(
        inc.from_name.is_none(),
        "Alice 不在 Bob 的通讯录里，from_name 必须是 None（否则测的就不是陌生人场景）"
    );
    assert_eq!(
        inc.self_name.as_deref(),
        Some("Alice"),
        "载荷必须带发送方自报的名字，否则界面只能显示一串密钥"
    );
    assert!(
        inc.can_reply(),
        "必须带齐 reply_qid + reply_relays，否则接收方无从回复"
    );
    assert_eq!(
        inc.reply_qid.as_deref(),
        Some(a_qid.as_str()),
        "reply_qid 必须是发送方自己的队列，投错地方等于没回"
    );
    assert!(
        !inc.reply_relays.is_empty(),
        "中继候选不能为空：只知道自己是从哪个中继收到的，也够回复"
    );

    // ---- 关键：Bob 用卡片把 Alice 加为联系人 ----
    let name = client::add_from_card(
        &b,
        inc.self_name.as_deref().unwrap_or(""),
        &inc.dh,
        &inc.from_id,
        inc.reply_qid.as_deref().unwrap_or(""),
        &inc.reply_relays,
    )
    .expect("有会话就应该允许加为联系人");
    assert_eq!(name, "Alice");

    // ---- 闭环：Bob 真的能发回去，而且 Alice 收得到 ----
    client::send(&b, "Alice", "在，刚看到").expect("回邀之后必须能发消息");
    let back = client::recv_once(&a, 15)
        .expect("recv_once 不应报错")
        .expect("Alice 必须能收到 Bob 的回复（这就是本测试的全部意义）");
    assert_eq!(back.text, "在，刚看到");

    // ---- 反向也通了：Alice 回一句，Bob 收得到 ----
    client::send(&a, "Bob", "好").expect("Alice 回一句");
    let third = client::recv_once(&b, 15)
        .expect("recv_once 不应报错")
        .expect("Bob 必须收到");
    assert_eq!(third.text, "好");
    assert_eq!(
        third.from_name.as_deref(),
        Some("Alice"),
        "加为联系人之后，from_name 应该能解析出来了"
    );

    // ---- 信任闸：从没通过信的身份不能被加进来 ----
    let err = client::add_from_card(
        &b,
        "查无此人",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
        "qid-forged",
        &[relay_url.clone()],
    )
    .expect_err("没有会话的身份必须被拒绝");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("会话"),
        "错误信息要说清原因（缺会话），实际：{msg}"
    );

    relay.kill().ok();
    std::fs::remove_dir_all(&a).ok();
    std::fs::remove_dir_all(&b).ok();
    std::fs::remove_dir_all(&data).ok();
}
