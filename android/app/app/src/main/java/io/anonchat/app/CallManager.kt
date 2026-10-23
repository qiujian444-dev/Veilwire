package io.anonchat.app

import android.content.Context
import android.media.AudioAttributes
import android.media.AudioFocusRequest
import android.media.AudioManager
import android.util.Log
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import org.json.JSONObject
import org.webrtc.AudioSource
import org.webrtc.AudioTrack
import org.webrtc.Camera2Enumerator
import org.webrtc.CameraVideoCapturer
import org.webrtc.DataChannel
import org.webrtc.DefaultVideoDecoderFactory
import org.webrtc.DefaultVideoEncoderFactory
import org.webrtc.EglBase
import org.webrtc.IceCandidate
import org.webrtc.MediaConstraints
import org.webrtc.MediaStream
import org.webrtc.MediaStreamTrack
import org.webrtc.PeerConnection
import org.webrtc.PeerConnectionFactory
import org.webrtc.RtpReceiver
import org.webrtc.RtpTransceiver
import org.webrtc.SdpObserver
import org.webrtc.SessionDescription
import org.webrtc.SurfaceTextureHelper
import org.webrtc.SurfaceViewRenderer
import org.webrtc.VideoSource
import org.webrtc.VideoTrack
import java.util.concurrent.Executors

/**
 * 通话管理器：WebRTC 本体 + 与 Rust 信令状态机的粘合。
 *
 * 职责边界很清楚：
 * - **Rust** 负责"这通电话走到哪一步了"、信令怎么编码、怎么发出去、怎么校验 call_id
 * - **这里** 只负责 PeerConnection 的生命周期、媒体采集、渲染
 *
 * 为什么不把状态机也写在 Kotlin：它要处理乱序、重投、旧信令串台，
 * 这些必须能穷举测试。放在 Rust 里可以跑单元测试，放在这里只能靠手点。
 */
object CallManager {
    private const val TAG = "AnonChat"

    /** 来电时的振铃超时由 Rust 侧控制，这里只负责每秒 tick 一次 */
    private const val TICK_MS = 1_000L

    data class UiState(
        val state: String = "idle",
        val peerName: String = "",
        val peerDh: String = "",
        val video: Boolean = false,
        val reason: String = "",
        /** new / checking / connected / failed / disconnected */
        val iceState: String = "",
        val muted: Boolean = false,
        val speakerOn: Boolean = true,
        val elapsedSecs: Long = 0,
        /// 正在重新协商（换网络后重建媒体通道）。通话本身没断。
        val renegotiating: Boolean = false,
        /** 结构化错误。文案由 CallScreen 按当前语言生成，这里不写死 */
        val err: AppError? = null,
    ) {
        val visible get() = state != "idle"
        val ringing get() = state == "outgoing" || state == "incoming"
    }

    var ui by mutableStateOf(UiState())
        private set

    /** 远端画面交给 UI 层挂载，避免这里持有 View 引用 */
    var remoteRenderer: SurfaceViewRenderer? = null
    var localRenderer: SurfaceViewRenderer? = null

    /** 当前真正绑定了 sink 的渲染器（幂等绑定用，防止同一 renderer 被 addSink 两次花屏/双帧） */
    private var boundRemoteSink: SurfaceViewRenderer? = null
    private var boundLocalSink: SurfaceViewRenderer? = null

    /** 供 UI 初始化 SurfaceViewRenderer */
    val eglContext get() = eglBase?.eglBaseContext

    /** 仅供自媒体自检使用，业务代码不要拿它建连接 */
    internal fun factoryForTest(): PeerConnectionFactory? = factory

    private var appCtx: Context? = null
    private var factory: PeerConnectionFactory? = null
    private var eglBase: EglBase? = null
    private lateinit var executor: java.util.concurrent.ExecutorService

    private var pc: PeerConnection? = null
    private var audioSource: AudioSource? = null
    private var audioTrack: AudioTrack? = null

    /** 通话期间持有的音频焦点。不持有它，其他音乐 App 不会停（见 acquireAudioFocus 注释） */
    private var focusRequest: AudioFocusRequest? = null
    private var videoSource: VideoSource? = null
    private var videoTrack: VideoTrack? = null
    private var capturer: CameraVideoCapturer? = null
    private var surfaceHelper: SurfaceTextureHelper? = null
    private var remoteVideoTrack: VideoTrack? = null

    /**
     * answer/offer 还没设成远端描述时先攒着。
     *
     * 这是 WebRTC 的经典坑：对端的 ICE candidate 完全可能先到，
     * 此时 addIceCandidate 会直接抛 "remote description is null"，
     * 候选丢了就只能等重传，弱网下经常表现成"通了但没声音"。
     */
    private val pendingCandidates = mutableListOf<IceCandidate>()
    private var remoteDescSet = false

    /** 每次通话只写一条记录，避免 dismiss/endCall 各写一次 */
    private var loggedEnd = false

    private var netCallback: android.net.ConnectivityManager.NetworkCallback? = null
    @Volatile private var firstNet = true
    @Volatile private var restartPending = false

    private var ticker: Thread? = null
    @Volatile private var ticking = false
    private var connectedAt = 0L

