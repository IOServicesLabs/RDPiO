//! rdpio for Android: the Hyper Accelerator client's core, called from the
//! app (`io.ioservices.rdpio.Native`) over JNI.
//!
//! The app owns the screen and the input devices; this library owns the
//! network, the decoders (they render straight onto the app's Surface) and
//! the sound. A Hyper session and a Windows 365 one can be open together; the
//! app shows one (the current) and switches between them at once.

mod audio;
mod autores;
mod backlog;
mod capture;
mod client;
mod decoder;
mod held;
mod hint;
mod keys;
mod lan;
mod notify;
mod rdp;
#[allow(dead_code)]
mod reliable;
mod rx;
mod tune;

// Windows 365: the desktop client's RDP/W365 stack (crates/rdp-client/src),
// compiled in unchanged, beside Android's versions of its Windows-only parts
// (src/rdp/: the socket wait, the camera, the UDP side-band and allocator
// stats).
#[path = "../../../crates/rdp-client/src/arm_broker.rs"]
#[allow(dead_code)]
mod arm_broker;
#[path = "../../../crates/rdp-client/src/congestion.rs"]
#[allow(dead_code)]
mod congestion;
#[path = "../../../crates/rdp-client/src/connect.rs"]
#[allow(dead_code)]
mod connect;
#[path = "../../../crates/rdp-client/src/feed.rs"]
#[allow(dead_code)]
mod feed;
#[path = "../../../crates/rdp-client/src/gateway.rs"]
#[allow(dead_code)]
mod gateway;
#[path = "../../../crates/rdp-client/src/metrics.rs"]
#[allow(dead_code)]
mod metrics;
#[path = "../../../crates/rdp-client/src/rdstls_auth.rs"]
#[allow(dead_code)]
mod rdstls_auth;
#[path = "../../../crates/rdp-client/src/rdstls_v3.rs"]
#[allow(dead_code)]
mod rdstls_v3;
#[path = "../../../crates/rdp-client/src/rendezvous.rs"]
#[allow(dead_code)]
mod rendezvous;
#[path = "../../../crates/rdp-client/src/reverse_connect.rs"]
#[allow(dead_code)]
mod reverse_connect;
#[path = "../../../crates/rdp-client/src/rng.rs"]
#[allow(dead_code)]
mod rng;
#[path = "../../../crates/rdp-client/src/session.rs"]
#[allow(dead_code)]
mod session;
#[path = "../../../crates/rdp-client/src/stun.rs"]
#[allow(dead_code)]
mod stun;
#[path = "../../../crates/rdp-client/src/timezone.rs"]
#[allow(dead_code)]
mod timezone;
#[path = "../../../crates/rdp-client/src/transport.rs"]
#[allow(dead_code)]
mod transport;
#[path = "../../../crates/rdp-client/src/w365.rs"]
#[allow(dead_code)]
mod w365;
#[path = "../../../crates/rdp-client/src/websocket.rs"]
#[allow(dead_code)]
mod websocket;
#[path = "../../../crates/rdp-client/src/tls_rustls.rs"]
#[allow(dead_code)]
mod tls;
#[path = "rdp/allocator.rs"]
mod allocator;
#[path = "rdp/mf_camera.rs"]
#[allow(dead_code)]
mod mf_camera;
#[path = "rdp/net_wait.rs"]
#[allow(dead_code)]
mod net_wait;
#[path = "rdp/udp.rs"]
#[allow(dead_code)]
mod udp;

use std::sync::{Arc, Mutex};

use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::sys::{jboolean, jbyteArray, jint, jstring, JNI_TRUE};
use jni::JNIEnv;
use rdp_hyper::input::InputEvent;

use client::Client;
use rdp::driver::RdpSession;

/// An open session, of either kind.
#[derive(Clone)]
enum Session {
    Hyper(Arc<Client>),
    Rdp(Arc<RdpSession>),
}

impl Session {
    fn close(&self) {
        match self {
            Session::Hyper(c) => {
                // The decoder lets go of the Surface first (the app destroys it next).
                c.set_surface(None);
                c.close();
            }
            Session::Rdp(r) => {
                r.set_surface(None);
                r.close();
            }
        }
    }

