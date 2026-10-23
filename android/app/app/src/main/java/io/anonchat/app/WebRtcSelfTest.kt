package io.anonchat.app

import android.content.Context
import android.util.Log
import org.webrtc.AudioSource
import org.webrtc.AudioTrack
import org.webrtc.DataChannel
import org.webrtc.IceCandidate
import org.webrtc.MediaConstraints
import org.webrtc.MediaStream
import org.webrtc.PeerConnection
import org.webrtc.RtpReceiver
import org.webrtc.SdpObserver
import org.webrtc.SessionDescription

/**
 * WebRTC 自检：在**一台设备内部**建两条 PeerConnection 互连。
 *
 * 为什么需要它：验证媒体栈能不能用，正常情况下需要两台设备。但"两台设备"这个条件
 * 在自动化里很难稳定满足（要两个模拟器、要能互访）。而自检能覆盖掉绝大部分风险：
 *
 * - libwebrtc 的 .so 能不能加载（加载失败通常表现为 App 启动就崩）
 * - 编解码器有没有注册（音频协商失败会在这里暴露）
 * - UDP 绑定与 host candidate 收集是否正常
 * - DTLS 握手能不能完成（SDP 协商正确性的最终检验）
 *
 * **覆盖不到**：真实网络的 NAT 穿透、两端采集设备差异、实际音质。
 * 这些仍然只能靠两台真机。
 *
 * 只在调试时通过 intent 触发，正常使用不会跑。
 */
object WebRtcSelfTest {
    private const val TAG = "AnonChat"

    fun run(ctx: Context) {
        Thread {
            val ok = try {
                runInner(ctx)
            } catch (t: Throwable) {
                Log.e(TAG, "SELFTEST FAIL 异常: ${t.message}", t)
                false
            }
            Log.i(TAG, if (ok) "SELFTEST PASS" else "SELFTEST FAIL")
        }.apply { name = "rtc-selftest"; start() }
    }

    private fun runInner(ctx: Context): Boolean {
        val factory = PeerConnectionFactoryHolder.factory(ctx) ?: run {
            Log.e(TAG, "SELFTEST FAIL 工厂初始化失败")
            return false
        }
        val cfg = PeerConnection.RTCConfiguration(emptyList()).apply {
            sdpSemantics = PeerConnection.SdpSemantics.UNIFIED_PLAN
        }

        val cands = mutableListOf<IceCandidate>()
        var pc1: PeerConnection? = null
        var pc2: PeerConnection? = null
        var connected = false
        var remote1: SessionDescription? = null
        var remote2: SessionDescription? = null

        // 候选先攒着，两边远端描述都设好之后再互灌，避免顺序问题
        val observer = object : PeerConnection.Observer {
            override fun onIceCandidate(c: IceCandidate) {
                synchronized(cands) { cands.add(c) }
            }
            override fun onIceCandidatesRemoved(c: Array<out IceCandidate>) {}
            override fun onIceConnectionChange(s: PeerConnection.IceConnectionState) {
                Log.i(TAG, "SELFTEST ICE -> $s")
                if (s == PeerConnection.IceConnectionState.CONNECTED) connected = true
            }
            override fun onIceConnectionReceivingChange(r: Boolean) {}
            override fun onIceGatheringChange(s: PeerConnection.IceGatheringState) {}
            override fun onSignalingChange(s: PeerConnection.SignalingState) {}
            override fun onAddStream(stream: MediaStream) {}
            override fun onRemoveStream(stream: MediaStream) {}
            override fun onDataChannel(channel: DataChannel) {}
            override fun onRenegotiationNeeded() {}
            override fun onAddTrack(r: RtpReceiver, m: Array<out MediaStream>) {}
        }

        pc1 = factory.createPeerConnection(cfg, observer)
        pc2 = factory.createPeerConnection(cfg, observer)
        if (pc1 == null || pc2 == null) {
            Log.e(TAG, "SELFTEST FAIL 建 PeerConnection 失败")
            return false
        }

        // 两端各自加一条音频轨：有真实媒体流，ICE 才会真正进入 connected
        val src1 = factory.createAudioSource(MediaConstraints())
        val t1 = factory.createAudioTrack("st-a", src1)
        pc1.addTrack(t1, listOf("stream"))
        val src2 = factory.createAudioSource(MediaConstraints())
        val t2 = factory.createAudioTrack("st-b", src2)
        pc2.addTrack(t2, listOf("stream"))

        val constraints = MediaConstraints().apply {
            mandatory.add(MediaConstraints.KeyValuePair("OfferToReceiveAudio", "true"))
            mandatory.add(MediaConstraints.KeyValuePair("OfferToReceiveVideo", "false"))
        }

        // pc1 createOffer
        val offer = syncSdp { cb -> pc1.createOffer(cb, constraints) } ?: return false
        setLocal(pc1, offer)
        setRemote(pc2, offer)
        remote2 = offer

        // pc2 createAnswer
        val answer = syncSdp { cb -> pc2.createAnswer(cb, constraints) } ?: return false
        setLocal(pc2, answer)
        setRemote(pc1, answer)
        remote1 = answer

        // 互灌候选
        Thread.sleep(1500)
        synchronized(cands) {
            Log.i(TAG, "SELFTEST 收集到 ${cands.size} 个候选")
            cands.forEach { c ->
                runCatching { pc1.addIceCandidate(c) }
                runCatching { pc2.addIceCandidate(c) }
            }
        }

        // 等连接建立
        val deadline = System.currentTimeMillis() + 20_000
        while (System.currentTimeMillis() < deadline && !connected) {
            Thread.sleep(250)
        }

        // 再灌一次：ContinualGathering 可能在这之后才出候选
        synchronized(cands) {
            cands.forEach { c ->
                runCatching { pc1.addIceCandidate(c) }
                runCatching { pc2.addIceCandidate(c) }
            }
        }
        Thread.sleep(1000)

        runCatching { src1.dispose(); src2.dispose() }
        runCatching { pc1.close(); pc1.dispose() }
        runCatching { pc2.close(); pc2.dispose() }
        return connected
    }

