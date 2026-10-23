# 把 TURN 内嵌进中继程序（设计）

> 目标：`relay serve` 一个进程同时提供 **WS 中继** 和 **TURN/STUN**，部署时只跑一个容器，
> 不再单独维护 coturn。对外仍是一个标准的 `turn://user:pass@host:3478` 地址，App 侧零改动。

## 1. 为什么现在做

- 现在服务器上有两个进程：`anonchat-relay`（Rust） + `coturn`（systemd 直装）。
  coturn 是外部依赖，配置踩过两个坑：镜像自带 conf 与自定义 conf 冲突（`external-ip` 重复定义）、
  该机器对"绑定具体内网 IP 的 UDP socket"不交付公网流量（必须绑 `0.0.0.0`）。
  自己实现后这些知识沉淀在代码里，部署只需一条命令。
- 与产品定位一致：中继由社区/用户自建，**部署一次就自带 TURN**，自建门槛显著降低。

## 2. 实现范围（RFC 子集，够用即止）

### 做

| 能力 | RFC | 说明 |
|---|---|---|
| STUN Binding | 5389 | 返回 `XOR-MAPPED-ADDRESS`；`FINGERPRINT` 可选校验 |
| 长凭证认证 | 5389 §10 | `MESSAGE-INTEGRITY`（HMAC-SHA1）+ 401 + nonce/realm |
| Allocate / Refresh | 5766 §6 | UDP 分配，生命周期与刷新 |
| CreatePermission | 5766 §9 | 按对端 IP 放行 |
| ChannelBind + ChannelData | 5766 §11 | 通道绑定后的高效数据面 |
| Send / Data Indication | 5766 §10 | 未走通道时的数据面 |
| 端口段管理 | — | 可配置 relay 端口段，分配/回收 |

### 不做（明确划界）

- TURN over TCP / TLS / DTLS（移动网络 UDP 被封时的兜底，见 §8 决策 2）
- IPv6、`ALTERNATE-SERVER`、多 realm
- 与 WS 中继共享端口（TURN 是 UDP，中继是 TCP，物理上无法共端口）

## 3. 依赖与代码组织

新增依赖（**本地 cargo 缓存已有，可离线构建**，已核对 `.crate` 存在）：

```
sha1 = "0.10"      # MESSAGE-INTEGRITY
hmac = "0.12"      # HMAC-SHA1
crc32fast = "1.5"  # FINGERPRINT
```

不引入 `webrtc-rs/turn` 等现成 crate：本地无缓存、依赖树重，且我们只需要上面这个子集。
tokio 现有 features（`net` 含 UDP）足够，不需要改。

```
crates/relay/src/
├── main.rs          # 现有：CLI 解析、WS 服务
├── turn/
│   ├── mod.rs       # 启动入口：spawn TURN 监听
│   ├── codec.rs     # STUN 消息编解码（属性 TLV、padding、FINGERPRINT）
│   ├── auth.rs      # nonce 生成/校验、MESSAGE-INTEGRITY、错误响应
│   ├── alloc.rs     # 分配表：五元组 → relay socket、权限、通道、TTL
│   └── server.rs    # 事件循环：收包分发、数据面转发、超时清理
```

## 4. 配置（全可选，不传则行为与现在完全一致）

```
relay serve --addr 0.0.0.0:8080 --data /data \
  --turn 0.0.0.0:3478 \                  # 开关：不传就不启 TURN
  --turn-external-ip 203.0.113.10 \    # NAT 后的公网 IP，relayed address 用它通告
  --turn-user turnuser --turn-pass xxx \
  --turn-realm turnuser \
  --turn-port-min 49160 --turn-port-max 49200
```

- **监听地址默认 `0.0.0.0`**：这台 VPC 上绑具体内网 IP 的 UDP socket 收不到公网包（已实测），
  代码里不要提供"绑单网卡"的默认值，避免后人再踩。
- `--turn-external-ip` 必填（NAT 环境）：漏了会出现"分配成功但媒体不通"。
- 凭据缺失时启动直接报错，不做"匿名 TURN"（开放 TURN = 免费代理，会被滥用）。

## 5. 安全与配额（必须有，否则等于开放代理）

- 每 IP 同时分配数上限（默认 8）、每分配生命周期上限（默认 600s，可 Refresh 续期）
- relay 端口按需分配、超时回收；端口段耗尽时返回 486 Allocation Quota Reached
- 数据面只转发"已授权对端"（permission/通道）的包，其它丢弃
- 日志只记分配/回收/错误，不记数据内容；错误码与 coturn 对齐便于排查

## 6. 里程碑与验收

