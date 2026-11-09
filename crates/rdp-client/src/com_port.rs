//! Local COM ports for serial redirection (MS-RDPESP): a port opens when the
//! session opens it, so other apps keep it until then. Requests start in the
//! order they came (a baud rate lands before the write that needs it), but a
//! read or a wait on the port can last as long as the port likes, so each is
//! waited for on a thread of its own, which then wakes the session worker to
//! send its answer.

use std::sync::Arc;

use rdp_channels::rdpdr::{
    PortBackend, PortDone, PortRequest, STATUS_ACCESS_DENIED, STATUS_CANCELLED, STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
};
use windows::core::{Error, HSTRING};
use windows::Win32::Devices::Communication::GetCommPorts;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, ERROR_SHARING_VIOLATION,
    GENERIC_READ, GENERIC_WRITE, HANDLE, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::CreateEventW;
use windows::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED};

/// An open port; closed once the last request on it has finished.
struct Open(HANDLE);

// SAFETY: a file handle can be used from any thread; overlapped I/O is made for
// several requests at once.
unsafe impl Send for Open {}
unsafe impl Sync for Open {}

impl Drop for Open {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub struct Win32ComPort {
    path: HSTRING,
    open: Option<Arc<Open>>,
}

/// This PC's COM ports, as (name, port).
pub fn available() -> Vec<(String, Win32ComPort)> {
    let mut numbers = [0u32; 256];
    let mut found = 0u32;
    let err = unsafe { GetCommPorts(&mut numbers, &mut found) };
    if err != 0 {
        tracing::debug!(err, "no COM ports listed");
        return Vec::new();
    }
    numbers
        .iter()
        .take(found as usize)
        .map(|n| {
            let name = format!("COM{n}");
            let port = Win32ComPort { path: HSTRING::from(format!(r"\\.\{name}")), open: None };
            (name, port)
        })
        .collect()
}

fn status_of(e: &Error) -> u32 {
    let is = |w: WIN32_ERROR| e.code() == w.to_hresult();
    if is(ERROR_ACCESS_DENIED) || is(ERROR_SHARING_VIOLATION) {
        STATUS_ACCESS_DENIED
    } else if is(ERROR_OPERATION_ABORTED) {
        STATUS_CANCELLED
    } else {
        STATUS_UNSUCCESSFUL
    }
}

/// Finish `done` and wake the session worker to send the answer.
fn finish(done: PortDone, status: u32, written: u32, data: Vec<u8>) {
    done(status, written, data);
    crate::net_wait::worker_wake::signal();
}

/// An overlapped call under way: what the port is using stays here until it
/// has finished, whichever thread that ends up on.
struct Pending {
    port: Arc<Open>,
    ov: Box<OVERLAPPED>,
    /// What a write or control call is sending.
    _input: Vec<u8>,
    /// What a read or control call is filling.
    output: Vec<u8>,
    is_write: bool,
    done: Option<PortDone>,
}

// SAFETY: the OVERLAPPED's event is a handle, and the call it tracks only ever
// touches this struct's heap buffers, which don't move with it.
unsafe impl Send for Pending {}

impl Pending {
    /// Wait for the call, then answer with what it did.
    fn wait(&mut self) {
        let mut moved = 0u32;
        let result = unsafe { GetOverlappedResult(self.port.0, &*self.ov, &mut moved, true) };
        let Some(done) = self.done.take() else { return };
        match result {
            Ok(()) if self.is_write => finish(done, STATUS_SUCCESS, moved, Vec::new()),
            Ok(()) => {
                let mut data = std::mem::take(&mut self.output);
                data.truncate(moved as usize);
                finish(done, STATUS_SUCCESS, 0, data);
            }
            Err(e) => finish(done, status_of(&e), 0, Vec::new()),
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        // Never waited for (no thread to be had): call it off, so the buffers
        // aren't freed under the port.
        if self.done.is_some() {
            unsafe {
                let _ = CancelIoEx(self.port.0, Some(&*self.ov));
            }
            self.wait();
        }
        unsafe {
            let _ = CloseHandle(self.ov.hEvent);
        }
    }
}

/// Start `request` on `port` now, and wait for it on a thread.
fn start(port: Arc<Open>, request: PortRequest, done: PortDone) {
    let event = match unsafe { CreateEventW(None, true, false, None) } {
        Ok(event) => event,
        Err(e) => return finish(done, status_of(&e), 0, Vec::new()),
    };
    let ov = Box::new(OVERLAPPED { hEvent: event, ..Default::default() });
    let mut pending = Pending { port, ov, _input: Vec::new(), output: Vec::new(), is_write: false, done: Some(done) };
    let h = pending.port.0;
    let ov: *mut OVERLAPPED = &mut *pending.ov;
    let started = match request {
        PortRequest::Read { length } => {
            pending.output = vec![0u8; length as usize];
            unsafe { ReadFile(h, Some(&mut pending.output), None, Some(ov)) }
        }
        PortRequest::Write { data } => {
            pending._input = data;
            pending.is_write = true;
            unsafe { WriteFile(h, Some(&pending._input), None, Some(ov)) }
        }
        PortRequest::Control { code, input, output_length } => {
            pending._input = input;
            pending.output = vec![0u8; output_length as usize];
            unsafe {
                DeviceIoControl(
                    h,
                    code,
                    Some(pending._input.as_ptr().cast()),
                    pending._input.len() as u32,
                    Some(pending.output.as_mut_ptr().cast()),
                    pending.output.len() as u32,
                    None,
                    Some(ov),
                )
            }
        }
        PortRequest::Open | PortRequest::Close => return,
    };
    if let Err(e) = started {
        if e.code() != ERROR_IO_PENDING.to_hresult() {
            // Never started, so nothing to wait for.
            if let Some(done) = pending.done.take() {
                finish(done, status_of(&e), 0, Vec::new());
            }
            return;
        }
    }
    let spawned = std::thread::Builder::new().name("rdpdr-com".into()).spawn(move || {
        let mut pending = pending;
        pending.wait();
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "no thread for a COM port request");
    }
}

impl PortBackend for Win32ComPort {
    fn request(&mut self, request: PortRequest, done: PortDone) {
        match request {
            PortRequest::Open => {
                let opened = unsafe {
                    CreateFileW(
                        &self.path,
                        (GENERIC_READ | GENERIC_WRITE).0,
                        FILE_SHARE_NONE,
                        None,
                        OPEN_EXISTING,
                        FILE_FLAG_OVERLAPPED,
                        None,
                    )
                };
                match opened {
                    Ok(h) => {
                        self.open = Some(Arc::new(Open(h)));
                        finish(done, STATUS_SUCCESS, 0, Vec::new());
                    }
                    Err(e) => {
                        tracing::warn!(port = %self.path, error = %e, "couldn't open a redirected COM port");
                        finish(done, status_of(&e), 0, Vec::new());
                    }
                }
            }
            PortRequest::Close => {
                if let Some(port) = self.open.take() {
                    // What's waiting on it ends now (and lets go of the handle).
                    unsafe {
                        let _ = CancelIoEx(port.0, None);
                    }
                }
                finish(done, STATUS_SUCCESS, 0, Vec::new());
            }
            other => match self.open.clone() {
                Some(port) => start(port, other, done),
                None => finish(done, STATUS_UNSUCCESSFUL, 0, Vec::new()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On a real PC: `cargo test -- --ignored --nocapture this_pcs_redirects`.
    /// A port that's free opens and closes; one in use says so.
    #[test]
    #[ignore = "reads this PC's own printers and COM ports"]
    fn this_pcs_redirects() {
        let printers = crate::printer::Win32Printer::all();
        for (name, driver, default, _) in &printers {
            println!("printer {name:?} driver {driver:?} default {default}");
        }
        assert!(printers.iter().filter(|p| p.2).count() <= 1);
        for (name, mut port) in available() {
            let (tx, rx) = std::sync::mpsc::channel();
            port.request(PortRequest::Open, Box::new(move |status, _, _| tx.send(status).unwrap()));
            println!("{name}: open status {:#x}", rx.recv().unwrap());
            let (tx, rx) = std::sync::mpsc::channel();
            port.request(PortRequest::Close, Box::new(move |status, _, _| tx.send(status).unwrap()));
            assert_eq!(rx.recv().unwrap(), STATUS_SUCCESS);
        }
    }
}