    // ------------------------------------------------------------ 初始化

    fun init(ctx: Context) {
        if (factory != null) return
        appCtx = ctx.applicationContext
        executor = Executors.newSingleThreadExecutor()

        PeerConnectionFactory.initialize(
            PeerConnectionFactory.InitializationOptions.builder(appCtx)
                .setFieldTrials("")
                .createInitializationOptions()
        )
        eglBase = EglBase.create()
        factory = PeerConnectionFactory.builder()
            .setVideoEncoderFactory(DefaultVideoEncoderFactory(eglBase!!.eglBaseContext, true, true))
            .setVideoDecoderFactory(DefaultVideoDecoderFactory(eglBase!!.eglBaseContext))
            .createPeerConnectionFactory()
    }

    private fun newPeerConnection(): PeerConnection? {
        val f = factory ?: return null
        val cfg = PeerConnection.RTCConfiguration(iceServers(appCtx)).apply {
            sdpSemantics = PeerConnection.SdpSemantics.UNIFIED_PLAN
            bundlePolicy = PeerConnection.BundlePolicy.MAXBUNDLE
            iceTransportsType = PeerConnection.IceTransportsType.ALL
            // 网络切换（WiFi↔流量）时继续收集，否则换网就断
            continualGatheringPolicy = PeerConnection.ContinualGatheringPolicy.GATHER_CONTINUALLY
        }
        return f.createPeerConnection(cfg, observer)
    }

    /**
     * ICE 服务器。
     *
     * STUN 在**同运营商/同局域网**场景下够用；跨网段必须靠 TURN。
     * 默认只给一个国内可达的 STUN，因为匿名软件不该把流量指向境外服务。
     * TURN 由用户/社区自建，填在设置里（中继部署指南里有 coturn 的做法）。
     */
    private fun iceServers(ctx: Context?): List<PeerConnection.IceServer> {
        val prefs = ctx?.getSharedPreferences("anon", Context.MODE_PRIVATE)
        val list = mutableListOf<PeerConnection.IceServer>()

        val stun = prefs?.getString("stun", "")?.trim().orEmpty()
            .ifBlank { "stun:stun.miwifi.com:3478" }
        list.add(PeerConnection.IceServer.builder(stun).createIceServer())

        val turn = prefs?.getString("turn", "")?.trim().orEmpty()
        if (turn.isNotBlank()) {
            val user = prefs?.getString("turn_user", "").orEmpty()
            val pass = prefs?.getString("turn_pass", "").orEmpty()
            // 用户可能填 turn://user:pass@host 或裸 host:port，统一归一成 WebRTC 认的写法
            val b = PeerConnection.IceServer.builder(normalizeTurnUrl(turn))
            if (user.isNotBlank()) b.setUsername(user).setPassword(pass)
            list.add(b.createIceServer())
        }
        return list
    }

    /**
     * 把用户手填的各种 TURN 写法归一成 WebRTC 认识的 `turn:` / `turns:` URL。
     *
     * 认得的写法：`turn:1.2.3.4:3478`、`1.2.3.4:3478`、`turn://user:pass@1.2.3.4:3478`。
     * 凭据一律丢掉——它们走 setUsername/setPassword，混在 URL 里 WebRTC 不认。
     */
    fun normalizeTurnUrl(raw: String): String = TurnLink.normalize(raw)

    /** 从粘贴的一行文本里拆出 TURN 地址和账号密码 */
    fun parseTurnLink(raw: String): TurnLink.Parsed? = TurnLink.parse(raw)

    /** ICE 探测结果。relay > 0 才算 TURN 真的能用 */
    enum class IceErr { NOT_INIT, PC_FAILED }

    data class IceProbe(
        val host: Int,
        val srflx: Int,
        val relay: Int,
        val timedOut: Boolean,
        /**
         * 失败原因用错误码而不是拼好的句子：数据层不该依赖 Context，
         * 文案由 UI 层按当前语言生成（见 MainActivity.probeSummary）。
         */
        val errorKind: IceErr? = null,
        /** 配了 TURN 但没有 relay 候选 = TURN 没通，这和"没配 TURN"是两种问题 */
        val hasTurn: Boolean = false,
    )

