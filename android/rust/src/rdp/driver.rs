//! A Windows 365 session on the tablet: the desktop client's connection and
//! session loop (`connect`, `session`), driven the way its Windows window
//! drives them, with Android's screen, sound, mic, camera and clipboard.
//!
//! Threads, as on the desktop: the session worker owns the connection (the
//! network, every channel, the input the app queues, which wakes it); a
//! decode thread composes the EGFX commands it hands over and shows them.
//! The app sees the same surface as a Hyper session ([`crate::Session`]).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ndk::native_window::NativeWindow;
use rdp_core::ClientConfig;
use rdp_hyper::input::{InputEvent, TouchPhase};
use rdp_pdu::input as inpdu;

use crate::client::Cursor;
use crate::held::Held;
use crate::rdp::compositor::Compositor;
use crate::rdp::sound::{MicIn, PcmOut};
use crate::session::{self, CursorUpdate, FrameSink};

/// What the app reads, and the screen it draws on.
struct Shared {
    state: Mutex<String>,
    surface: Mutex<Option<NativeWindow>>,
    compositor: Mutex<Compositor>,
    paused: AtomicBool,
    cursor: Mutex<Option<Cursor>>,
    cursor_visible: AtomicBool,
    cursor_gen: AtomicU32,
    clipboard_in: Mutex<Option<String>>,
    clipboard_gen: AtomicU32,
    /// The tablet's clipboard text, for the session.
    clipboard_out: Mutex<Option<String>>,
    frames_shown: AtomicU32,
    /// Time spent composing (decode + convert + copy), µs, for the stats line.
    compose_us: std::sync::atomic::AtomicU64,
}

