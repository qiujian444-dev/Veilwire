//! Android JNI 桥
//!
//! 只做三件事：把 Java 字符串转成 Rust、调用 `client` 的同步 API、把结果包成 JSON 返回。
//! **所有业务逻辑都在 `client` 里**，这里不做判断，避免 Android 和 CLI 行为走偏。
//!
//! 返回约定（统一信封，Kotlin 侧只认这两种形状）：
//!   成功 -> {"ok":true,"data": <任意 JSON，可为 null>}
//!   失败 -> {"ok":false,"error":"给用户看的错误信息"}
//!
//! 为什么不抛 Java 异常：异常跨 JNI 边界容易丢栈，而且 Kotlin 侧处理 JSON 更直接。

use std::panic::{catch_unwind, AssertUnwindSafe};

use jni::objects::{JClass, JString};
use jni::sys::jstring;
use jni::JNIEnv;
use serde_json::{json, Value};

fn get_string(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s)
        .map(|js| js.into())
        .unwrap_or_default()
}

/// 可选字符串：null / 空串都视为 None
fn get_opt(env: &mut JNIEnv, s: &JString) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let v = get_string(env, s);
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

fn reply(env: &mut JNIEnv, v: Value) -> jstring {
    let s = v.to_string();
    match env.new_string(s) {
        Ok(js) => js.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

fn ok(env: &mut JNIEnv, data: Value) -> jstring {
    reply(env, json!({"ok": true, "data": data}))
}

fn err(env: &mut JNIEnv, msg: impl std::fmt::Display) -> jstring {
    reply(env, json!({"ok": false, "error": msg.to_string()}))
}

/// 统一包装：捕获 panic，避免一个 bug 直接把 App 搞崩
fn guard<F>(env: &mut JNIEnv, f: F) -> jstring
where
    F: FnOnce(&mut JNIEnv) -> Result<Value, String>,
{
    match catch_unwind(AssertUnwindSafe(|| f(env))) {
        Ok(Ok(data)) => ok(env, data),
        Ok(Err(e)) => err(env, e),
        Err(_) => err(env, "内部错误（Rust panic），请查看 logcat"),
    }
}

// ---------------------------------------------------------------- 导出函数

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_init(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    name: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let n = get_opt(&mut env, &name);
    guard(&mut env, |_e| {
        client::init(&d, n.as_deref()).map(|id| json!(id)).map_err(|e| format!("{e:#}"))
    })
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_identity(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    guard(&mut env, |_e| {
        client::identity(&d).map(|v| json!(v)).map_err(|e| format!("{e:#}"))
    })
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_invite(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    relay: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let r = get_string(&mut env, &relay);
    guard(&mut env, |_e| {
        client::invite(&d, &r).map(|v| json!(v)).map_err(|e| format!("{e:#}"))
    })
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_ensureQueue(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    relay: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let r = get_string(&mut env, &relay);
    guard(&mut env, |_e| {
        client::ensure_queue(&d, &r)
            .map(|v| json!(v))
            .map_err(|e| format!("{e:#}"))
    })
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_addFromCard(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    name: JString,
    dh: JString,
    pk: JString,
    qid: JString,
    relays_json: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let n = get_string(&mut env, &name);
    let h = get_string(&mut env, &dh);
    let p = get_string(&mut env, &pk);
    let q = get_string(&mut env, &qid);
    let rj = get_string(&mut env, &relays_json);
    guard(&mut env, |_e| {
        let relays: Vec<String> = serde_json::from_str(&rj).unwrap_or_default();
        client::add_from_card(&d, &n, &h, &p, &q, &relays)
            .map(|v| json!(v))
            .map_err(|e| format!("{e:#}"))
    })
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_add(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    code: JString,
    alias: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let c = get_string(&mut env, &code);
    let a = get_opt(&mut env, &alias);
    guard(&mut env, |_e| {
        client::add(&d, &c, a.as_deref())
            .map(|v| json!(v))
            .map_err(|e| format!("{e:#}"))
    })
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_send(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    to: JString,
    text: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let t = get_string(&mut env, &to);
    let x = get_string(&mut env, &text);
    guard(&mut env, |_e| {
        // 返回载荷 ID（rid）：撤回功能靠它引用这条消息
        client::send(&d, &t, &x)
            .map(|rid| json!(rid))
            .map_err(|e| format!("{e:#}"))
    })
}

/// 发送阅后即焚的文字消息。burn_secs 是首次展示后的焚毁倒计时（秒）。
#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_sendBurn(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    to: JString,
    text: JString,
    burn_secs: jni::sys::jlong,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let t = get_string(&mut env, &to);
    let x = get_string(&mut env, &text);
    guard(&mut env, |_e| {
        client::send_burn(&d, &t, &x, burn_secs.max(1) as u64)
            .map(|rid| json!(rid))
            .map_err(|e| format!("{e:#}"))
    })
}

/// 批量发送（文件分块）。bodies_json 是 JSON 字符串数组，如 `["{...}","{...}"]`。
#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_sendBatch(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    to: JString,
    kind: JString,
    bodies_json: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let t = get_string(&mut env, &to);
    let k = get_string(&mut env, &kind);
    let bj = get_string(&mut env, &bodies_json);
    guard(&mut env, |_e| {
        let bodies: Vec<String> = serde_json::from_str(&bj)
            .map_err(|e| format!("bodies 不是合法的 JSON 字符串数组: {e}"))?;
        // 返回每条消息的载荷 ID 数组（与 bodies 一一对应）
        client::send_batch(&d, &t, &k, &bodies)
            .map(|ids| json!(ids))
            .map_err(|e| format!("{e:#}"))
    })
}

/// 盲 ack 清空自己的中继收件队列（紧急清除），返回 ack 掉的条数。
#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_purgeQueue(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    guard(&mut env, |_e| {
        client::purge_queue(&d)
            .map(|n| json!(n))
            .map_err(|e| format!("{e:#}"))
    })
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_recvOnce(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    timeout_secs: jni::sys::jlong,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let t = timeout_secs.max(1) as u64;
    guard(&mut env, |_e| {
        client::recv_once(&d, t)
            .map(|opt| serde_json::to_value(opt).unwrap_or(Value::Null))
            .map_err(|e| format!("{e:#}"))
    })
}

/// 切换中继：更新 self.relays 与所有联系人的 relays，连新中继验证通过才落盘。
#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_setRelay(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    url: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let u = get_string(&mut env, &url);
    guard(&mut env, |_e| {
        client::set_relay(&d, &u)
            .map(|n| json!(n))
            .map_err(|e| format!("{e:#}"))
    })
}

/// 从 Mainline DHT 自动发现中继（BEP5 get_peers）。
///
/// 纯查询、不改任何本地配置：返回 `["ws://1.2.3.4:8443", ...]` 候选列表，
/// 由上层决定是否采用。**必须在后台线程调用**（默认 25 秒量级）。
#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_discoverRelays(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    timeout_secs: jni::sys::jlong,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let t = timeout_secs.max(5) as u64;
    guard(&mut env, |_e| {
        client::discover_relays(&d, t)
            .map(|v| json!(v))
            .map_err(|e| format!("{e:#}"))
    })
}

/// 取 TURN 临时凭据（缓存优先，必要时向中继索取）。
///
/// 返回 `{url,user,pass,ttl,expires_at}` 或 `null`（中继没开 TURN / 网络不通）。
/// 会发起一次网络请求，**必须在后台线程调用**。
#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_turnCreds(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    guard(&mut env, |_e| {
        let v = client::turn_creds(&d);
        Ok(match v {
            Some(c) => serde_json::to_value(c).unwrap_or(Value::Null),
            None => Value::Null,
        })
    })
}