    /**
     * 用当前配置真正建一个 PeerConnection，看能收集到哪类 ICE 候选。
     *
     * 为什么不能只 ping 端口：TURN 服务器"端口开着"和"凭据能用、能分配中继地址"
     * 是两件事，前者通后者不通的情况特别多（用户填了错的密码、服务端没开 relay）。
     * 只有真的收到 `typ relay` 候选，才能断言这条配置能打通跨网段通话。
     *
     * 阻塞调用，最长 timeoutMs，请在 IO 线程调。
     */
    fun probeIce(
        stun: String,
        turn: String,
        user: String,
        pass: String,
        timeoutMs: Long = 12_000,
    ): IceProbe {
        val f = factory ?: return IceProbe(0, 0, 0, false, IceErr.NOT_INIT)
        val servers = mutableListOf<PeerConnection.IceServer>()
        servers.add(
            PeerConnection.IceServer.builder(
                stun.trim().ifBlank { "stun:stun.miwifi.com:3478" }
            ).createIceServer()
        )
        if (turn.isNotBlank()) {
            val b = PeerConnection.IceServer.builder(normalizeTurnUrl(turn))
            if (user.isNotBlank()) b.setUsername(user).setPassword(pass)
            servers.add(b.createIceServer())
        }
        val cfg = PeerConnection.RTCConfiguration(servers).apply {
            sdpSemantics = PeerConnection.SdpSemantics.UNIFIED_PLAN
            bundlePolicy = PeerConnection.BundlePolicy.MAXBUNDLE
            iceTransportsType = PeerConnection.IceTransportsType.ALL
        }
        val hostN = java.util.concurrent.atomic.AtomicInteger(0)
        val srflxN = java.util.concurrent.atomic.AtomicInteger(0)
        val relayN = java.util.concurrent.atomic.AtomicInteger(0)
        val finished = java.util.concurrent.CountDownLatch(1)

        val probe = f.createPeerConnection(cfg, object : PeerConnection.Observer {
            override fun onIceCandidate(candidate: IceCandidate) {
                val s = candidate.sdp
                when {
                    s.contains("typ relay") -> relayN.incrementAndGet()
                    s.contains("typ srflx") -> srflxN.incrementAndGet()
                    s.contains("typ host") -> hostN.incrementAndGet()
                }
            }

            override fun onIceCandidatesRemoved(candidates: Array<out IceCandidate>) {}
            override fun onIceConnectionChange(newState: PeerConnection.IceConnectionState) {}
            override fun onIceConnectionReceivingChange(receiving: Boolean) {}
            override fun onIceGatheringChange(newState: PeerConnection.IceGatheringState) {
                if (newState == PeerConnection.IceGatheringState.COMPLETE) finished.countDown()
            }
            override fun onSignalingChange(newState: PeerConnection.SignalingState) {}
            override fun onAddStream(stream: MediaStream) {}
            override fun onRemoveStream(stream: MediaStream) {}
            override fun onDataChannel(channel: DataChannel) {}
            override fun onRenegotiationNeeded() {}
            override fun onAddTrack(receiver: RtpReceiver, mediaStreams: Array<out MediaStream>) {}
            override fun onTrack(transceiver: RtpTransceiver) {}
        }) ?: return IceProbe(0, 0, 0, false, IceErr.PC_FAILED)

        // 必须有媒体或 data channel，否则 WebRTC 懒得去收集候选
        probe.createDataChannel("probe", DataChannel.Init())
        probe.createOffer(
            object : SdpObserver {
                override fun onCreateSuccess(sdp: SessionDescription) {
                    probe.setLocalDescription(object : SdpObserver {
                        override fun onCreateSuccess(p0: SessionDescription?) {}
                        override fun onSetSuccess() {}
                        override fun onCreateFailure(p0: String?) {}
                        override fun onSetFailure(p0: String?) { finished.countDown() }
                    }, sdp)
                }
                override fun onSetSuccess() {}
                override fun onCreateFailure(msg: String?) { finished.countDown() }
                override fun onSetFailure(msg: String?) { finished.countDown() }
            },
            MediaConstraints()
        )
        finished.await(timeoutMs, java.util.concurrent.TimeUnit.MILLISECONDS)
        val timedOut = finished.count > 0
        runCatching { probe.close() }
        runCatching { probe.dispose() }
        return IceProbe(hostN.get(), srflxN.get(), relayN.get(), timedOut, null, turn.isNotBlank())
    }

    // ------------------------------------------------------------ 主叫

    /** 发起通话。调用前必须已拿到麦克风（视频还要相机）权限。 */
    fun startCall(peerName: String, video: Boolean): String? {
        // 返回值只进 logcat（调用方 Log.w），技术信息，不本地化
        val ctx = appCtx ?: return "not initialized"
        resetPeer()

        val (callId, err) = Repo.callStart(peerName, video)
        if (callId == null) return err ?: "start failed"

        val pc = newPeerConnection()
        if (pc == null) {
            Repo.callHangup("internal_error")
            return "create PeerConnection failed"
        }
        this.pc = pc

        if (!attachLocalMedia(pc, video)) {
            Repo.callHangup("no_media")
            resetPeer()
            return "cannot open microphone"
        }

        loggedEnd = false
        connectedAt = 0
        ui = UiState(state = "outgoing", peerName = peerName, video = video)
        startTicker()

        pc.createOffer(sdpObserver(
            onOk = { sdp ->
                pc.setLocalDescription(setObserver("localDescription(offer)"), sdp)
                val r = Repo.callSetOffer(sdp.description)
                if (!r.ok) {
                    ui = ui.copy(err = AppError(ErrKind.CORE_FAILURE, r.error))
                    Log.w(TAG, "send offer failed: ${r.error}")
                }
            },
            onFail = { e -> ui = ui.copy(err = AppError(ErrKind.CALL_OFFER_FAILED, e)) }
        ), offerConstraints(video))

        return null
    }

    // ------------------------------------------------------------ 被叫

