"""极简 TURN 客户端：用来验证 relay 内嵌的 TURN 是否真的能用。

覆盖：Binding → Allocate（含 401 换 nonce）→ CreatePermission → ChannelBind
      → ChannelData 去程/回程 → Refresh(0) 释放。

用法：
  python turn_probe.py <host> <port> <user> <pass> [realm]
退出码 0 = 全通；非 0 会打印卡在哪一步。
"""
import hashlib
import hmac
import os
import socket
import struct
import sys
import zlib

MAGIC = 0x2112A442
BINDING, ALLOCATE, REFRESH, SEND, DATA, CREATE_PERMISSION, CHANNEL_BIND = (
    0x001, 0x003, 0x004, 0x006, 0x007, 0x008, 0x009)
REQ, IND, OK, ERR = 0, 1, 2, 3

A_MAPPED, A_USERNAME, A_MI, A_ERROR, A_CHANNEL, A_LIFETIME = 0x0001, 0x0006, 0x0008, 0x0009, 0x000C, 0x000D
A_XOR_PEER, A_DATA, A_REALM, A_NONCE, A_XOR_RELAY, A_XOR_MAPPED, A_REQ_TRANSPORT = (
    0x0012, 0x0013, 0x0014, 0x0015, 0x0016, 0x0020, 0x0019)
A_SOFTWARE, A_FINGERPRINT = 0x8022, 0x8028


def mtype(method, cls):
    return (method & 0x000F) | ((method & 0x0070) << 1) | ((method & 0x0F80) << 2) \
        | ((cls & 2) << 7) | ((cls & 1) << 4)


def parse_type(t):
    m = (t & 0x000F) | ((t & 0x00E0) >> 1) | ((t & 0x3E00) >> 2)
    c = ((t >> 7) & 2) | ((t >> 4) & 1)
    return m, c


def pad(b):
    return b + b"\0" * ((4 - len(b) % 4) % 4)


def xor_addr(sa):
    ip, port = sa
    return struct.pack(">BBHI", 0, 1, port ^ (MAGIC >> 16),
                       struct.unpack(">I", socket.inet_aton(ip))[0] ^ MAGIC)


def parse_xor(v):
    port = struct.unpack(">H", v[2:4])[0] ^ (MAGIC >> 16)
    ip = struct.unpack(">I", v[4:8])[0] ^ MAGIC
    return socket.inet_ntoa(struct.pack(">I", ip)), port


def build(method, cls, tid, attrs, key=None):
    body = b"".join(struct.pack(">HH", t, len(v)) + pad(v) for t, v in attrs)
    head_tail = struct.pack(">I", MAGIC) + tid
    if key is None:
        head = struct.pack(">HH", mtype(method, cls), len(body))
        return head + head_tail + body
    mi_len = len(body) + 24
    head = struct.pack(">HH", mtype(method, cls), mi_len)
    mac = hmac.new(key, head + head_tail + body, hashlib.sha1).digest()
    return head + head_tail + body + struct.pack(">HH", A_MI, 20) + mac


def parse(data):
    t, ln = struct.unpack(">HH", data[:4])
    assert struct.unpack(">I", data[4:8])[0] == MAGIC, "magic 不对"
    tid = data[8:20]
    attrs, off = [], 20
    while off + 4 <= 20 + ln:
        at, al = struct.unpack(">HH", data[off:off + 4])
        attrs.append((at, data[off + 4:off + 4 + al], off))
        off += 4 + ((al + 3) & ~3)
    return {"type": t, "method": parse_type(t)[0], "class": parse_type(t)[1],
            "tid": tid, "attrs": attrs, "raw": data}


def get(msg, ty):
    for t, v, _ in msg["attrs"]:
        if t == ty:
            return v
    return None


def check_mi(msg, key):
    """校验响应里的 MESSAGE-INTEGRITY（覆盖到 MI 属性之前，长度字段指向 MI 末尾）"""
    for t, v, off in msg["attrs"]:
        if t == A_MI:
            buf = bytearray(msg["raw"][:off])
            struct.pack_into(">H", buf, 2, off + 24 - 20)
            want = hmac.new(key, bytes(buf), hashlib.sha1).digest()
            return want == v
    return None


