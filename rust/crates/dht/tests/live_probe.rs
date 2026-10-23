//! 诊断：从默认种子出发，crawl 与 get_peers 各能拿到多少。
//! 手动跑：cargo test -p dht --test live_probe -- --ignored --nocapture
use std::time::Duration;

#[tokio::test]
#[ignore]
async fn probe_bootstrap_quality() {
    let mut seeds = Vec::new();
    for (h, p) in dht::default_seeds() {
        match tokio::net::lookup_host((h.as_str(), p)).await {
            Ok(mut it) => {
                if let Some(sa) = it.next() {
                    println!("  DNS {h} -> {}", sa.ip());
                    seeds.push(dht::Node {
                        id: [0u8; 20],
                        addr: (sa.ip(), sa.port()),
                    });
                }
            }
            Err(e) => println!("  DNS {h} 失败 {e}"),
        }
    }
    println!("种子 {} 个", seeds.len());

    let krpc = dht::Krpc::new().await.unwrap();
    let target = dht::service_key();
    let nodes = dht::crawl(&krpc, &seeds, &target, Duration::from_secs(30)).await;
    println!("crawl 30s -> {} 个可用节点", nodes.len());
    for n in nodes.iter().take(6) {
        println!("  {}:{}", n.addr.0, n.addr.1);
    }

    let (peers, _nodes) = dht::lookup(&krpc, &seeds, &target, Duration::from_secs(40)).await;
    println!("lookup 40s -> {} 个 peer", peers.len());
    for p in &peers {
        println!("  {}:{}", p.0, p.1);
    }
}
