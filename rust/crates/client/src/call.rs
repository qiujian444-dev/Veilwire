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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnswerBody {
    pub call_id: String,
    pub sdp: String,
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
        self.enter(State::Outgoing, now);
        Ok(vec![self.state_action()])
    }

    /// 本端拿到本地 SDP 后，产出要发出去的 offer
    pub fn make_offer_actions(&self, sdp: &str) -> Vec<Action> {
        if self.state != State::Outgoing {
            return Vec::new();
        }
        if sdp.len() > MAX_SDP_LEN {
            return Vec::new();
        }
        let body = OfferBody {
            call_id: self.call_id.clone(),
            sdp: sdp.to_string(),
            video: self.video,
            restart: false,
        };
        vec![Action::Send {
            to_dh: self.peer_dh.clone(),
            kind: crate::call_kind::OFFER.to_string(),
            body: serde_json::to_string(&body).unwrap_or_default(),
        }]
    }

    /// 本端产出的 answer 动作（被叫接听时用）
    pub fn make_answer_actions(&self, sdp: &str) -> Vec<Action> {
        let body = AnswerBody {
            call_id: self.call_id.clone(),
            sdp: sdp.to_string(),
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
        if self.state != State::Active || sdp.len() > MAX_SDP_LEN {
            return Vec::new();
        }
        self.renegotiating = true;
        let body = OfferBody {
            call_id: self.call_id.clone(),
            sdp: sdp.to_string(),
            video: self.video,
            restart: true,
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
        if !Self::check_id(&body.call_id) || body.sdp.len() > MAX_SDP_LEN {
            return Vec::new();
        }

        // 同一通话重复到达：只认第一次，避免 UI 反复弹窗
        if !self.mark_seen(msg_id) {
            return Vec::new();
        }

        // 通话中的重协商（对端换了网络）：状态保持 Active，只是要重建媒体通道。
        // 必须放在"忙线拒接"之前判断，否则正在说话的通话会被当成新来电挂掉。
        if body.restart {
            if body.call_id != self.call_id || self.state != State::Active {
                // 旧通话的重协商、或还没接通就要重协商：都不合理，忽略
                return Vec::new();
            }
            self.renegotiating = true;
            return vec![self.state_action()];
        }

        // 同一通电话的重发 offer：状态不变，但要把 answer 补发一次
        // （对方可能没收到我先前的 answer）
        if self.call_id == body.call_id && matches!(self.state, State::Incoming | State::Active) {
            return vec![self.state_action()];
        }

        if self.state.is_busy() {
            let end = EndBody {
                call_id: body.call_id.clone(),
                reason: "busy".to_string(),
            };
            return vec![Action::Send {
                to_dh: from_dh.to_string(),
                kind: crate::call_kind::REJECT.to_string(),
                body: serde_json::to_string(&end).unwrap_or_default(),
            }];
        }

        self.call_id = body.call_id.clone();
        self.peer_dh = from_dh.to_string();
        self.video = body.video;
        self.reason.clear();
        self.enter(State::Incoming, now);
        vec![self.state_action()]
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
        // call_id 不匹配的一律忽略：可能是一通已经放弃的旧通话
        if body.call_id != self.call_id {
            return Vec::new();
        }
        // 两种合法情形：初次应答（Outgoing），或重协商应答（Active + renegotiating）
        let first_answer = self.state == State::Outgoing;
        let renegotiation_answer = self.state == State::Active && self.renegotiating;
        if !first_answer && !renegotiation_answer {
            return Vec::new();
        }
        if from_dh != self.peer_dh {
            return Vec::new();
        }
        if body.sdp.len() > MAX_SDP_LEN {
            return Vec::new();
        }
        self.renegotiating = false;
        self.enter(State::Active, now);
        vec![self.state_action()]
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
        // 已被对方拒接（我方 Outgoing）也算结束
        let _ = from_dh;
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
        }
    }

    fn restart_offer(id: &str) -> OfferBody {
        OfferBody {
            call_id: id.into(),
            sdp: "v=0 restart-sdp".into(),
            video: false,
            restart: true,
        }
    }

    fn answer(id: &str) -> AnswerBody {
        AnswerBody {
            call_id: id.into(),
            sdp: "v=0 fake-answer".into(),
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

        let acts = a.on_answer(B, &answer("c1"), 200);
        assert_eq!(a.state(), State::Active);
        assert!(matches!(
            acts.first(),
            Some(Action::State { renegotiating: false, .. })
        ), "应答到达后必须清掉重协商标记");
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
        let acts = b.on_offer("dh_carol", &offer("c2"), "m2", 110);
        assert_eq!(b.state(), State::Active, "当前通话不能被打断");
        assert_eq!(b.call_id(), "c1", "call_id 不能被覆盖");
        let rej = find_send(&acts, crate::call_kind::REJECT).expect("应回 reject");
        match rej {
            Action::Send { to_dh, body, .. } => {
                assert_eq!(to_dh, "dh_carol");
                assert!(body.contains("\"c2\""), "reject 要针对新来电: {body}");
                assert!(body.contains("busy"));
            }
            _ => unreachable!(),
        }
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
}
