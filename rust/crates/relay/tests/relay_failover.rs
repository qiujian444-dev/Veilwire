//! 多中继回退：联系人记了多个中继时，第一个用不了必须自动退到下一个。
//!
//! 背景：`merge_relays` 最多保留 4 个中继地址（对方自报的 + 实际投递成功的），
//! 但早期实现发送时只用 `relays.first()`。第一位挂掉就等于消息发不出去，
//! 剩下三个存在磁盘上的地址从来没被用过——记了等于没记。
//!
//! 这三个用例分别钉住：
//!
//! 1. `send_falls_back_to_second_relay`：首位连不上 → 用第二个成功，并把能用的提到首位
//! 2. `no_queue_on_first_relay_falls_through`：首位**连得上**但没他的队列（`no_queue`）
//!    → 必须继续试。只判断"连没连上"是不够的，这是最容易写错的一步
//! 3. `all_relays_dead_reports_failure`：全挂时必须明确报错，不能静默丢弃消息；
//!    中继恢复后同一条会话仍能继续发（失败的加密结果不能落盘）

use std::process::{Command, Stdio};
use std::time::Duration;

fn tmpdir(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("relay-fo-{tag}-{nanos}"));
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

/// 直接改 contacts.json 里的中继顺序。
///
/// 为什么不用公开 API：这个顺序本来是由协议交互自然演进出来的
/// （先收到对方自报，投递成功后再排到前面）。测试要复现的是
/// "列表里存着一个坏地址"这个既成事实，所以只能直接写盘。
fn set_relays(dir: &str, dh: &str, relays: &[String]) {
    let p = std::path::Path::new(dir).join("contacts.json");
    let txt = std::fs::read_to_string(&p).expect("读不到 contacts.json");
    let mut v: serde_json::Value = serde_json::from_str(&txt).expect("contacts.json 不是 JSON");
    let arr = v.as_array_mut().expect("contacts.json 应是数组");
    let c = arr
        .iter_mut()
        .find(|c| c.get("dh").and_then(|x| x.as_str()) == Some(dh))
        .expect("contacts.json 里没有这个联系人");
    c["relays"] = serde_json::json!(relays);
    std::fs::write(&p, serde_json::to_vec_pretty(&v).unwrap()).expect("写 contacts.json 失败");
}

fn relays_of(dir: &str, dh: &str) -> Vec<String> {
    client::contacts(dir)
        .unwrap()
        .into_iter()
        .find(|c| c.dh == dh)
        .expect("找不到联系人")
        .relays
}

/// 首位连不上时退到第二个，并把能用的提到首位（下次不再撞墙）。
#[test]
fn send_falls_back_to_second_relay() {
    let r1 = start_relay("a");
    let da = tmpdir("alice");
    let db = tmpdir("bob");
    client::init(&da, Some("Alice")).unwrap();
    client::init(&db, Some("Bob")).unwrap();
    client::ensure_queue(&da, &r1.url).unwrap();
    client::ensure_queue(&db, &r1.url).unwrap();

    let inv = client::invite(&da, &r1.url).unwrap();
    client::add(&db, &inv, None).unwrap();
    let dh = client::contacts(&db).unwrap()[0].dh.clone();

    // 首位塞一个必然连不上的地址（刚释放的端口，没人监听）
    let dead = format!("ws://127.0.0.1:{}", free_port());
    set_relays(&db, &dh, &[dead.clone(), r1.url.clone()]);

    client::send(&db, &dh, "fallback-ok").expect("首位连不上时应当自动退到第二个中继");
    assert!(recv_text(&da, "fallback-ok", 15), "Alice 没收到退避后投递的消息");
    println!("✓ 首位不可达时投递成功");

    let order = relays_of(&db, &dh);
    println!("投递后的顺序: {order:?}");
    assert_eq!(order[0], r1.url, "能用的中继应当被提到首位（自愈）");
    assert!(
        order.contains(&dead),
        "只重排不删：死地址留着，万一只是临时不可达"
    );
}

/// 首位连得上、但那儿没有他的队列（`no_queue`）时，也必须继续试下一个。
///
/// 这是最容易漏的一步：只判断"连没连上"会以为投递成功，实际上中继明确回了失败。
#[test]
fn no_queue_on_first_relay_falls_through() {
    let r1 = start_relay("a");
    let r_other = start_relay("b"); // 活着，但上面没有 Alice 的队列
    let da = tmpdir("alice");
    let db = tmpdir("bob");
    client::init(&da, Some("Alice")).unwrap();
    client::init(&db, Some("Bob")).unwrap();
    client::ensure_queue(&da, &r1.url).unwrap();
    client::ensure_queue(&db, &r_other.url).unwrap();

    let inv = client::invite(&da, &r1.url).unwrap();
    client::add(&db, &inv, None).unwrap();
    let dh = client::contacts(&db).unwrap()[0].dh.clone();

    // 首位指向一台不持有 Alice 队列的中继：连得上，put 会回 no_queue
    set_relays(&db, &dh, &[r_other.url.clone(), r1.url.clone()]);

    client::send(&db, &dh, "skip-no-queue").expect("no_queue 之后应当继续试下一个中继");
    assert!(recv_text(&da, "skip-no-queue", 15), "Alice 没收到消息");
    println!("✓ no_queue 时继续试下一个中继");

    assert_eq!(
        relays_of(&db, &dh)[0],
        r1.url,
        "真正投成功的那台应当被提到首位"
    );
}

/// 全部中继都不可用时必须报错；中继恢复后同一条会话仍能继续发。
#[test]
fn all_relays_dead_reports_failure() {
    let r1 = start_relay("a");
    let da = tmpdir("alice");
    let db = tmpdir("bob");
    client::init(&da, Some("Alice")).unwrap();
    client::init(&db, Some("Bob")).unwrap();
    client::ensure_queue(&da, &r1.url).unwrap();
    client::ensure_queue(&db, &r1.url).unwrap();

    let inv = client::invite(&da, &r1.url).unwrap();
    client::add(&db, &inv, None).unwrap();
    let dh = client::contacts(&db).unwrap()[0].dh.clone();

    let dead1 = format!("ws://127.0.0.1:{}", free_port());
    let dead2 = format!("ws://127.0.0.1:{}", free_port());
    set_relays(&db, &dh, &[dead1, dead2]);

    let e = client::send(&db, &dh, "nobody-home").expect_err("全挂时必须报错");
    println!("✓ 全挂时报错: {e:#}");
    assert!(recv_text(&da, "nobody-home", 2) == false, "不该有消息送达");

    // 恢复成可用地址后，同一条 Olm 会话必须还能正常发。
    // 如果失败的加密结果被落了盘，这里就会因为棘轮错位而解不开。
    set_relays(&db, &dh, &[r1.url.clone()]);
    client::send(&db, &dh, "back-online").expect("恢复后应当能继续发送");
    assert!(
        recv_text(&da, "back-online", 15),
        "恢复后发的消息 Alice 没收到（失败的加密结果不该落盘）"
    );
    println!("✓ 恢复后同一条会话继续可用");
}
