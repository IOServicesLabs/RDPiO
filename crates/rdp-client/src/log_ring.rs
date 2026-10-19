//! The last few thousand log lines, in memory, for the launcher's Activity
//! page and its Workspace tabs: the launcher's own (its tracing output, through
//! [`Tee`]) and its sessions' (their log files, tailed by the launcher).
//!
//! Off until [`enable`]: other rdpio processes keep nothing.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Lines kept (all sources together); older ones go first.
const KEEP: usize = 4000;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Line {
    pub seq: u64,
    /// Which session ("launcher" for the launcher's own lines).
    pub src: String,
    /// The session's name as the user knows it.
    pub name: String,
    /// RFC 3339, as the log printed it ("" for a line without one).
    pub time: String,
    /// "ERROR", "WARN", "INFO", "DEBUG", "TRACE" (or "" for an unparsed line).
    pub level: String,
    /// Where in rdpio it came from ("perf" for the once-a-second stats).
    pub target: String,
    pub text: String,
}

static LINES: Mutex<VecDeque<Line>> = Mutex::new(VecDeque::new());
static SEQ: AtomicU64 = AtomicU64::new(0);
static ON: AtomicBool = AtomicBool::new(false);
static NOTIFY: OnceLock<fn()> = OnceLock::new();
/// The launcher's own lines, in a file as well (see [`keep_file`]).
static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// Start keeping lines; `notify` runs (on the pushing thread) after each.
pub fn enable(notify: fn()) {
    let _ = NOTIFY.set(notify);
    ON.store(true, Ordering::Release);
}

pub fn enabled() -> bool {
    ON.load(Ordering::Acquire)
}

/// [`Tee`]'s lines go to `path` too (the last run's kept as `.prev`): the
/// ring is gone with the process, and with it what led up to a restart.
pub fn keep_file(path: &std::path::Path) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > 0) {
        let _ = std::fs::rename(path, path.with_extension("log.prev"));
    }
    *FILE.lock().unwrap() = std::fs::File::create(path).ok();
}

/// One line as `tracing`'s formatter prints it
/// (`2026-10-18T09:12:34.123456Z  INFO rdpio::launcher: text k=v`); anything
/// else is kept whole.
pub fn push(src: &str, name: &str, raw: &str) {
    if !enabled() {
        return;
    }
    let raw = strip_ansi(raw);
    let raw = raw.trim_end();
    if raw.trim().is_empty() {
        return;
    }
    let (time, level, target, text) = parse(raw);
    keep(src, name, time, level, target, text);
}

/// A line that isn't from `tracing` (a session's last words on stderr).
pub fn push_plain(src: &str, name: &str, level: &str, text: &str) {
    if !enabled() || text.trim().is_empty() {
        return;
    }
    keep(src, name, "", level, "", text.trim_end());
}

fn keep(src: &str, name: &str, time: &str, level: &str, target: &str, text: &str) {
    {
        let mut lines = LINES.lock().unwrap();
        if lines.len() >= KEEP {
            lines.pop_front();
        }
        lines.push_back(Line {
            seq: SEQ.fetch_add(1, Ordering::Relaxed) + 1,
            src: src.into(),
            name: name.into(),
            time: time.into(),
            level: level.into(),
            target: target.into(),
            text: text.into(),
        });
    }
    if let Some(n) = NOTIFY.get() {
        n();
    }
}

/// The lines after `seq` (all of them for 0), oldest first, at most `max`
/// (the newest, when there are more).
pub fn since(seq: u64, max: usize) -> Vec<Line> {
    let lines = LINES.lock().unwrap();
    let start = lines.partition_point(|l| l.seq <= seq);
    let start = start.max(lines.len().saturating_sub(max));
    lines.range(start..).cloned().collect()
}

/// The newest line's number (0 before any).
#[cfg(test)]
fn last_seq() -> u64 {
    LINES.lock().unwrap().back().map_or(0, |l| l.seq)
}

/// The last error `src` logged (its text), if any is still kept.
pub fn last_error(src: &str) -> Option<String> {
    LINES.lock().unwrap().iter().rev().find(|l| l.src == src && l.level == "ERROR").map(|l| l.text.clone())
}

/// Forget what's kept (numbers go on from where they were).
pub fn clear() {
    LINES.lock().unwrap().clear();
}

fn parse(raw: &str) -> (&str, &str, &str, &str) {
    let mut rest = raw.trim_start();
    let first = rest.split_whitespace().next().unwrap_or("");
    let time = if first.len() >= 19 && first.as_bytes()[0].is_ascii_digit() && first.contains('T') {
        rest = rest[first.len()..].trim_start();
        first
    } else {
        ""
    };
    let word = rest.split_whitespace().next().unwrap_or("");
    let level = if matches!(word, "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE") {
        rest = rest[word.len()..].trim_start();
        word
    } else {
        ""
    };
    // "rdpio::hyper_client: text" → target "hyper_client".
    let mut target = "";
    if !level.is_empty() {
        if let Some((t, tail)) = rest.split_once(": ") {
            if !t.contains(' ') {
                target = t.strip_prefix("rdpio::").unwrap_or(t);
                rest = tail;
            }
        }
    }
    (time, level, target, rest)
}

fn strip_ansi(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('\x1b') {
        return s.into();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // CSI: ESC [ … final byte in @–~.
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out.into()
}

/// `tracing`'s writer for a process without a log file: stdout as before, and
/// the ring once [`enable`]d (the launcher's own lines).
pub struct Tee;

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stdout().write_all(buf);
        if let Some(f) = FILE.lock().unwrap().as_mut() {
            let _ = f.write_all(buf);
        }
        if enabled() {
            for line in String::from_utf8_lossy(buf).lines() {
                push("launcher", "rdpio", line);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracing_lines_come_apart() {
        let (t, l, g, x) = parse("2026-10-18T09:12:34.123456Z  INFO rdpio::hyper_client: Hyper direct: signing in addr=1.2.3.4:5");
        assert_eq!((t, l, g, x), ("2026-10-18T09:12:34.123456Z", "INFO", "hyper_client", "Hyper direct: signing in addr=1.2.3.4:5"));
        let (_, l, g, _) = parse("2026-10-18T09:12:34.123456Z  INFO perf: hyper shown_fps=60");
        assert_eq!((l, g), ("INFO", "perf"));
        // Not a tracing line: kept whole.
        assert_eq!(parse("thread 'main' panicked at x"), ("", "", "", "thread 'main' panicked at x"));
        assert_eq!(strip_ansi("\x1b[32m INFO\x1b[0m x"), " INFO x");
    }

    #[test]
    fn the_ring_hands_out_what_is_new() {
        ON.store(true, Ordering::Release);
        let before = last_seq();
        push("a", "A", "2026-10-18T09:12:34.123456Z  WARN rdpio::x: one");
        push_plain("a", "A", "ERROR", "two");
        let got = since(before, 100);
        let mine: Vec<_> = got.iter().filter(|l| l.src == "a").map(|l| (l.level.as_str(), l.text.as_str())).collect();
        assert_eq!(mine, [("WARN", "one"), ("ERROR", "two")]);
        assert!(since(last_seq(), 100).is_empty());
    }
}