def _refresh_zero(cli, srv, user, realm, nonce, key):
    tid = os.urandom(12)
    cli.sendto(build(REFRESH, REQ, tid, [
        (A_LIFETIME, struct.pack(">I", 0)),
        (A_USERNAME, user.encode()), (A_REALM, realm.encode()), (A_NONCE, nonce.encode()),
    ], key), srv)
    r = wait_resp(cli, tid)
    assert r["class"] == OK, f"Refresh(0) 失败：{why(r)}"
    step(9, "释放 OK")


PENDING = []  # 收到的异步指示（Data Indication），等真正需要数据时再取


def wait_resp(sock, tid):
    """按事务 ID 取响应：TURN 的指示随时会插进来，不能假设"下一包就是响应"。"""
    while True:
        m = parse(sock.recvfrom(2048)[0])
        if m["tid"] == tid:
            return m
        PENDING.append(m)


def next_data(sock):
    """取一包数据：优先用已经收到的指示，否则现读一包"""
    while PENDING:
        m = PENDING.pop(0)
        if m["class"] == IND:
            return m, get(m, A_DATA)
    return None, sock.recvfrom(2048)[0]


def why(msg):
    """把响应压成一句人话，失败时能直接看出错误码"""
    if msg["class"] != ERR:
        return f"class={msg['class']}"
    e = get(msg, A_ERROR) or b"\0\0\0\0"
    return f"{e[2]*100+e[3]} {e[4:].decode(errors='replace')}"


def step(n, text):
    print(f"[{n}] {text}")