    /** 收到一条通话信令，交给 Rust 状态机，再按结果驱动 WebRTC/UI */
    fun onSignal(inc: Incoming) {
        val ctx = appCtx ?: return
        val r = Repo.callInbound(inc)
        val ev = r.data
        if (!r.ok || ev == null) {
            Log.w(TAG, "signal handling failed: ${r.error}")
            return
        }
        Log.i(TAG, "signal ${inc.kind} -> state=${ev.state} call=${ev.callId}")

        // ICE 先处理：它可能是 answer 之前就到的那一批
        ev.iceCandidate?.let { c ->
            if (c.isNotEmpty()) addRemoteCandidate(c, ev.iceMid, ev.iceMline)
        }

        when (ev.state) {
            "incoming" -> {
                loggedEnd = false
                connectedAt = 0
                ui = UiState(
                    state = "incoming",
                    peerName = Repo.nameOfDh(ev.peerDh)
                        ?: (Repo.STRANGER_PREFIX + ev.peerDh.take(8)),
                    peerDh = ev.peerDh,
                    video = ev.video,
                )
                startTicker()
                CallNotifier.showIncoming(appCtx)
                // 振铃由状态机驱动：进 incoming 响，离开（接听/拒接/超时/关闭）必停。
                // 之前没有这一步，来电是"静音来电"——界面弹了但什么都不响。
                Log.i(TAG, "incoming call -> ringing video=${ui.video}")
                CallRinger.start(appCtx)
            }

            "active" -> {
                CallRinger.stop() // 接听瞬间必须停铃
                // 接通才抢焦点：响铃期间先不动用户的音乐，接通瞬间才停
                acquireAudioFocus()
                startNetworkWatch()
                ui = ui.copy(state = "active", peerDh = ev.peerDh, reason = ev.reason)
                // 重协商应答也会走到这里。只有第一次接通才记开始时间，
                // 否则换一次网络通话计时就被清零了。
                if (connectedAt == 0L) connectedAt = System.currentTimeMillis()
                startTicker()
                CallNotifier.cancel(appCtx)

                // 主叫侧：answer 到达（初次或重协商应答）都要设远端描述
                if (inc.kind == "call.answer" && ev.remoteSdp != null) {
                    applyRemoteSdp(ev.remoteSdp)
                }
            }

            "ended" -> {
                endCall(ev.reason, fromPeer = true)
            }
        }

        // 被叫侧收到 offer：建 PeerConnection 并设远端描述（但先不接听）
        if (ev.state == "incoming" && ev.remoteSdp != null) {
            prepareIncoming(ev)
        }

        // 通话中收到重协商 offer（对端换了网络）：不弹界面，直接应答
        if (inc.kind == "call.offer" && ev.state == "active" && ev.renegotiating &&
            ev.remoteSdp != null
        ) {
            onRenegotiateOffer(ev.remoteSdp)
        }
    }

    /** 对端换网络后发来的重协商 offer：设远端 → createAnswer → 回传 */
    private fun onRenegotiateOffer(remoteSdp: String) {
        val pc = pc ?: return
        Log.i(TAG, "renegotiation offer received, rebuilding media")
        ui = ui.copy(renegotiating = true)
        applyRemoteSdp(remoteSdp, SessionDescription.Type.OFFER) {
            pc.createAnswer(sdpObserver(
                onOk = { ans ->
                    pc.setLocalDescription(setObserver("localDescription(re-answer)"), ans)
                    val r = Repo.callAccept(ans.description)
                    ui = ui.copy(renegotiating = false)
                    if (!r.ok) Log.w(TAG, "send renegotiation answer failed: ${r.error}")
                },
                onFail = { e ->
                    ui = ui.copy(renegotiating = false, err = AppError(ErrKind.CALL_RENEGOTIATE_FAILED, "$e"))
                }
            ), offerConstraints(ui.video))
        }
    }

    private fun prepareIncoming(ev: CallEvent) {
        resetPeer()
        val pc = newPeerConnection()
        if (pc == null) {
            Repo.callReject("internal_error")
            return
        }
        this.pc = pc
        // 被叫此时还不能采集（用户没点接听），只准备好接收
        val remote = SessionDescription(SessionDescription.Type.OFFER, ev.remoteSdp!!)
        pc.setRemoteDescription(
            remoteDescObserver(
                onOk = {
                    remoteDescSet = true
                    flushPendingCandidates()
                    Log.i(TAG, "remote offer set (waiting for user to answer)")
                },
                onFail = { ui = ui.copy(err = AppError(ErrKind.CALL_BUILD_FAILED)) },
            ),
            remote
        )
    }

