//! Windows network-change listener for immediate auto-reconnect.
//!
//! Uses `NotifyIpInterfaceChange` (IP Helper) to wake the reconnect loop as soon
//! as an interface comes up, instead of waiting out the full exponential backoff.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::OnceLock;
use windows::Win32::Foundation::{HANDLE, WIN32_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    NotifyIpInterfaceChange, MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE,
};
use windows::Win32::Networking::WinSock::AF_UNSPEC;

static NET_CHANGE_TX: OnceLock<Sender<()>> = OnceLock::new();

/// Subscribe to IPv4/IPv6 interface-up/down events. Returns a receiver that fires
/// once for any interface change. The listener thread exits automatically when
/// the program terminates (the OS closes the notification handle).
pub fn subscribe() -> Receiver<()> {
    let (tx, rx) = mpsc::channel();
    let _ = NET_CHANGE_TX.set(tx);

    std::thread::spawn(|| {
        unsafe {
            let mut handle = HANDLE(std::ptr::null_mut());
            let status: WIN32_ERROR = NotifyIpInterfaceChange(
                AF_UNSPEC,
                Some(ip_interface_change_callback),
                None,
                false,
                &mut handle,
            );
            if status.0 != 0 {
                tracing::debug!(
                    status = status.0,
                    "NotifyIpInterfaceChange failed; network-change wake disabled"
                );
                return;
            }
            // Keep the thread alive so the callback stays registered. The handle is
            // closed automatically when the process exits.
            loop {
                std::thread::park();
            }
        }
    });

    rx
}

unsafe extern "system" fn ip_interface_change_callback(
    _caller_context: *const std::ffi::c_void,
    _row: *const MIB_IPINTERFACE_ROW,
    _notification_type: MIB_NOTIFICATION_TYPE,
) {
    if let Some(tx) = NET_CHANGE_TX.get() {
        // Unbounded channel; ignore send errors if the receiver is gone.
        let _ = tx.send(());
    }
}

/// Wait for either `duration` to elapse or a network-change event.
pub fn wait_with_network_wake(delay: std::time::Duration, rx: &Receiver<()>) -> bool {
    match rx.recv_timeout(delay) {
        Ok(()) => true,
        Err(mpsc::RecvTimeoutError::Timeout) => false,
        Err(mpsc::RecvTimeoutError::Disconnected) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_timeout_returns_false() {
        let rx = subscribe();
        let woke = wait_with_network_wake(std::time::Duration::from_millis(10), &rx);
        assert!(!woke);
    }
}