def main():
    if len(sys.argv) < 5:
        print(__doc__)
        return 2
    host, port, user, password = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
    realm_expected = sys.argv[5] if len(sys.argv) > 5 else None

    cli = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    cli.settimeout(5)
    srv = (host, port)

    # 1) Binding
    tid = os.urandom(12)
    cli.sendto(build(BINDING, REQ, tid, []), srv)
    r = wait_resp(cli, tid)
    assert r["class"] == OK, f"Binding 失败 class={r['class']}"
    mapped = parse_xor(get(r, A_XOR_MAPPED))
    step(1, f"Binding OK，服务端看到我们 {mapped[0]}:{mapped[1]}")

    # 2) Allocate → 401
    tid = os.urandom(12)
    cli.sendto(build(ALLOCATE, REQ, tid, [
        (A_REQ_TRANSPORT, struct.pack(">BBH", 17, 0, 0)),
        (A_LIFETIME, struct.pack(">I", 600)),
    ]), srv)
    r = wait_resp(cli, tid)
    err = get(r, A_ERROR)
    code = err[2] * 100 + err[3] if err else 0
    assert r["class"] == ERR and code == 401, f"期望 401，实际 {code}"
    realm = get(r, A_REALM).decode()
    nonce = get(r, A_NONCE).decode()
    if realm_expected:
        assert realm == realm_expected, f"realm 不一致: {realm}"
    step(2, f"401 OK，realm={realm} nonce={nonce[:8]}…")

    key = hashlib.md5(f"{user}:{realm}:{password}".encode()).digest()

    # 3) Allocate（带凭证）
    tid = os.urandom(12)
    req = build(ALLOCATE, REQ, tid, [
        (A_REQ_TRANSPORT, struct.pack(">BBH", 17, 0, 0)),
        (A_LIFETIME, struct.pack(">I", 600)),
        (A_USERNAME, user.encode()),
        (A_REALM, realm.encode()),
        (A_NONCE, nonce.encode()),
    ], key)
    cli.sendto(req, srv)
    r = wait_resp(cli, tid)
    if r["class"] == ERR:
        code = get(r, A_ERROR)[2] * 100 + get(r, A_ERROR)[3]
        raise AssertionError(f"Allocate 失败，错误 {code}")
    relay = parse_xor(get(r, A_XOR_RELAY))
    mi_ok = check_mi(r, key)
    step(3, f"Allocate OK，relay={relay[0]}:{relay[1]}，响应 MI 校验={mi_ok}")

    # 4) 对端 socket（模拟 WebRTC 的另一端）
    peer = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    peer.settimeout(5)
    if os.environ.get("PEER_PUBLIC"):
        # 跨公网测试：对端在本机 NAT 后面，得先问服务端"我长这样"，
        # 再朝 relay 地址打一个包凿洞（否则服务端回来的包会被 NAT 丢掉）。
        peer.bind(("0.0.0.0", 0))
        tid = os.urandom(12)
        peer.sendto(build(BINDING, REQ, tid, []), srv)
        peer_addr = parse_xor(get(wait_resp(peer, tid), A_XOR_MAPPED))
        peer.sendto(b"hole-punch", relay)
        step(4, f"对端 socket 公网地址 {peer_addr[0]}:{peer_addr[1]}（已向 relay 凿洞）")
    else:
        peer.bind(("127.0.0.1", 0))
        peer_addr = peer.getsockname()
        step(4, f"对端 socket 就绪 {peer_addr[0]}:{peer_addr[1]}")

    # 5) CreatePermission
    tid = os.urandom(12)
    cli.sendto(build(CREATE_PERMISSION, REQ, tid, [
        (A_XOR_PEER, xor_addr(peer_addr)),
        (A_USERNAME, user.encode()), (A_REALM, realm.encode()), (A_NONCE, nonce.encode()),
    ], key), srv)
    r = wait_resp(cli, tid)
    assert r["class"] == OK, f"CreatePermission 失败：{why(r)}"
    step(5, "CreatePermission OK")

    # 6) ChannelBind
    chan = 0x4001
    tid = os.urandom(12)
    cli.sendto(build(CHANNEL_BIND, REQ, tid, [
        (A_CHANNEL, struct.pack(">HH", chan, 0)),
        (A_XOR_PEER, xor_addr(peer_addr)),
        (A_USERNAME, user.encode()), (A_REALM, realm.encode()), (A_NONCE, nonce.encode()),
    ], key), srv)
    r = wait_resp(cli, tid)
    assert r["class"] == OK, f"ChannelBind 失败：{why(r)}"
    step(6, f"ChannelBind OK（通道 {chan:#x}）")

    if os.environ.get("SKIP_DATA"):
        step(7, "跳过数据面（对端不在服务器本地，仅验协议层）")
        step(8, "跳过")
        _refresh_zero(cli, srv, user, realm, nonce, key)
        print("ALL PASS (protocol only)")
        return 0

    # 7) 去程：ChannelData → 对端
    payload = b"hello-through-turn"
    cli.sendto(struct.pack(">HH", chan, len(payload)) + payload, srv)
    got, src = peer.recvfrom(2048)
    assert got == payload, f"对端收到的内容不对: {got!r}"
    step(7, f"去程 OK：对端从 {src[0]}:{src[1]} 收到 {got!r}")

    # 8) 回程：对端回给 relay 地址 → 客户端
    peer.sendto(b"echo-" + payload, src)
    cli.settimeout(5)
    # 凿洞包可能先被授权、以 Data Indication 回来，所以这里只认含 echo 的那包
    for _ in range(5):
        ind, raw = next_data(cli)
        if ind is not None:
            body, how = ind, "Data Indication"
        elif raw[0] & 0xC0 == 0x40:
            n, ln = struct.unpack(">HH", raw[:4])
            body, how = raw[4:4 + ln], f"ChannelData 通道 {n:#x}"
        else:
            body, how = get(parse(raw), A_DATA), "Data Indication"
        if body and payload in body:
            step(8, f"回程 OK（{how}）: {body!r}")
            break
    else:
        raise AssertionError("回程没收到 echo（只收到其它包）")

    # 9) Refresh(0) 释放
    _refresh_zero(cli, srv, user, realm, nonce, key)
    print("ALL PASS")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as e:
        print("FAIL:", e)
        sys.exit(1)