    /** 用户点了接听 */
    fun accept() {
        val c = pc ?: return
        if (!attachLocalMedia(c, ui.video)) {
            ui = ui.copy(err = AppError(ErrKind.CALL_MIC_FAILED))
            return
        }
        c.createAnswer(sdpObserver(
            onOk = { sdp ->
                c.setLocalDescription(setObserver("localDescription(answer)"), sdp)
                val r = Repo.callAccept(sdp.description)
                if (!r.ok) {
                    ui = ui.copy(err = AppError(ErrKind.CORE_FAILURE, r.error))
                } else {
                    /*
                     * 这里是"接听"自己的状态转换，**不经过 onSignal 的 active 分支**
                     * （那条分支是给主叫收到 answer、以及重协商应答用的）。
                     * 之前只写了 ui.copy(state="active")，导致三个必做动作全漏：
                     *  - 铃声不停（0.11.3 的振铃接听后一直响，用户实测到的就是这个）
                     *  - 不抢音频焦点（别的 App 音乐继续放）
                     *  - 不监听网络变化（通话中换网不重协商，变成"双方都在但听不见"）
                     */
                    CallRinger.stop()
                    acquireAudioFocus()
                    startNetworkWatch()
                    ui = ui.copy(state = "active")
                    connectedAt = System.currentTimeMillis()
                }
            },
            onFail = { e -> ui = ui.copy(err = AppError(ErrKind.CALL_ANSWER_FAILED, e)) }
        ), offerConstraints(ui.video))
    }

    fun reject() {
        Repo.callReject("declined")
        endCall("declined", send = false)
    }

    fun hangup() {
        Repo.callHangup("local_ended")
        endCall("local_ended", send = false)
    }

    /** 本地结束，收尾但不重复发信令 */
    fun endCall(reason: String, send: Boolean = false, fromPeer: Boolean = false) {
        if (send) Repo.callHangup(reason)
        // 通话记录：不留痕迹的话，未接来电在界面上等于没发生过
        if (!loggedEnd) {
            loggedEnd = true
            val who = ui.peerName.ifBlank { "未知联系人" }
            // fromPeer 不用进消息体：读取方就是"对方发的记录"，方向由 outgoing 决定
            runCatching { Repo.appendCallLog(who, callLogBody(reason, fromPeer)) }
        }
        CallNotifier.cancel(appCtx)
        CallRinger.stop() // 拒接/超时/对方挂断都会走 endCall，铃声在这里收尾
        ui = ui.copy(state = "ended", reason = reason)
        stopTicker()
        closePeer()
        // 留 1.5 秒给"通话已结束"提示，然后回 idle
        Thread {
            Thread.sleep(1_500)
            Repo.callReset()
            if (ui.state == "ended") ui = UiState()
        }.start()
    }

    /**
     * 通话记录的消息体。
     *
     * 只存结构（原因 + 时长），不存句子：这条文本会持久化进消息历史，
     * 而显示语言由看的人决定。旧版本存的是拼好的中文，Bubble 渲染时
     * 解析不了就原样显示（兼容老数据）。
     */
    private fun callLogBody(reason: String, fromPeer: Boolean): String =
        JSONObject()
            .put("r", reason)
            .put("fp", fromPeer)
            .put("d", if (connectedAt > 0) (System.currentTimeMillis() - connectedAt) / 1000 else 0L)
            .toString()

    /** UI 强制关闭（用户点了返回） */
    fun dismiss() {
        CallRinger.stop()
        if (ui.state == "incoming") reject()
        else if (ui.visible && ui.state != "ended") hangup()
        Repo.callReset()
        ui = UiState()
    }

    // ------------------------------------------------------------ 媒体

    private fun attachLocalMedia(pc: PeerConnection, video: Boolean): Boolean {
        return try {
            val f = factory ?: return false
            val src = f.createAudioSource(MediaConstraints())
            val track = f.createAudioTrack("anonchat-audio", src)
            pc.addTrack(track, listOf("anonchat-stream"))
            audioSource = src
            audioTrack = track

            if (video) {
                val vs = f.createVideoSource(false)
                val vt = f.createVideoTrack("anonchat-video", vs)
                pc.addTrack(vt, listOf("anonchat-stream"))
                videoSource = vs
                videoTrack = vt
                startCapture(vs)
            }
            routeAudio(speaker = true)
            true
        } catch (t: Throwable) {
            Log.w(TAG, "open local media failed: ${t.message}")
            false
        }
    }

    private fun startCapture(vs: VideoSource) {
        val ctx = appCtx ?: return
        val enum = Camera2Enumerator(ctx)
        val name = enum.deviceNames.firstOrNull { enum.isFrontFacing(it) }
            ?: enum.deviceNames.firstOrNull()
            ?: return
        val c = enum.createCapturer(name, null) ?: return
        val helper = SurfaceTextureHelper.create("cap", eglBase?.eglBaseContext)
        // capturerObserver 是 VideoSource 自己的属性，由 initialize 注入，
        // 不要反过来赋值（类型不匹配，而且会破坏 VideoSource 的转发）
        c.initialize(helper, ctx, vs.capturerObserver)
        c.startCapture(640, 480, 30)
        capturer = c
        surfaceHelper = helper
    }

    private fun routeAudio(speaker: Boolean) {
        val am = appCtx?.getSystemService(Context.AUDIO_SERVICE) as? AudioManager ?: return
        am.mode = AudioManager.MODE_IN_COMMUNICATION
        @Suppress("DEPRECATION")
        am.isSpeakerphoneOn = speaker
    }