/// 删除联系人：联系人条目 + Olm 会话状态一起清。返回 [是否存在, 清掉的会话数]。
#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_removeContact(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    dh: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let h = get_string(&mut env, &dh);
    guard(&mut env, |_e| {
        client::remove_contact(&d, &h)
            .map(|(existed, dropped)| json!([existed, dropped]))
            .map_err(|e| format!("{e:#}"))
    })
}

/// 设置联系人备注名（空串 = 清除）。返回生效后的备注（null 表示已清除）。
#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_setContactAlias(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    dh: JString,
    alias: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let h = get_string(&mut env, &dh);
    let a = get_string(&mut env, &alias);
    guard(&mut env, |_e| {
        client::set_contact_alias(&d, &h, &a)
            .map(|v| json!(v))
            .map_err(|e| format!("{e:#}"))
    })
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_contacts(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    guard(&mut env, |_e| {
        client::contacts(&d)
            .map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
            .map_err(|e| format!("{e:#}"))
    })
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_selfQueue(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    guard(&mut env, |_e| {
        client::self_queue(&d)
            .map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
            .map_err(|e| format!("{e:#}"))
    })
}

// ---------------------------------------------------------------- 通话
//
// 信令的发送在 Rust 侧完成（它持有会话密钥和中继连接），
// 这里只把"该刷新什么 UI"回给 Kotlin。Kotlin 不接触任何密钥或信令编码。

fn to_value(ev: client::CallEvent) -> Value {
    serde_json::to_value(ev).unwrap_or(Value::Null)
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_callStart(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    to: JString,
    video: jni::sys::jboolean,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let t = get_string(&mut env, &to);
    let v = video != 0;
    guard(&mut env, |_e| {
        client::call_start(&d, &t, v)
            .map(|id| json!(id))
            .map_err(|e| format!("{e:#}"))
    })
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_callSetOffer(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    sdp: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let s = get_string(&mut env, &sdp);
    guard(&mut env, |_e| {
        client::call_set_offer(&d, &s)
            .map(to_value)
            .map_err(|e| format!("{e:#}"))
    })
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_callAccept(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    sdp: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let s = get_string(&mut env, &sdp);
    guard(&mut env, |_e| {
        client::call_accept(&d, &s)
            .map(to_value)
            .map_err(|e| format!("{e:#}"))
    })
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_callReject(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    reason: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let r = get_string(&mut env, &reason);
    guard(&mut env, |_e| {
        client::call_reject(&d, &r)
            .map(to_value)
            .map_err(|e| format!("{e:#}"))
    })
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_callHangup(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    reason: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let r = get_string(&mut env, &reason);
    guard(&mut env, |_e| {
        client::call_hangup(&d, &r)
            .map(to_value)
            .map_err(|e| format!("{e:#}"))
    })
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_callIce(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    candidate: JString,
    mid: JString,
    mline: jni::sys::jint,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let c = get_string(&mut env, &candidate);
    let m = get_string(&mut env, &mid);
    guard(&mut env, |_e| {
        client::call_ice(&d, &c, &m, mline)
            .map(to_value)
            .map_err(|e| format!("{e:#}"))
    })
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_callTick(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    guard(&mut env, |_e| {
        client::call_tick(&d)
            .map(to_value)
            .map_err(|e| format!("{e:#}"))
    })
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_callState(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    guard(&mut env, |_e| Ok(to_value(client::call_state(&d))))
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_callReset(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    guard(&mut env, |_e| {
        client::call_reset(&d);
        Ok(Value::Null)
    })
}

#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_callInbound(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    from_dh: JString,
    kind: JString,
    body: JString,
    msg_id: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let f = get_string(&mut env, &from_dh);
    let k = get_string(&mut env, &kind);
    let b = get_string(&mut env, &body);
    let id = get_string(&mut env, &msg_id);
    guard(&mut env, |_e| {
        client::call_inbound(&d, &f, &k, &b, &id)
            .map(to_value)
            .map_err(|e| format!("{e:#}"))
    })
}

/// 取走 Rust 侧的告警（解密失败、信令发送失败等）。
///
/// Android 上 Rust 的 stderr 会被系统丢弃，logcat 里看不到，
/// 所以必须有一条显式通道把这些信息带到 Kotlin 侧。
#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_takeWarnings(
    mut env: JNIEnv,
    _c: JClass,
) -> jstring {
    guard(&mut env, |_e| {
        Ok(serde_json::to_value(client::take_warnings()).unwrap_or(Value::Null))
    })
}

/// 通话中换网络后发起重协商（ICE restart）。
/// SDP 必须是带 IceRestart 的本地 offer。
#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_callRenegotiate(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    sdp: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let s = get_string(&mut env, &sdp);
    guard(&mut env, |_e| {
        client::call_renegotiate(&d, &s)
            .map(to_value)
            .map_err(|e| format!("{e:#}"))
    })
}

/// 安全码：两个身份公钥 → 60 位数字。用户当面/电话念一遍就知道有没有中间人。
#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_safetyCode(
    mut env: JNIEnv,
    _c: JClass,
    a: JString,
    b: JString,
) -> jstring {
    let x = get_string(&mut env, &a);
    let y = get_string(&mut env, &b);
    guard(&mut env, |_e| {
        client::safety_code(&x, &y)
            .map(|v| json!(v))
            .map_err(|e| format!("{e:#}"))
    })
}

/// 本机昵称（邀请码里会带出去）
#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_selfName(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    guard(&mut env, |_e| {
        client::local_name(&d)
            .map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
            .map_err(|e| format!("{e:#}"))
    })
}

/// 查看恢复短语（12 个词）。UI 侧负责先给出"这就是身份本体"的警告。
#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_backupPhrase(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    guard(&mut env, |_e| {
        client::backup_phrase(&d).map(|v| json!(v)).map_err(|e| format!("{e:#}"))
    })
}

/// 用恢复短语恢复身份
#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_restore(
    mut env: JNIEnv,
    _c: JClass,
    dir: JString,
    phrase: JString,
    name: JString,
) -> jstring {
    let d = get_string(&mut env, &dir);
    let p = get_string(&mut env, &phrase);
    let n = get_opt(&mut env, &name);
    guard(&mut env, |_e| {
        client::restore(&d, &p, n.as_deref())
            .map(|v| json!(v))
            .map_err(|e| format!("{e:#}"))
    })
}

/// 探测中继：WS 握手计时，返回毫秒。用于中继选择页排序。
#[no_mangle]
pub extern "system" fn Java_io_anonchat_core_Native_relayProbe(
    mut env: JNIEnv,
    _c: JClass,
    url: JString,
) -> jstring {
    let u = get_string(&mut env, &url);
    guard(&mut env, |_e| {
        client::relay_probe(&u).map(|v| json!(v)).map_err(|e| format!("{e:#}"))
    })
}
