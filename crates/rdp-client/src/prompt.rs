//! Interactive console prompts.

use std::io::{self, BufRead, Write};

/// Prompt on stderr and read a line of input with terminal echo disabled (so a
/// typed password is not shown). Falls back to plain reading if there is no
/// console (e.g. piped input). The returned string has trailing CR/LF stripped.
#[cfg(windows)]
pub fn read_password(prompt: &str) -> io::Result<String> {
    use windows::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE, ENABLE_ECHO_INPUT,
        ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, STD_INPUT_HANDLE,
    };

    eprint!("{prompt}");
    io::stderr().flush()?;

    // Disable echo on the console input handle for the duration of the read.
    let restore = unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE).map_err(io::Error::other)?;
        let mut mode = CONSOLE_MODE(0);
        if GetConsoleMode(handle, &mut mode).is_ok() {
            let quiet = (mode & !ENABLE_ECHO_INPUT) | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT;
            let _ = SetConsoleMode(handle, quiet);
            Some((handle, mode))
        } else {
            None // not a real console (piped/redirected): read normally
        }
    };

    let mut line = String::new();
    let read = io::stdin().lock().read_line(&mut line);

    if let Some((handle, mode)) = restore {
        unsafe {
            let _ = SetConsoleMode(handle, mode);
        }
        eprintln!(); // echo the newline the user's (hidden) Enter didn't show
    }
    read?;

    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(not(windows))]
pub fn read_password(prompt: &str) -> io::Result<String> {
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}
