//! Event-driven waiting for the RDP session worker (Android): the desktop's
//! `net_wait` (WSAEventSelect) on poll(2) and an eventfd.
//!
//! The worker blocks until the socket is readable/closed, another thread
//! queued work for it ([`worker_wake::signal`]: input, clipboard, resize,
//! touch, mic, camera), or a timeout for timed chores. The socket stays in
//! blocking mode; the driver gives it a 1 ms read timeout, so the session's
//! drain-until-`WouldBlock` read ends promptly (Linux's timed-out `recv`
//! loses nothing, unlike Windows') without writes ever seeing `WouldBlock`.

use std::io;
use std::os::unix::io::RawFd;
use std::time::Duration;

pub mod worker_wake {
    use std::sync::atomic::{AtomicI32, Ordering};

    static FD: AtomicI32 = AtomicI32::new(-1);

    /// The wake eventfd, created on first call (-1 if that failed: the
    /// worker then wakes on its timeouts only).
    pub fn handle() -> i32 {
        let fd = FD.load(Ordering::Acquire);
        if fd >= 0 {
            return fd;
        }
        let new = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if new < 0 {
            return -1;
        }
        match FD.compare_exchange(-1, new, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => new,
            Err(existing) => {
                unsafe { libc::close(new) };
                existing
            }
        }
    }

    /// Wake the worker (any thread; a no-op until it first waits).
    pub fn signal() {
        let fd = FD.load(Ordering::Acquire);
        if fd >= 0 {
            let one: u64 = 1;
            unsafe { libc::write(fd, (&one as *const u64).cast(), 8) };
        }
    }

    /// Clear pending signals (after a wake).
    pub(super) fn drain(fd: i32) {
        let mut v: u64 = 0;
        unsafe { libc::read(fd, (&mut v as *mut u64).cast(), 8) };
    }
}

/// A wait over the session socket, the worker's wake event and optionally a
/// second socket.
pub struct SocketWait {
    socket: RawFd,
    wake: RawFd,
    also: Option<RawFd>,
}

impl SocketWait {
    pub fn new(socket: RawFd) -> io::Result<Self> {
        Ok(Self { socket, wake: worker_wake::handle(), also: None })
    }

    pub fn watch_also(&mut self, socket: RawFd) -> io::Result<()> {
        self.also = Some(socket);
        Ok(())
    }

    pub fn unwatch_also(&mut self) {
        self.also = None;
    }

    pub fn wait(&self, timeout: Duration) {
        let mut fds = [
            libc::pollfd { fd: self.socket, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: self.wake, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: self.also.unwrap_or(-1), events: libc::POLLIN, revents: 0 },
        ];
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if n > 0 && fds[1].revents & libc::POLLIN != 0 {
            worker_wake::drain(self.wake);
        }
    }
}
