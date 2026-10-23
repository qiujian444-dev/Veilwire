"""部署后自检：确认一台中继真的在干活，而不是"端口开着"。

比 curl 一个端口强的地方在于它走完整协议：
  1. WebSocket 握手（101）
  2. ping → pong
  3. get_turn → 中继签发的临时 TURN 凭据（0.15.0 起）
  4. new → 新建队列（验证写入通路，队列靠 TTL 自然过期）
  5. --dht：从 Mainline DHT 反查这台中继是否被公布出去

用法：
  python relay_check.py 1.2.3.4:8080
  python relay_check.py 1.2.3.4:8080 --dht
  python relay_check.py 1.2.3.4:8080 --dht --wait 90

退出码 0 = 全通。任何一步失败都会指出卡在哪。
"""
import base64
import hashlib
import os
import socket
import struct
import sys
import time

# ---------------------------------------------------------------- WebSocket


def ws_connect(host, port, timeout=10):
    s = socket.create_connection((host, port), timeout=timeout)
    s.settimeout(timeout)
    key = base64.b64encode(os.urandom(16)).decode()
    req = (
        f"GET / HTTP/1.1\r\nHost: {host}:{port}\r\n"
        "Upgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    s.sendall(req.encode())
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = s.recv(1024)
        if not chunk:
            raise RuntimeError(f"握手期间连接被关闭：{buf[:120]!r}")
        buf += chunk
    head = buf.split(b"\r\n\r\n")[0].decode(errors="replace")
    if "101" not in head.split("\r\n")[0]:
        raise RuntimeError(f"握手失败：{head.split(chr(13))[0]}")
    return s, buf.split(b"\r\n\r\n", 1)[1]


def ws_send_text(s, text):
    payload = text.encode()
    header = bytearray([0x81])  # FIN + text
    n = len(payload)
    if n < 126:
        header.append(0x80 | n)
    elif n < 65536:
        header.append(0x80 | 126)
        header += struct.pack("!H", n)
    else:
        header.append(0x80 | 127)
        header += struct.pack("!Q", n)
    mask = os.urandom(4)
    header += mask
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    s.sendall(bytes(header) + masked)


def ws_recv_text(s, pending=b""):
    def need(n, buf):
        while len(buf) < n:
            chunk = s.recv(4096)
            if not chunk:
                raise RuntimeError("读取帧时连接被关闭")
            buf += chunk
        return buf

    buf = need(2, pending)
    b0, b1 = buf[0], buf[1]
    opcode = b0 & 0x0F
    masked = bool(b1 & 0x80)
    n = b1 & 0x7F
    off = 2
    if n == 126:
        buf = need(off + 2, buf)
        n = struct.unpack("!H", buf[off:off + 2])[0]
        off += 2
    elif n == 127:
        buf = need(off + 8, buf)
        n = struct.unpack("!Q", buf[off:off + 8])[0]
        off += 8
    if masked:
        buf = need(off + 4, buf)
        mask = buf[off:off + 4]
        off += 4
        buf = need(off + n, buf)
        payload = bytes(b ^ mask[i % 4] for i, b in enumerate(buf[off:off + n]))
    else:
        buf = need(off + n, buf)
        payload = buf[off:off + n]
    rest = buf[off + n:]
    if opcode == 0x8:
        raise RuntimeError("中继主动关闭了连接")
    if opcode == 0x9:  # ping：回一个 pong 再继续等
        return None, rest
    return payload.decode(errors="replace"), rest


def ws_call(s, frame, want, pending=b"", tries=6):
    """发一帧，等指定 t 类型的响应（中间可能夹着别的帧）。"""
    import json

    ws_send_text(s, json.dumps(frame))
    for _ in range(tries):
        txt, pending = ws_recv_text(s, pending)
        if txt is None:
            continue
        msg = json.loads(txt)
        if msg.get("t") == want:
            return msg, pending
    raise RuntimeError(f"没有等到 {want} 响应")


# ---------------------------------------------------------------- Mainline DHT

SEED_HOSTS = [
    ("router.bittorrent.com", 6881),
    ("dht.transmissionbt.com", 6881),
    ("router.utorrent.com", 6881),
    ("dht.libtorrent.org", 25401),
]


def benc(x):
    if isinstance(x, bool):
        raise TypeError("bencode 没有 bool")
    if isinstance(x, int):
        return b"i" + str(x).encode() + b"e"
    if isinstance(x, str):
        x = x.encode()
    if isinstance(x, bytes):
        return str(len(x)).encode() + b":" + x
    if isinstance(x, list):
        return b"l" + b"".join(benc(i) for i in x) + b"e"
    if isinstance(x, dict):
        items = [(k.encode() if isinstance(k, str) else k, v) for k, v in x.items()]
        items.sort(key=lambda kv: kv[0])
        return b"d" + b"".join(benc(k) + benc(v) for k, v in items) + b"e"
    raise TypeError(type(x))


def bdec(b, i=0):
    c = b[i:i + 1]
    if c == b"i":
        j = b.index(b"e", i)
        return int(b[i + 1:j]), j + 1
    if c == b"l":
        i += 1
        out = []
        while b[i:i + 1] != b"e":
            v, i = bdec(b, i)
            out.append(v)
        return out, i + 1
    if c == b"d":
        i += 1
        out = {}
        while b[i:i + 1] != b"e":
            k, i = bdec(b, i)
            v, i = bdec(b, i)
            out[k] = v
        return out, i + 1
    j = b.index(b":", i)
    n = int(b[i:j])
    return b[j + 1:j + 1 + n], j + 1 + n


def krpc(sock, target, q, args, timeout=2.5):
    """发一次 KRPC 并只接受"来源与事务号都对得上"的响应。

    这两个校验缺一不可：发往 6881 的请求会被路径注入伪造响应，
    伪造包的事务号是错的；光看来源也不够，因为来源会被伪造成目标 IP。
    """
    tid = os.urandom(2)
    sock.settimeout(timeout)
    sock.sendto(benc({"a": args, "q": q, "t": tid, "y": "q"}), target)
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            data, src = sock.recvfrom(4096)
        except socket.timeout:
            break
        if src[0] != target[0]:
            continue
        try:
            r = bdec(data)[0]
        except Exception:
            continue
        if r.get(b"t") != tid:
            continue
        return r
    return None


def nodes_of(raw):
    out = []
    for i in range(0, len(raw) - 25, 26):
        addr = (".".join(str(x) for x in raw[i + 20:i + 24]),
                int.from_bytes(raw[i + 24:i + 26], "big"))
        if addr[1]:
            out.append((raw[i:i + 20], addr))
    return out


def peers_of(values):
    """BEP5 的 values 是「6 字节字符串组成的 list」，不是拼接的 bytes。"""
    out = []
    if isinstance(values, bytes):
        values = [values[i:i + 6] for i in range(0, len(values) - 5, 6)]
    for v in values or []:
        if len(v) >= 6:
            out.append((".".join(str(x) for x in v[0:4]),
                        int.from_bytes(v[4:6], "big")))
    return out


def looks_injected(nodes):
    """8 条记录地址或 node id 全相同 = 注入特征，真实 k-bucket 不会这样。"""
    if len(nodes) < 6:
        return False
    addrs = {a for _, a in nodes}
    ids = {n for n, _ in nodes}
    return len(addrs) == 1 or len(ids) == 1


def _xor(a, b):
    return int.from_bytes(bytes(x ^ y for x, y in zip(a, b)), "big")


def dht_crawl(seeds, target, budget, threads=10, cap=400):
    """朝 target 方向迭代 find_node，返回真正响应过的节点 [(id, addr)]。

    为什么要先爬一遍：bootstrap 种子的 node id 离服务键很远，它们手里
    根本不存这条记录。直接对它们 get_peers 永远是空结果——必须先走到
    "离目标最近的那一圈"，那里才是记录所在。
    """
    import queue
    import threading

    seen = {(ip, port) for _, (ip, port) in seeds}
    q = queue.Queue()
    for _, addr in seeds:
        q.put(addr)
    live, lock = [], threading.Lock()
    stop = time.time() + budget

    def worker():
        sk = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        while time.time() < stop:
            try:
                addr = q.get(timeout=0.3)
            except queue.Empty:
                continue
            r = krpc(sk, addr, "find_node", {"id": os.urandom(20), "target": target}, timeout=2.0)
            if not r:
                continue
            rr = r.get(b"r", {})
            got = nodes_of(rr.get(b"nodes", b""))
            if looks_injected(got):
                continue  # 路径注入：整包丢弃，别把假节点灌进候选表
            nid = rr.get(b"id", b"")
            with lock:
                if len(nid) == 20:
                    live.append((nid, addr))
                for cnid, caddr in got:
                    if caddr not in seen and len(seen) < cap:
                        seen.add(caddr)
                        q.put(caddr)
        sk.close()

    ts = [threading.Thread(target=worker, daemon=True) for _ in range(threads)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    return live


def dht_find(service, want, budget):
    """两段式反查：先爬到服务键附近，再在最近的一圈里 get_peers。"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    seeds = []
    for host, port in SEED_HOSTS:
        try:
            ip = socket.gethostbyname(host)
        except Exception:
            continue
        r = krpc(sock, (ip, port), "ping", {"id": os.urandom(20)})
        if r and b"r" in r:
            seeds.append((os.urandom(20), (ip, port)))
    print(f"  bootstrap 可用 {len(seeds)}/{len(SEED_HOSTS)}")
    if not seeds:
        return {}

    nodes = dht_crawl(seeds, service, budget * 0.6)
    print(f"  爬到 {len(nodes)} 个活跃节点，开始查最近的一圈")
    if not nodes:
        return {}

    found, seen = {}, set()
    deadline = time.time() + budget * 0.4
    cand = list(nodes)
    while time.time() < deadline and len(found) < 40:
        cand.sort(key=lambda n: _xor(n[0], service))
        batch = [c for c in cand if c[1] not in seen][:10]
        if not batch:
            break
        for nid, addr in batch:
            seen.add(addr)
            r = krpc(sock, addr, "get_peers",
                     {"id": os.urandom(20), "info_hash": service}, timeout=2.0)
            if not r:
                continue
            rr = r.get(b"r", {})
            got = nodes_of(rr.get(b"nodes", b""))
            if looks_injected(got):
                continue
            for peer in peers_of(rr.get(b"values", [])):
                found[peer] = addr
            for cnid, caddr in got:
                cand.append((cnid, caddr))
        if want in found:
            break
    return found


# ---------------------------------------------------------------- main


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    spec = sys.argv[1]
    want_dht = "--dht" in sys.argv
    budget = 60
    if "--wait" in sys.argv:
        budget = int(sys.argv[sys.argv.index("--wait") + 1])
    if "://" in spec:
        spec = spec.split("://", 1)[1]
    host, _, port_s = spec.partition(":")
    port = int(port_s or 8080)
    ok = True

    print(f"== 1) WebSocket 握手 {host}:{port}")
    try:
        s, pending = ws_connect(host, port)
        print("   ✓ 101 Switching Protocols")
    except Exception as e:
        print(f"   ✗ {e}")
        return 1

    print("== 2) 心跳")
    try:
        pong, pending = ws_call(s, {"t": "ping"}, "pong", pending)
        print("   ✓ pong")
    except Exception as e:
        print(f"   ✗ {e}")
        ok = False

    creds = None
    print("== 3) 索取 TURN 临时凭据")
    try:
        msg, pending = ws_call(s, {"t": "get_turn"}, "turn_creds", pending)
        creds = msg
        user, ttl, url = msg.get("user", ""), msg.get("ttl", 0), msg.get("url", "")
        assert user.startswith("eph-"), f"用户名不像临时凭据: {user}"
        assert ttl > 0, f"ttl 不合理: {ttl}"
        assert host in url, f"返回的 TURN 地址不是这台机器: {url}"
        print(f"   ✓ {url}  user={user}  ttl={ttl}s")
    except Exception as e:
        # 老版本中继没这个能力是正常的，说清楚而不是判失败
        print(f"   · 没有拿到临时凭据（{e}）——若是 0.14 及更早的中继属正常")

    print("== 4) 新建队列（验证写入通路）")
    try:
        msg, pending = ws_call(s, {"t": "new", "v": 1}, "new_ok", pending)
        print(f"   ✓ qid={msg.get('qid')}  ttl={msg.get('ttl')}")
    except Exception as e:
        print(f"   ✗ {e}")
        ok = False

    s.close()

    if want_dht:
        print(f"== 5) 从 Mainline DHT 反查这台中继（最多等 {budget}s）")
        service = hashlib.sha1(b"veilwire-relays-v1").digest()
        found = dht_find(service, (host, port), budget)
        if (host, port) in found:
            print(f"   ✓ 已公布：{host}:{port}")
        else:
            print(f"   ✗ 没查到 {host}:{port}（共 {len(found)} 条结果：{list(found)[:6]}）")
            print("     可能原因：刚公布还没传播开、中继没开 --dht-announce、或公布用的端口不是这个")
            ok = False

    if creds and ok:
        print("\n想进一步验 TURN 转发，可以用上面这组临时凭据跑：")
        print(f"  python turn_probe.py {host} 3478 {creds['user']} {creds['pass']}")
    print("\n结果：" + ("全通" if ok else "有失败项"))
    return 0 if ok else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
