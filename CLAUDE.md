# CLAUDE.md

给在本仓库里干活的 AI 编码助手（以及人类贡献者）的操作手册。
**读完这一页就能开工**，剩下的细节在 `docs/` 和代码注释里。

> 本文件与 `AGENTS.md` 必须与代码保持同步。改了行为、改了命令、加了约定，
> 就在同一个提交里更新这里——文档落后于代码，下一个接手的人（人或 AI）就会照着错的说明干活。

---

## 1. 这是什么

**Veilwire**：去中心化匿名即时通讯。端到端加密，消息不经过任何中心服务器存储用户数据，
只在中继（relay）上短暂排队；支持文字、文件、语音、图片、视频、阅后即焚、撤回、语音/视频通话。

三个组成部分：

| 部分 | 语言 | 位置 | 作用 |
|---|---|---|---|
| 核心协议与业务层 | Rust | `rust/crates/` | 加密、身份、消息编解码、中继客户端、通话状态机、DHT |
| Android 客户端 | Kotlin + Compose | `android/app/` | UI、通知、WebRTC、相册/相机、前台服务 |
| 中继 | Rust（同一个 workspace） | `rust/crates/relay/` | 无标识队列 + 内嵌 TURN + DHT 自公布 |

**当前版本**：`0.15.9` / `versionCode 36`（见 `android/app/app/build.gradle.kts`）
**协议版本**：`proto::PROTOCOL_VERSION`，改帧结构必须同时考虑旧客户端兼容性。

---

## 2. 构建与测试

### 2.1 必须知道的三个环境坑

1. **Rust 只能在纯 ASCII 路径下编译**。仓库路径含中文会让链接器失败，
   所以 `scripts/build-android.sh` 会把 `rust/` 复制到 `/c/anonchat-build/src` 再编。
   手工编译时同理。
2. **`~/.cargo/bin/cargo.exe` 可能是空文件**（被清理工具截断过）。
   用工具链内的绝对路径：
   `~/.rustup/toolchains/stable-x86_64-pc-windows-msvc/bin/cargo.exe`
3. **Gradle 必须用 JDK 17**，环境变量里的全局 `JAVA_HOME` 可能是 11。
   脚本里已经硬编码 `C:/Program Files/Java/jdk-17`。

### 2.2 常用命令

```bash
# Android 单测（会先编 Rust .so，必须走脚本，不能直接 gradle test）
bash scripts/test-android.sh

# 打 release 包（不配密钥会回退 debug 签名，不能对外分发）
bash scripts/build-android.sh release

# 只构建 Rust 工作区
cd /c/anonchat-build/src && \
  CARGO_TARGET_DIR=/c/anonchat-build/target \
  cargo test --workspace --exclude android-jni

# 中继集成测试（会起真实 relay 进程，较慢，约 1~3 分钟）
cargo test -p relay --tests -- --test-threads=1
```

**说明**：`cargo test --workspace` 约 11~14 分钟（含多个起真实中继的集成测试）。
只改 Kotlin 时跑 `scripts/test-android.sh` 就够。

### 2.3 签名

发布包读环境变量，缺省回退 debug 签名：

```
VEILWIRE_KEYSTORE / VEILWIRE_KEYSTORE_PASSWORD / VEILWIRE_KEY_ALIAS / VEILWIRE_KEY_PASSWORD
```

密钥在 `keys/`（已 gitignore）。**不要**把密钥、密码、真实服务器地址写进仓库或提交信息。

---

## 3. 代码地图

### 3.1 Rust

```
rust/crates/
├── proto/        帧结构（ClientFrame/ServerFrame）、错误码、ID 生成。改协议从这里开始
├── identity/     身份（Ed25519）、邀请码编解码（含中继地址与一次性密钥）
├── secure/       加密层（Olm 双棘轮）、Wire 信封、失败分类 FailKind
├── dht/          Mainline DHT（BEP5）轻客户端：bencode、KRPC、注入防御、crawl/lookup/announce
├── relay/        中继二进制：WS 队列存储 + TURN 子集 + DHT 自公布
│   └── src/turn/ TURN 服务端（Binding/Allocate/Refresh/CreatePermission/ChannelBind/Data）
├── client/       业务层，CLI 与 Android 共用
│   ├── lib.rs    对外 API（init/add/send/recv_once/set_relay/discover_relays/turn_creds…）
│   └── call.rs   通话状态机（代际防护、glare 破局）
├── punch/        UDP 打洞 / NAT 类型探测
├── cli/          命令行客户端
└── android-jni/  JNI 边界（把 client 的同步 API 暴露给 Kotlin）
```

