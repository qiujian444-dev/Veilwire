//! 通话信令状态机
//!
//! **纯逻辑，不碰 WebRTC、不碰网络**。这样才可能把"乱序""重投""旧信令串台"
//! 这些真实会遇到的问题穷举测掉。
//!
//! 信令复用加密消息队列，所以有两个必须自己兜住的特性：
//! - **可能重复**：中继在没收到 ACK 时会重投，同一条 offer 可能到两次
//! - **可能乱序**：队列是 FIFO，但客户端重连、离线补投会让顺序不可靠
//!
//! 因此每条信令都必须用 `call_id` 做校验。校验不严会出现什么：
//! - 上一通电话的 `end` 挂在半路，重投时到了，把**当前**这通电话挂掉
//! - 对方挂断后立刻再打，新的 offer 和旧的 end 交错，状态来回跳
//!
//! 这两种情况在真实弱网下都会发生，不是理论问题。

use serde::{Deserialize, Serialize};

/// 振铃超时：超过这个时间没有应答就自动结束，避免"一直响"
pub const RING_TIMEOUT_SECS: u64 = 45;
/// 重协商超时：answer 迟迟不到就解除 renegotiating 标志（通话保持 Active）。
/// 比 RING_TIMEOUT 短：卡在重协商的用户等 20 秒就应能再次手动重连。
pub const RENEGOTIATE_TIMEOUT_SECS: u64 = 20;

/// 单条 SDP 的长度上限。正常 offer 约 2~4KB，8KB 已经很宽松了。
pub const MAX_SDP_LEN: usize = 8 * 1024;

/// 单个 ICE candidate 的长度上限
pub const MAX_CANDIDATE_LEN: usize = 1024;

// ---------------------------------------------------------------- 信令体

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OfferBody {
    pub call_id: String,
    pub sdp: String,
    #[serde(default)]
    pub video: bool,
    /// 是否为**通话中重新协商**（ICE restart）。
    ///
    /// 换网络（WiFi↔流量）后媒体通道会失效，需要带 `IceRestart` 重新收集候选。
    /// 重协商复用 `call.offer`，靠这个标志区分"新来电"和"换条路继续"——
    /// 不加这个标志，对端会把重协商当成新来电，正在说的话就被打断了。
    #[serde(default)]
    pub restart: bool,
    /// 重新协商的代际号。0 = 首次协商或旧版客户端；每次重协商 +1。
    ///
    /// 为什么要这个：换网络后双方会各自发起重协商，旧代际的 SDP / ICE candidate
    /// 可能迟到到达。没有代际号时它们会被当成当前代际应用，
    /// 表现是"通话接通了但两边都没声音"——媒体通道被旧候选带偏了。
    #[serde(default)]
    pub gen: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnswerBody {
    pub call_id: String,
    pub sdp: String,
    /// 应答对应的代际号，必须与收到的 offer 一致；不一致说明是旧代际的应答
    #[serde(default)]
    pub gen: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IceBody {
    pub call_id: String,
    /// candidate 字符串；空串表示候选收集结束（end-of-candidates）
    pub candidate: String,
    #[serde(default)]
    pub mid: String,
    #[serde(default)]
    pub mline: i32,
    /// 候选所属代际。低于当前代际的候选来自上一条网络路径，必须丢弃。
    #[serde(default)]
    pub gen: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EndBody {
    pub call_id: String,
    #[serde(default)]
    pub reason: String,
}

// ---------------------------------------------------------------- 状态

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// 空闲
    Idle,
    /// 我方主叫，已发 offer，等对方应答
    Outgoing,
    /// 对方主叫，等用户决定接不接
    Incoming,
    /// 通话中
    Active,
    /// 已结束（保留 call_id 用于过滤迟到的信令）
    Ended,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Outgoing => "outgoing",
            State::Incoming => "incoming",
            State::Active => "active",
            State::Ended => "ended",
        }
    }

    /// 是否占用通话通道
    pub fn is_busy(&self) -> bool {
        matches!(self, State::Outgoing | State::Incoming | State::Active)
    }
}

// ---------------------------------------------------------------- 动作

/// 状态机产出的指令，由上层（Android / CLI）执行。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "act", rename_all = "snake_case")]
pub enum Action {
    /// 需要把这条信令发给对方
    Send {
        to_dh: String,
        kind: String,
        body: String,
    },
    /// 状态变了，UI 需要刷新
    State {
        state: String,
        call_id: String,
        peer_dh: String,
        video: bool,
        reason: String,
        /// true 表示需要重新协商（收到重协商 offer，或自己刚发出）
        renegotiating: bool,
    },
}

// ---------------------------------------------------------------- 状态机

#[derive(Debug, Clone)]
pub struct Call {
    state: State,
    call_id: String,
    peer_dh: String,
    video: bool,
    /// 进入当前状态的时间（unix 秒），用于振铃超时
    since: u64,
    reason: String,
    /// 已处理过的信令 message-id，防止同一条 offer 触发两次 UI 弹窗
    seen: Vec<String>,
    /// 正在重新协商（ICE restart）。通话仍是 Active，只是媒体通道要重建。
    renegotiating: bool,
    /// 当前媒体代际。0 = 首次协商；每次重新协商由发起方 +1，另一方采纳。
    gen: u64,
    /// 我方已发出 offer、正在等待的应答代际。None = 没有在等我方应答。
    /// 用途：放弃自己的 offer（对端代际更高时让路）后，迟到的应答必须被丢弃。
    awaiting: Option<u64>,
    /// 对方已走到的最新代际，用于拒绝重放 / 回退的旧 offer
    peer_gen: u64,
    /// 本通电话我是主叫吗？双方同时重新协商时用它破僵局（主叫赢）。
    /// 用角色而不是身份比较：双方角色天然互补，不需要额外交换任何东西。
    caller: bool,
    /// 我最近一次发出的 answer（SDP + 代际）。同通话的 offer 重发到达时
    /// 用它补发——旧注释承诺过这件事，实际一直没做，主叫只能干等到超时。
    last_answer: Option<(String, u64)>,
    /// 进入 renegotiating 的时刻。answer 永不到达时（对端掉线）靠它超时退出，
    /// 否则 UI 会永远停在"重建媒体通道"。
    renegotiate_since: Option<u64>,
}

impl Default for Call {
    fn default() -> Self {
        Self::new()
    }
}