    fn set_surface(&self, w: Option<ndk::native_window::NativeWindow>) {
        match self {
            Session::Hyper(c) => c.set_surface(w),
            Session::Rdp(r) => r.set_surface(w),
        }
    }

    fn set_paused(&self, paused: bool) {
        match self {
            Session::Hyper(c) => c.set_paused(paused),
            Session::Rdp(r) => r.set_paused(paused),
        }
    }

    fn input(&self, events: impl IntoIterator<Item = InputEvent>) {
        match self {
            Session::Hyper(c) => c.input(events),
            Session::Rdp(r) => r.input(events),
        }
    }

    fn release_all(&self) {
        match self {
            Session::Hyper(c) => c.release_all(),
            Session::Rdp(r) => r.release_all(),
        }
    }

    fn status(&self) -> String {
        match self {
            Session::Hyper(c) => c.status(),
            Session::Rdp(r) => r.status(),
        }
    }
}

/// The open sessions (at most one of each kind) and which one the app shows.
struct Sessions {
    hyper: Option<Arc<Client>>,
    rdp: Option<Arc<RdpSession>>,
    rdp_current: bool,
}

static SESSIONS: Mutex<Sessions> = Mutex::new(Sessions { hyper: None, rdp: None, rdp_current: false });

fn current_of(s: &Sessions) -> Option<Session> {
    if s.rdp_current {
        s.rdp.clone().map(Session::Rdp)
    } else {
        s.hyper.clone().map(Session::Hyper)
    }
}

fn current() -> Option<Session> {
    current_of(&SESSIONS.lock().unwrap())
}

fn client() -> Option<Arc<Client>> {
    match current() {
        Some(Session::Hyper(c)) => Some(c),
        _ => None,
    }
}

/// Lower this thread's nice value (decode, network and audio threads run
/// ahead of the app's other work, as the desktop client's do).
pub(crate) fn raise_thread(nice: i32) {
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, nice);
    }
}

fn string(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(Into::into).unwrap_or_default()
}