### 3.2 Android

```
android/app/app/src/main/java/io/anonchat/
├── core/Native.kt        JNI 声明（与 android-jni 的符号一一对应）
├── core/AppError.kt      结构化错误码 ErrKind
├── app/Repo.kt           业务门面：消息存储、联系人、PeerCard、错误信封
├── app/MsgStore.kt       追加日志式消息存储（含键迁移 mv 记录）
├── app/MainActivity.kt   几乎所有界面（Compose），含聊天页/设置/中继选择
├── app/CallManager.kt    通话：PeerConnection、ICE、TURN 凭据、状态机
├── app/CallScreen.kt     通话界面
├── app/MessagingService.kt 前台服务：收消息循环、通知、告警泵
├── app/UiErrors.kt       ErrKind → 本地化文案
└── app/RelayList.kt      中继列表加载 + 测速
```

---

## 4. 不能踩的线

这些每一条都对应一个真实事故，改动前先读注释。

### 4.1 存储语义 ≠ 显示语义

- **会话键必须是 `dh`**（对方身份标识）。不是昵称、不是备注名。
  一旦用名字当键，改名/加好友就会把历史劈成两段。
- `Repo.STRANGER_PREFIX = "陌生人 "` 是**存储前缀**，`peer_cards.json` 靠它反查。
  界面上要显示本地化文字时走 `displayNameOf()`，不要改这个常量。
- 语言只影响 UI。落盘路径、会话键、协议字段一律语言中立。

### 4.2 路由三原则

1. **联系人的 `relays` 表示"他的队列在哪"**，与我在哪台中继无关。
   任何"批量改写联系人中继"的操作都会让所有对外发送打错地方，而且静默失败。
2. **发送只用 `relays.first()` 那台成功即可，失败要接着试下一个**（见 `put_first`）。
   但判断依据是 `put` 成功，不是"连上了"——活着的中继也会回 `no_queue`。
3. **接收**只能连"我自己的队列所在的那台"（`me.relays[0]`）。
   中继的 `Sub` 对不存在的队列**静默成功**，换一台收就会变成"看似在线、永远收不到"。
   要迁移只能走 `set_relay`。

### 4.3 每条消息都必须能回信

`payload.reply_qid` 为空 = 对方无法回复、也无法加好友。
所以 `send_kind` 在 `me.qid` 为空时会先补建队列（候选：已知中继 → 本次投递目标）。
**不要**把这段兜底去掉，也不要让它静默失败。

### 4.4 锁

- `dev_lock(dir)` **不可重入**。已经在锁内的函数不要调用会再次加锁的公开 API
  （例如 `send_kind` 里不能调 `ensure_queue`，要用 `ensure_own_queue`）。
- 互斥量不要跨 `await` 持有。

### 4.5 通话代际（generation）

重协商必须带代际号：`on_answer` 严格匹配 `awaiting`，`on_ice` 丢弃 `gen < self.gen`，
重放/旧 offer 忽略，glare 时"代际高者赢、同代际主叫赢"，**低于本端代际的
restart offer 一律拒**（`peer_gen < gen < self.gen` 的中间值曾从守卫缝隙漏进去
被采纳，双方 SDP 交叉错位且无法自愈——审计 P2-3）。
**别给 `gen == 0` 开"兼容旧版"的后门**——那正好放过要丢的过期信令。

### 4.6 信令的 API 边界：只透出"被接受"的 SDP

`call_inbound` 组装 CallEvent 时，**offer 的 remote_sdp/video 只能在状态机
判定"接受"后带出**（用 `on_offer_ex` 的 accepted 标志，仿照 ICE 分支）。
无条件透出的话：忙线拒接的第三方 offer 会污染正在进行的通话、被代际守卫
丢弃的迟到 SDP 会复活、重投的 offer 会反复触如来电准备路径（审计 P1-2）。

### 4.7 密文离开本机之前，棘轮必须已落盘

`send_batch`/`send_kind` 都是"先加密推进棘轮 → 再走网络"。任何
"投递成功才 save"的写法，在中途失败时会把棘轮回滚到已投出的密钥之前——
对端把后续消息判 Duplicate 静默吞掉，或 MAC 失败无限重投（审计 P1-3）。

### 4.8 DHT 的三个必做防御 + 解析器的两条铁律