    /**
     * 申请音频焦点。**通话接通时必须调用**，否则别的 App 的音乐不会停。
     *
     * 之前只设了 MODE_IN_COMMUNICATION + 扬声器，没有申请焦点——Android 的音频焦点
     * 是"协作机制"：不 request，音乐 App 不会（也没有义务）暂停。用户实测"接通后
     * 手机音乐还在放"就是这个原因。
     *
     * GAIN_TRANSIENT：通话是临时的、不是独占媒体通道（结束后音乐应当恢复）。
     * 焦点被抢（比如来了真电话）：把自己麦克风关掉避免串音；拿回来再按用户静音状态恢复。
     */
    private fun acquireAudioFocus() {
        val am = appCtx?.getSystemService(Context.AUDIO_SERVICE) as? AudioManager ?: return
        if (focusRequest != null) return
        val attrs = AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_VOICE_COMMUNICATION)
            .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
            .build()
        val req = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN_TRANSIENT)
            .setAudioAttributes(attrs)
            .setAcceptsDelayedFocusGain(false)
            .setWillPauseWhenDucked(false)
            .setOnAudioFocusChangeListener { change ->
                when (change) {
                    AudioManager.AUDIOFOCUS_LOSS,
                    AudioManager.AUDIOFOCUS_LOSS_TRANSIENT ->
                        runCatching { audioTrack?.setEnabled(false) }
                    AudioManager.AUDIOFOCUS_GAIN ->
                        runCatching { audioTrack?.setEnabled(!ui.muted) }
                }
            }
            .build()
        runCatching { am.requestAudioFocus(req) }
        focusRequest = req
    }

    /** 通话结束释放焦点：音乐 App 才能自动恢复播放 */
    private fun releaseAudioFocus() {
        val am = appCtx?.getSystemService(Context.AUDIO_SERVICE) as? AudioManager
        focusRequest?.let { runCatching { am?.abandonAudioFocusRequest(it) } }
        focusRequest = null
    }

    fun toggleMute() {
        val t = audioTrack ?: return
        val next = !ui.muted
        t.setEnabled(!next)
        ui = ui.copy(muted = next)
    }

    fun toggleSpeaker() {
        val next = !ui.speakerOn
        routeAudio(next)
        ui = ui.copy(speakerOn = next)
    }

    /**
     * 把远端/本地视频轨绑到渲染器上。
     *
     * **不能对 null 调 addSink**：WebRTC 的 `VideoTrack.addSink(null)` 直接抛
     * IllegalArgumentException——视频通话闪退（0.11.1 真机+模拟器双端复现）的根因。
     * UI 挂载顺序是 RemoteVideo 先、LocalVideo 后（接听前没有本地预览），
     * 所以第一次 bindRenderers 会带着 null 的 local 进来，必须跳过等它挂上来。
     * 反向同理：远端轨道可能先于渲染器到达（onTrack 回调），也要能补绑。
     */
    fun bindRenderers(remote: SurfaceViewRenderer?, local: SurfaceViewRenderer?) {
        remoteRenderer = remote
        localRenderer = local
        attachRemote()
        attachLocal()
    }

    /** 轨道和渲染器**都就绪**才绑；重复绑同一 renderer 跳过 */
    private fun attachRemote() {
        val r = remoteRenderer ?: return
        val t = remoteVideoTrack ?: return
        if (boundRemoteSink === r) return
        boundRemoteSink?.let { old -> runCatching { t.removeSink(old) } }
        runCatching { t.addSink(r) }
        boundRemoteSink = r
    }

    private fun attachLocal() {
        val r = localRenderer ?: return
        val t = videoTrack ?: return
        if (boundLocalSink === r) return
        boundLocalSink?.let { old -> runCatching { t.removeSink(old) } }
        runCatching { t.addSink(r) }
        boundLocalSink = r
    }

    // ------------------------------------------------------------ 换网络重协商

    /**
     * 监听默认网络变化。
     *
     * 换网络（WiFi↔流量）之后旧候选全部失效，必须带 IceRestart 重新协商，
     * 否则通话就变成"双方都还在，但谁也听不见"。
     */
    private fun startNetworkWatch() {
        if (netCallback != null) return
        val cm = appCtx?.getSystemService(Context.CONNECTIVITY_SERVICE)
                as? android.net.ConnectivityManager ?: return
        val cb = object : android.net.ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: android.net.Network) {
                // 注册时会立刻回调一次当前网络，这一次要跳过
                if (firstNet) {
                    firstNet = false
                    return
                }
                Log.i(TAG, "default network changed, preparing renegotiation")
                scheduleRestart()
            }

            override fun onLost(network: android.net.Network) {
                // 只丢了旧网络：等 onAvailable 拿到新网络再重建，此刻重启没有意义
            }
        }
        runCatching { cm.registerDefaultNetworkCallback(cb) }
            .onFailure { Log.w(TAG, "register network callback failed: ${it.message}") }
        netCallback = cb
    }

    private fun stopNetworkWatch() {
        val cm = appCtx?.getSystemService(Context.CONNECTIVITY_SERVICE)
                as? android.net.ConnectivityManager
        netCallback?.let { runCatching { cm?.unregisterNetworkCallback(it) } }
        netCallback = null
        firstNet = true
        restartPending = false
    }

    /** 延迟一点再重启：网络切换过程中会连续回调，立刻重启会来回抖 */
    private fun scheduleRestart() {
        if (restartPending || ui.state != "active") return
        restartPending = true
        Thread {
            Thread.sleep(1_500)
            restartPending = false
            if (ui.state == "active") forceReconnect()
        }.apply { isDaemon = true; name = "ice-restart"; start() }
    }

    /**
     * 手动或自动触发 ICE 重启。
     *
     * 对用户也开放（通话界面上的"重连"）：自动检测不一定每次都准
     * （比如同一网络下 NAT 映射过期），留个人工兜底比让用户干等好。
     */
    fun forceReconnect() {
        val pc = pc ?: return
        if (ui.state != "active" || ui.renegotiating) return
        Log.i(TAG, "ICE restart requested")
        ui = ui.copy(renegotiating = true, err = null)
        val c = MediaConstraints().apply {
            mandatory.add(MediaConstraints.KeyValuePair("IceRestart", "true"))
            mandatory.add(MediaConstraints.KeyValuePair("OfferToReceiveAudio", "true"))
            mandatory.add(MediaConstraints.KeyValuePair("OfferToReceiveVideo", ui.video.toString()))
        }
        pc.createOffer(
            sdpObserver(
                onOk = { sdp ->
                    pc.setLocalDescription(setObserver("localDescription(restart)"), sdp)
                    val r = Repo.callRenegotiate(sdp.description)
                    if (!r.ok) {
                        ui = ui.copy(renegotiating = false, err = AppError(ErrKind.CORE_FAILURE, r.error))
                    } else {
                        Log.i(TAG, "renegotiation offer sent")
                    }
                },
                onFail = { e ->
                    ui = ui.copy(renegotiating = false, err = AppError(ErrKind.CALL_RECONNECT_FAILED, "$e"))
                }
            ),
            c
        )
    }

    // ------------------------------------------------------------ 远端描述与候选

    private fun applyRemoteSdp(
        sdp: String,
        type: SessionDescription.Type = SessionDescription.Type.ANSWER,
        onDone: () -> Unit = {},
    ) {
        val pc = pc ?: return
        // 类型由调用点决定，不要靠解析 SDP 猜：unified plan 下 answer 里同样会出现
        // a=sendrecv / m=audio，靠内容判断必然出错。
        pc.setRemoteDescription(
            remoteDescObserver(
                onOk = {
                    remoteDescSet = true
                    flushPendingCandidates()
                    onDone()
                },
                onFail = { e -> ui = ui.copy(err = AppError(ErrKind.CALL_NEGOTIATE_FAILED, e)) },
            ),
            SessionDescription(type, sdp)
        )
    }

    private fun addRemoteCandidate(candidate: String, mid: String, mline: Int) {
        val pc = pc ?: return
        val c = IceCandidate(mid, mline, candidate)
        if (!remoteDescSet) {
            // 远端描述还没设：先缓存，等设好了统一灌进去
            pendingCandidates.add(c)
            return
        }
        runCatching { pc.addIceCandidate(c) }
            .onFailure { Log.w(TAG, "addIceCandidate failed: ${it.message}") }
    }

    private fun flushPendingCandidates() {
        val pc = pc ?: return
        val list = pendingCandidates.toList()
        pendingCandidates.clear()
        list.forEach {
            runCatching { pc.addIceCandidate(it) }
                .onFailure { e -> Log.w(TAG, "flush queued candidates failed: ${e.message}") }
        }
        if (list.isNotEmpty()) Log.i(TAG, "flushed ${list.size} late ICE candidates")
    }

    // ------------------------------------------------------------ PeerConnection 回调

    private val observer = object : PeerConnection.Observer {
        override fun onIceCandidate(candidate: IceCandidate) {
            // 空 candidate 表示收集结束，不需要发
            if (candidate.sdp.isEmpty()) return
            executor.execute {
                val r = Repo.callIce(candidate.sdp, candidate.sdpMid ?: "", candidate.sdpMLineIndex)
                if (!r.ok) Log.w(TAG, "send ICE failed: ${r.error}")
            }
        }

        override fun onIceCandidatesRemoved(candidates: Array<out IceCandidate>) {}

        override fun onIceConnectionChange(newState: PeerConnection.IceConnectionState) {
            Log.i(TAG, "ICE state: $newState")
            ui = ui.copy(iceState = newState.name.lowercase())
            if (newState == PeerConnection.IceConnectionState.FAILED) {
                ui = ui.copy(err = AppError(ErrKind.CALL_NETWORK_DOWN))
            }
        }

        override fun onIceConnectionReceivingChange(receiving: Boolean) {}
        override fun onIceGatheringChange(newState: PeerConnection.IceGatheringState) {}

        override fun onSignalingChange(newState: PeerConnection.SignalingState) {}

        override fun onAddStream(stream: MediaStream) {
            stream.videoTracks.firstOrNull()?.let {
                remoteVideoTrack = it
                attachRemote()
            }
        }

        override fun onRemoveStream(stream: MediaStream) {}

        override fun onDataChannel(channel: DataChannel) {}
        override fun onRenegotiationNeeded() {}

        override fun onAddTrack(receiver: RtpReceiver, mediaStreams: Array<out MediaStream>) {
            val track = receiver.track()
            if (track is VideoTrack) {
                remoteVideoTrack = track
                attachRemote()
            }
        }

        override fun onTrack(transceiver: RtpTransceiver) {
            val track = transceiver.receiver.track()
            if (track is VideoTrack) {
                remoteVideoTrack = track
                attachRemote()
            }
        }
    }

    // ------------------------------------------------------------ 定时器

    private fun startTicker() {
        if (ticking) return
        ticking = true
        ticker = Thread {
            while (ticking) {
                try {
                    Thread.sleep(TICK_MS)
                } catch (_: InterruptedException) {
                    break
                }
                if (!ticking) break
                // 兜底：只要不在"来电响铃中"，铃声就不该还在响。
                // 0.11.6 之前接听走 accept() 绕过了状态机的停铃分支，铃声会一直响到挂断；
                // 这里每秒校正一次，任何新增的状态转换路径漏了停铃都能自愈。
                if (ui.state != "incoming") {
                    if (CallRinger.isRinging) Log.i(TAG, "ticker fallback stop ringing, state=${ui.state}")
                    CallRinger.stop()
                }
                // 振铃超时由 Rust 状态机判断，这里只是驱动
                val r = Repo.callTick()
                val ev = r.data
                if (ev != null && ev.state == "ended") {
                    endCall(ev.reason, send = false)
                    break
                }
                if (ui.state == "active" && connectedAt > 0) {
                    ui = ui.copy(elapsedSecs = (System.currentTimeMillis() - connectedAt) / 1000)
                }
            }
        }.apply { isDaemon = true; name = "call-tick"; start() }
    }

    private fun stopTicker() {
        ticking = false
        ticker?.interrupt()
        ticker = null
    }

    // ------------------------------------------------------------ 清理

    private fun closePeer() {
        runCatching { capturer?.stopCapture() }
        runCatching { capturer?.dispose() }
        runCatching { surfaceHelper?.dispose() }
        runCatching { videoSource?.dispose() }
        runCatching { audioSource?.dispose() }
        runCatching { pc?.close() }
        runCatching { pc?.dispose() }
        runCatching { boundRemoteSink?.let { remoteVideoTrack?.removeSink(it) } }
        runCatching { boundLocalSink?.let { videoTrack?.removeSink(it) } }
        boundRemoteSink = null
        boundLocalSink = null
        pc = null
        capturer = null
        surfaceHelper = null
        videoSource = null
        videoTrack = null
        audioSource = null
        audioTrack = null
        remoteVideoTrack = null
        remoteDescSet = false
        pendingCandidates.clear()
        stopNetworkWatch()
        // 顺序要紧：先放焦点，再改 mode。反过来的话音乐 App 可能收到"通话模式已结束"
        // 却仍被判为失去焦点，恢复播放要等更久。
        releaseAudioFocus()
        (appCtx?.getSystemService(Context.AUDIO_SERVICE) as? AudioManager)?.let {
            it.mode = AudioManager.MODE_NORMAL
        }
    }

    private fun resetPeer() = closePeer()

    // ------------------------------------------------------------ SDP 观测器

    private fun offerConstraints(video: Boolean): MediaConstraints {
        val m = MediaConstraints()
        m.mandatory.add(MediaConstraints.KeyValuePair("OfferToReceiveAudio", "true"))
        m.mandatory.add(MediaConstraints.KeyValuePair("OfferToReceiveVideo", video.toString()))
        return m
    }

    /** 只处理 createXxx 的结果，set 的结果不回调（否则 set 成功会误触发 create 分支） */
    private fun sdpObserver(
        onOk: (SessionDescription) -> Unit = {},
        onFail: (String) -> Unit = {},
    ) = object : SdpObserver {
        override fun onCreateSuccess(sdp: SessionDescription) = onOk(sdp)
        override fun onSetSuccess() {}
        override fun onCreateFailure(error: String) = onFail(error)
        override fun onSetFailure(error: String) = onFail(error)
    }

    /**
     * setRemoteDescription 专用的观察器。
     *
     * 必须用 `onSetSuccess` 而不是 `onCreateSuccess`。
     * 之前这里复用了 createOffer/createAnswer 的观察器（它的 onSetSuccess 是空的），
     * 结果 `remoteDescSet` 永远是 false，迟到到达的 ICE candidate 被无限期缓存、
     * 一个都灌不进去——表现在真机上就是"接通了但两边都没声音"。
     */
    private fun remoteDescObserver(
        onOk: () -> Unit,
        onFail: (String) -> Unit = {},
    ) = object : SdpObserver {
        override fun onCreateSuccess(sdp: SessionDescription) {}
        override fun onSetSuccess() {
            onOk()
        }
        override fun onCreateFailure(error: String) {}
        override fun onSetFailure(error: String) {
            Log.w(TAG, "setRemoteDescription failed: $error")
            onFail(error)
        }
    }

    private fun setObserver(tag: String) = object : SdpObserver {
        override fun onCreateSuccess(sdp: SessionDescription) {}
        override fun onSetSuccess() {
            Log.i(TAG, "$tag set ok")
        }
        override fun onCreateFailure(error: String) {}
        override fun onSetFailure(error: String) {
            Log.w(TAG, "$tag set failed: $error")
        }
    }
}
