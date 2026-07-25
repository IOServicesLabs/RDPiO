//! Last-resort crash diagnostics.
//!
//! A native access violation (e.g. inside a hosted third-party add-in like the
//! Teams WebRTC redirector) unwinds past Rust's panic machinery and kills the
//! process with nothing in the log. This installs a process-wide unhandled-
//! exception filter that, right before the crash, logs the exception code and the
//! **module + offset** of the faulting instruction — so a crash points at its
//! culprit (rdpio's own code vs. a loaded DLL) instead of vanishing. It catches
//! faults on any thread, including add-in worker threads.

#![cfg(windows)]

use windows::core::PCWSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Diagnostics::Debug::{
    SetUnhandledExceptionFilter, EXCEPTION_POINTERS,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};

unsafe extern "system" fn on_unhandled(info: *const EXCEPTION_POINTERS) -> i32 {
    unsafe {
        if info.is_null() || (*info).ExceptionRecord.is_null() {
            return 0; // EXCEPTION_CONTINUE_SEARCH
        }
        let rec = &*(*info).ExceptionRecord;
        let code = rec.ExceptionCode.0 as u32;
        let addr = rec.ExceptionAddress as usize;

        // Resolve which loaded module the faulting address belongs to.
        let mut module = HMODULE::default();
        let mut name = String::from("<unknown>");
        let mut base = 0usize;
        if GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(addr as *const u16), // per FROM_ADDRESS: the "name" is the address
            &mut module,
        )
        .is_ok()
        {
            base = module.0 as usize;
            let mut buf = [0u16; 260];
            let n = GetModuleFileNameW(Some(module), &mut buf);
            if n > 0 {
                name = String::from_utf16_lossy(&buf[..n as usize]);
            }
        }
        let offset = addr.wrapping_sub(base);

        tracing::error!(
            code = format!("0x{code:08X}"),
            addr = format!("0x{addr:016X}"),
            module = %name,
            module_offset = format!("0x{offset:X}"),
            "FATAL: unhandled native exception — rdpio is crashing (see module/offset for the culprit)"
        );
    }
    0 // EXCEPTION_CONTINUE_SEARCH: let the OS finish the crash now that we've logged
}

/// Install the process-wide unhandled-exception logger. Call once, right after
/// tracing is initialized so the fatal line reaches the log file.
pub fn install() {
    unsafe {
        SetUnhandledExceptionFilter(Some(on_unhandled));
    }
}
