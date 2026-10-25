//! The app's wake-up: an eventfd the UI thread's looper watches, signalled
//! when something it shows changed (the pointer's shape, the clipboard, the
//! session's state, the stream's size). The app then reads the status at
//! once instead of noticing on its next timed poll (up to 100 ms later for a
//! pointer shape), and that poll can run less often.
//!
//! Made only when the app asks for it (`debug.rdpio.uinotify=0`: never, and
//! [`poke`] is a load and nothing else).

use std::sync::OnceLock;

static FD: OnceLock<i32> = OnceLock::new();

/// The eventfd (made on first call), or -1 where there's none.
pub fn fd() -> i32 {
    *FD.get_or_init(|| {
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            log::warn!("ui notify: no eventfd ({})", std::io::Error::last_os_error());
        }
        fd
    })
}

/// Something the app shows changed: wake its UI thread. (Pokes before it
/// reads add up in the counter: one wake-up for many.)
pub fn poke() {
    if let Some(&fd) = FD.get() {
        if fd >= 0 {
            let one: u64 = 1;
            unsafe { libc::write(fd, &one as *const u64 as *const libc::c_void, 8) };
        }
    }
}
