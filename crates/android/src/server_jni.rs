//! JNI bridge of the phone server (`io.github.pablosoage.glidedesk.server.NativeServer`):
//! start and stop it, hand it each MediaCodec output buffer, and read back what to do
//! ([`crate::server::Event`], as seven ints).

use std::sync::Mutex;
use std::time::Duration;

use jni::JNIEnv;
use jni::objects::{JByteBuffer, JClass, JIntArray, JString};
use jni::sys::{jboolean, jint, jlong, jstring};

use crate::server::{Config, Server};

/// Why the last `nativeStart` failed.
static START_ERROR: Mutex<String> = Mutex::new(String::new());

fn server<'a>(handle: jlong) -> Option<&'a Server> {
    // SAFETY: handles come from nativeStart and stay valid until nativeStop, after which
    // the Kotlin side does not use them (it joins its threads first).
    (handle != 0).then(|| unsafe { &*(handle as *const Server) })
}

fn string(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(Into::into).unwrap_or_default()
}

fn java_string(env: &JNIEnv, s: &str) -> jstring {
    env.new_string(s)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_github_pablosoage_glidedesk_server_NativeServer_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    port: jint,
    user: JString,
    password: JString,
    state_dir: JString,
    hevc: jboolean,
    own_display: jboolean,
    max_size: jint,
    max_bitrate: jint,
    display_width: jint,
    display_height: jint,
) -> jlong {
    let config = Config {
        port: port.clamp(1, 65535) as u16,
        user: string(&mut env, &user),
        password: string(&mut env, &password),
        state_dir: string(&mut env, &state_dir).into(),
        hevc: hevc != 0,
        own_display: own_display != 0,
        max_size: max_size.clamp(320, 8192) as u32,
        max_bitrate: max_bitrate.max(1) as u32,
        display: (display_width.max(1) as u32, display_height.max(1) as u32),
    };
    match Server::start(config) {
        Ok(s) => Box::into_raw(Box::new(s)) as jlong,
        Err(e) => {
            *START_ERROR.lock().unwrap() = format!("{e:#}");
            0
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_pablosoage_glidedesk_server_NativeServer_nativeStartError(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let msg = START_ERROR.lock().unwrap().clone();
    java_string(&env, &msg)
}

/// One MediaCodec output buffer: `size` bytes at `offset` of the direct `buffer`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_github_pablosoage_glidedesk_server_NativeServer_nativeFrame(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
    buffer: JByteBuffer,
    offset: jint,
    size: jint,
    flags: jint,
    width: jint,
    height: jint,
) {
    let Some(s) = server(handle) else { return };
    let (Ok(ptr), Ok(capacity)) = (
        env.get_direct_buffer_address(&buffer),
        env.get_direct_buffer_capacity(&buffer),
    ) else {
        return;
    };
    let (Ok(offset), Ok(size)) = (usize::try_from(offset), usize::try_from(size)) else {
        return;
    };
    if offset.checked_add(size).is_none_or(|end| end > capacity) {
        return;
    }
    // SAFETY: the range lies within the direct buffer (checked above), which the codec
    // keeps valid until Kotlin releases it after this call returns.
    let data = unsafe { std::slice::from_raw_parts(ptr.add(offset), size) };
    s.frame(data, flags, width.max(1) as u32, height.max(1) as u32);
}

/// The phone's screen size changed (rotation).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_pablosoage_glidedesk_server_NativeServer_nativeDisplay(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    width: jint,
    height: jint,
) {
    if let Some(s) = server(handle) {
        s.display(width.max(1) as u32, height.max(1) as u32);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_pablosoage_glidedesk_server_NativeServer_nativeEncoderFailed(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if let Some(s) = server(handle) {
        s.encoder_failed();
    }
}

/// Waits up to `timeout_ms` for the next event and writes it to `out` (seven ints); false
/// when there was none.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_pablosoage_glidedesk_server_NativeServer_nativeNextEvent(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
    out: JIntArray,
    timeout_ms: jint,
) -> jboolean {
    let Some(s) = server(handle) else { return 0 };
    let timeout = Duration::from_millis(timeout_ms.max(0) as u64);
    let Some(ev) = s.next_event(timeout) else {
        return 0;
    };
    jboolean::from(env.set_int_array_region(&out, 0, &ev.to_ints()).is_ok())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_pablosoage_glidedesk_server_NativeServer_nativeStatus(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    let status = server(handle).map(Server::status).unwrap_or_default();
    java_string(&env, &status)
}

/// The certificate's SHA-256 fingerprint in hex, as clients print it.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_pablosoage_glidedesk_server_NativeServer_nativeFingerprint(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    let fp = server(handle)
        .map(|s| glidedesk_client::trust::hex(&s.fingerprint()))
        .unwrap_or_default();
    java_string(&env, &fp)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_pablosoage_glidedesk_server_NativeServer_nativeStop(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: the handle came from nativeStart and is not used again by the Kotlin side.
    drop(unsafe { Box::from_raw(handle as *mut Server) });
}