fn jstr(env: &mut JNIEnv, s: &str) -> jstring {
    env.new_string(s).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_init(_env: JNIEnv, _class: JClass) {
    android_logger::init_once(android_logger::Config::default().with_tag("rdpio").with_max_level(log::LevelFilter::Info));
}

/// The codecs this device decodes in hardware, best first ("hevc,h264").
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_codecs(mut env: JNIEnv, _class: JClass) -> jstring {
    jstr(&mut env, &decoder::device_codecs().join(","))
}

/// Sign in (blocking; call off the UI thread). "" when it worked, else why not.
#[allow(clippy::too_many_arguments)]
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_connect(
    mut env: JNIEnv,
    _class: JClass,
    host: JString,
    port: jint,
    password: JString,
    width: jint,
    height: jint,
    refresh_hz: jint,
    scale: jint,
    office: jboolean,
    codec: JString,
) -> jstring {
    let (host, password, codec) = (string(&mut env, &host), string(&mut env, &password), string(&mut env, &codec));
    let old = SESSIONS.lock().unwrap().hyper.take();
    if let Some(old) = old {
        Session::Hyper(old).close();
    }
    let codecs = match codec.as_str() {
        "" | "auto" => decoder::device_codecs(),
        c => vec![c.to_string()],
    };
    let want = rdp_hyper::auth::Want {
        codecs: if codecs.is_empty() { vec!["h264".into()] } else { codecs },
        width: width.max(320) as u32,
        height: height.max(200) as u32,
        refresh_hz: refresh_hz.clamp(30, 240) as u32,
        monitors: 1,
        // The host scales its desktop to this screen (never up).
        fit: true,
        office: office == JNI_TRUE,
        fast_text: true,
        cookie: None,
        scale: scale.max(0) as u32,
        app: client::APP_VERSION.into(),
        build: format!("android+{}", client::APP_VERSION),
        protocols: (rdp_hyper::wire::MIN_VERSION, rdp_hyper::wire::VERSION),
    };
    match Client::connect(&host, port.clamp(1, 65535) as u16, &password, want) {
        Ok(c) => {
            let paused = {
                let mut s = SESSIONS.lock().unwrap();
                s.hyper = Some(c);
                std::mem::replace(&mut s.rdp_current, false).then(|| s.rdp.clone()).flatten()
            };
            if let Some(r) = paused {
                // (Kept open, unseen: switch back to it at once.)
                Session::Rdp(r).set_paused(true);
            }
            jstr(&mut env, "")
        }
        Err(e) => {
            log::warn!("sign-in failed: {e}");
            jstr(&mut env, &e)
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_disconnect(_env: JNIEnv, _class: JClass) {
    // The one shown ends; the other (if any) becomes current.
    let ended = {
        let mut s = SESSIONS.lock().unwrap();
        let ended = if s.rdp_current { s.rdp.take().map(Session::Rdp) } else { s.hyper.take().map(Session::Hyper) };
        s.rdp_current = s.rdp.is_some();
        ended
    };
    if let Some(e) = ended {
        e.close();
    }
}

/// The Surface to show the stream on, or null when it's destroyed (returns
/// once the decoder stopped using it).
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_setSurface(env: JNIEnv, _class: JClass, surface: JObject) {
    let Some(c) = current() else { return };
    let window = if surface.is_null() {
        None
    } else {
        unsafe { ndk::native_window::NativeWindow::from_surface(env.get_raw() as *mut _, surface.as_raw() as *mut _) }
    };
    c.set_surface(window);
}

#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_setPaused(_env: JNIEnv, _class: JClass, paused: jboolean) {
    if let Some(c) = current() {
        c.set_paused(paused == JNI_TRUE);
        if paused == JNI_TRUE {
            c.release_all();
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_setOffice(_env: JNIEnv, _class: JClass, office: jboolean) {
    if let Some(c) = client() {
        c.set_office(office == JNI_TRUE);
    }
}

#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_office(_env: JNIEnv, _class: JClass) -> jboolean {
    client().is_some_and(|c| c.office()) as jboolean
}

#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_setView(_env: JNIEnv, _class: JClass, width: jint, height: jint) {
    match current() {
        Some(Session::Hyper(c)) => c.set_view(width.max(0) as u32, height.max(0) as u32),
        Some(Session::Rdp(r)) => r.set_view(width.max(0) as u32, height.max(0) as u32),
        None => {}
    }
}

/// Automatic resolution: the scale (% of the screen) for the stream, given
/// the one in use (`current`, unchanged when nothing says to move) and the mode.
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_autoScale(_env: JNIEnv, _class: JClass, current: jint, office: jboolean) -> jint {
    let current = current.max(0) as u32;
    match client() {
        Some(c) => c.auto_scale(current, office == JNI_TRUE) as jint,
        None => current as jint,
    }
}

/// The app's files folder (where the PCs' local addresses are kept).
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_setDataDir(mut env: JNIEnv, _class: JClass, dir: JString) {
    lan::set_dir(string(&mut env, &dir).into());
}

/// A hidden on/off for measuring one improvement (`debug.rdpio.<name>`).
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_ab(mut env: JNIEnv, _class: JClass, name: JString, default: jboolean) -> jboolean {
    let name = string(&mut env, &name);
    tune::ab(&name, default != 0) as jboolean
}

/// The eventfd signalled when the status has news (the pointer, the clipboard,
/// the state, the size): the app's UI thread watches it. -1 where there's none.
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_notifyFd(_env: JNIEnv, _class: JClass) -> jint {
    notify::fd()
}

/// Windows' display scale (%) for Windows 365: the next connection's, and the
/// open one's at once.
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_setDesktopScale(_env: JNIEnv, _class: JClass, percent: jint) {
    let percent = percent.max(0) as u32;
    let rdp = SESSIONS.lock().unwrap().rdp.clone();
    match rdp {
        Some(r) => r.set_scale(percent),
        None => crate::session::DESKTOP_SCALE.store(percent, std::sync::atomic::Ordering::Relaxed),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_setRefresh(_env: JNIEnv, _class: JClass, hz: jint) {
    if let Some(c) = client() {
        c.set_refresh(hz.max(0) as u32);
    }
}

/// A hardware key. False when it has no PC scancode (the app may handle it).
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_key(_env: JNIEnv, _class: JClass, scan_code: jint, key_code: jint, down: jboolean) -> jboolean {
    let Some((scancode, extended)) = keys::to_set1(scan_code.max(0) as u32, key_code.max(0) as u32) else {
        return 0;
    };
    if let Some(c) = current() {
        c.input([InputEvent::Key { scancode, extended, down: down == JNI_TRUE }]);
    }
    1
}

/// Typed text (the on-screen keyboard): each character pressed and released.
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_text(mut env: JNIEnv, _class: JClass, text: JString) {
    let text = string(&mut env, &text);
    if let Some(c) = current() {
        c.input(text.encode_utf16().flat_map(|code| [InputEvent::Char { code, down: true }, InputEvent::Char { code, down: false }]));
    }
}

/// The pointer at `x`, `y` (0..=65535 across the picture).
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_mouseAbs(_env: JNIEnv, _class: JClass, x: jint, y: jint) {
    if let Some(c) = current() {
        c.input([InputEvent::MouseAbs { x: x.clamp(0, 65535) as u16, y: y.clamp(0, 65535) as u16 }]);
    }
}

#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_mouseRel(_env: JNIEnv, _class: JClass, dx: jint, dy: jint) {
    if let Some(c) = current() {
        c.input([InputEvent::MouseRel { dx: dx.clamp(-32768, 32767) as i16, dy: dy.clamp(-32768, 32767) as i16 }]);
    }
}

/// 0 left, 1 right, 2 middle, 3 back, 4 forward.
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_button(_env: JNIEnv, _class: JClass, button: jint, down: jboolean) {
    if let Some(c) = current() {
        c.input([InputEvent::Button { button: button.clamp(0, 4) as u8, down: down == JNI_TRUE }]);
    }
}

/// Wheel in WHEEL_DELTA units (120 a notch).
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_wheel(_env: JNIEnv, _class: JClass, delta: jint, horizontal: jboolean) {
    if let Some(c) = current() {
        c.input([InputEvent::Wheel { delta: delta.clamp(-32768, 32767) as i16, horizontal: horizontal == JNI_TRUE }]);
    }
}

/// A finger on the screen, as Windows touch on the host (`phase`: 0 down,
/// 1 move, 2 up, 3 cancel; position 0..=65535 across the picture). Dropped
/// for a host that doesn't take touch (the status says which).
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_touch(_env: JNIEnv, _class: JClass, id: jint, phase: jint, x: jint, y: jint) {
    use rdp_hyper::input::TouchPhase;
    let phase = match phase {
        0 => TouchPhase::Down,
        1 => TouchPhase::Move,
        2 => TouchPhase::Up,
        _ => TouchPhase::Cancel,
    };
    if let Some(c) = current() {
        c.input([InputEvent::Touch { id: id.clamp(0, 255) as u8, phase, x: x.clamp(0, 65535) as u16, y: y.clamp(0, 65535) as u16 }]);
    }
}

#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_ctrlAltDel(_env: JNIEnv, _class: JClass) {
    if let Some(c) = current() {
        c.input([InputEvent::SecureAttention]);
    }
}

#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_releaseAll(_env: JNIEnv, _class: JClass) {
    if let Some(c) = current() {
        c.release_all();
    }
}

/// `state|width|height|cursor gen|clipboard gen|touch` + "\n" + the stats.
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_status(mut env: JNIEnv, _class: JClass) -> jstring {
    let s = current().map_or_else(|| "ended: not connected|0|0|0|0|0\n".to_string(), |c| c.status());
    jstr(&mut env, &s)
}

/// The host's pointer: width, height, hot x, hot y (u16 LE each), then RGBA
/// rows top-down. Null while it's hidden; empty when it's shown but no shape
/// came yet.
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_cursor(env: JNIEnv, _class: JClass) -> jbyteArray {
    let pack = |cur: Option<&client::Cursor>| {
        let mut out = Vec::new();
        if let Some(cur) = cur {
            for v in [cur.width, cur.height, cur.hot_x, cur.hot_y] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.extend_from_slice(&cur.rgba);
        }
        out
    };
    let out = match current() {
        Some(Session::Hyper(c)) if c.cursor_visible.load(std::sync::atomic::Ordering::Relaxed) => pack(c.cursor.lock().unwrap().as_ref()),
        Some(Session::Rdp(r)) if r.cursor_visible() => r.with_cursor(pack),
        _ => return std::ptr::null_mut(),
    };
    env.byte_array_from_slice(&out).map(|a| a.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// Text the host put on its clipboard (taken: null until it changes again).
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_clipboard(mut env: JNIEnv, _class: JClass) -> jstring {
    let text = match current() {
        Some(Session::Hyper(c)) => c.clipboard.lock().unwrap().take(),
        Some(Session::Rdp(r)) => r.take_clipboard(),
        None => None,
    };
    match text {
        Some(t) => jstr(&mut env, &t),
        None => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_sendClipboard(mut env: JNIEnv, _class: JClass, text: JString) {
    let text = string(&mut env, &text);
    match current() {
        Some(Session::Hyper(c)) => c.send_clipboard(text),
        Some(Session::Rdp(r)) => r.send_clipboard(text),
        None => {}
    }
}

/// A notice from the host to show for a few seconds (taken).
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_notice(mut env: JNIEnv, _class: JClass) -> jstring {
    match client().and_then(|c| c.notice.lock().unwrap().take()) {
        Some(t) => jstr(&mut env, &t),
        None => std::ptr::null_mut(),
    }
}

/// What the user allowed (the app asks when a session first wants one).
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_setPermissions(_env: JNIEnv, _class: JClass, mic: jboolean, camera: jboolean) {
    capture::set_permissions(mic == JNI_TRUE, camera == JNI_TRUE);
}

/// How many cameras this tablet has (a host offers apps its camera then).
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_setCameraCount(_env: JNIEnv, _class: JClass, count: jint) {
    capture::CAMERA_COUNT.store(count.max(0) as u32, std::sync::atomic::Ordering::Relaxed);
}

/// A session wants the microphone and the permission isn't granted yet.
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_micWanted(_env: JNIEnv, _class: JClass) -> jboolean {
    capture::MIC_WANTED.load(std::sync::atomic::Ordering::Relaxed) as jboolean
}

/// `width,height,fps,bitrate,generation,keyframes` while a session wants
/// the camera, else "".
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_cameraWanted(mut env: JNIEnv, _class: JClass) -> jstring {
    jstr(&mut env, &capture::camera_wanted())
}

/// One encoded camera frame: `size` bytes at `offset` in the encoder's
/// (direct) output buffer.
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_cameraFrame(
    env: JNIEnv,
    _class: JClass,
    buffer: jni::objects::JByteBuffer,
    offset: jint,
    size: jint,
    keyframe: jboolean,
) {
    let (Ok(ptr), Ok(cap)) = (env.get_direct_buffer_address(&buffer), env.get_direct_buffer_capacity(&buffer)) else { return };
    let (offset, size) = (offset.max(0) as usize, size.max(0) as usize);
    if ptr.is_null() || offset + size > cap {
        return;
    }
    let data = unsafe { std::slice::from_raw_parts(ptr.add(offset), size) };
    capture::camera_frame(data, keyframe == JNI_TRUE);
}

// ---- Windows 365 ----

/// The Microsoft sign-in page to open, and the address it ends at (the app
/// takes the `code` from it): "url\nredirect".
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_w365SignInUrl(mut env: JNIEnv, _class: JClass) -> jstring {
    jstr(&mut env, &format!("{}\n{}", rdp::cloud::authorize_url(), rdp::cloud::redirect_prefix()))
}

/// Finish signing in with the redirect's code, or sign in again with a kept
/// refresh token (`refresh` true). Blocking. "ok\nuser\nrefresh token", or
/// "error\nwhy".
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_w365SignIn(mut env: JNIEnv, _class: JClass, code: JString, refresh: jboolean) -> jstring {
    let code = string(&mut env, &code);
    let r = if refresh == JNI_TRUE { rdp::cloud::restore(&code) } else { rdp::cloud::sign_in(&code) };
    let out = match r {
        Ok((user, refresh)) => format!("ok\n{user}\n{refresh}"),
        Err(e) => {
            log::warn!("Windows 365 sign-in: {e}");
            format!("error\n{e}")
        }
    };
    jstr(&mut env, &out)
}

#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_w365SignOut(_env: JNIEnv, _class: JClass) {
    rdp::cloud::sign_out();
}

/// The signed-in account, or "".
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_w365User(mut env: JNIEnv, _class: JClass) -> jstring {
    jstr(&mut env, &rdp::cloud::user().unwrap_or_default())
}

/// The account's Cloud PCs (blocking): "ok\nname\tkey\n…" (key: an imported
/// one's, else empty), or "error\nwhy".
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_w365List(mut env: JNIEnv, _class: JClass) -> jstring {
    let out = match rdp::cloud::list() {
        Ok(pcs) => std::iter::once("ok".to_string()).chain(pcs.into_iter().map(|(n, k)| format!("{n}\t{k}"))).collect::<Vec<_>>().join("\n"),
        Err(e) => {
            log::warn!("Windows 365 Cloud PCs: {e}");
            format!("error\n{e}")
        }
    };
    jstr(&mut env, &out)
}

/// Add a Cloud PC from an imported file's bytes: "ok\nkey\nname", or "error\nwhy".
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_w365Import(mut env: JNIEnv, _class: JClass, file: JByteArray) -> jstring {
    let bytes = env.convert_byte_array(&file).unwrap_or_default();
    let out = match rdp::cloud::import(&bytes) {
        Ok((key, name)) => format!("ok\n{key}\n{name}"),
        Err(e) => format!("error\n{e}"),
    };
    jstr(&mut env, &out)
}

/// Drop an imported Cloud PC (by its key).
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_w365Forget(mut env: JNIEnv, _class: JClass, key: JString) {
    let key: String = env.get_string(&key).map(Into::into).unwrap_or_default();
    rdp::cloud::forget(&key);
}

/// Connect to Cloud PC `index` of the list (blocking). "" when it worked,
/// else why not. It becomes the current session; a Hyper one stays open.
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_w365Connect(mut env: JNIEnv, _class: JClass, index: jint, password: JString, width: jint, height: jint) -> jstring {
    let password = string(&mut env, &password);
    let old = SESSIONS.lock().unwrap().rdp.take();
    if let Some(old) = old {
        Session::Rdp(old).close();
    }
    let r = rdp::cloud::config(index.max(0) as usize, &password, width.max(0) as u32, height.max(0) as u32).and_then(RdpSession::connect);
    match r {
        Ok(session) => {
            let hyper = {
                let mut s = SESSIONS.lock().unwrap();
                s.rdp = Some(session);
                s.rdp_current = true;
                s.hyper.clone()
            };
            if let Some(h) = hyper {
                // (Kept open, unseen: switch back to it at once.)
                Session::Hyper(h).set_paused(true);
            }
            jstr(&mut env, "")
        }
        Err(e) => {
            log::warn!("Windows 365 connect: {e}");
            jstr(&mut env, &e)
        }
    }
}

/// The open sessions, the current one first: "hyper" and/or "w365".
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_sessions(mut env: JNIEnv, _class: JClass) -> jstring {
    let out = {
        let s = SESSIONS.lock().unwrap();
        let mut kinds = Vec::new();
        if s.hyper.is_some() {
            kinds.push("hyper");
        }
        if s.rdp.is_some() {
            kinds.push("w365");
        }
        if s.rdp_current {
            kinds.reverse();
        }
        kinds.join(",")
    };
    jstr(&mut env, &out)
}

/// Show the other open session: the shown one lets go of the screen and
/// pauses (its connection stays up), the other resumes; the app then hands
/// the screen over with `setSurface`. False when there's no other.
#[no_mangle]
pub extern "system" fn Java_io_ioservices_rdpio_Native_switchSession(_env: JNIEnv, _class: JClass) -> jboolean {
    let (from, to) = {
        let mut s = SESSIONS.lock().unwrap();
        let other = if s.rdp_current { s.hyper.is_some() } else { s.rdp.is_some() };
        if !other {
            return 0;
        }
        let from = current_of(&s);
        s.rdp_current = !s.rdp_current;
        (from, current_of(&s))
    };
    if let Some(f) = from {
        f.release_all();
        f.set_surface(None);
        f.set_paused(true);
    }
    if let Some(t) = to {
        t.set_paused(false);
    }
    1
}
