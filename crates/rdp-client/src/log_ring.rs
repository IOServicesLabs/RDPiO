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
/// The once-a-second stats lines ("perf") are kept apart, about ten minutes'
/// worth for one session: they would otherwise push every real line out of
/// [`KEEP`] within the hour.
const KEEP_PERF: usize = 600;

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

/// Both kinds under one lock, so a line's number and its place in its list are
/// decided together and the two lists never need two locks at once.
struct Ring {
    lines: VecDeque<Line>,
    perf: VecDeque<Line>,
}

static RING: Mutex<Ring> = Mutex::new(Ring { lines: VecDeque::new(), perf: VecDeque::new() });
static SEQ: AtomicU64 = AtomicU64::new(0);
static ON: AtomicBool = AtomicBool::new(false);
/// Whether stats lines are wanted (the page's Stats box is ticked): they are
/// always kept, but only then handed out and announced.
static WANT_PERF: AtomicBool = AtomicBool::new(false);
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
    let perf = target == "perf";
    {
        let mut ring = RING.lock().unwrap();
        let (list, cap) = if perf { (&mut ring.perf, KEEP_PERF) } else { (&mut ring.lines, KEEP) };
        if list.len() >= cap {
            list.pop_front();
        }
        list.push_back(Line {
            seq: SEQ.fetch_add(1, Ordering::Relaxed) + 1,
            src: src.into(),
            name: name.into(),
            time: time.into(),
            level: level.into(),
            target: target.into(),
            text: text.into(),
        });
    }
    // A stats line nobody looks at wakes nobody.
    if perf && !WANT_PERF.load(Ordering::Relaxed) {
        return;
    }
    if let Some(n) = NOTIFY.get() {
        n();
    }
}

/// Hand out (and announce) the stats lines too, or stop.
pub fn want_perf(on: bool) {
    WANT_PERF.store(on, Ordering::Relaxed);
}

/// The lines after `seq` (all of them for 0), oldest first, at most `max`
/// (the newest, when there are more). Stats lines only while [`want_perf`].
pub fn since(seq: u64, max: usize) -> Vec<Line> {
    let with_perf = WANT_PERF.load(Ordering::Relaxed);
    let ring = RING.lock().unwrap();
    let tail = |list: &VecDeque<Line>| {
        let start = list.partition_point(|l| l.seq <= seq);
        start.max(list.len().saturating_sub(max))
    };
    let a = ring.lines.range(tail(&ring.lines)..);
    if !with_perf {
        return a.cloned().collect();
    }
    let b = ring.perf.range(tail(&ring.perf)..);
    // Both are in order: merge them, then keep the newest `max`.
    let mut out: Vec<&Line> = Vec::with_capacity(a.len() + b.len());
    let (mut a, mut b) = (a.peekable(), b.peekable());
    loop {
        let next = match (a.peek(), b.peek()) {
            (Some(x), Some(y)) if x.seq < y.seq => a.next(),
            (Some(_), Some(_)) => b.next(),
            (Some(_), None) => a.next(),
            (None, Some(_)) => b.next(),
            (None, None) => break,
        };
        out.extend(next);
    }
    let skip = out.len().saturating_sub(max);
    out[skip..].iter().map(|l| (*l).clone()).collect()
}

/// The newest line's number (0 before any).
#[cfg(test)]
fn last_seq() -> u64 {
    let ring = RING.lock().unwrap();
    let a = ring.lines.back().map_or(0, |l| l.seq);
    let b = ring.perf.back().map_or(0, |l| l.seq);
    a.max(b)
}

/// The last error `src` logged (its text), if any is still kept.
pub fn last_error(src: &str) -> Option<String> {
    RING.lock().unwrap().lines.iter().rev().find(|l| l.src == src && l.level == "ERROR").map(|l| l.text.clone())
}

/// Forget what's kept (numbers go on from where they were).
pub fn clear() {
    let mut ring = RING.lock().unwrap();
    ring.lines.clear();
    ring.perf.clear();
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
                push("launcher", "This window", line);
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

    #[test]
    fn stats_lines_stay_apart_until_asked_for() {
        ON.store(true, Ordering::Release);
        let before = last_seq();
        push("p", "P", "2026-10-18T09:12:34.123456Z  INFO rdpio::session: connected");
        for _ in 0..KEEP_PERF + 5 {
            push("p", "P", "2026-10-18T09:12:35.123456Z  INFO perf: fps=60");
        }
        push_plain("p", "P", "ERROR", "lost the link");
        let mine = |lines: Vec<Line>| lines.into_iter().filter(|l| l.src == "p").map(|l| (l.seq, l.target)).collect::<Vec<_>>();
        // Stats never push the real lines out, and stay hidden unless wanted.
        want_perf(false);
        let real = mine(since(before, 100));
        assert_eq!(real.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(), ["session", ""]);
        assert_eq!(last_error("p").as_deref(), Some("lost the link"));
        want_perf(true);
        let all = mine(since(before, 10_000));
        want_perf(false);
        assert!(all.windows(2).all(|w| w[0].0 < w[1].0), "merged in order");
        assert_eq!(all.iter().filter(|(_, t)| t == "perf").count(), KEEP_PERF);
        assert_eq!(all.first().map(|(_, t)| t.as_str()), Some("session"));
        assert_eq!(all.last().map(|(_, t)| t.as_str()), Some(""));
    }
}
