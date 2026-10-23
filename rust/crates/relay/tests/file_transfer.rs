//! 回归：文件分块传输走现有加密消息通道。
//!
//! 设计（2026-10-21，任务⑤）：文件不引入新通道，拆成多条 kind="file" 的消息，
//! body 为 JSON {fid, name, size, idx, total, data(base64)}。发送端用
//! `send_batch` 一次连接推全部块（逐块 send 会各自做一次 WS 握手，不可用）。
//!
//! 这里端到端验证：真 relay 进程 + 真客户端，把一段"文件"切成多块发出，
//! 接收端按 idx 拼回的字节必须与原文件逐字节一致；乱序到达也能按 idx 组装
//! （队列是 FIFO，但接收端不依赖顺序——JSON 里带 idx，拼装以 idx 为准）。

use base64::engine::general_purpose::STANDARD_NO_PAD as B64;
use base64::Engine;
use serde_json::Value;
use std::process::{Command, Stdio};
use std::time::Duration;

fn tmpdir(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("relay-ft-{tag}-{nanos}"));
    d.to_str().unwrap().to_string()
}

const CHUNK: usize = 1024; // 测试用小块，凑够多块即可

fn chunk_body(fid: &str, name: &str, total: usize, idx: usize, data: &[u8]) -> String {
    serde_json::json!({
        "fid": fid,
        "name": name,
        "size": CHUNK * total,
        "idx": idx,
        "total": total,
        "data": B64.encode(data),
    })
    .to_string()
}

#[test]
fn file_chunks_roundtrip_through_relay() {
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

    // ---- 双方互加（Alice 收，Bob 发）----
    let a = tmpdir("alice");
    client::init(&a, Some("Alice")).unwrap();
    let code = client::invite(&a, &relay_url).expect("invite 应成功");
    let b = tmpdir("bob");
    client::init(&b, Some("Bob")).unwrap();
    client::add(&b, &code, None).expect("Bob 添加 Alice");

    // ---- "文件"：5 块、每块内容不同且可校验 ----
    let total = 5;
    let raw: Vec<u8> = (0..CHUNK * total).map(|i| (i % 251) as u8).collect();
    let fid = format!("ft-{}", &client::invite(&b, &relay_url).unwrap()[..12.min(12)]);

    let bodies: Vec<String> = (0..total)
        .map(|i| {
            chunk_body(
                &fid,
                "测试.bin",
                total,
                i,
                &raw[i * CHUNK..(i + 1) * CHUNK],
            )
        })
        .collect();

    // ---- Bob 一次连接把全部块推过去 ----
    client::send_batch(&b, "Alice", "file", &bodies).expect("批量发送文件块");

    // ---- Alice 逐块收，按 idx 拼装（不依赖到达顺序）----
    let mut got: Vec<Option<Vec<u8>>> = vec![None; total];
    let mut name = String::new();
    for _ in 0..total {
        let m = client::recv_once(&a, 15)
            .expect("收块不应报错")
            .expect("每一块都必须到达");
        assert_eq!(m.kind, "file", "收到的应是 file 类型消息");
        let v: Value = serde_json::from_str(m.body.as_deref().expect("file 必须带 body"))
            .expect("body 应是合法 JSON");
        assert_eq!(v["fid"].as_str().unwrap(), fid, "fid 必须匹配");
        let idx = v["idx"].as_u64().unwrap() as usize;
        let t = v["total"].as_u64().unwrap() as usize;
        assert_eq!(t, total, "total 必须匹配");
        name = v["name"].as_str().unwrap().to_string();
        got[idx] = Some(B64.decode(v["data"].as_str().unwrap()).expect("data 应是合法 base64"));
    }

    assert_eq!(name, "测试.bin");
    let mut assembled = Vec::with_capacity(raw.len());
    for (i, c) in got.iter().enumerate() {
        let c = c.as_ref().unwrap_or_else(|| panic!("第 {i} 块缺失"));
        assembled.extend_from_slice(c);
    }
    assert_eq!(assembled.len(), raw.len(), "拼回的长度必须一致");
    assert!(assembled == raw, "拼回的内容必须逐字节一致");

    // 多块之后普通文本仍能正常收发（棘轮推进没有因批量发送而错乱）
    client::send(&b, "Alice", "文件之后文本照常").unwrap();
    let m = client::recv_once(&a, 15).unwrap().expect("文本应能收到");
    assert_eq!(m.text, "文件之后文本照常");

    relay.kill().ok();
    std::fs::remove_dir_all(&a).ok();
    std::fs::remove_dir_all(&b).ok();
    std::fs::remove_dir_all(&data).ok();
}
