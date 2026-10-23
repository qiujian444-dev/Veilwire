//! Mainline DHT（BEP5）轻客户端：中继自注册 + 客户端发现共用同一套实现。
//!
//! 两个方向：
//!   - 中继侧 `run(cfg)`：周期性把自己公布到 DHT（`announce_peer`）；
//!   - 客户端侧 `discover()`：迭代 `get_peers` 找到别人公布的中继地址。
//!
//! 为什么是 BEP5 而不是 BEP44：
//!   BEP44 的存储键是 `SHA1(公钥 + salt)`，一个键只接受同一密钥的单调 seq，
//!   多个中继无法共用一个键；而且客户端必须**先知道公钥**才能查，做不了"发现"。
//!   BEP5 的 peer 发现天生支持任意多节点注册到同一个 infohash，才是对的形式。
//!
//! ## 三条必须的防御（实测踩出来的）
//!
//! 1. **来源 IP 校验**：响应来源必须等于查询目标，否则丢弃。
//!    发往 6881 端口的查询会被路径注入伪造响应。
//! 2. **事务 ID 校验**：`t` 字段必须与请求一致，否则丢弃。
//! 3. **重复记录检测**：一批 `nodes` 里 node_id 或 ip:port 全相同 → 整包丢弃。
//!    真实的 Kademlia k-bucket 不可能出现 8 条完全一样的记录，这是注入特征。
//!
//! 另外：bootstrap 域名普遍被污染（实测 `router.utorrent.com` 会被解析到
//! Facebook 的 IP 段），所以**必须把成功连过的节点持久化**，下次优先用它们。

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::net::UdpSocket;

// ------------------------------------------------------------------ bencode

/// 只覆盖 KRPC 需要的类型。dict 用 BTreeMap 保证 key 有序（bencode 要求）。
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Val>),
    Dict(BTreeMap<Vec<u8>, Val>),
}

impl Val {
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Val::Bytes(b) => Some(b),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        self.as_bytes().and_then(|b| std::str::from_utf8(b).ok())
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Val::Int(i) => Some(*i),
            _ => None,
        }
    }
    pub fn get(&self, k: &[u8]) -> Option<&Val> {
        match self {
            Val::Dict(m) => m.get(k),
            _ => None,
        }
    }
}

pub fn bencode(v: &Val) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    enc(v, &mut out);
    out
}