    private fun setLocal(pc: PeerConnection, sdp: SessionDescription) {
        val done = java.util.concurrent.CountDownLatch(1)
        pc.setLocalDescription(object : SdpObserver {
            override fun onCreateSuccess(s: SessionDescription) {}
            override fun onSetSuccess() = done.countDown()
            override fun onCreateFailure(e: String) = done.countDown()
            override fun onSetFailure(e: String) {
                Log.w(TAG, "SELFTEST setLocal 失败: $e"); done.countDown()
            }
        }, sdp)
        done.await(5, java.util.concurrent.TimeUnit.SECONDS)
    }

    private fun setRemote(pc: PeerConnection, sdp: SessionDescription) {
        val done = java.util.concurrent.CountDownLatch(1)
        pc.setRemoteDescription(object : SdpObserver {
            override fun onCreateSuccess(s: SessionDescription) {}
            override fun onSetSuccess() = done.countDown()
            override fun onCreateFailure(e: String) = done.countDown()
            override fun onSetFailure(e: String) {
                Log.w(TAG, "SELFTEST setRemote 失败: $e"); done.countDown()
            }
        }, sdp)
        done.await(5, java.util.concurrent.TimeUnit.SECONDS)
    }

    /** WebRTC 的回调是异步的，这里用 latch 把它同步化，只为自检方便 */
    private fun syncSdp(call: (SdpObserver) -> Unit): SessionDescription? {
        val done = java.util.concurrent.CountDownLatch(1)
        var result: SessionDescription? = null
        call(object : SdpObserver {
            override fun onCreateSuccess(s: SessionDescription) {
                result = s; done.countDown()
            }
            override fun onSetSuccess() {}
            override fun onCreateFailure(e: String) {
                Log.w(TAG, "SELFTEST createSdp 失败: $e"); done.countDown()
            }
            override fun onSetFailure(e: String) {}
        })
        done.await(8, java.util.concurrent.TimeUnit.SECONDS)
        return result
    }
}

/** 复用 CallManager 已经建好的工厂，避免重复初始化 libwebrtc */
object PeerConnectionFactoryHolder {
    fun factory(ctx: Context): org.webrtc.PeerConnectionFactory? {
        CallManager.init(ctx)
        return CallManager.factoryForTest()
    }
}