| 阶段 | 内容 | 验收 |
|---|---|---|
| M1 | codec + Binding | 单测：编解码往返、FINGERPRINT、XOR-MAPPED-ADDRESS；用 `turnutils_stunclient` 打通 |
| M2 | 认证 | 401/nonce/438 行为与 coturn 对照；错误凭据被拒 |
| M3 | 分配与数据面 | `turnutils_uclient` 完整跑通 **0 丢包**（与现网 coturn 同标准） |
| M4 | 配额与清理 | 打满端口段返回 486；超时后端口回收、无泄漏 |
| M5 | 打包部署 | Dockerfile `EXPOSE 3478/udp` + 端口段；一条 `docker run` 起全套 |
| M6 | App 联调 | App「测试连通性」拿到 ≥1 relay 候选；真机跨网语音/视频通话 |
| M7 | 退役 coturn | 观察 24h 无异常后停掉 coturn 服务 |

**回退**：不传 `--turn` 即回到纯 WS 中继；coturn 保留到 M7 之前，随时可切回。

## 7. 风险

1. **RFC 细节易错**：nonce 生命周期、permission 与 channel 的关系、错误码。
   对策：每步都和 coturn 抓包对照，`turnutils_*` 全过再进 App 联调。
2. **NAT/external-ip**：relayed address 必须通告公网 IP，且监听绑 `0.0.0.0`（本项目已踩过两次）。
3. **端口段与安全组**：需与云安全组放行范围一致（当前 49160–49200 已放行）。
4. **滥用**：默认静态凭据 + 每 IP 配额；后续可做"凭据绑定 qid"的动态下发（需要协议扩展，不在本期）。

## 8. 待你拍板

1. **凭据模式**：静态 `user/pass`（与现在 coturn 一致，最简单）／动态下发（每设备不同，需协议扩展）？
   → 建议本期静态，动态留待需要撤销单个客户端时再做。
2. **要不要 TURN over TCP/TLS**：UDP 被运营商封锁的网络下必需，但工作量和协议复杂度都会上一个台阶。
   → 建议本期只做 UDP，真遇到打不通再补。
3. **端口段大小**：现在 60 个（49160–49200，够 60 路并发）。扩到 200 需同时在安全组放行。
   → 建议维持 60，先用观察实际并发。
4. **联调顺序**：先在本机跑新版中继 + 模拟器验证（不动线上），还是直接上云？
   → 建议先本机，线上保留 coturn 直到 M6 通过。

---

## 9. 实现与验证（已完成，2026-10-23）

### 代码

```
crates/relay/src/turn/
├── mod.rs     # 模块入口
├── codec.rs   # STUN 编解码 + MESSAGE-INTEGRITY + FINGERPRINT（含 6 个单测）
├── auth.rs    # 长凭证：MD5(user:realm:pass) + nonce 轮换（含 5 个单测）
├── alloc.rs   # 端口池 / 分配 / 权限 / 通道 / 到期清理（含 5 个单测）
└── server.rs  # 事件循环 + 协议处理 + 数据面
```

依赖只加了 `md-5 / sha1 / hmac / crc32fast`（Cargo.lock 已更新）。

### 启动方式

```bash
# 只做 WS 中继（与旧版完全一致）
relay serve --addr 0.0.0.0:8080 --data /data

# 自带 TURN
relay serve --addr 0.0.0.0:8080 --data /data \
  --turn 0.0.0.0:3478 --turn-external-ip 203.0.113.10 \
  --turn-user turnuser --turn-pass <密码> \
  --turn-port-min 49160 --turn-port-max 49200 \
  --turn-max-per-ip 8
```

可选 `--turn-allow-private-peers`：允许把数据中转给回环/内网地址（**仅本机联调**）。
默认拒绝，这是 SSRF 防护——TURN 是经典的打内网跳板，coturn 默认也拒绝。

### 实测结果（本机，`turn_probe.py`）

| 项 | 结果 |
|---|---|
| Binding | ✅ 返回 XOR-MAPPED-ADDRESS |
| 401 → 带凭证重发 | ✅ realm/nonce 正确，HMAC-SHA1 被接受 |
| Allocate | ✅ 分配 relay 端口，响应 MI 可校验 |
| CreatePermission / ChannelBind | ✅ |
| 数据面去程（ChannelData → 对端） | ✅ 对端从 relay 地址收到原文 |
| 数据面回程（对端 → relay → 客户端） | ✅ 收到 ChannelData 回包 |
| Refresh(0) 释放 | ✅ 端口回收后可复用 |
| 同源重复 Allocate | ✅ 返回 437 Allocation Mismatch |
| 同 IP 超配额 | ✅ 返回 486 Allocation Quota Reached（`--turn-max-per-ip 1` 实测） |
| SSRF 防护 | ✅ 默认拒绝 127.0.0.1 对端的 permission |
| 与 WS 中继共存 | ✅ 同一进程 `cli demo` 走 28080 正常收发 |