fn enc(v: &Val, out: &mut Vec<u8>) {
    match v {
        Val::Int(i) => {
            out.push(b'i');
            out.extend_from_slice(i.to_string().as_bytes());
            out.push(b'e');
        }
        Val::Bytes(b) => {
            out.extend_from_slice(b.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(b);
        }
        Val::List(l) => {
            out.push(b'l');
            for it in l {
                enc(it, out);
            }
            out.push(b'e');
        }
        Val::Dict(m) => {
            out.push(b'd');
            for (k, v) in m.iter() {
                out.extend_from_slice(k.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(k);
                enc(v, out);
            }
            out.push(b'e');
        }
    }
}

pub fn bdecode(buf: &[u8]) -> Result<Val> {
    let (v, used) = dec(buf, 0)?;
    if used != buf.len() {
        return Err(anyhow!("bencode 尾部有多余字节"));
    }
    Ok(v)
}

fn dec(b: &[u8], mut i: usize) -> Result<(Val, usize)> {
    let c = *b.get(i).ok_or_else(|| anyhow!("bencode 越界"))?;
    match c {
        b'i' => {
            let end = b[i..]
                .iter()
                .position(|&x| x == b'e')
                .ok_or_else(|| anyhow!("整数缺少终止符"))?
                + i;
            let s = std::str::from_utf8(&b[i + 1..end])?;
            Ok((Val::Int(s.parse()?), end + 1))
        }
        b'l' => {
            i += 1;
            let mut out = Vec::new();
            while *b.get(i).ok_or_else(|| anyhow!("列表越界"))? != b'e' {
                let (v, n) = dec(b, i)?;
                out.push(v);
                i = n;
            }
            Ok((Val::List(out), i + 1))
        }
        b'd' => {
            i += 1;
            let mut m = BTreeMap::new();
            while *b.get(i).ok_or_else(|| anyhow!("字典越界"))? != b'e' {
                let (k, n) = dec(b, i)?;
                let key = k.as_bytes().ok_or_else(|| anyhow!("字典 key 必须是字符串"))?.to_vec();
                let (v, n2) = dec(b, n)?;
                m.insert(key, v);
                i = n2;
            }
            Ok((Val::Dict(m), i + 1))
        }
        b'0'..=b'9' => {
            let colon = b[i..]
                .iter()
                .position(|&x| x == b':')
                .ok_or_else(|| anyhow!("字符串缺少冒号"))?
                + i;
            let len: usize = std::str::from_utf8(&b[i..colon])?.parse()?;
            let start = colon + 1;
            let end = start + len;
            if end > b.len() {
                return Err(anyhow!("字符串越界"));
            }
            Ok((Val::Bytes(b[start..end].to_vec()), end))
        }
        _ => Err(anyhow!("无法识别的 bencode 类型 {c}")),
    }
}

// ------------------------------------------------------------------ 节点表

#[derive(Debug, Clone)]
pub struct Node {
    pub id: [u8; 20],
    pub addr: (IpAddr, u16),
}

impl Node {
    fn dist(&self, target: &[u8; 20]) -> [u8; 20] {
        let mut d = [0u8; 20];
        for i in 0..20 {
            d[i] = self.id[i] ^ target[i];
        }
        d
    }
}

/// 从 `nodes` 紧凑字段解析（每 26 字节：20 id + 4 IPv4 + 2 端口）
fn parse_nodes(v: Option<&Val>) -> Vec<Node> {
    let Some(b) = v.and_then(|x| x.as_bytes()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for chunk in b.chunks_exact(26) {
        let mut id = [0u8; 20];
        id.copy_from_slice(&chunk[..20]);
        let ip = std::net::Ipv4Addr::new(chunk[20], chunk[21], chunk[22], chunk[23]);
        let port = u16::from_be_bytes([chunk[24], chunk[25]]);
        if port != 0 && chunk[20..24] != [0, 0, 0, 0] {
            out.push(Node {
                id,
                addr: (IpAddr::V4(ip), port),
            });
        }
    }
    out
}

/// 注入检测：一批节点里 id 全同、或地址全同（且数量够多）→ 判为伪造。
fn looks_poisoned(nodes: &[Node]) -> bool {
    if nodes.len() < 2 {
        return false;
    }
    let uniq_id = nodes.iter().map(|n| n.id).collect::<std::collections::HashSet<_>>();
    if uniq_id.len() == 1 {
        return true;
    }
    let uniq_addr = nodes.iter().map(|n| n.addr).collect::<std::collections::HashSet<_>>();
    nodes.len() >= 6 && uniq_addr.len() == 1
}

// ------------------------------------------------------------------ KRPC

pub struct Krpc {
    sock: UdpSocket,
}

impl Krpc {
    pub async fn new() -> Result<Self> {
        // 绑 0.0.0.0:0：我们只做客户端，不需要固定端口
        let sock = UdpSocket::bind("0.0.0.0:0").await?;
        Ok(Krpc { sock })
    }

    /// 发一条查询并等匹配的响应。
    ///
    /// 三重过滤（见模块注释）：来源 IP、事务 ID、以及调用方的注入检测。
    /// 这里拒绝一切来源不符或 tid 不符的包——GFW 式注入会伪造来源，但那需要
    /// 精确猜测我们的源端口；更常见的是"用别的地址回包"，这里一并挡掉。
    async fn query(
        &self,
        addr: (IpAddr, u16),
        q: &str,
        args: Val,
        timeout: Duration,
    ) -> Result<Val> {
        let tid: Vec<u8> = {
            let mut t = vec![0u8; 4];
            getrandom::getrandom(&mut t).map_err(|_| anyhow!("熵源不可用"))?;
            t
        };
        let mut msg = BTreeMap::new();
        msg.insert(b"a".to_vec(), args);
        msg.insert(b"q".to_vec(), Val::Bytes(q.as_bytes().to_vec()));
        msg.insert(b"t".to_vec(), Val::Bytes(tid.clone()));
        msg.insert(b"y".to_vec(), Val::Bytes(b"q".to_vec()));
        let payload = bencode(&Val::Dict(msg));

        self.sock.send_to(&payload, addr).await?;

        let deadline = tokio::time::Instant::now() + timeout;
        let mut buf = vec![0u8; 4096];
        loop {
            let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remain.is_zero() {
                return Err(anyhow!("KRPC 超时"));
            }
            let (n, from) = match tokio::time::timeout(remain, self.sock.recv_from(&mut buf)).await {
                Ok(r) => r?,
                Err(_) => return Err(anyhow!("KRPC 超时")),
            };
            // 防御 1：来源必须完全一致（用 SocketAddr 比较，避免 v6 scope 之类差异）
            let want = std::net::SocketAddr::new(addr.0, addr.1);
            if from != want {
                continue;
            }
            let Ok(v) = bdecode(&buf[..n]) else { continue };
            // 防御 2：事务 ID 必须匹配
            if v.get(b"t").and_then(|x| x.as_bytes()) != Some(tid.as_slice()) {
                continue;
            }
            return Ok(v);
        }
    }

    async fn find_node(&self, addr: (IpAddr, u16), target: &[u8; 20], id: &[u8; 20]) -> Result<Vec<Node>> {
        let mut a = BTreeMap::new();
        a.insert(b"id".to_vec(), Val::Bytes(id.to_vec()));
        a.insert(b"target".to_vec(), Val::Bytes(target.to_vec()));
        let v = self
            .query(addr, "find_node", Val::Dict(a), Duration::from_secs(3))
            .await?;
        let nodes = parse_nodes(v.get(b"r").and_then(|r| r.get(b"nodes")));
        if looks_poisoned(&nodes) {
            return Err(anyhow!("响应疑似注入（节点表重复）"));
        }
        Ok(nodes)
    }

    async fn get_peers_token(&self, addr: (IpAddr, u16), infohash: &[u8; 20], id: &[u8; 20]) -> Result<Vec<u8>> {
        let mut a = BTreeMap::new();
        a.insert(b"id".to_vec(), Val::Bytes(id.to_vec()));
        a.insert(b"info_hash".to_vec(), Val::Bytes(infohash.to_vec()));
        let v = self
            .query(addr, "get_peers", Val::Dict(a), Duration::from_secs(3))
            .await?;
        v.get(b"r")
            .and_then(|r| r.get(b"token"))
            .and_then(|t| t.as_bytes())
            .map(|b| b.to_vec())
            .ok_or_else(|| anyhow!("无 token"))
    }

    async fn announce(&self, addr: (IpAddr, u16), infohash: &[u8; 20], port: u16, token: &[u8], id: &[u8; 20]) -> Result<()> {
        let mut a = BTreeMap::new();
        a.insert(b"id".to_vec(), Val::Bytes(id.to_vec()));
        a.insert(b"implied_port".to_vec(), Val::Int(0));
        a.insert(b"info_hash".to_vec(), Val::Bytes(infohash.to_vec()));
        a.insert(b"port".to_vec(), Val::Int(port as i64));
        a.insert(b"token".to_vec(), Val::Bytes(token.to_vec()));
        let v = self
            .query(addr, "announce_peer", Val::Dict(a), Duration::from_secs(3))
            .await?;
        if v.get(b"e").is_some() {
            return Err(anyhow!("announce 被拒绝"));
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ 配置

pub struct DhtConfig {
    /// 对外服务的 WS 端口（写进 DHT 的那个）
    pub ws_port: u16,
    /// TURN 端口（可选；有就多发一个 infohash）
    pub turn_port: Option<u16>,
    /// 重新公布的间隔
    pub interval: Duration,
    /// 节点表缓存文件
    pub state_path: PathBuf,
    /// 初始种子（被污染时靠缓存兜底）
    pub seeds: Vec<(String, u16)>,
    /// 每轮爬取时长
    pub crawl: Duration,
}

impl DhtConfig {
    pub fn from_args(args: &[String], data_dir: &Path, ws_port: u16, turn_port: Option<u16>) -> Option<Self> {
        let i = args.iter().position(|a| a == "--dht-announce")?;
        // --dht-announce 后面可以跟一个"公布用的端口"，不跟就用实际监听端口
        let advertised = args
            .get(i + 1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(ws_port);
        let interval_secs = args
            .iter()
            .position(|a| a == "--dht-interval")
            .and_then(|j| args.get(j + 1))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(600);
        Some(DhtConfig {
            ws_port: advertised,
            turn_port,
            interval: Duration::from_secs(interval_secs.max(60)),
            state_path: data_dir.join("dht-nodes.json"),
            seeds: default_seeds(),
            crawl: Duration::from_secs(45),
        })
    }
}

// ------------------------------------------------------------------ 服务键

/// 消息中继目录。客户端用同一个常量去 `get_peers`。
pub const WS_INFOHASH_SEED: &[u8] = b"veilwire-relays-v1";
/// TURN 端口目录（与 WS 分开，因为端口不同）
pub const TURN_INFOHASH_SEED: &[u8] = b"veilwire-turn-v1";

/// 服务键（20 字节 SHA-1），客户端与中继必须一致。
pub fn service_key() -> [u8; 20] {
    sha1(WS_INFOHASH_SEED)
}

/// TURN 服务键。
pub fn turn_key() -> [u8; 20] {
    sha1(TURN_INFOHASH_SEED)
}

// ------------------------------------------------------------------ 爬取与公布

/// 从种子 + 缓存出发，迭代爬到离 target 最近的一批节点。
/// 朝 target 方向迭代 `find_node`，返回响应过的节点（按距离排序）。
///
/// **必须并发**。早先是单线程串行：每个查询 3 秒超时，45 秒预算只够问十几个节点，
/// 在丢包严重的网络里等于没爬（实测安卓模拟器上 45 秒只拿到 6 个节点——刚好是种子本身，
/// 一个邻居都没进来，于是"发现"永远返回空）。
///
/// 每个并发查询**自带 socket**：`query()` 是"发出去然后在这个 socket 上等自己的
/// 事务号"，多个查询挤一个 socket 会互相把对方的响应丢掉。
///
/// `krpc` 参数保留只为兼容既有调用方（中继的定时公布也走这里），不参与查询。
pub async fn crawl(krpc: &Krpc, seeds: &[Node], target: &[u8; 20], dur: Duration) -> Vec<Node> {
    use std::collections::HashSet;
    let _ = krpc;
    const WIDTH: usize = 16;

    let mut queued: HashSet<(IpAddr, u16)> = HashSet::new();
    let mut queried: HashSet<(IpAddr, u16)> = HashSet::new();
    let mut found: BTreeMap<(IpAddr, u16), Node> = BTreeMap::new();
    let mut cand: Vec<Node> = Vec::new();
    for s in seeds {
        if queued.insert(s.addr) {
            cand.push(s.clone());
        }
    }
    let deadline = tokio::time::Instant::now() + dur;
    let my_id = random_id();

    while tokio::time::Instant::now() < deadline {
        cand.sort_by_key(|n| n.dist(target));
        let batch: Vec<Node> = cand
            .iter()
            .filter(|n| !queried.contains(&n.addr))
            .take(WIDTH)
            .cloned()
            .collect();
        if batch.is_empty() {
            break;
        }
        for n in &batch {
            queried.insert(n.addr);
        }

        let mut set = tokio::task::JoinSet::new();
        for n in batch {
            let t = *target;
            let id = my_id;
            set.spawn(async move {
                let k = Krpc::new().await.ok()?;
                let r = k.find_node(n.addr, &t, &id).await;
                Some((n, r))
            });
        }
        while let Some(joined) = set.join_next().await {
            let Ok(Some((node, r))) = joined else { continue };
            let Ok(nodes) = r else { continue };
            found.insert(node.addr, node);
            for n in nodes {
                if queued.insert(n.addr) {
                    cand.push(n);
                }
            }
        }
    }

    let mut v: Vec<Node> = found.into_values().collect();
    v.sort_by_key(|n| n.dist(target));
    v
}

fn random_id() -> [u8; 20] {
    let mut id = [0u8; 20];
    let _ = getrandom::getrandom(&mut id);
    id
}

/// 对最近的若干节点公布自己。返回成功数。
pub async fn announce_all(
    krpc: &Krpc,
    nodes: &[Node],
    infohash: &[u8; 20],
    port: u16,
    limit: usize,
) -> usize {
    let mut ok = 0;
    let my_id = random_id();
    for n in nodes.iter().take(limit) {
        let Ok(token) = krpc.get_peers_token(n.addr, infohash, &my_id).await else {
            continue;
        };
        if krpc.announce(n.addr, infohash, port, &token, &my_id).await.is_ok() {
            ok += 1;
        }
    }
    ok
}

// ------------------------------------------------------------------ 持久化

pub fn load_nodes(path: &Path) -> Vec<Node> {
    let Ok(txt) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
        return Vec::new();
    };
    let Some(arr) = v.get("nodes").and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|o| {
            let id_hex = o.get("id")?.as_str()?;
            let ip: IpAddr = o.get("ip")?.as_str()?.parse().ok()?;
            let port = o.get("port")?.as_u64()? as u16;
            let raw = hex_decode(id_hex)?;
            if raw.len() != 20 {
                return None;
            }
            let mut id = [0u8; 20];
            id.copy_from_slice(&raw);
            Some(Node { id, addr: (ip, port) })
        })
        .collect()
}

pub fn save_nodes(path: &Path, nodes: &[Node]) {
    let arr: Vec<serde_json::Value> = nodes
        .iter()
        .take(512)
        .map(|n| {
            serde_json::json!({
                "id": hex_encode(&n.id),
                "ip": n.addr.0.to_string(),
                "port": n.addr.1,
            })
        })
        .collect();
    let doc = serde_json::json!({ "v": 1, "nodes": arr });
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // 先写临时文件再 rename：避免进程被杀时留下半截 JSON
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, doc.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

// ------------------------------------------------------------------ 客户端发现

/// 客户端发现的结果：一个 peer 地址。
pub type Peer = (IpAddr, u16);

/// 迭代 `get_peers` 收集所有公布者。
///
/// 做法与 BT 客户端一致：从最近的一批节点开始，边查边把返回的 `nodes`
/// 按 XOR 距离重排，直到收敛或超时。**只信来源与事务号都匹配的响应**。
///
/// 一批查询是**并发**的：串行查 8 个节点、每个 3 秒超时，一轮就是 24 秒，
/// 手机端冷启动完全不可接受（实测要 60s+）。这里每个并发任务用独立 socket，
/// 避免多个查询共用一个 socket 时互抢响应包（那会让 tid 过滤把别人的包丢掉）。
pub async fn lookup(
    krpc: &Krpc,
    seeds: &[Node],
    infohash: &[u8; 20],
    dur: Duration,
) -> (Vec<Peer>, Vec<Node>) {
    use std::collections::HashSet;
    let mut peers: BTreeMap<Peer, ()> = BTreeMap::new();
    let mut queried: HashSet<Peer> = HashSet::new();
    let my_id = random_id();
    let deadline = tokio::time::Instant::now() + dur;

    // 先爬到一批"离目标最近"的节点；直接拿种子去 get_peers 通常什么都拿不到
    let crawled = crawl(krpc, seeds, infohash, (dur / 2).max(Duration::from_secs(5))).await;
    let mut cand: Vec<Node> = if crawled.len() >= 4 { crawled } else { seeds.to_vec() };

    while tokio::time::Instant::now() < deadline {
        cand.sort_by_key(|n| n.dist(infohash));
        let batch: Vec<Node> = cand
            .iter()
            .filter(|n| !queried.contains(&n.addr))
            .take(12)
            .cloned()
            .collect();
        if batch.is_empty() {
            break;
        }
        for n in &batch {
            queried.insert(n.addr);
        }

        let mut set = tokio::task::JoinSet::new();
        for n in batch {
            let t = *infohash;
            let id = my_id;
            set.spawn(async move {
                let Ok(k) = Krpc::new().await else { return None };
                get_peers(&k, n.addr, &t, &id).await
            });
        }
        while let Some(joined) = set.join_next().await {
            let Ok(Some((ps, ns))) = joined else { continue };
            for p in ps {
                peers.insert(p, ());
            }
            for c in ns {
                if !queried.contains(&c.addr) {
                    cand.push(c);
                }
            }
        }
    }
    // 顺带把用过的节点表返回：调用方可以持久化，下次冷启动不必再解析被污染的域名
    (peers.into_keys().collect(), cand)
}

/// get_peers：返回 (已公布的 peer, 更近的节点)
async fn get_peers(
    krpc: &Krpc,
    addr: Peer,
    infohash: &[u8; 20],
    id: &[u8; 20],
) -> Option<(Vec<Peer>, Vec<Node>)> {
    let mut a = BTreeMap::new();
    a.insert(b"id".to_vec(), Val::Bytes(id.to_vec()));
    a.insert(b"info_hash".to_vec(), Val::Bytes(infohash.to_vec()));
    let v = krpc
        .query(addr, "get_peers", Val::Dict(a), Duration::from_secs(3))
        .await
        .ok()?;
    let r = v.get(b"r")?;
    let peers = parse_peer_values(r.get(b"values"));
    let nodes = parse_nodes(r.get(b"nodes"));
    // 节点表被注入时仍然采信 peers（peers 是从 values 字段来的，另一条路径），
    // 但不再用这批 nodes 扩散，避免把伪造节点灌进候选集。
    if looks_poisoned(&nodes) {
        Some((peers, Vec::new()))
    } else {
        Some((peers, nodes))
    }
}

/// `values` 是「6 字节 compact peer 字符串组成的 list」。
///
/// 这里踩过一次坑：早期把它当单个拼接的 bytes 处理，导致真实 peers 全被丢掉，
/// 于是得出"网络上没有 peer"的错误结论。BEP5 允许 list 和单个 bytes 两种形态，
/// 两种都要认。
fn parse_peer_values(v: Option<&Val>) -> Vec<Peer> {
    let Some(v) = v else { return Vec::new() };
    let chunks: Vec<&[u8]> = match v {
        Val::List(l) => l.iter().filter_map(|x| x.as_bytes()).collect(),
        Val::Bytes(b) => b.chunks_exact(6).collect(),
        _ => return Vec::new(),
    };
    chunks
        .into_iter()
        .filter_map(|c| {
            if c.len() < 6 {
                return None;
            }
            let ip = std::net::Ipv4Addr::new(c[0], c[1], c[2], c[3]);
            let port = u16::from_be_bytes([c[4], c[5]]);
            if port == 0 {
                None
            } else {
                Some((IpAddr::V4(ip), port))
            }
        })
        .collect()
}

/// 客户端一站式发现：读缓存 → 不够就补种子 → 爬取 + get_peers → 写回缓存。
///
/// `cache` 是节点表缓存路径（建议放在设备目录下），下次可以跳过域名解析。
/// 一次发现的过程数据。
///
/// **为什么要连过程一起返回**：Rust 的 stderr 在 Android 上被系统丢弃，
/// 光返回"没找到"根本没法区分是 DNS 挂了、UDP 出不去、还是时间不够。
/// 有了这三个数字，看一行日志就知道卡在哪一步。
pub struct Discovery {
    pub peers: Vec<Peer>,
    /// 参与播种的节点数（缓存 + 解析成功的 bootstrap 域名）
    pub seeds: usize,
    /// 成功响应过的节点数
    pub nodes: usize,
}

pub async fn discover(cache: &Path, target: [u8; 20], dur: Duration) -> Discovery {
    let mut seeds = load_nodes(cache);

    // 内置 IP 快照**无条件**参与，不看缓存多少。
    //
    // 因为缓存里完全可能是那批投毒节点：它们能正常响应 ping/find_node，
    // 只是节点表永远是空的——于是"用了缓存"反而把自己钉死在原地，
    // 12 个内置 IP 里只要有一个活着，爬取就能重新展开。
    {
        let mut seen: std::collections::HashSet<_> = seeds.iter().map(|n| n.addr).collect();
        for (ip, port) in STATIC_SEEDS {
            let Ok(ip) = ip.parse::<IpAddr>() else { continue };
            let addr = (ip, *port);
            if seen.insert(addr) {
                seeds.push(Node {
                    id: random_id(), // 真实 id 由响应补上
                    addr,
                });
            }
        }
    }

    // 域名只在连一个可用地址都没有时才去解析（正常永远走不到这里），
    // 因为这套网络里它们基本都被污染了，解析反而浪费时间。
    if seeds.is_empty() {
        seeds = resolve_seeds(&default_seeds()).await;
    }
    if seeds.is_empty() {
        return Discovery {
            peers: Vec::new(),
            seeds: 0,
            nodes: 0,
        };
    }
    let seed_count = seeds.len();
    let Ok(krpc) = Krpc::new().await else {
        return Discovery {
            peers: Vec::new(),
            seeds: seed_count,
            nodes: 0,
        };
    };
    let (peers, nodes) = lookup(&krpc, &seeds, &target, dur).await;

    // 缓存节点表：即使这次没发现 peer，节点表本身也是资产
    // （下次冷启动不必再依赖被污染的 bootstrap 域名）
    if !nodes.is_empty() {
        save_nodes(cache, &nodes);
    }

    Discovery {
        peers,
        seeds: seed_count,
        nodes: nodes.len(),
    }
}

/// 内置 bootstrap 快照：**IP 直连，不过 DNS**。
///
/// 为什么必须有这一份：这套网络的 DNS 解析 bootstrap 域名会给出投毒地址。
/// 实测（2026-10）：
///   - `router.bittorrent.com` → 108.160.162.31 / 108.160.167.30（Dropbox 的段）
///   - `dht.transmissionbt.com` → 212.129.33.59（那个只会回 8 条完全相同记录的节点）
///
/// 投毒节点**会正常响应**，只是节点表是空的。于是"爬邻居"永远原地踏步：
/// 45 秒预算只拿到 6 个节点——刚好是种子自己，一个邻居都没进来，
/// 表现就是"自动发现永远找不到中继"，而且没有任何报错。
///
/// 域名仍保留在列表里（万一哪天通了），但这批 IP 排在最前面：
/// 只要有一个能用，就完全不依赖 DNS。
///
/// ⚠️ 这份快照会腐烂（里面的家用节点随时下线，端口也在变）。
/// 它的定位是"首次安装时的最后一根拐杖"：一旦成功爬到节点，
/// 节点表就写进缓存（`dht-nodes.json`），之后冷启动都用缓存，不再需要它。
/// 挑选标准：当时实测 `find_node` 有响应且返回 >= 8 个真实邻居（非注入）。
pub const STATIC_SEEDS: &[(&str, u16)] = &[
    // 公共 bootstrap（libtorrent），端口 25401 是它的标准端口
    ("185.157.221.247", 25401),
    ("185.212.237.153", 59504),
    ("194.44.26.64", 3948),
    ("217.65.122.153", 5481),
    ("195.28.149.178", 5113),
    // 下面这些是普通客户端节点，用来提高"至少有一个活"的概率，不要指望长期稳定
    ("223.166.34.36", 55555),
    ("123.109.83.123", 35255),
    ("5.77.204.166", 14765),
    ("218.86.186.26", 51413),
    ("84.236.75.218", 51413),
    ("84.86.183.76", 31687),
    ("109.173.237.124", 51413),
];

/// 默认 bootstrap 种子：内置 IP 快照在前，域名在后。
pub fn default_seeds() -> Vec<(String, u16)> {
    let mut out: Vec<(String, u16)> = STATIC_SEEDS
        .iter()
        .map(|(ip, port)| ((*ip).to_string(), *port))
        .collect();
    out.extend([
        ("router.bittorrent.com".to_string(), 6881),
        ("dht.transmissionbt.com".to_string(), 6881),
        ("router.utorrent.com".to_string(), 6881),
        ("dht.libtorrent.org".to_string(), 25401),
    ]);
    out
}

// ------------------------------------------------------------------ 主循环

/// 解析种子域名。全部失败时返回空（靠缓存兜底）。
async fn resolve_seeds(seeds: &[(String, u16)]) -> Vec<Node> {
    let mut out = Vec::new();
    for (host, port) in seeds {
        // 每个域名最多等 3 秒：被污染/黑洞的 DNS 会让 lookup_host 挂很久，
        // 而这里是最开局的一步，拖掉的时间直接等于"发现失败"。
        // IP 字符串不会真的走 DNS，立刻返回。
        let Ok(res) = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::lookup_host((host.as_str(), *port)),
        )
        .await
        else {
            continue;
        };
        if let Ok(mut it) = res {
            if let Some(sa) = it.next() {
                out.push(Node {
                    id: random_id(), // 真实 id 由 find_node 响应补上，这里先占位
                    addr: (sa.ip(), sa.port()),
                });
            }
        }
    }
    out
}

/// 后台任务：启动时公布一次，之后按 interval 重复。
pub async fn run(cfg: DhtConfig) {
    let krpc = match Krpc::new().await {
        Ok(k) => k,
        Err(e) => {
            eprintln!("[dht] 初始化失败: {e:#}");
            return;
        }
    };

    let ws_key: [u8; 20] = sha1(WS_INFOHASH_SEED);
    let turn_key: Option<[u8; 20]> = cfg.turn_port.map(|_| sha1(TURN_INFOHASH_SEED));

    // 先用上一轮缓存，再用种子补充
    let mut seeds = load_nodes(&cfg.state_path);
    let cached_n = seeds.len();
    if seeds.len() < 8 {
        let resolved = resolve_seeds(&cfg.seeds).await;
        let mut seen: std::collections::HashSet<_> = seeds.iter().map(|n| n.addr).collect();
        for n in resolved {
            if seen.insert(n.addr) {
                seeds.push(n);
            }
        }
    }
    println!(
        "[dht] 启动：缓存节点 {cached_n} 个，种子合计 {} 个；公布 WS 端口 {}",
        seeds.len(),
        cfg.ws_port
    );

    loop {
        let nodes = crawl(&krpc, &seeds, &ws_key, cfg.crawl).await;
        if nodes.len() < 3 {
            eprintln!(
                "[dht] ⚠ 本轮只爬到 {} 个可用节点，公布可能无效（缓存 {} 个）",
                nodes.len(),
                load_nodes(&cfg.state_path).len()
            );
        }
        let ok_ws = announce_all(&krpc, &nodes, &ws_key, cfg.ws_port, 24).await;
        let mut ok_turn = 0;
        if let (Some(tk), Some(tp)) = (turn_key, cfg.turn_port) {
            ok_turn = announce_all(&krpc, &nodes, &tk, tp, 24).await;
        }
        println!(
            "[dht] 已公布：WS 端口 {} 成功 {ok_ws} 个节点{}（候选 {}）",
            cfg.ws_port,
            cfg.turn_port
                .map(|p| format!("；TURN 端口 {p} 成功 {ok_turn} 个节点"))
                .unwrap_or_default(),
            nodes.len()
        );
        if !nodes.is_empty() {
            save_nodes(&cfg.state_path, &nodes);
            seeds = nodes.clone();
        }
        tokio::time::sleep(cfg.interval).await;
    }
}

/// SHA-1（BEP5 的 infohash 就是 20 字节 SHA-1）。
fn sha1(data: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(data);
    let d = h.finalize();
    let mut out = [0u8; 20];
    out.copy_from_slice(&d);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bencode_roundtrip() {
        let mut m = BTreeMap::new();
        m.insert(b"id".to_vec(), Val::Bytes(vec![1, 2, 3]));
        m.insert(b"n".to_vec(), Val::Int(42));
        m.insert(
            b"l".to_vec(),
            Val::List(vec![Val::Int(1), Val::Bytes(b"x".to_vec())]),
        );
        let v = Val::Dict(m);
        let e = bencode(&v);
        assert_eq!(bdecode(&e).unwrap(), v);
    }

    #[test]
    fn bencode_dict_is_sorted() {
        // bencode 要求 dict 的 key 按字节序排列，否则节点会拒收（错误码 203）
        let mut m = BTreeMap::new();
        m.insert(b"z".to_vec(), Val::Int(1));
        m.insert(b"a".to_vec(), Val::Int(2));
        let e = bencode(&Val::Dict(m));
        assert_eq!(e, b"d1:ai2e1:zi1ee".to_vec());
    }

    #[test]
    fn parse_compact_nodes() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&[7u8; 20]); // id
        raw.extend_from_slice(&[1, 2, 3, 4]); // ip
        raw.extend_from_slice(&6881u16.to_be_bytes()); // port
        let nodes = parse_nodes(Some(&Val::Bytes(raw)));
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].addr.0.to_string(), "1.2.3.4");
        assert_eq!(nodes[0].addr.1, 6881);
    }

    #[test]
    fn detect_duplicate_poison() {
        let dup = |id: u8| Node {
            id: [id; 20],
            addr: ("1.2.3.4".parse().unwrap(), 6881),
        };
        // 8 条 id 与地址完全相同 → 注入（真实 k-bucket 不可能重复）
        assert!(looks_poisoned(&vec![dup(9); 8]));

        // id 不同但地址全同、条目够多 → 也判注入
        let same_addr: Vec<Node> = (0..8u8).map(dup).collect();
        assert!(looks_poisoned(&same_addr));

        // 正常多样 → 不判注入
        let ok: Vec<Node> = (0..8u8)
            .map(|i| Node {
                id: [i; 20],
                addr: (std::net::Ipv4Addr::new(10, 0, 0, i).into(), 6881),
            })
            .collect();
        assert!(!looks_poisoned(&ok));

        // 少量节点不武断判定
        assert!(!looks_poisoned(&[dup(1)]));
    }

    #[test]
    fn infohash_is_sha1_of_seed() {
        let k = sha1(WS_INFOHASH_SEED);
        assert_eq!(k.len(), 20);
        assert_eq!(hex_encode(&k), "efbbbe88c129ff9b39ee161dcfe004da61fcd9e3");
    }

    #[test]
    fn parse_peer_values_both_forms() {
        // BEP5 允许 values 是「6 字节字符串的 list」，也允许是拼接的单个 bytes。
        // 早期实现只认后者，把真实 peers 全丢了，误判"网络上没有 peer"。
        let one = vec![203u8, 0, 113, 10, 0x0d, 0x96]; // 203.0.113.10:3478
        let list = Val::List(vec![Val::Bytes(one.clone()), Val::Bytes(one.clone())]);
        let peers = parse_peer_values(Some(&list));
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].0.to_string(), "203.0.113.10");
        assert_eq!(peers[0].1, 3478);

        let joined = Val::Bytes({
            let mut v = one.clone();
            v.extend_from_slice(&one);
            v
        });
        assert_eq!(parse_peer_values(Some(&joined)).len(), 2);

        // 端口为 0 的记录必须丢掉（表示"已下线"）
        let dead = Val::List(vec![Val::Bytes(vec![1, 2, 3, 4, 0, 0])]);
        assert!(parse_peer_values(Some(&dead)).is_empty());
    }

    #[test]
    fn nodes_cache_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "dht-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("nodes.json");
        let nodes = vec![Node {
            id: [3u8; 20],
            addr: ("203.0.113.5".parse().unwrap(), 4242),
        }];
        save_nodes(&p, &nodes);
        let back = load_nodes(&p);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].addr.1, 4242);
        assert_eq!(back[0].id, [3u8; 20]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
