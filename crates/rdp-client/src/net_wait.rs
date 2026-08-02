//! Event-driven waiting for the session worker (Windows).
//!
//! The graphics-session worker multiplexes one thread across the TLS socket,
//! input forwarding, clipboard, resize and device chores. It used to pace all
//! of that with a 1 ms `SO_RCVTIMEO` poll — 60–1000 wakeups per second for the
//! life of the session, idle or not, which kept the CPU package out of its deep
//! C-states and showed up directly as battery drain (mstsc is event-driven
//! here). [`SocketWait`] replaces the poll: the socket is switched to event
//! notification (`WSAEventSelect`, which also makes it non-blocking) and the
//! worker blocks on *socket readable/closed* OR *another thread queued work*
//! (the [`worker_wake`] event, signalled by the input/clipboard/resize/touch/
//! stop producers) OR a timeout for time-based chores. Idle cost drops to ~2
//! wakeups a second while input still ships the instant it is queued.
//!
//! Consequence of the non-blocking mode: reads surface `WouldBlock` instead of
//! blocking (the TLS layer and `poll_tpkt_pdu` already resume cleanly across
//! that), and writes can too — the TLS write path rides those out internally so
//! a record is never truncated.

#![cfg(windows)]

use std::io;
use std::time::Duration;

use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Networking::WinSock::{
    ioctlsocket, WSACloseEvent, WSACreateEvent, WSAEnumNetworkEvents, WSAEventSelect,
    WSAGetLastError, FD_CLOSE, FD_READ, FIONBIO, SOCKET, WSAEVENT, WSANETWORKEVENTS,
};
use windows::Win32::System::Threading::WaitForMultipleObjects;

/// The worker's wake event: auto-reset, created lazily by the worker's wait
/// loop, signalled by any thread that queues work for it (input, clipboard,
/// resize, touch, stop). A signal with no waiter is remembered until the next
/// wait; a signal before the event exists is dropped, which is fine — the
/// worker always runs one full servicing pass before its first wait.
pub mod worker_wake {
    use core::ffi::c_void;
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};

    static HANDLE_RAW: AtomicIsize = AtomicIsize::new(0);

    /// The wake event's raw handle, created on first call (0 if creation
    /// failed — the caller then degrades to timeout pacing).
    pub fn handle() -> isize {
        let h = HANDLE_RAW.load(Ordering::Acquire);
        if h != 0 {
            return h;
        }
        match unsafe { windows::Win32::System::Threading::CreateEventW(None, false, false, None) }
        {
            Ok(ev) => {
                let raw = ev.0 as isize;
                match HANDLE_RAW.compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => raw,
                    Err(existing) => {
                        // Lost a creation race; keep the winner's event.
                        unsafe {
                            let _ = CloseHandle(ev);
                        }
                        existing
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "no worker wake event; worker will poll on timeouts");
                0
            }
        }
    }

    /// Signal the worker. Cheap (one `SetEvent`), callable from any thread; a
    /// no-op until the worker has created the event.
    pub fn signal() {
        let h = HANDLE_RAW.load(Ordering::Acquire);
        if h != 0 {
            unsafe {
                let _ =
                    windows::Win32::System::Threading::SetEvent(HANDLE(h as *mut c_void));
            }
        }
    }
}

/// Multiplexed wait over "the socket has data / closed" and "another thread
/// queued work for the worker". Creating one switches the socket into
/// event-notification (non-blocking) mode; dropping it switches back.
pub struct SocketWait {
    socket: SOCKET,
    sock_event: WSAEVENT,
    wake: isize,
}

// SAFETY: both members are kernel handles, valid across threads; the struct is
// moved into the worker thread at session start.
unsafe impl Send for SocketWait {}

impl SocketWait {
    /// Put `socket` into event-notification mode for read/close and build the
    /// worker's wait set. The socket becomes non-blocking as a side effect
    /// (WSAEventSelect semantics) — the session's read AND write paths must
    /// tolerate `WouldBlock`, which the TLS transport does.
    pub fn new(socket: std::os::windows::io::RawSocket) -> io::Result<Self> {
        let socket = SOCKET(socket as usize);
        let sock_event = unsafe { WSACreateEvent() }
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let rc = unsafe { WSAEventSelect(socket, Some(sock_event), (FD_READ | FD_CLOSE) as i32) };
        if rc != 0 {
            let err = unsafe { WSAGetLastError() };
            unsafe {
                let _ = WSACloseEvent(sock_event);
            }
            return Err(io::Error::from_raw_os_error(err.0));
        }
        Ok(Self {
            socket,
            sock_event,
            wake: worker_wake::handle(),
        })
    }

    /// Block until the socket is readable/closed, a producer signals the
    /// worker, or `timeout` elapses. Spurious wakeups are fine — the caller's
    /// loop re-checks every work source each pass.
    pub fn wait(&self, timeout: Duration) {
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let sock_handle = HANDLE(self.sock_event.0 as *mut core::ffi::c_void);
        let wake_handle = HANDLE(self.wake as *mut core::ffi::c_void);
        let handles: &[HANDLE] = if self.wake != 0 {
            &[sock_handle, wake_handle]
        } else {
            &[sock_handle]
        };
        let rc = unsafe { WaitForMultipleObjects(handles, false, timeout_ms) };
        if rc == WAIT_OBJECT_0 {
            // Acknowledge the socket notification: WSAEnumNetworkEvents resets
            // the (manual-reset) event atomically. FD_READ re-arms on the next
            // recv that leaves the buffer non-empty, so draining reads until
            // WouldBlock before the next wait never loses a wakeup.
            let mut ne = WSANETWORKEVENTS::default();
            unsafe {
                let _ = WSAEnumNetworkEvents(self.socket, self.sock_event, &mut ne);
            }
        }
        // The wake event is auto-reset; timeouts need no bookkeeping.
    }
}

impl Drop for SocketWait {
    fn drop(&mut self) {
        unsafe {
            // Detach event notification and put the socket back into blocking
            // mode for whoever touches the transport during teardown.
            let _ = WSAEventSelect(self.socket, None, 0);
            let mut blocking_mode: u32 = 0;
            let _ = ioctlsocket(self.socket, FIONBIO, &mut blocking_mode);
            let _ = WSACloseEvent(self.sock_event);
        }
    }
}