impl Shared {
    /// Show what changed (when there's a screen and someone can see it).
    fn show(&self) {
        if self.paused.load(Ordering::Relaxed) {
            return;
        }
        let surface = self.surface.lock().unwrap();
        if let Some(w) = surface.as_ref() {
            self.compositor.lock().unwrap().show(w);
            self.frames_shown.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub struct RdpSession {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    input_tx: mpsc::Sender<Vec<inpdu::EventBytes>>,
    input: Mutex<InputState>,
    view: Mutex<(u32, u32)>,
    stats: Mutex<(std::time::Instant, u32, String)>,
}

struct InputState {
    held: Held,
    /// The pointer, in desktop pixels (relative moves start from it).
    pointer: (i32, i32),
    /// Fingers down (RDPEI contact ids), for the up/cancel flags.
    fingers: std::collections::BTreeSet<u8>,
}

/// The session's clipboard, backed by the app's.
struct Clipboard(Arc<Shared>);

impl rdp_channels::cliprdr::ClipboardProvider for Clipboard {
    fn get_text(&mut self) -> Option<String> {
        self.0.clipboard_out.lock().unwrap().clone()
    }

    fn set_text(&mut self, text: &str) {
        *self.0.clipboard_in.lock().unwrap() = Some(text.to_string());
        self.0.clipboard_gen.fetch_add(1, Ordering::Relaxed);
        crate::notify::poke();
    }
}

/// Where the session worker paints slow-path bitmaps and the pointer.
struct NetSink(Arc<Shared>);

impl FrameSink for NetSink {
    fn blit(&mut self, x: u16, y: u16, w: u16, h: u16, rgba: &[u8]) {
        self.0.compositor.lock().unwrap().bitmap(x, y, w, h, rgba);
    }

    fn present(&mut self) {
        self.0.show();
    }

    fn cursor(&mut self, update: CursorUpdate) {
        match update {
            CursorUpdate::Hide => self.0.cursor_visible.store(false, Ordering::Relaxed),
            CursorUpdate::Default => {
                *self.0.cursor.lock().unwrap() = None;
                self.0.cursor_visible.store(true, Ordering::Relaxed);
            }
            CursorUpdate::Shape { width, height, hot_x, hot_y, rgba } => {
                *self.0.cursor.lock().unwrap() = Some(Cursor { width, height, hot_x, hot_y, rgba });
                self.0.cursor_visible.store(true, Ordering::Relaxed);
            }
        }
        self.0.cursor_gen.fetch_add(1, Ordering::Relaxed);
        crate::notify::poke();
    }

    fn resize(&mut self, w: u16, h: u16) {
        self.0.compositor.lock().unwrap().resize(w as u32, h as u32);
    }
}

impl RdpSession {
    /// Connect and sign in (blocking: the app calls it off its UI thread).
    pub fn connect(mut config: ClientConfig) -> Result<Arc<Self>, String> {
        let (width, height) = (config.width as u32, config.height as u32);
        log::info!("Windows 365: connecting ({width}x{height})");
        let established = crate::connect::establish_reconnect(&mut config, None).map_err(|e| e.to_string())?;
        let crate::connect::Established { transport, mut session, .. } = established;
        log::info!("Windows 365: signed in; the desktop is starting");

        let shared = Arc::new(Shared {
            state: Mutex::new("playing".into()),
            surface: Mutex::new(None),
            compositor: Mutex::new(Compositor::new(width, height)),
            paused: AtomicBool::new(false),
            cursor: Mutex::new(None),
            cursor_visible: AtomicBool::new(true),
            cursor_gen: AtomicU32::new(0),
            clipboard_in: Mutex::new(None),
            clipboard_gen: AtomicU32::new(0),
            clipboard_out: Mutex::new(None),
            frames_shown: AtomicU32::new(0),
            compose_us: std::sync::atomic::AtomicU64::new(0),
        });
        session.set_clipboard_provider(Box::new(Clipboard(shared.clone())));
        session.set_audio_sink(Box::new(PcmOut::new()));

        let (input_tx, input_rx) = mpsc::channel::<Vec<inpdu::EventBytes>>();
        let _ = input_tx.send(vec![inpdu::sync_event(0)]);
        let stop = Arc::new(AtomicBool::new(false));
        let me = Arc::new(Self {
            shared: shared.clone(),
            stop: stop.clone(),
            input_tx,
            input: Mutex::new(InputState { held: Held::default(), pointer: (width as i32 / 2, height as i32 / 2), fingers: Default::default() }),
            view: Mutex::new((width, height)),
            stats: Mutex::new((std::time::Instant::now(), 0, String::new())),
        });

        std::thread::Builder::new()
            .name("w365-session".into())
            .spawn(move || Self::run(transport, session, shared, input_rx, stop))
            .map_err(|e| e.to_string())?;
        Ok(me)
    }

    fn run(
        mut transport: crate::connect::Transport,
        mut session: session::ActiveSession,
        shared: Arc<Shared>,
        input_rx: mpsc::Receiver<Vec<inpdu::EventBytes>>,
        stop: Arc<AtomicBool>,
    ) {
        crate::raise_thread(-10);
        // Event-driven: the worker sleeps until the gateway socket has data
        // or the app queued something (see `net_wait`).
        let wait = transport.raw_socket().and_then(|fd| crate::net_wait::SocketWait::new(fd).ok());
        match transport.raw_socket().filter(|_| wait.is_some()) {
            // As on Windows (whose event wait makes the socket non-blocking):
            // a read with nothing left returns at once, and the worker goes
            // back to its wait — instead of sitting out a 1 ms timeout first,
            // every time it has drained the socket. (The TLS and WebSocket
            // layers ride out `WouldBlock` on writes, as they do there.)
            Some(fd) => unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                log::info!("Windows 365 socket: non-blocking");
            },
            None => {
                let _ = transport.set_read_timeout(Some(Duration::from_millis(1)));
            }
        }

        let (decode_tx, decode_rx) = mpsc::channel::<Vec<rdp_pdu::gfx::GfxCommand>>();
        let backlog = Arc::new(AtomicUsize::new(0));
        let decode_backlog = backlog.clone();
        let decode_shared = shared.clone();
        let decode = std::thread::Builder::new()
            .name("w365-decode".into())
            .spawn(move || {
                crate::raise_thread(-10);
                Self::decode_loop(decode_rx, &decode_shared, &decode_backlog);
            })
            .ok();

        let mut sink = NetSink(shared.clone());
        let mut mic = MicIn::new();
        let result = session::run_graphics_session(
            &mut transport,
            &mut session,
            &mut sink,
            &input_rx,
            Some(&mut mic),
            None,
            rdp_graphics::egfx::CAPS_AVC420_ONLY.to_vec(),
            decode_tx,
            backlog,
            None,
            &stop,
            None,
            wait,
        );
        if let Some(d) = decode {
            let _ = d.join();
        }
        let why = match result {
            Ok(()) if stop.load(Ordering::Relaxed) => "ended: closed".to_string(),
            Ok(()) => "ended: the Cloud PC ended the session".to_string(),
            Err(e) => {
                let info = session::server_error_info();
                if info != 0 {
                    format!("ended: the Cloud PC ended the session (0x{info:08X})")
                } else {
                    format!("ended: connection lost ({e})")
                }
            }
        };
        log::info!("Windows 365: {why}");
        *shared.state.lock().unwrap() = why;
        crate::notify::poke();
    }

    /// Compose each batch of EGFX commands, showing the newest whole frame
    /// (a backlog is drawn at once, shown once).
    fn decode_loop(rx: mpsc::Receiver<Vec<rdp_pdu::gfx::GfxCommand>>, shared: &Shared, backlog: &AtomicUsize) {
        use rdp_pdu::gfx::GfxCommand;
        // (The desktop runs at up to 60 fps.)
        let hint = crate::hint::Hint::for_this_thread(Duration::from_micros(16_667));
        while let Ok(first) = rx.recv() {
            let work = std::time::Instant::now();
            let mut batches = vec![first];
            while let Ok(more) = rx.try_recv() {
                batches.push(more);
            }
            backlog.fetch_sub(batches.len(), Ordering::Relaxed);
            let mut in_frame = false;
            {
                let mut c = shared.compositor.lock().unwrap();
                for command in batches.iter().flatten() {
                    match command {
                        GfxCommand::StartFrame { .. } => in_frame = true,
                        GfxCommand::EndFrame { .. } => in_frame = false,
                        _ => {}
                    }
                    c.apply(command);
                }
            }
            if !in_frame {
                shared.show();
            }
            let took = work.elapsed();
            shared.compose_us.fetch_add(took.as_micros() as u64, Ordering::Relaxed);
            if let Some(h) = &hint {
                h.report(took);
            }
        }
    }

    // ---- From the app (as for a Hyper session) ----

    pub fn close(&self) {
        self.stop.store(true, Ordering::Relaxed);
        crate::net_wait::worker_wake::signal();
    }

    pub fn set_surface(&self, window: Option<NativeWindow>) {
        let mut s = self.shared.surface.lock().unwrap();
        *s = window;
        if let Some(w) = s.as_ref() {
            let mut c = self.shared.compositor.lock().unwrap();
            let (cw, ch) = c.size();
            c.resize(cw, ch);
            // (`resize` to the same size keeps the pixels, marks all of it.)
            c.show(w);
        }
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
        // Nobody can see it: the Cloud PC stops sending pictures.
        let size = self.shared.compositor.lock().unwrap().size();
        session::set_output_hidden(paused, size);
        if !paused {
            self.shared.show();
        }
    }

    /// The screen's size changed (turned, split screen): the Cloud PC's
    /// desktop follows (after it settles).
    pub fn set_view(&self, w: u32, h: u32) {
        let (w, h) = (w.clamp(200, 8192) & !1, h.clamp(200, 8192) & !1);
        let mut view = self.view.lock().unwrap();
        if *view == (w, h) {
            return;
        }
        *view = (w, h);
        session::request_resize(vec![rdp_pdu::gcc::MonitorDef { left: 0, top: 0, right: w as i32 - 1, bottom: h as i32 - 1, primary: true }]);
    }

    /// Windows' display scale (%) changed: the Cloud PC's desktop takes it at
    /// its current size.
    pub fn set_scale(&self, percent: u32) {
        if session::DESKTOP_SCALE.swap(percent, Ordering::Relaxed) == percent {
            return;
        }
        let (w, h) = *self.view.lock().unwrap();
        session::request_resize(vec![rdp_pdu::gcc::MonitorDef { left: 0, top: 0, right: w as i32 - 1, bottom: h as i32 - 1, primary: true }]);
    }

    pub fn input(&self, events: impl IntoIterator<Item = InputEvent>) {
        let (dw, dh) = self.shared.compositor.lock().unwrap().size();
        let mut st = self.input.lock().unwrap();
        let mut out = Vec::new();
        let mut touches = Vec::new();
        for ev in events {
            for ev in st.held.apply(ev) {
                Self::translate(&mut st, ev, (dw, dh), &mut out, &mut touches);
            }
        }
        drop(st);
        if !touches.is_empty() {
            session::queue_touch(touches);
        }
        if !out.is_empty() {
            let _ = self.input_tx.send(out);
            crate::net_wait::worker_wake::signal();
        }
    }

    fn translate(
        st: &mut InputState,
        ev: InputEvent,
        (dw, dh): (u32, u32),
        out: &mut Vec<inpdu::EventBytes>,
        touches: &mut Vec<rdp_channels::rdpei::RdpInputContact>,
    ) {
        use inpdu::*;
        let abs = |v: u16, size: u32| (v as u32 * size.saturating_sub(1) / 65535) as i32;
        let key = |sc: u16, ext: bool, down: bool| keyboard_event(if down { 0 } else { KBDFLAGS_RELEASE } | if ext { KBDFLAGS_EXTENDED } else { 0 }, sc);
        match ev {
            InputEvent::Key { scancode, extended, down } => out.push(key(scancode, extended, down)),
            InputEvent::Char { code, down } => out.push(unicode_event(if down { 0 } else { KBDFLAGS_RELEASE }, code)),
            InputEvent::MouseAbs { x, y } => {
                st.pointer = (abs(x, dw), abs(y, dh));
                out.push(mouse_event(PTRFLAGS_MOVE, st.pointer.0 as u16, st.pointer.1 as u16));
            }
            InputEvent::MouseRel { dx, dy } => {
                st.pointer = ((st.pointer.0 + dx as i32).clamp(0, dw as i32 - 1), (st.pointer.1 + dy as i32).clamp(0, dh as i32 - 1));
                out.push(mouse_event(PTRFLAGS_MOVE, st.pointer.0 as u16, st.pointer.1 as u16));
            }
            InputEvent::Button { button, down } => {
                let (x, y) = (st.pointer.0 as u16, st.pointer.1 as u16);
                let flag = match button {
                    0 => Some(PTRFLAGS_BUTTON1),
                    1 => Some(PTRFLAGS_BUTTON2),
                    2 => Some(PTRFLAGS_BUTTON3),
                    _ => None,
                };
                match flag {
                    Some(f) => out.push(mouse_event(f | if down { PTRFLAGS_DOWN } else { 0 }, x, y)),
                    None => {
                        let f = if button == 3 { PTRXFLAGS_BUTTON1 } else { PTRXFLAGS_BUTTON2 };
                        out.push(mouse_x_event(f | if down { PTRXFLAGS_DOWN } else { 0 }, x, y));
                    }
                }
            }
            InputEvent::Wheel { delta, horizontal } => {
                // 9-bit signed rotation per event: big turns go in steps.
                let mut left = delta as i32;
                while left != 0 {
                    let step = left.clamp(-255, 255);
                    left -= step;
                    let base = if horizontal { PTRFLAGS_HWHEEL } else { PTRFLAGS_WHEEL };
                    let rot = if step < 0 { PTRFLAGS_WHEEL_NEGATIVE | ((step & 0xFF) as u16) } else { step as u16 };
                    out.push(mouse_event(base | rot, st.pointer.0 as u16, st.pointer.1 as u16));
                }
            }
            InputEvent::Touch { id, phase, x, y } => {
                use rdp_channels::rdpei::*;
                let flags = match phase {
                    TouchPhase::Down => {
                        st.fingers.insert(id);
                        CONTACT_FLAG_DOWN | CONTACT_FLAG_INRANGE | CONTACT_FLAG_INCONTACT
                    }
                    TouchPhase::Move => CONTACT_FLAG_UPDATE | CONTACT_FLAG_INRANGE | CONTACT_FLAG_INCONTACT,
                    TouchPhase::Up => {
                        st.fingers.remove(&id);
                        CONTACT_FLAG_UP
                    }
                    TouchPhase::Cancel => {
                        if !st.fingers.remove(&id) {
                            return;
                        }
                        CONTACT_FLAG_UP | CONTACT_FLAG_CANCELED
                    }
                };
                touches.push(RdpInputContact { id, x: abs(x, dw), y: abs(y, dh), flags });
            }
            InputEvent::SecureAttention => {
                // Ctrl+Alt+Del as keys: the Cloud PC's RDP stack turns it into
                // the secure attention sequence.
                for (sc, ext) in [(0x1D, false), (0x38, false), (0x53, true)] {
                    out.push(key(sc, ext, true));
                }
                for (sc, ext) in [(0x53, true), (0x38, false), (0x1D, false)] {
                    out.push(key(sc, ext, false));
                }
            }
        }
    }

    pub fn release_all(&self) {
        let (dw, dh) = self.shared.compositor.lock().unwrap().size();
        let mut st = self.input.lock().unwrap();
        let (mut out, mut touches) = (Vec::new(), Vec::new());
        for ev in st.held.release_all() {
            Self::translate(&mut st, ev, (dw, dh), &mut out, &mut touches);
        }
        drop(st);
        if !touches.is_empty() {
            session::queue_touch(touches);
        }
        if !out.is_empty() {
            let _ = self.input_tx.send(out);
            crate::net_wait::worker_wake::signal();
        }
    }

    pub fn send_clipboard(&self, text: String) {
        *self.shared.clipboard_out.lock().unwrap() = Some(text);
        session::clipboard_changed();
    }

    pub fn take_clipboard(&self) -> Option<String> {
        self.shared.clipboard_in.lock().unwrap().take()
    }

    pub fn cursor_visible(&self) -> bool {
        self.shared.cursor_visible.load(Ordering::Relaxed)
    }

    pub fn with_cursor<R>(&self, f: impl FnOnce(Option<&Cursor>) -> R) -> R {
        f(self.shared.cursor.lock().unwrap().as_ref())
    }

    /// `state|width|height|cursor generation|clipboard generation|touch`,
    /// then the stats, as a Hyper session reports.
    pub fn status(&self) -> String {
        let (w, h) = self.shared.compositor.lock().unwrap().size();
        let mut stats = self.stats.lock().unwrap();
        if stats.0.elapsed() >= Duration::from_secs(1) {
            let shown = self.shared.frames_shown.load(Ordering::Relaxed);
            let frames = shown - stats.1;
            let fps = frames as f32 / stats.0.elapsed().as_secs_f32();
            let compose = self.shared.compose_us.swap(0, Ordering::Relaxed) as f32 / 1000.0 / frames.max(1) as f32;
            let copied = std::mem::take(&mut self.shared.compositor.lock().unwrap().copied) as f32 / 1e6 / stats.0.elapsed().as_secs_f32();
            *stats = (std::time::Instant::now(), shown, format!("Windows 365 · {w}x{h} · {fps:.0} fps\ncompose {compose:.1} ms a frame  screen copy {copied:.1} MB/s"));
        }
        format!(
            "{}|{}|{}|{}|{}|{}\n{}",
            self.shared.state.lock().unwrap(),
            w,
            h,
            self.shared.cursor_gen.load(Ordering::Relaxed),
            self.shared.clipboard_gen.load(Ordering::Relaxed),
            session::rdpei_active() as u8,
            stats.2
        )
    }
}

impl Drop for RdpSession {
    fn drop(&mut self) {
        self.close();
    }
}
