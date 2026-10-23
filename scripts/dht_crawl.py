"""Mainline DHT 完整爬取测试 —— 验证"大陆能否连上足够多的 DHT 节点"。

方法：从种子出发做 BFS 式 find_node 爬取，同时做 ping 存活验证，
统计能积累多少真实可达节点，再测 BEP44 put/get 成功率。

这个脚本是为了回答一个具体问题：如果只有少量 bootstrap 入口且被污染，
是否仍然能通过迭代爬取加入网络？如果能，规模多大？

用法：python scripts/dht_crawl.py [秒数]
"""
import socket, sys, os, time, hashlib, collections, threading, queue

# ------------------------------------------------------------------ bencode

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
        j = b.index(b"e", i)
        return int(b[i+1:j]), j + 1
    if c == b"l":
        i += 1
        o = []
        while b[i:i+1] != b"e":
            v, i = bdec(b, i)
            o.append(v)
        return o, i + 1
    if c == b"d":
        i += 1
        o = {}
        while b[i:i+1] != b"e":
            k, i = bdec(b, i)
            v, i = bdec(b, i)
            o[k] = v
        return o, i + 1
    j = b.index(b":", i)
    n = int(b[i:j])
    return b[j+1:j+1+n], j+1+n

# ------------------------------------------------------------------ 种子

SEEDS_V4 = [
    ("router.bittorrent.com", 6881),
    ("router.utorrent.com", 6881),
    ("dht.transmissionbt.com", 6881),
    ("dht.libtorrent.org", 25401),
    ("dht.aelitis.com", 6881),
    ("router.bitcomet.com", 6881),
    ("dht.bt.am", 2710),
    ("bt.dht.love", 6881),
    ("dht.zq.edu.pl", 6881),
    ("router.bittorrent.net", 6881),
]

def resolve(host, port):
    try:
        return socket.gethostbyname(host), socket.gethostbyname(host)
    except Exception:
        return None, None

# ------------------------------------------------------------------ RPC

MY_ID = os.urandom(20)
lock = threading.Lock()
live_nodes = {}      # (ip,port) -> node_id
dead_count = 0
banned = set()

def krpc(sock, ip, port, q, args, timeout=3.0, tid=None):
    tid = tid or os.urandom(2)
    msg = benc({"a": args, "q": q, "t": tid, "y": "q"})
    sock.settimeout(timeout)
    sock.sendto(msg, (ip, port))
    deadline = time.time() + timeout
    while time.time() < deadline:
        d, src = sock.recvfrom(2048)
        # 必须过滤来源：只接受来自目标 IP 的响应
        if src[0] != ip:
            continue
        try:
            r = bdec(d)[0]
        except Exception:
            continue
        if r.get(b"t") != tid:
            continue
        return r
    raise socket.timeout()

def parse_nodes(nr):
    out = []
    for i in range(0, len(nr) - 25, 26):
        nid = nr[i:i+20]
        ip = ".".join(str(x) for x in nr[i+20:i+24])
        port = int.from_bytes(nr[i+24:i+26], "big")
        if port and ip != "0.0.0.0":
            out.append((ip, port, nid))
    return out

# ------------------------------------------------------------------ 阶段1：种子

print("=" * 68)
print("阶段 1：种子入口可达性")
print("=" * 68)

seeds = []
for host, port in SEEDS_V4:
    try:
        ip = socket.gethostbyname(host)
    except Exception:
        print(f"  DNS失败   {host}")
        continue
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        r = krpc(s, ip, port, "ping", {"id": MY_ID}, timeout=3)
        if b"r" in r:
            print(f"  ✓ PONG    {host:26s} {ip}:{port}")
            seeds.append((ip, port))
        else:
            print(f"  ✗ 异常    {host:26s} {ip}:{port} {r.get(b'e')}")
    except Exception as e:
        print(f"  ✗ {type(e).__name__:8s} {host:26s} {ip}:{port}")
    finally:
        s.close()

print(f"\n可用种子 {len(seeds)} / {len(SEEDS_V4)}")
if not seeds:
    print("==> 无任何可用入口")
    raise SystemExit(1)

# ------------------------------------------------------------------ 阶段2：爬取

DURATION = int(sys.argv[1]) if len(sys.argv) > 1 else 45
print()
print("=" * 68)
print(f"阶段 2：迭代爬取 {DURATION} 秒")
print("=" * 68)

seen = set()          # 已见过 (ip,port)
frontier = queue.Queue()
for ip, port in seeds:
    seen.add((ip, port))
    frontier.put((ip, port))

stop_at = time.time() + DURATION
stat = collections.Counter()

def worker():
    global dead_count
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    while time.time() < stop_at:
        try:
            ip, port = frontier.get(timeout=0.5)
        except queue.Empty:
            continue
        target = hashlib.sha1(os.urandom(8)).digest()
        try:
            r = krpc(s, ip, port, "find_node", {"id": MY_ID, "target": target}, timeout=2.5)
            stat["ok"] += 1
            nr = r.get(b"r", {}).get(b"nodes", b"")
            with lock:
                live_nodes[(ip, port)] = r.get(b"r", {}).get(b"id", b"")
            for nip, nport, nid in parse_nodes(nr):
                with lock:
                    if (nip, nport) not in seen:
                        seen.add((nip, nport))
                        frontier.put((nip, nport))
        except Exception:
            stat["fail"] += 1
    s.close()

ths = [threading.Thread(target=worker, daemon=True) for _ in range(12)]
for t in ths:
    t.start()
for t in ths:
    t.join()

print(f"  查询成功 {stat['ok']} / 失败 {stat['fail']}（成功率 {stat['ok']*100//max(1,stat['ok']+stat['fail'])}%）")
print(f"  发现唯一地址 {len(seen)} 个")
print(f"  响应有效（真实可达）{len(live_nodes)} 个")

# ------------------------------------------------------------------ 阶段3：BEP44

print()
print("=" * 68)
print("阶段 3：BEP44 写入测试（对前 12 个可达节点）")
print("=" * 68)

val = b"veilwire-crawl-" + os.urandom(8)
vhash = hashlib.sha1(benc(val)).digest()
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
put_ok = []
put_fail = 0
supports_bep44 = 0

for ip, port in list(live_nodes.keys())[:12]:
    try:
        r = krpc(s, ip, port, "get", {"id": MY_ID, "target": vhash}, timeout=2.5)
        tok = r.get(b"r", {}).get(b"token")
        if not tok:
            print(f"  {ip}:{port} 无 token")
            put_fail += 1
            continue
        r2 = krpc(s, ip, port, "put", {"id": MY_ID, "token": tok, "v": val}, timeout=2.5)
        if b"r" in r2:
            print(f"  ✓ PUT     {ip}:{port}")
            put_ok.append((ip, port))
        else:
            err = r2.get(b"e")
            print(f"  ✗ REJ     {ip}:{port} {err}")
            # 205=值太大 206=签名错 203=token错 204=方法不支持
            put_fail += 1
    except Exception as e:
        print(f"  ✗ {type(e).__name__:9s} {ip}:{port}")
        put_fail += 1

print(f"\n  put 成功 {len(put_ok)} / 失败 {put_fail}")

if put_ok:
    print("\n  回读验证：")
    hit = 0
    for ip, port in put_ok:
        try:
            r = krpc(s, ip, port, "get", {"id": MY_ID, "target": vhash}, timeout=2.5)
            got = r.get(b"r", {}).get(b"v")
            if got == val:
                hit += 1
        except Exception:
            pass
    print(f"    命中 {hit} / {len(put_ok)}")