impl Call {
    pub fn new() -> Self {
        Self {
            state: State::Idle,
            call_id: String::new(),
            peer_dh: String::new(),
            video: false,
            since: 0,
            reason: String::new(),
            seen: Vec::new(),
            renegotiating: false,
            gen: 0,
            awaiting: None,
            peer_gen: 0,
            caller: false,
            last_answer: None,
            renegotiate_since: None,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }
    pub fn call_id(&self) -> &str {
        &self.call_id
    }
    pub fn peer_dh(&self) -> &str {
        &self.peer_dh
    }
    pub fn video(&self) -> bool {
        self.video
    }
    pub fn reason(&self) -> &str {
        &self.reason
    }
    pub fn renegotiating(&self) -> bool {
        self.renegotiating
    }
    /// 当前媒体代际（调试 / 测试用）
    pub fn gen(&self) -> u64 {
        self.gen
    }

    fn enter(&mut self, st: State, now: u64) {
        if st != State::Active {
            self.renegotiating = false;
        }
        self.state = st;
        self.since = now;
    }

    fn state_action(&self) -> Action {
        Action::State {
            state: self.state.as_str().to_string(),
            call_id: self.call_id.clone(),
            peer_dh: self.peer_dh.clone(),
            video: self.video,
            reason: self.reason.clone(),
            renegotiating: self.renegotiating,
        }
    }

    fn check_id(id: &str) -> bool {
        !id.is_empty() && id.len() <= 64
    }

    /// 记住已处理的信令 ID。只留最近 64 条，够覆盖重投窗口。
    fn mark_seen(&mut self, id: &str) -> bool {
        if id.is_empty() {
            return true; // 没带 ID 的就不去重了
        }
        if self.seen.iter().any(|x| x == id) {
            return false;
        }
        if self.seen.len() >= 64 {
            self.seen.remove(0);
        }
        self.seen.push(id.to_string());
        true
    }

    // ------------------------------------------------------------ 主叫

    /// 发起通话。返回 `Err` 表示当前忙。
    pub fn start_outgoing(
        &mut self,
        call_id: &str,
        peer_dh: &str,
        video: bool,
        now: u64,
    ) -> Result<Vec<Action>, String> {
        if self.state.is_busy() {
            return Err(format!("当前正在通话中（{}）", self.state.as_str()));
        }
        if !Self::check_id(call_id) {
            return Err("call_id 不合法".into());
        }
        self.call_id = call_id.to_string();
        self.peer_dh = peer_dh.to_string();
        self.video = video;
        self.reason.clear();
        self.seen.clear();
        self.renegotiating = false;
        // 新的一通电话：代际从 0 开始。
        // 这里就把 awaiting 置上：`start_outgoing` 的语义就是"我要发 offer 并等应答"，
        // 真实调用方拿到 SDP 之前也可能收到应答（极快返回），不能等到
        // make_offer_actions 才登记，否则那条应答会被当成过期丢掉。
        self.gen = 0;
        self.awaiting = Some(0);
        self.peer_gen = 0;
        self.caller = true;
        self.enter(State::Outgoing, now);
        Ok(vec![self.state_action()])
    }

    /// 本端拿到本地 SDP 后，产出要发出去的 offer
    pub fn make_offer_actions(&mut self, sdp: &str) -> Vec<Action> {
        if self.state != State::Outgoing {
            return Vec::new();
        }
        if sdp.len() > MAX_SDP_LEN {
            return Vec::new();
        }
        // 首次 offer 也要记"在等应答"，这样旧代际的应答能被识别并丢弃
        self.awaiting = Some(self.gen);
        let body = OfferBody {
            call_id: self.call_id.clone(),
            sdp: sdp.to_string(),
            video: self.video,
            restart: false,
            gen: self.gen,
        };
        vec![Action::Send {
            to_dh: self.peer_dh.clone(),
            kind: crate::call_kind::OFFER.to_string(),
            body: serde_json::to_string(&body).unwrap_or_default(),
        }]
    }

    /// 本端产出的 answer 动作（被叫接听时用）
    pub fn make_answer_actions(&self, sdp: &str) -> Vec<Action> {
        self.make_answer_actions_with_gen(sdp.to_string(), self.gen)
    }

    fn make_answer_actions_with_gen(&self, sdp: String, gen: u64) -> Vec<Action> {
        let body = AnswerBody {
            call_id: self.call_id.clone(),
            sdp,
            gen,
        };
        vec![Action::Send {
            to_dh: self.peer_dh.clone(),
            kind: crate::call_kind::ANSWER.to_string(),
            body: serde_json::to_string(&body).unwrap_or_default(),
        }]
    }

    /// 通话中换网络后发起重协商（ICE restart）。
    ///
    /// 只有 Active 才允许：还没接通就重协商没有意义，会把状态机搞乱。
    pub fn make_restart_offer_actions(&mut self, sdp: &str) -> Vec<Action> {
        // 状态与长度分开判：混在一个条件里的话，SDP 超长会被上层报成
        // "当前不在通话中"——用户做对了操作却得到答非所问的错误（P3）。
        if self.state != State::Active {
            return Vec::new();
        }
        if sdp.len() > MAX_SDP_LEN {
            return Vec::new();
        }
        self.renegotiating = true;
        // 每发起一次重协商就推进代际：旧代际的 SDP / candidate / answer 从此失效
        self.gen += 1;
        self.awaiting = Some(self.gen);
        let body = OfferBody {
            call_id: self.call_id.clone(),
            sdp: sdp.to_string(),
            video: self.video,
            restart: true,
            gen: self.gen,
        };
        vec![
            Action::Send {
                to_dh: self.peer_dh.clone(),
                kind: crate::call_kind::OFFER.to_string(),
                body: serde_json::to_string(&body).unwrap_or_default(),
            },
            self.state_action(),
        ]
    }

    /// 本地 ICE candidate 要发给对方
    pub fn candidate_action(&self, candidate: &str, mid: &str, mline: i32) -> Vec<Action> {
        if !matches!(self.state, State::Outgoing | State::Incoming | State::Active) {
            return Vec::new();
        }
        if candidate.len() > MAX_CANDIDATE_LEN {
            return Vec::new();
        }
        let body = IceBody {
            call_id: self.call_id.clone(),
            candidate: candidate.to_string(),
            mid: mid.to_string(),
            mline,
            gen: self.gen,
        };
        vec![Action::Send {
            to_dh: self.peer_dh.clone(),
            kind: crate::call_kind::ICE.to_string(),
            body: serde_json::to_string(&body).unwrap_or_default(),
        }]
    }

    // ------------------------------------------------------------ 被叫

    /// 收到 offer。
    ///
    /// 忙线时不抢占，而是让上层回一条 reject —— 抢断会让正在通话的双方都被踢掉。
    pub fn on_offer(&mut self, from_dh: &str, body: &OfferBody, msg_id: &str, now: u64) -> Vec<Action> {
        self.on_offer_ex(from_dh, body, msg_id, now).0
    }

    /// [on_offer] 的完整版：附带"这份 offer 是否被状态机接受"。
    ///
    /// 调用方（`call_inbound`）只允许把**被接受**的 offer 的 SDP 透出给 UI：
    /// 被忙线拒接的第三方 offer、被代际守卫丢弃的迟到重协商，其 SDP 一旦
    /// 透出，UI 按契约执行 setRemoteDescription 就会污染正在进行的通话
    /// （这正是状态机防了半天、却在 API 层漏掉的那一刀）。
    pub fn on_offer_ex(
        &mut self,
        from_dh: &str,
        body: &OfferBody,
        msg_id: &str,
        now: u64,
    ) -> (Vec<Action>, bool) {
        if !Self::check_id(&body.call_id) || body.sdp.len() > MAX_SDP_LEN {
            return (Vec::new(), false);
        }

        // 同一通话重复到达：只认第一次，避免 UI 反复弹窗
        if !self.mark_seen(msg_id) {
            return (Vec::new(), false);
        }

        // 通话中的重协商（对端换了网络）：状态保持 Active，只是要重建媒体通道。
        // 必须放在"忙线拒接"之前判断，否则正在说话的通话会被当成新来电挂掉。
        if body.restart {
            if body.call_id != self.call_id || self.state != State::Active {
                // 旧通话的重协商、或还没接通就要重协商：都不合理，忽略
                return (Vec::new(), false);
            }

            // 旧代际（重放 / 迟到）直接丢：否则会把已经重建好的媒体通道退回上一条网络路径
            if body.gen != 0 && body.gen <= self.peer_gen {
                return (Vec::new(), false);
            }

            // 双方同时换网络会各自发重协商（glare）。规则：
            //   1. 对端代际更高 → 我方让路，放弃自己那个还没被应答的 offer
            //   2. 代际相同 → 主叫赢，被叫让路
            //   3. 对端代际更低 → 一律拒。早先只拒 `<= peer_gen`，于是
            //      `peer_gen < body.gen < self.gen` 的中间值从缝隙漏进去被采纳
            //      （本端已推进到更高代际说明刚发过更新的重协商，这份是过时的），
            //      双方 SDP 交叉错位且无任何超时兜底——"接通了但没声音"且永不恢复。
            // 让路时清掉 `awaiting`，这样迟到的"对我方 offer 的应答"会被拒绝。
            if body.gen != 0 {
                if body.gen > self.gen {
                    self.gen = body.gen;
                    self.awaiting = None;
                } else if body.gen == self.gen {
                    if self.awaiting.is_some() && self.caller {
                        // 我是主叫：我的 offer 优先，忽略对方的
                        return (Vec::new(), false);
                    }
                    self.awaiting = None;
                } else {
                    // 低于本端当前代际：过时 offer，拒绝
                    return (Vec::new(), false);
                }
                self.peer_gen = body.gen;
            }

            self.renegotiating = true;
            self.renegotiate_since = Some(now);
            return (vec![self.state_action()], true);
        }

        // 同一通电话的重发 offer（对方没收到我先前的 answer）：状态不变，
        // 把缓存里的 answer 补发一次。早先注释承诺了这件事但没做——主叫侧
        // 收不到 answer 只能响到 45 秒超时。
        if self.call_id == body.call_id && matches!(self.state, State::Incoming | State::Active) {
            let mut acts = vec![self.state_action()];
            if self.state == State::Active {
                if let Some((sdp, gen)) = &self.last_answer {
                    acts.extend(self.make_answer_actions_with_gen(sdp.clone(), *gen));
                }
            }
            return (acts, true);
        }

        if self.state.is_busy() {
            let end = EndBody {
                call_id: body.call_id.clone(),
                reason: "busy".to_string(),
            };
            // 忙线拒接是合法动作（对方需要知道），但 SDP 绝不能透出：
            // 那是第三方的 offer，UI 应用它会毁掉正在进行的通话。
            return (
                vec![Action::Send {
                    to_dh: from_dh.to_string(),
                    kind: crate::call_kind::REJECT.to_string(),
                    body: serde_json::to_string(&end).unwrap_or_default(),
                }],
                false,
            );
        }

        self.call_id = body.call_id.clone();
        self.peer_dh = from_dh.to_string();
        self.video = body.video;
        self.reason.clear();
        self.renegotiating = false;
        self.gen = 0;
        self.awaiting = None;
        self.peer_gen = 0;
        self.caller = false;
        // 去重表按通话清空：表只有 64 格，跨通话残留的旧 msg_id 一旦与
        // 新信令 id 相撞（重发/上游 id 复用），新来电会被静默吞掉。
        // 清空后必须把当前 msg_id 重新记上（mark_seen 在函数开头做过、
        // 被这里抹掉了），否则同一条 offer 的重投会穿透去重。
        self.seen.clear();
        self.mark_seen(msg_id);
        self.enter(State::Incoming, now);
        (vec![self.state_action()], true)
    }

    /// 用户点了接听。返回 answer 动作；`sdp` 由上层在设置远端描述后生成。
    pub fn accept(&mut self, sdp: &str, now: u64) -> Vec<Action> {
        // Incoming：初次接听；Active + renegotiating：对端重协商后的应答
        let answering_restart = self.state == State::Active && self.renegotiating;
        if self.state != State::Incoming && !answering_restart {
            return Vec::new();
        }
        if sdp.len() > MAX_SDP_LEN {
            return Vec::new();
        }
        self.renegotiating = false;
        self.renegotiate_since = None;
        self.awaiting = None;
        // 记下来：对方没收到这条 answer 而重发 offer 时，用缓存补发
        self.last_answer = Some((sdp.to_string(), self.gen));
        self.enter(State::Active, now);
        let mut v = vec![self.state_action()];
        v.extend(self.make_answer_actions(sdp));
        v
    }

    /// 用户点了拒接
    pub fn reject(&mut self, reason: &str, now: u64) -> Vec<Action> {
        if self.state != State::Incoming {
            return Vec::new();
        }
        self.reason = if reason.is_empty() {
            "declined".to_string()
        } else {
            reason.to_string()
        };
        let end = EndBody {
            call_id: self.call_id.clone(),
            reason: self.reason.clone(),
        };
        let to = self.peer_dh.clone();
        self.enter(State::Ended, now);
        vec![
            Action::Send {
                to_dh: to,
                kind: crate::call_kind::REJECT.to_string(),
                body: serde_json::to_string(&end).unwrap_or_default(),
            },
            self.state_action(),
        ]
    }

    // ------------------------------------------------------------ 双方

    /// 收到 answer（主叫侧）
    pub fn on_answer(&mut self, from_dh: &str, body: &AnswerBody, now: u64) -> Vec<Action> {
        self.on_answer_ex(from_dh, body, now).0
    }

    /// [on_answer] 的完整版：附带"这份 answer 是否被状态机接受"。
    ///
    /// 与 on_offer_ex 同一道理：被 call_id/代际/来源守卫丢弃的 answer，其 SDP
    /// 不能透出 UI——UI 按契约 setRemoteDescription 会把本地 gen=2 的 offer
    /// 配上远端 gen=1 的 answer，媒体通道错位（二轮审计 P2-1）。
    pub fn on_answer_ex(&mut self, from_dh: &str, body: &AnswerBody, now: u64) -> (Vec<Action>, bool) {
        // call_id 不匹配一律忽略：可能来自已经放弃的旧通话
        if body.call_id != self.call_id {
            return (Vec::new(), false);
        }
        // 两种合法情形：初次应答（Outgoing），或重协商应答（Active + renegotiating）
        let first_answer = self.state == State::Outgoing;
        let renegotiation_answer = self.state == State::Active && self.renegotiating;
        if !first_answer && !renegotiation_answer {
            return (Vec::new(), false);
        }
        if from_dh != self.peer_dh {
            return (Vec::new(), false);
        }
        if body.sdp.len() > MAX_SDP_LEN {
            return (Vec::new(), false);
        }

        // 代际校验：只接受"对我方当前这个 offer"的应答。
        //
        // 这是整个代际机制的核心。没有它会发生什么：我方连发两次重协商，
        // 第一次的应答迟到到达，被当成第二次的应答应用 —— 本地描述装的是
        // 上一条网络路径的 SDP，媒体通道就此错位，用户听到的是"接通了但没声音"。
        //
        // 严格相等，不给 gen==0 开后门：早期实现试过"gen 为 0 就放行（兼容旧版）"，
        // 结果第一个用例就挂了——gen=0 的旧应答正好能穿过这道口子，
        // 代际防护等于没做。旧版对端的首次通话 gen 全是 0，仍然正常。
        if self.awaiting != Some(body.gen) {
            return (Vec::new(), false);
        }

        self.awaiting = None;
        self.peer_gen = body.gen;
        self.renegotiating = false;
        self.renegotiate_since = None;
        self.enter(State::Active, now);
        (vec![self.state_action()], true)
    }

    /// 收到远端 ICE。**不做状态检查**，只要 call_id 对就交给上层 →
    /// 因为 candidate 完全可能先于 answer 到达，丢了就得等重传甚至协商失败。
    pub fn on_ice(&mut self, from_dh: &str, body: &IceBody) -> Option<IceBody> {
        if body.call_id != self.call_id {
            return None;
        }
        if from_dh != self.peer_dh {
            return None;
        }
        if body.candidate.len() > MAX_CANDIDATE_LEN {
            return None;
        }

        // 丢弃旧代际的候选：它们指向换网络之前的那条路径。
        // 把它们 addIceCandidate 进去会把新通道的选路带偏，典型症状是
        // "ICE 显示 connected，但音频一直不出声"。
        // 同样是严格比较：首次通话双方 gen 都是 0，不影响正常流程。
        if body.gen < self.gen {
            return None;
        }
        Some(body.clone())
    }

    /// 对方挂了 / 拒接
    pub fn on_end(&mut self, from_dh: &str, body: &EndBody, now: u64) -> Vec<Action> {
        // 关键：call_id 不匹配一律忽略。
        // 少了这一条，上一通电话迟到的 end 会把当前通话挂掉。
        if body.call_id != self.call_id {
            return Vec::new();
        }
        if !self.state.is_busy() {
            return Vec::new();
        }
        // 来源校验与 on_answer/on_ice 对齐：知道 call_id 不等于是对端本人。
        // call_id 只在加密信令内传播，泄露面小，但防御口径必须一致。
        // 注意拒接场景（Busy 时我方 reject、对方回 end 收尾）不会走到这——
        // 那时状态已 Ended，is_busy 已经拦住了。
        if from_dh != self.peer_dh {
            return Vec::new();
        }
        self.reason = if body.reason.is_empty() {
            "peer_ended".to_string()
        } else {
            body.reason.clone()
        };
        self.enter(State::Ended, now);
        vec![self.state_action()]
    }

    /// 本端挂断
    pub fn hangup(&mut self, reason: &str, now: u64) -> Vec<Action> {
        if !self.state.is_busy() {
            return Vec::new();
        }
        self.reason = if reason.is_empty() {
            "local_ended".to_string()
        } else {
            reason.to_string()
        };
        let end = EndBody {
            call_id: self.call_id.clone(),
            reason: self.reason.clone(),
        };
        let to = self.peer_dh.clone();
        self.enter(State::Ended, now);
        vec![
            Action::Send {
                to_dh: to,
                kind: crate::call_kind::END.to_string(),
                body: serde_json::to_string(&end).unwrap_or_default(),
            },
            self.state_action(),
        ]
    }

    /// 定时驱动：振铃超时自动挂断。上层应每秒调一次。
    pub fn tick(&mut self, now: u64) -> Vec<Action> {
        match self.state {
            State::Outgoing | State::Incoming if now.saturating_sub(self.since) >= RING_TIMEOUT_SECS => {
                let is_out = self.state == State::Outgoing;
                self.reason = if is_out {
                    "no_answer".to_string()
                } else {
                    "missed".to_string()
                };
                let end = EndBody {
                    call_id: self.call_id.clone(),
                    reason: self.reason.clone(),
                };
                let to = self.peer_dh.clone();
                // 主叫超时：告诉对方别响了；被叫超时：对方自己也会超时，只更新本地
                self.enter(State::Ended, now);
                if is_out {
                    vec![
                        Action::Send {
                            to_dh: to,
                            kind: crate::call_kind::END.to_string(),
                            body: serde_json::to_string(&end).unwrap_or_default(),
                        },
                        self.state_action(),
                    ]
                } else {
                    vec![self.state_action()]
                }
            }
            // 重协商卡死兜底：对端掉线时 answer 永远不来，UI 会永远停在
            // "重建媒体通道"。超过重协商时限就本地解除标志——通话保持 Active
            // （旧媒体通道能通就继续用），用户可再手动点一次重连。
            State::Active if self.renegotiating => {
                let since = self.renegotiate_since.unwrap_or(self.since);
                if now.saturating_sub(since) >= RENEGOTIATE_TIMEOUT_SECS {
                    self.renegotiating = false;
                    self.renegotiate_since = None;
                    self.awaiting = None;
                    vec![self.state_action()]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    /// 结束态回到空闲。UI 关闭通话界面时调。
    pub fn reset(&mut self) {
        if self.state == State::Ended {
            self.state = State::Idle;
            self.call_id.clear();
            self.peer_dh.clear();
            self.reason.clear();
            // 同上：去重表只对"当前通话"有意义，回 Idle 一并清
            self.seen.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "dh_alice";
    const B: &str = "dh_bob";

    fn offer(id: &str) -> OfferBody {
        OfferBody {
            call_id: id.into(),
            sdp: "v=0 fake-sdp".into(),
            video: false,
            restart: false,
            gen: 0,
        }
    }

    fn restart_offer(id: &str) -> OfferBody {
        OfferBody {
            call_id: id.into(),
            sdp: "v=0 restart-sdp".into(),
            video: false,
            restart: true,
            gen: 0,
        }
    }

    fn answer(id: &str) -> AnswerBody {
        AnswerBody {
            call_id: id.into(),
            sdp: "v=0 fake-answer".into(),
            gen: 0,
        }
    }

    fn find_send<'a>(acts: &'a [Action], kind: &str) -> Option<&'a Action> {
        acts.iter().find(|a| matches!(a, Action::Send { kind: k, .. } if k == kind))
    }


    // ---------------------------------------------------------- ICE 重协商
    //
    // 场景：通话中从 WiFi 切到流量。媒体通道废了，必须带 IceRestart 重新协商，
    // 但**不能把通话打断**——用户不应该听到振铃或看到来电界面。

    /// 主叫发起重协商：状态保持 Active，只置标记
    #[test]
    fn restart_offer_from_caller_keeps_active() {
        let mut a = Call::new();
        a.start_outgoing("c1", B, false, 100).unwrap();
        a.on_answer(B, &answer("c1"), 101);
        assert_eq!(a.state(), State::Active);

        let acts = a.make_restart_offer_actions("v=0 restart");
        assert_eq!(a.state(), State::Active, "重协商不能改变通话状态");
        // 必须有一条带 restart 标志的 offer 发出去
        let sent = find_send(&acts, crate::call_kind::OFFER).expect("应发出重协商 offer");
        match sent {
            Action::Send { body, .. } => {
                assert!(body.contains("\"restart\":true"), "offer 必须带 restart 标志: {body}");
                assert!(body.contains("\"c1\""), "call_id 必须与当前通话一致: {body}");
            }
            _ => unreachable!(),
        }
    }

    /// 被叫收到重协商 offer：仍是 Active，不弹来电，且要标记待应答
    #[test]
    fn restart_offer_from_peer_does_not_ring() {
        let mut b = Call::new();
        b.on_offer(A, &offer("c1"), "m1", 100);
        b.accept("v=0 ans", 101);
        assert_eq!(b.state(), State::Active);

        let acts = b.on_offer(A, &restart_offer("c1"), "m2", 200);
        assert_eq!(b.state(), State::Active, "重协商不能回到 incoming");
        assert!(!acts.iter().any(|x| matches!(x, Action::Send { kind, .. } if kind == crate::call_kind::REJECT)),
            "绝不能回 reject：那会把正在说的话挂断");
        match acts.first() {
            Some(Action::State { renegotiating, state, .. }) => {
                assert!(*renegotiating, "必须告诉上层需要重新协商");
                assert_eq!(state, "active");
            }
            other => panic!("应返回状态事件，实际 {other:?}"),
        }

        // 上层 createAnswer 后回来
        let acts = b.accept("v=0 re-ans", 201);
        assert_eq!(b.state(), State::Active);
        assert!(find_send(&acts, crate::call_kind::ANSWER).is_some());
    }

    /// 主叫收到重协商应答：清标记，通话继续
    #[test]
    fn restart_answer_clears_flag() {
        let mut a = Call::new();
        a.start_outgoing("c1", B, false, 100).unwrap();
        a.on_answer(B, &answer("c1"), 101);
        a.make_restart_offer_actions("v=0 restart");

        // 重协商的应答必须带当前代际（1）。用 gen=0 的应答在这里已经属于
        // 过期信令，会被丢弃——这正是代际防护要的效果，见 stale_answer_* 用例。
        let acts = a.on_answer(B, &answer_gen("c1", 1), 200);
        assert_eq!(a.state(), State::Active);
        assert!(matches!(
            acts.first(),
            Some(Action::State { renegotiating: false, .. })
        ), "应答到达后必须清掉重协商标记");
    }

    /// 同通话的 offer 重发到达（对方没收到我的 answer）：必须用缓存把
    /// answer 补发一次。早先只返回状态动作，主叫收不到应答只能响到超时。
    #[test]
    fn duplicate_offer_gets_cached_answer_resent() {
        let mut b = Call::new();
        b.on_offer(A, &offer("c1"), "m1", 100);
        b.accept("v=0 my-answer", 101);
        assert_eq!(b.state(), State::Active);

        // 对端重投同一 offer（新 msg_id，绕过去重）
        let (acts, accepted) = b.on_offer_ex(A, &offer("c1"), "m1-replay", 110);
        assert!(accepted);
        let ans = find_send(&acts, crate::call_kind::ANSWER).expect("应补发 answer");
        match ans {
            Action::Send { body, .. } => {
                assert!(body.contains("my-answer"), "补发的必须是缓存的应答: {body}");
                assert!(body.contains("\"gen\":0"), "首次通话补发 gen=0: {body}");
            }
            _ => unreachable!(),
        }
        assert_eq!(b.state(), State::Active, "状态不能被重投的 offer 打乱");
    }

    /// 重协商的 answer 永不到达（对端掉线）：超时后必须本地解除 renegotiating，
    /// 不能让 UI 永远停在"重建媒体通道"。通话保持 Active。
    #[test]
    fn renegotiation_times_out_and_recovers() {
        let mut a = Call::new();
        a.start_outgoing("c1", B, false, 100).unwrap();
        a.on_answer(B, &answer("c1"), 101);
        a.make_restart_offer_actions("v=0 restart"); // reneg 开始于 ~110

        // 未到时限：不该有动作
        assert!(a.tick(115).is_empty());
        // 重协商超时：必须解除标记、保持 Active
        let acts = a.tick(110 + RENEGOTIATE_TIMEOUT_SECS);
        assert!(matches!(
            acts.first(),
            Some(Action::State { renegotiating: false, .. })
        ), "超时后必须解除重协商标记");
        assert_eq!(a.state(), State::Active, "通话保持，不走 Ended");
    }

    /// 旧通话的重协商信令必须忽略，否则会把当前通话搞乱
    #[test]
    fn restart_offer_for_other_call_is_ignored() {
        let mut b = Call::new();
        b.on_offer(A, &offer("c1"), "m1", 100);
        b.accept("v=0 ans", 101);

        let acts = b.on_offer(A, &restart_offer("c0"), "m2", 200);
        assert!(acts.is_empty());
        assert_eq!(b.state(), State::Active);
        assert_eq!(b.call_id(), "c1", "call_id 不能被旧信令改掉");
    }

    /// 还没接通就来的重协商没有意义，必须忽略而不是把状态推成 Active
    #[test]
    fn restart_offer_before_active_is_ignored() {
        let mut b = Call::new();
        b.on_offer(A, &offer("c1"), "m1", 100);
        assert_eq!(b.state(), State::Incoming);

        let acts = b.on_offer(A, &restart_offer("c1"), "m2", 110);
        assert!(acts.is_empty());
        assert_eq!(b.state(), State::Incoming, "不能因为重协商就直接接通");
    }

    /// 重协商期间别人打进来，仍然要回 busy，且不能影响当前通话
    #[test]
    fn third_party_during_restart_still_gets_busy() {
        let mut b = Call::new();
        b.on_offer(A, &offer("c1"), "m1", 100);
        b.accept("v=0 ans", 101);
        b.on_offer(A, &restart_offer("c1"), "m2", 200);

        let acts = b.on_offer("dh_carol", &offer("c2"), "m3", 210);
        assert_eq!(b.state(), State::Active);
        assert_eq!(b.call_id(), "c1");
        assert!(find_send(&acts, crate::call_kind::REJECT).is_some(), "应回 busy");
    }

    /// 挂断后不能残留重协商标记
    #[test]
    fn renegotiating_flag_cleared_on_end() {
        let mut a = Call::new();
        a.start_outgoing("c1", B, false, 100).unwrap();
        a.on_answer(B, &answer("c1"), 101);
        a.make_restart_offer_actions("v=0 restart");
        a.hangup("local_ended", 200);
        assert!(!a.renegotiating);
    }

    /// 完整一通电话：主叫 → 振铃 → 接听 → 通话 → 挂断
    #[test]
    fn full_call_lifecycle() {
        let mut a = Call::new();
        let mut b = Call::new();

        // A 发起
        let acts = a.start_outgoing("c1", B, false, 100).unwrap();
        assert_eq!(a.state(), State::Outgoing);
        assert_eq!(acts.len(), 1);
        let off = a.make_offer_actions(&offer("c1").sdp);
        assert!(find_send(&off, crate::call_kind::OFFER).is_some());

        // B 收到
        let acts = b.on_offer(A, &offer("c1"), "m1", 101);
        assert_eq!(b.state(), State::Incoming);
        assert_eq!(b.call_id(), "c1");

        // B 接听
        let acts = b.accept("v=0 answer", 105);
        assert_eq!(b.state(), State::Active);
        assert!(find_send(&acts, crate::call_kind::ANSWER).is_some());

        // A 收到 answer
        let acts = a.on_answer(B, &answer("c1"), 106);
        assert_eq!(a.state(), State::Active);

        // A 挂断
        let acts = a.hangup("local_ended", 200);
        assert_eq!(a.state(), State::Ended);
        assert!(find_send(&acts, crate::call_kind::END).is_some());

        // B 收到挂断
        let acts = b.on_end(A, &EndBody { call_id: "c1".into(), reason: "local_ended".into() }, 201);
        assert_eq!(b.state(), State::Ended);
        assert!(!acts.is_empty());
    }

    /// 忙线时必须回 reject，而且**不能打断正在进行的通话**
    #[test]
    fn busy_rejects_without_disturbing_current_call() {
        let mut b = Call::new();
        b.on_offer(A, &offer("c1"), "m1", 100);
        b.accept("v=0 ans", 101);
        assert_eq!(b.state(), State::Active);

        // 另一个人打进来
        let acts = b.on_offer(A, &offer("c2"), "m2", 110);
        assert_eq!(b.state(), State::Active, "当前通话不能被打断");
        assert_eq!(b.call_id(), "c1", "call_id 不能被覆盖");
        let rej = find_send(&acts, crate::call_kind::REJECT).expect("应回 reject");
        match rej {
            Action::Send { to_dh, body, .. } => {
                assert_eq!(to_dh, A);
                assert!(body.contains("\"c2\""), "reject 要针对新来电: {body}");
                assert!(body.contains("busy"));
            }
            _ => unreachable!(),
        }
    }

    /// 忙线时第三方的 offer：`on_offer_ex` 必须报告**未接受**——
    /// `call_inbound` 据此不把这份 SDP 透出 UI。否则 UI 按契约
    /// setRemoteDescription 会把正在进行的通话媒体描述整个换掉。
    #[test]
    fn busy_offer_is_not_accepted_for_ui() {
        let mut b = Call::new();
        b.on_offer(A, &offer("c1"), "m1", 100);
        b.accept("v=0 ans", 101);

        let (_, accepted) = b.on_offer_ex("dh_carol", &offer("c2"), "m2", 110);
        assert!(!accepted, "忙线拒接的 offer 不能透出给 UI");

        // 对照：正常来电必须报告接受（否则 UI 拿不到 SDP 建不了媒体）
        let mut idle = Call::new();
        let (_, accepted2) = idle.on_offer_ex(A, &offer("c9"), "m9", 200);
        assert!(accepted2);
    }

    /// glare 缝隙：本端连续两次重协商推进到 gen=2、还在等应答时，对端
    /// gen=1 的重协商 offer 迟到到达。早先 `peer_gen < body.gen < self.gen`
    /// 的中间值从守卫缝隙漏进去被采纳，双方 SDP 交叉错位且无超时兜底。
    /// 低于本端当前代际的 restart offer 必须整体拒绝。
    #[test]
    fn restart_offer_below_local_gen_is_rejected() {
        let mut a = Call::new();
        a.on_offer(B, &offer("c1"), "m1", 100);
        a.accept("v=0 ans", 101);
        assert_eq!(a.state(), State::Active);

        // 本端连续两次重协商：gen 0 → 1 → 2，还在等第二次的应答
        a.make_restart_offer_actions("v=0 r1");
        a.make_restart_offer_actions("v=0 r2");

        // 对端 gen=1 的 offer 迟到：peer_gen=0、1 < self.gen=2 → 必须拒
        let (acts, accepted) = a.on_offer_ex(B, &restart_offer_gen("c1", 1), "m9", 200);
        assert!(acts.is_empty(), "中间代际的 offer 不该产生任何动作");
        assert!(!accepted, "过时 offer 的 SDP 不能透出 UI");
        assert_eq!(a.state(), State::Active, "通话状态不受影响");

        // 对照：高于本端代际的让路路径照常工作
        let (_, accepted2) = a.on_offer_ex(B, &restart_offer_gen("c1", 5), "m10", 210);
        assert!(accepted2, "更高代际仍应被接受（让路）");
    }

    /// 中继重投同一条 offer（同 msg_id）：去重后必须报告未接受，
    /// 否则 UI 会反复 prepareIncoming（重响铃/重建 PeerConnection）。
    #[test]
    fn duplicate_offer_is_not_accepted_for_ui() {
        let mut b = Call::new();
        let (_, first) = b.on_offer_ex(A, &offer("c1"), "same-msg", 100);
        assert!(first);
        let (_, dup) = b.on_offer_ex(A, &offer("c1"), "same-msg", 101);
        assert!(!dup, "重投的 offer 不该再次透出 SDP");
    }

    /// 上一通电话迟到的 end 不能把当前通话挂掉 —— 弱网重投下真实会发生
    #[test]
    fn late_end_from_previous_call_is_ignored() {
        let mut a = Call::new();

        // 第一通：c1，已结束
        a.start_outgoing("c1", B, false, 100).unwrap();
        a.on_answer(B, &answer("c1"), 101);
        a.hangup("local_ended", 110);
        a.reset();
        assert_eq!(a.state(), State::Idle);

        // 第二通：c2，通话中
        a.start_outgoing("c2", B, false, 200).unwrap();
        a.on_answer(B, &answer("c2"), 201);
        assert_eq!(a.state(), State::Active);

        // c1 的 end 迟到了
        let acts = a.on_end(B, &EndBody { call_id: "c1".into(), reason: "local_ended".into() }, 210);
        assert!(acts.is_empty(), "旧通话的信令必须被忽略");
        assert_eq!(a.state(), State::Active, "当前通话不能被挂掉");
    }

    /// 同一条 offer 重投多次，只能弹一次窗
    #[test]
    fn duplicate_offer_does_not_retrigger() {
        let mut b = Call::new();
        let acts1 = b.on_offer(A, &offer("c1"), "m1", 100);
        assert_eq!(acts1.len(), 1);
        assert_eq!(b.state(), State::Incoming);

        // 中继重投同一条
        let acts2 = b.on_offer(A, &offer("c1"), "m1", 101);
        assert!(acts2.is_empty(), "重复的 offer 不该再发状态通知");
    }

    /// answer 的 call_id 不匹配要忽略：可能来自已经放弃的旧通话
    #[test]
    fn answer_with_wrong_call_id_ignored() {
        let mut a = Call::new();
        a.start_outgoing("c2", B, false, 100).unwrap();
        let acts = a.on_answer(B, &answer("c1"), 101);
        assert!(acts.is_empty());
        assert_eq!(a.state(), State::Outgoing, "仍在等真正的那通 answer");
    }

    /// ICE 可能先于 answer 到达，不能因为状态不对就丢掉
    #[test]
    fn ice_before_answer_is_passed_through() {
        let mut a = Call::new();
        a.start_outgoing("c1", B, false, 100).unwrap();
        assert_eq!(a.state(), State::Outgoing);

        let ice = IceBody {
            call_id: "c1".into(),
            candidate: "candidate:1 1 UDP 2130706431 192.168.1.2 5000 typ host".into(),
            mid: "audio".into(),
            mline: 0,
            gen: 0,
        };
        let got = a.on_ice(B, &ice).expect("answer 之前的 ICE 也要转发");
        assert_eq!(got.candidate, ice.candidate);
        assert_eq!(a.state(), State::Outgoing, "状态不能被 ICE 改变");
    }

    /// 别人发来的 ICE 必须丢弃，防止串台
    #[test]
    fn ice_from_stranger_is_dropped() {
        let mut a = Call::new();
        a.start_outgoing("c1", B, false, 100).unwrap();
        let ice = IceBody {
            call_id: "c1".into(),
            candidate: "x".into(),
            mid: String::new(),
            mline: 0,
            gen: 0,
        };
        assert!(a.on_ice("dh_carol", &ice).is_none());
    }

    /// 振铃超时要自动结束，否则会一直响
    #[test]
    fn ring_timeout_ends_outgoing() {
        let mut a = Call::new();
        a.start_outgoing("c1", B, false, 1000).unwrap();
        assert!(a.tick(1000 + RING_TIMEOUT_SECS - 1).is_empty(), "还没超时");
        let acts = a.tick(1000 + RING_TIMEOUT_SECS);
        assert_eq!(a.state(), State::Ended);
        assert_eq!(a.reason(), "no_answer");
        assert!(find_send(&acts, crate::call_kind::END).is_some());
    }

    /// 被叫侧超时只更新本地，不需要发 end（对方的振铃计时器会自己超时）
    #[test]
    fn ring_timeout_on_incoming_does_not_send() {
        let mut b = Call::new();
        b.on_offer(A, &offer("c1"), "m1", 1000);
        let acts = b.tick(1000 + RING_TIMEOUT_SECS);
        assert_eq!(b.state(), State::Ended);
        assert_eq!(b.reason(), "missed");
        assert!(find_send(&acts, crate::call_kind::END).is_none());
    }

    /// 忙时不能重复发起
    #[test]
    fn cannot_start_two_calls() {
        let mut a = Call::new();
        a.start_outgoing("c1", B, false, 100).unwrap();
        assert!(a.start_outgoing("c2", B, false, 101).is_err());
    }

    /// 非法 call_id 与超长 SDP 一律拒绝
    #[test]
    fn rejects_malformed_input() {
        let mut a = Call::new();
        assert!(a.start_outgoing("", B, false, 100).is_err());
        assert!(a.start_outgoing(&"x".repeat(65), B, false, 100).is_err());

        let mut b = Call::new();
        let big = OfferBody {
            call_id: "c1".into(),
            sdp: "x".repeat(MAX_SDP_LEN + 1),
            video: false,
            restart: false,
            gen: 0,
        };
        assert!(b.on_offer(A, &big, "m1", 100).is_empty());
        assert_eq!(b.state(), State::Idle, "超长 SDP 不能改变状态");
    }

    /// 视频通话标记要能传递
    #[test]
    fn video_flag_propagates() {
        let mut a = Call::new();
        a.start_outgoing("c1", B, true, 100).unwrap();
        let acts = a.make_offer_actions("v=0 sdp");
        match find_send(&acts, crate::call_kind::OFFER).unwrap() {
            Action::Send { body, .. } => assert!(body.contains("\"video\":true"), "{body}"),
            _ => unreachable!(),
        }

        let mut b = Call::new();
        let mut o = offer("c1");
        o.video = true;
        b.on_offer(A, &o, "m1", 101);
        assert!(b.video(), "被叫要知道这是视频电话");
    }

    // ================================================================
    // 代际防护（generation）—— 换网络后"接通了但没声音"的根因
    // ================================================================

    fn restart_offer_gen(id: &str, gen: u64) -> OfferBody {
        OfferBody {
            call_id: id.into(),
            sdp: "v=0 restart-sdp".into(),
            video: false,
            restart: true,
            gen,
        }
    }

    fn answer_gen(id: &str, gen: u64) -> AnswerBody {
        AnswerBody {
            call_id: id.into(),
            sdp: "v=0 fake-answer".into(),
            gen,
        }
    }

    fn ice_gen(id: &str, gen: u64) -> IceBody {
        IceBody {
            call_id: id.into(),
            candidate: "candidate:1 1 UDP 1 10.0.0.1 1000 typ host".into(),
            mid: "audio".into(),
            mline: 0,
            gen,
        }
    }

    /// 把通话推进到 Active（主叫视角）
    fn active_as_caller() -> Call {
        let mut a = Call::new();
        a.start_outgoing("c1", B, false, 100).unwrap();
        a.make_offer_actions("v=0 sdp");
        let acts = a.on_answer(B, &answer_gen("c1", 0), 101);
        assert!(!acts.is_empty(), "首个应答应当被接受");
        assert_eq!(a.state(), State::Active);
        a
    }

    /// 迟到的旧代际应答必须丢弃，否则本地描述会装成上一条网络路径的 SDP
    #[test]
    fn stale_answer_for_older_generation_is_dropped() {
        let mut a = active_as_caller();
        a.make_restart_offer_actions("v=0 restart");
        assert_eq!(a.gen(), 1, "重协商应当把代际推进到 1");

        // 第一次重协商的应答（gen=0）迟到
        let acts = a.on_answer(B, &answer_gen("c1", 0), 200);
        assert!(acts.is_empty(), "旧代际应答不能被应用");
        assert!(a.renegotiating(), "仍在等待当前代际的应答");

        // 当前代际的应答才生效
        let acts = a.on_answer(B, &answer_gen("c1", 1), 201);
        assert!(!acts.is_empty());
        assert!(!a.renegotiating());
    }

    /// 换网络前的 ICE candidate 不能再喂给新的 PeerConnection
    #[test]
    fn stale_ice_from_older_generation_is_dropped() {
        let mut a = active_as_caller();
        a.make_restart_offer_actions("v=0 restart"); // gen -> 1
        a.on_answer(B, &answer_gen("c1", 1), 200);

        assert!(
            a.on_ice(B, &ice_gen("c1", 0)).is_none(),
            "gen=0 的候选属于旧路径，必须丢弃"
        );
        assert!(
            a.on_ice(B, &ice_gen("c1", 1)).is_some(),
            "当前代际的候选要放行"
        );
        // 也不给 gen=0 开后门：它同样属于旧路径
        assert!(
            a.on_ice(B, &ice_gen("c1", 0)).is_none(),
            "gen=0 也是旧代际，必须丢弃"
        );
    }

    /// 重复/重放的旧重协商 offer 必须忽略（否则媒体通道被退回旧路径）
    #[test]
    fn replayed_restart_offer_is_ignored() {
        let mut b = Call::new();
        b.on_offer(A, &offer("c1"), "m1", 100);
        b.accept("v=0 ans", 101);

        let acts = b.on_offer(A, &restart_offer_gen("c1", 2), "m2", 200);
        assert!(!acts.is_empty(), "更高代际要接受");
        assert_eq!(b.gen(), 2);

        let acts = b.on_offer(A, &restart_offer_gen("c1", 2), "m3", 201);
        assert!(acts.is_empty(), "同一代际重复到达要忽略");

        let acts = b.on_offer(A, &restart_offer_gen("c1", 1), "m4", 202);
        assert!(acts.is_empty(), "更低代际要忽略");
    }

    /// 对端代际更高时我方让路：放弃自己等待中的 offer
    #[test]
    fn lower_generation_yields_to_higher() {
        let mut a = active_as_caller();
        a.make_restart_offer_actions("v=0 mine"); // 我方 gen=1，等待应答
        assert!(a.renegotiating());

        // 对端已经走到 gen=3
        let acts = a.on_offer(B, &restart_offer_gen("c1", 3), "m9", 300);
        assert!(!acts.is_empty(), "要让路并准备应答");
        assert_eq!(a.gen(), 3, "代际要采纳对方的");

        // 我方那个被放弃的 offer 的应答现在回来，必须丢弃
        let acts = a.on_answer(B, &answer_gen("c1", 1), 301);
        assert!(acts.is_empty(), "已放弃的 offer 的应答不能应用");
    }

    /// 双方同时重协商：主叫赢，被叫让路（双方算出的结果一致，不会互相等死）
    #[test]
    fn glare_equal_generation_caller_wins() {
        // 主叫视角
        let mut caller = active_as_caller();
        caller.make_restart_offer_actions("v=0 callers"); // gen=1
        let acts = caller.on_offer(B, &restart_offer_gen("c1", 1), "m1", 200);
        assert!(acts.is_empty(), "主叫应当忽略被叫的对等重协商");
        assert_eq!(caller.gen(), 1);
        let acts = caller.on_answer(B, &answer_gen("c1", 1), 201);
        assert!(!acts.is_empty(), "主叫的 offer 仍然会被应答并生效");

        // 被叫视角
        let mut callee = Call::new();
        callee.on_offer(A, &offer("c1"), "m0", 100);
        callee.accept("v=0 ans", 101);
        callee.make_restart_offer_actions("v=0 callees"); // 被叫也 gen=1
        let acts = callee.on_offer(A, &restart_offer_gen("c1", 1), "m1", 200);
        assert!(!acts.is_empty(), "被叫应当让路，准备应答主叫的 offer");
        let acts = callee.on_answer(A, &answer_gen("c1", 1), 201);
        assert!(acts.is_empty(), "让路后自己那个应答不能再应用");
    }

    /// 旧版本对端（不带代际号）仍然能正常重协商
    #[test]
    fn legacy_peer_without_generation_still_works() {
        let mut b = Call::new();
        b.on_offer(A, &offer("c1"), "m1", 100);
        b.accept("v=0 ans", 101);
        // 旧版对端发来的重协商：gen 缺省为 0
        let acts = b.on_offer(A, &restart_offer("c1"), "m2", 200);
        assert!(!acts.is_empty(), "旧版重协商必须照旧工作");
        assert!(b.renegotiating());
    }
}
