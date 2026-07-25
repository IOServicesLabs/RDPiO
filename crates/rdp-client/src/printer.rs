//! Win32 print-spooler sink for printer redirection (MS-RDPEPC).
//!
//! Spools redirected print jobs to the local default printer via the winspool
//! API: each job is `OpenPrinter` → `StartDocPrinter` (RAW datatype) →
//! `WritePrinter`* → `EndDocPrinter` → `ClosePrinter`. Implements
//! [`rdp_channels::rdpdr::PrinterSink`]. Runs on the session worker thread.
//! Blind Windows FFI — never executed here.

use rdp_channels::rdpdr::PrinterSink;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Graphics::Printing::{
    ClosePrinter, EndDocPrinter, EndPagePrinter, GetDefaultPrinterW, GetPrinterW, OpenPrinterW,
    StartDocPrinterW, StartPagePrinter, WritePrinter, DOC_INFO_1W, PRINTER_HANDLE, PRINTER_INFO_2W,
};

/// A redirected local printer spooled via winspool.
pub struct Win32Printer {
    /// The printer name (NUL-terminated UTF-16) to open per job.
    name: Vec<u16>,
    /// The open printer handle while a job is in progress.
    handle: Option<PRINTER_HANDLE>,
}

// SAFETY: created on the UI thread and moved once into the session worker, then
// only touched there. The printer HANDLE is owned (not shared); winspool handles
// are not thread-affine, so the single-owner move is sound (mirrors Win32Audio).
unsafe impl Send for Win32Printer {}

impl Win32Printer {
    /// Discover the default printer and its driver name. Returns
    /// `(print_name, driver_name, sink)` or `None` if there is no default
    /// printer (redirection then stays off).
    pub fn default_printer() -> Option<(String, String, Self)> {
        unsafe {
            // Friendly name of the default printer.
            let mut len = 0u32;
            let _ = GetDefaultPrinterW(None, &mut len); // sizes the buffer
            if len == 0 {
                return None;
            }
            let mut name_buf = vec![0u16; len as usize];
            if !GetDefaultPrinterW(Some(PWSTR(name_buf.as_mut_ptr())), &mut len).as_bool() {
                return None;
            }
            name_buf.truncate(len as usize); // includes the NUL
            let print_name = String::from_utf16_lossy(
                &name_buf[..name_buf.iter().position(|&c| c == 0).unwrap_or(name_buf.len())],
            );

            // The driver name (PRINTER_INFO_2W) so the server renders to it.
            let driver_name = driver_for(&name_buf).unwrap_or_default();

            Some((
                print_name,
                driver_name,
                Self {
                    name: name_buf,
                    handle: None,
                },
            ))
        }
    }
}

/// Read the driver name from PRINTER_INFO_2W for the printer named by `name`.
unsafe fn driver_for(name: &[u16]) -> Option<String> {
    let mut handle = PRINTER_HANDLE::default();
    OpenPrinterW(PCWSTR(name.as_ptr()), &mut handle, None).ok()?;
    // First call sizes the buffer.
    let mut needed = 0u32;
    let _ = GetPrinterW(handle, 2, None, &mut needed);
    let driver = if needed > 0 {
        let mut buf = vec![0u8; needed as usize];
        if GetPrinterW(handle, 2, Some(&mut buf), &mut needed).is_ok() {
            let info = &*(buf.as_ptr() as *const PRINTER_INFO_2W);
            if info.pDriverName.is_null() {
                None
            } else {
                info.pDriverName.to_string().ok()
            }
        } else {
            None
        }
    } else {
        None
    };
    let _ = ClosePrinter(handle);
    driver
}

impl PrinterSink for Win32Printer {
    fn start_job(&mut self) -> bool {
        unsafe {
            let mut handle = PRINTER_HANDLE::default();
            if OpenPrinterW(PCWSTR(self.name.as_ptr()), &mut handle, None).is_err() {
                tracing::warn!("OpenPrinter failed; print job dropped");
                return false;
            }
            // RAW datatype: the bytes are already the printer's page-description
            // language (the server rendered them with the announced driver).
            let mut doc_name: Vec<u16> = "rdpio print job".encode_utf16().chain([0]).collect();
            let mut datatype: Vec<u16> = "RAW".encode_utf16().chain([0]).collect();
            let info = DOC_INFO_1W {
                pDocName: PWSTR(doc_name.as_mut_ptr()),
                pOutputFile: PWSTR::null(),
                pDatatype: PWSTR(datatype.as_mut_ptr()),
            };
            if StartDocPrinterW(handle, 1, &info) == 0 {
                let _ = ClosePrinter(handle);
                tracing::warn!("StartDocPrinter failed; print job dropped");
                return false;
            }
            let _ = StartPagePrinter(handle);
            self.handle = Some(handle);
            true
        }
    }

    fn write(&mut self, data: &[u8]) {
        let Some(handle) = self.handle else {
            return;
        };
        if data.is_empty() {
            return;
        }
        unsafe {
            let mut written = 0u32;
            let _ = WritePrinter(
                handle,
                data.as_ptr() as *const core::ffi::c_void,
                data.len() as u32,
                &mut written,
            );
        }
    }

    fn end_job(&mut self) {
        if let Some(handle) = self.handle.take() {
            unsafe {
                let _ = EndPagePrinter(handle);
                let _ = EndDocPrinter(handle);
                let _ = ClosePrinter(handle);
            }
        }
    }
}

impl Drop for Win32Printer {
    fn drop(&mut self) {
        self.end_job();
    }
}
