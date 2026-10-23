//! 真实网络测试：客户端从 Mainline DHT 发现中继。
//!
//! 标了 `#[ignore]`，因为它依赖外网 DHT 且耗时 25s+，不适合放进常规回归。
//! 手动跑：
//!   cargo test -p client --test discover_live -- --ignored --nocapture
//!
//! 前置：另开一个终端跑
//!   relay serve --addr 127.0.0.1:18899 --data ./dhtdata --dht-announce 18899

#[test]
#[ignore]
fn discover_relays_from_mainline_dht() {
    let dir = std::env::temp_dir().join(format!(
        "discover-live-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_str().unwrap();

    client::init(d, Some("probe")).expect("init");

    let started = std::time::Instant::now();
    let relays = client::discover_relays(d, 40).expect("发现失败");
    println!(
        "耗时 {:?}，发现 {} 个候选：",
        started.elapsed(),
        relays.len()
    );
    for r in &relays {
        println!("  {r}");
    }

    // 至少要能发现东西；具体端口取决于当时哪个中继在公布
    assert!(
        !relays.is_empty(),
        "一个中继都没发现（检查是否有中继以 --dht-announce 运行，或网络是否不通）"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
