//! 回归：生成邀请码后**立即**收消息，必须拿到积压。
//!
//! 背景（2026-10-20）：App 端实测两次复现——队列刚建好（invite 完成）后
//! 收消息循环静默卡死，服务活着、无日志、永远收不到；强杀进程重启后才补投。
//! 根因是握手无超时 + 收消息超时按帧重置。修复后这里守护两件事：
//! 1. invite → 立刻 recv：积压必须马上到（不只是"重启后能到"）；
//! 2. 超时必须按时返回 None（deadline 语义），而不是无限等。
//!
//! 这是真实端到端：拉起真正的 relay 进程（cargo 自动构建并注入
//! CARGO_BIN_EXE_relay），Bob 真发、Alice 真收。

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn tmpdir(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("relay-ir-{tag}-{nanos}"));
    d.to_str().unwrap().to_string()
}

#[test]
fn invite_then_instant_recv_gets_backlog() {
    // ---- 起真 relay（先探测一个空闲端口）----
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

    // 等端口就绪（最多 10 秒）
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

    // ---- Alice：建身份 + 队列，然后**立刻**收 ----
    let a = tmpdir("alice");
    client::init(&a, Some("Alice")).unwrap();
    let code = client::invite(&a, &relay_url).expect("invite 应成功");

    // ---- Bob：添加 Alice 并趁她"不在线"时发两条 ----
    let b = tmpdir("bob");
    client::init(&b, Some("Bob")).unwrap();
    client::add(&b, &code, None).expect("Bob 添加 Alice");
    client::send(&b, "Alice", "积压第一条").expect("Bob 发送 1");
    client::send(&b, "Alice", "积压第二条").expect("Bob 发送 2");

    // ---- Alice 立刻收：新连接 + 首次订阅，积压必须投递 ----
    let t0 = Instant::now();
    let m1 = client::recv_once(&a, 15)
        .expect("recv_once 不应报错")
        .expect("生成邀请码后立刻收必须拿到积压（当年的卡死窗口）");
    assert!(
        t0.elapsed() < Duration::from_secs(15),
        "积压投递不应拖延，实际 {:?}",
        t0.elapsed()
    );
    assert_eq!(m1.text, "积压第一条");

    // 连接复用的第二次收：同进程缓存连接，第二条积压直接到
    let m2 = client::recv_once(&a, 15)
        .expect("第二次 recv_once 不应报错")
        .expect("第二条积压必须能收到");
    assert_eq!(m2.text, "积压第二条");

    // ---- 队列空了：超时必须**按时**返回 None（deadline 语义）----
    let t1 = Instant::now();
    let none = client::recv_once(&a, 3).expect("超时不是错误");
    let waited = t1.elapsed();
    assert!(none.is_none(), "空队列应返回 None");
    assert!(
        waited < Duration::from_secs(10),
        "3 秒超时应按时返回，实际等了 {waited:?}（按帧重置的老 bug 会拖到不可预期）"
    );

    // ---- 再来一条实时消息，立刻收到 ----
    client::send(&b, "Alice", "实时第三条").unwrap();
    let m3 = client::recv_once(&a, 15)
        .expect("第三次 recv_once 不应报错")
        .expect("实时消息必须能收到");
    assert_eq!(m3.text, "实时第三条");

    relay.kill().ok();
    std::fs::remove_dir_all(&a).ok();
    std::fs::remove_dir_all(&b).ok();
    std::fs::remove_dir_all(&data).ok();
}
