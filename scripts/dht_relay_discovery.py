"""抗投毒的 BEP5 中继自注册 + 冷启动发现 完整验证。

关键前提（前序测试结论）：
  - 发往 6881 端口的 KRPC 查询会被路径注入伪造响应，特征是"8 条记录 node_id 与 ip:port 全同"
  - 发往 6881 之外随机端口的查询是干净的（真实 DHT 节点大量使用 51413 / 28003 / 6887 等）
  → 所以爬取必须做「重复记录检测」，把伪造响应整包丢弃

本脚本完整跑一遍：
  1. 抗投毒爬取
  2. announce_peer 注册中继地址
  3. 全新客户端冷启动 get_peers 发现

用法：python scripts/dht_relay_discovery.py [爬取秒数]
"""
import socket, os, time, hashlib, threading, queue, sys, collections

def benc(x):
    if isinstance(x, bool):
        raise TypeError("bool")
    if isinstance(x, int):
        return b"i" + str(x).encode() + b"e"
    if isinstance(x, str):
        x = x.encode()
    if isinstance(x, bytes):
        return str(len(x)).encode() + b":" + x
    if isinstance(x, list):
        return b"l" + b"".join(benc(i) for i in x) + b"e"
    if isinstance(x, dict):
        it = [(k.encode() if isinstance(k, str) else k, v) for k, v in x.items()]
        it.sort(key=lambda kv: kv[0])
        return b"d" + b"".join(benc(k) + benc(v) for k, v in it) + b"e"
    raise TypeError(type(x))

def bdec(b, i=0):
    c = b[i:i+1]
    if c == b"i":
        j = b.index(b"e", i); return int(b[i+1:j]), j + 1
    if c == b"l":
        i += 1; o = []
        while b[i:i+1] != b"e":
            v, i = bdec(b, i); o.append(v)
        return o, i + 1
    if c == b"d":
        i += 1; o = {}
        while b[i:i+1] != b"e":
            k, i = bdec(b, i); v, i = bdec(b, i); o[k] = v
        return o, i + 1
    j = b.index(b":", i); n = int(b[i:j])
    return b[j+1:j+1+n], j+1+n

def parse_nodes_raw(nr):
    """返回 [(node_id, ip, port), ...]"""
    out = []
    for i in range(0, len(nr) - 25, 26):
        nid = nr[i:i+20]
        ipb = nr[i+20:i+24]
        port = int.from_bytes(nr[i+24:i+26], "big")
        if port and ipb != b"\0\0\0\0":
            out.append((nid, ".".join(str(x) for x in ipb), port))
    return out

def is_poisoned(nodes):
    """伪造特征的判定：>=4 条记录里有效去重数 <=1，或全相同 id。"""
    if len(nodes) < 2:
        return False
    ids = set(n[0] for n in nodes)
    addrs = set((n[1], n[2]) for n in nodes)
    return len(ids) == 1 or (len(nodes) >= 6 and len(addrs) == 1)

def parse_peers(pr):
    """解析 BEP5 get_peers 的 values 字段。

    注意：values 是「6 字节 compact peer 字符串组成的 list」，不是拼接的 bytes。
    早期版本把它当 bytes 处理，导致真实 peers 被丢掉、误判为 0。
    """
    if not pr:
        return []
    if isinstance(pr, list):
        chunks = pr
    elif isinstance(pr, bytes):
        chunks = [pr[i:i+6] for i in range(0, len(pr) - 5, 6)]
    else:
        return []
    out = []
    for c in chunks:
        if not isinstance(c, bytes) or len(c) < 6:
            continue
        ip = ".".join(str(x) for x in c[0:4])
        port = int.from_bytes(c[4:6], "big")
        if port:
            out.append((ip, port))
    return out

def xd(a, b):
    return int.from_bytes(bytes(x ^ y for x, y in zip(a, b)), "big")

def krpc(sock, ip, port, q, args, timeout=2.2):
    tid = os.urandom(4)
    sock.settimeout(timeout)
    sock.sendto(benc({"a": args, "q": q, "t": tid, "y": "q"}), (ip, port))
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            d, src = sock.recvfrom(4096)
        except socket.timeout:
            break
        if src[0] != ip:            # 来源不符直接丢
            continue
        try:
            r = bdec(d)[0]
        except Exception:
            continue
        if r.get(b"t") != tid:      # 事务号不符直接丢
            continue
        return r
    raise socket.timeout()

SEEDS = [("router.bittorrent.com", 6881), ("dht.transmissionbt.com", 6881),
         ("router.utorrent.com", 6881), ("dht.libtorrent.org", 25401),
         ("dht.aelitis.com", 6881), ("dht.bt.am", 2710)]

DUR = int(sys.argv[1]) if len(sys.argv) > 1 else 90
SERVICE = hashlib.sha1(b"veilwire-relays-v1").digest()
RELAY_IP, RELAY_PORT = "203.0.113.10", 3478      # 文档保留地址