来源 IP 校验 + 事务 ID 校验 + 重复记录检测，缺一不可。
另外：bootstrap 域名普遍被污染（会解析到无关地址），所以
`STATIC_SEEDS` 必须无条件参与，且**不要**拿 bootstrap 种子直接 `get_peers`——
那些节点离服务键太远，手里没有记录，必须先 `find_node` 爬到附近。
解析器两条铁律：bencode 字符串长度来自网络，**加法之前比长度**
（usize 回绕曾绕过越界检查导致远程 panic，审计 P1-1）；递归深度上限
`MAX_DEPTH=64`，放大接收缓冲前先确认这道闸还在。

### 4.9 网络读写的超时与原子落盘

- 所有网络写必须有超时（`Conn::send` 10s）：零窗口/半开路径上写会无限
  pending，而调用方持设备锁——不设超时就是静默挂死整个客户端（审计 P2-6）。
- `save_self`/`save_contacts`/`save_turn` 一律 tmp+rename 原子写：这两个
  文件有不持锁的读者，直接覆盖写到一半被读到就是半个 JSON（审计 P2-5）。
- `Conn::sub` 可能带回订阅瞬间推来的积压第一条（返回 `Option`）——调用方
  必须处理，不能吞（审计 P2-1）；解不开的坏帧按毒帧 ack + 连接回缓存
  （审计 P2-2）。

### 4.10 错误与文案

- Rust 返回给 UI 的错误是**结构化错误码**（`AppError(ErrKind, detail)`），
  技术细节走 `detail`/`error` 通道（语言中性），用户可见文案由 `UiErrors.kt` 映射。
- 新增一条用户可见文案 = 同时改 `values/strings.xml`、`values-zh/strings.xml`、`ErrKind`、
  `UiErrors.kt` 四处。英文是默认资源，中文是 `values-zh`。
- 不要在资源字符串里用 Markdown（`**bold**` 会原样显示出来）。
- Kotlin 侧取 Rust 的数字要 `(d as? Number)?.toLong()`；`as? Long` 会因为
  org.json 把小数字解析成 `Integer` 而永远得到 null。

---

## 5. 常见任务怎么做

| 任务 | 步骤 |
|---|---|
| 加一条用户可见文案 | 改两份 `strings.xml` → 若是错误则加 `ErrKind` + `UiErrors` → 跑 Android 单测 |
| 改阅后即焚 UI | 倒计时角标统一走 `BurnBadge`（`MainActivity.kt`，五种气泡共用）；到期清理的 UI 刷新**不要依赖 `sweepExpired` 返回值**（服务层全局清扫会抢先删掉，返回 0），用存储快照对比触发 `localBump++` |
| 加一个协议帧 | 改 `proto` 的 `ClientFrame`/`ServerFrame` → 中继处理 → client 调用 → **考虑旧客户端能不能解析**（serde 遇到未知 tag 直接失败） |
| 改发送/路由 | 先读 4.2，改完跑 `relay_failover` / `cross_relay_routing` / `reply_clue` |
| 改通话 | 改 `call.rs` 状态机要同步加单测；UI 侧看 `CallManager` 的 `ui` 状态 |
| 改中继 | 注意数据目录兼容性（`queues.log` 回放），以及 TURN 的 SSRF 默认拒绝回环 |
| 部署中继 | `docs/中继部署指南.md`；验证用 `scripts/relay_check.py <ip>:8080 --dht` |

---

## 6. 文档地图

| 文件 | 内容 |
|---|---|
| `README.md` / `README.zh-CN.md` | 对外介绍、下载、为什么这样设计 |
| `CLAUDE.md`（本文件） | 给 AI/贡献者的操作手册 |
| `docs/协议规范-v0.md` | 帧格式与语义 |
| `docs/通话信令-v0.md` | 通话信令与代际 |
| `docs/中继部署指南.md` | 部署、TURN、DHT 公布、自检 |
| `docs/DHT中继发现-可行性评估.md` | 为什么用 BEP5 而不是 BEP44、实测数据、实现状态 |
| `docs/安装与验收.md` | 装机验收步骤 |
| `docs/Jami-对比与借鉴.md` | 设计取舍的外部参照 |

---

## 7. 提交前检查

1. 改的是 Rust → `cargo test --workspace --exclude android-jni`（或至少相关 crate）
2. 改的是 Kotlin → `bash scripts/test-android.sh`
3. 改了界面文案/协议/命令 → **同步更新本文档或对应 `docs/`**
4. 确认没有把密钥、服务器地址、个人账号写进代码/文档/提交信息
5. 提交信息写清楚"为什么"，不只是"改了什么"