**关键校准**：MESSAGE-INTEGRITY / FINGERPRINT 的覆盖范围以 coturn 真实响应为准——
两者都只覆盖到**属性之前**（不含属性头），只把头部长度字段改成"含该属性"。含头算出的值永远校验不过。

**基准对照**：同一份探针脚本对线上 coturn 跑通 Binding→401→Allocate（含响应 MI 校验），
证明客户端实现与协议理解一致；数据面因对端在异机，改在本机验证。

### 尚未做（按约定的取舍）

- TURN over TCP/TLS、IPv6、ALTERNATE-SERVER
- 静态凭据（未做按设备动态下发）

---

## 10. 线上切换（方案 A：直接替换，2026-10-23 已完成）

coturn 已退役，`203.0.113.10` 上的中继现在**一个进程同时提供 WS 中继与 TURN**。

### 现网形态

```bash
docker run -d --name anonchat-relay --network host --restart unless-stopped \
  -v /var/lib/relay:/data \
  --entrypoint /usr/local/bin/relay \
  anonchat-relay:turn-0.13 serve --addr 0.0.0.0:8080 --data /data \
  --turn 0.0.0.0:3478 --turn-external-ip 203.0.113.10 \
  --turn-user turnuser --turn-pass <密码> --turn-realm turnuser \
  --turn-port-min 49160 --turn-port-max 49200 --turn-max-per-ip 8
```

- **必须 `--network host`**：TURN 要绑 41 个 UDP 端口，且 relayed address 要通告公网 IP，
  bridge 模式下 `-p` 映射既繁琐又会让源端口对不上。host 模式下容器进程直接绑宿主机网络栈。
- `--restart unless-stopped` + `systemctl disable coturn`（coturn 已禁止开机自启，
  否则重启后两者会抢 3478）。
- App 侧零改动：仍是 `turn://turnuser:<密码>@203.0.113.10:3478`。

### 切换前做过的预检（重要）

用 `--network host` 在 **49160**（安全组已放行段里的端口）起了个临时容器，跑完整数据面探针通过，
才动线上 3478。**结论：docker host 模式在这台机器上公网 UDP 是通的。**

> 修正一条旧结论：此前记录的“docker host/bridge 网络模式在该机都不交付包”是误判，
> 真正的原因是当时 coturn 绑的是具体内网地址而不是 `0.0.0.0`。我们强制绑 `0.0.0.0` 后一切正常。

### 验收结果（从公网 198.51.100.20 打 203.0.113.10:3478）

| 项 | 结果 |
|---|---|
| Binding / 401 / Allocate / MI 校验 | ✅ |
| CreatePermission / ChannelBind | ✅ |
| 去程：ChannelData → 对端 | ✅ 对端从 `203.0.113.10:49160` 收到原文 |
| 回程：对端 → relay → 客户端 | ✅ ChannelData 通道 `0x4001` 收到 echo |
| Refresh(0) 释放 | ✅ |
| 同 IP 连开 10 路 | ✅ 前 8 个分配成功、第 9/10 个返回 486，端口互不重复 |
| WS 中继 | ✅ `cli demo --relay ws://203.0.113.10:8080` Alice↔Bob 正常解密 |
| 数据卷恢复 | ✅ 重启后恢复 9 个队列 / 22 条未确认消息，零丢失 |

**基准对照**：切换前先用同一份探针对 coturn 跑通了完整的公网数据面，
所以“探针全绿”这件事有可比对象，不是自说自话。

### 回滚（三条命令）

```bash
docker rm -f anonchat-relay
docker rename anonchat-relay-old anonchat-relay && docker start anonchat-relay   # 旧版中继（纯 WS）
sudo systemctl enable --now coturn                                               # coturn 回到 3478
```

旧容器 `anonchat-relay-old`、旧镜像 `anonchat-relay:pre-turn-20261023`、
coturn 配置备份 `/root/turnserver.conf.bak-20261023` 都保留着，确认稳定后可清理。

### 已知限制

- 端口段 41 个（49160-49200），每 IP 最多 8 路分配 → 同时最多约 5 台设备通话，够用；不够再扩段并同步改安全组。
- 分配 TTL 600s，客户端异常掉线后最多占 10 分钟端口。
- 不做 TCP/TLS 兜底：若某运营商封 UDP，该网络下通话会失败（届时再补 443/TCP）。