print("=" * 74)
print("抗投毒 BEP5 中继自注册 + 冷启动发现")
print("=" * 74)

# --- 种子（避开被污染的 6881 也能用，只要别信伪造响应）
s0 = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
seeds = []
for h, p in SEEDS:
    try:
        ip = socket.gethostbyname(h)
        r = krpc(s0, ip, p, "ping", {"id": os.urandom(20)}, timeout=2.5)
        if b"r" in r:
            seeds.append((ip, p))
            print(f"  种子 ✓ {h} ({ip}:{p})")
    except Exception:
        pass
s0.close()
print(f"  可用种子 {len(seeds)}")
if not seeds:
    raise SystemExit("✗ 无种子")

# --- 抗投毒爬取
print(f"\n--- 爬取 {DUR}s（自动丢弃伪造响应）")
seen = set()
live = {}
frontier = queue.Queue()
for x in seeds:
    seen.add(x); frontier.put(x)
stop = time.time() + DUR
poison_dropped = [0]

def crawl_worker():
    sk = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    while time.time() < stop:
        try:
            ip, port = frontier.get(timeout=0.4)
        except queue.Empty:
            continue
        try:
            r = krpc(sk, ip, port, "find_node",
                     {"id": os.urandom(20), "target": SERVICE}, timeout=2.0)
            rr = r.get(b"r", {})
            nodes = parse_nodes_raw(rr.get(b"nodes", b"") + rr.get(b"nodes6", b"")[:0])
            if is_poisoned(nodes):
                poison_dropped[0] += 1
                continue
            nid = rr.get(b"id", b"")
            if len(nid) == 20:
                live[(ip, port)] = nid
            for cnid, cip, cport in nodes:
                if (cip, cport) not in seen:
                    seen.add((cip, cport)); frontier.put((cip, cport))
        except Exception:
            pass
    sk.close()

ts = [threading.Thread(target=crawl_worker, daemon=True) for _ in range(16)]
for t in ts: t.start()
for t in ts: t.join()

print(f"  丢弃伪造响应 {poison_dropped[0]} 次")
print(f"  发现地址 {len(seen)} 个 / 真实可达 {len(live)} 个")
if len(live) < 5:
    raise SystemExit("✗ 样本不足，重跑")

# --- announce
ranked = sorted(live.items(), key=lambda kv: xd(kv[1], SERVICE))
print(f"\n--- announce_peer 注册 {RELAY_IP}:{RELAY_PORT}")
ann = []
for (ip, port), nid in ranked[:30]:
    try:
        r = krpc(s0 := socket.socket(socket.AF_INET, socket.SOCK_DGRAM),
                 ip, port, "get_peers", {"id": os.urandom(20), "info_hash": SERVICE}, timeout=2.0)
        tok = r.get(b"r", {}).get(b"token")
        s0.close()
        if not tok:
            continue
        s1 = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        r2 = krpc(s1, ip, port, "announce_peer",
                  {"id": os.urandom(20), "info_hash": SERVICE,
                   "port": RELAY_PORT, "implied_port": 0, "token": tok}, timeout=2.0)
        s1.close()
        if b"r" in r2:
            ann.append((ip, port))
            print(f"    ✓ {ip}:{port}")
        else:
            print(f"    ✗ {ip}:{port} {r2.get(b'e')}")
    except Exception as e:
        print(f"    ✗ {ip}:{port} {type(e).__name__}")
print(f"  announce 成功 {len(ann)} / {min(30, len(ranked))}")

# --- 冷启动发现
print(f"\n--- 冷启动发现（全新客户端，只知 seeds）")
s2 = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
found = {}
queried = set()
cand = [(ip, port, b"\x00" * 20) for ip, port in seeds]

for rnd in range(8):
    cand.sort(key=lambda c: xd(c[2], SERVICE))
    batch = [c for c in cand if (c[0], c[1]) not in queried][:8]
    if not batch:
        break
    for ip, port, nid in batch:
        queried.add((ip, port))
        try:
            r = krpc(s2, ip, port, "get_peers", {"id": os.urandom(20), "info_hash": SERVICE}, timeout=2.0)
            rr = r.get(b"r", {})
            nodes = parse_nodes_raw(rr.get(b"nodes", b""))
            if is_poisoned(nodes):
                continue
            for pip, pport in parse_peers(rr.get(b"values", b"")):
                found[(pip, pport)] = (ip, port)
                print(f"    ★ {pip}:{pport}  (via {ip}:{port})")
            for cnid, cip, cport in nodes:
                if (cip, cport) not in queried:
                    cand.append((cip, cport, cnid))
        except Exception:
            pass
    print(f"    第{rnd+1}轮 查 {len(batch)} 节点 / 累计 {len(found)} peer")

print(f"\n  共查询 {len(queried)} 节点")
print(f"  发现 peer: {list(found.keys()) if found else '无'}")
print(f"  命中我们注册的地址: {'是' if (RELAY_IP, RELAY_PORT) in found else '否'}")
