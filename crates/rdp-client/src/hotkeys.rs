//! rdpio's keyboard shortcuts, written once: the launcher's Settings list, the
//! hint a session shows when it starts and the notices of the two silent
//! toggles all come from [`TABLE`], and a test checks that docs/hyper.md lists
//! every row. The keys themselves are handled in `window.rs` (sessions) and the
//! launcher page.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// Where a shortcut works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Any session (Remote Desktop or Hyper).
    All,
    /// A Hyper stream (in plain Remote Desktop the keys reach the remote app).
    Hyper,
    /// At the keyboard of the PC being streamed.
    Host,
    /// The launcher window.
    Launcher,
}

impl Scope {
    /// The name the launcher page groups by.
    pub fn name(self) -> &'static str {
        match self {
            Scope::All => "all",
            Scope::Hyper => "hyper",
            Scope::Host => "host",
            Scope::Launcher => "launcher",
        }
    }
}

pub struct Hotkey {
    pub keys: &'static str,
    /// What it does, as the launcher's Settings page says it.
    pub what: &'static str,
    /// Its line in a session's on-screen hint ("": not shown there).
    pub hint: &'static str,
    pub scope: Scope,
    /// It works in a Workspace tab too (a tab's size is the tab's).
    pub in_tab: bool,
}

const fn key(keys: &'static str, what: &'static str, hint: &'static str, scope: Scope) -> Hotkey {
    Hotkey { keys, what, hint, scope, in_tab: true }
}

pub const TABLE: &[Hotkey] = &[
    key("Ctrl+Shift+Q", "Disconnect (a Hyper host locks itself)", "quit", Scope::All),
    key(
        "Ctrl+Shift+M",
        "Lock the mouse in for games (in Hyper, Win and Alt+Tab then go to the host as well). Again to release",
        "lock the mouse in (games)",
        Scope::All,
    ),
    key("Ctrl+Alt+End", "Ctrl+Alt+Del on the remote PC", "Ctrl+Alt+Del on the host", Scope::All),
    key(
        "Ctrl+Shift+K",
        "Win, Alt+Tab and Alt+F4 go to the host (the default) or stay on this PC",
        "Alt+Tab, Win: host / this PC",
        Scope::Hyper,
    ),
    key("Ctrl+Shift+O", "Office mode (crisp text) or game mode", "office mode (crisp text) / game", Scope::Hyper),
    key("Ctrl+Shift+S", "Stats: frame rate, latency, bitrate, loss", "stats", Scope::Hyper),
    Hotkey { keys: "Ctrl+Shift+F", what: "Full screen or a window", hint: "fullscreen / window", scope: Scope::Hyper, in_tab: false },
    key("Ctrl+Alt+F12", "Take the PC back: the stream ends and its own keyboard and screen have it again", "", Scope::Host),
    key("Ctrl+N", "New connection", "", Scope::Launcher),
    key("Ctrl+F", "Find a PC (on Activity: filter the log)", "", Scope::Launcher),
    key("Ctrl+Tab", "Workspace: the next tab (with Shift, the one before)", "", Scope::Launcher),
    key("Ctrl+W", "Workspace: close the tab", "", Scope::Launcher),
    key("Ctrl+B", "Workspace: hide the list, so the session gets the window (again to show it)", "", Scope::Launcher),
];

/// The hint a session shows when it starts: its own keys, one per line
/// (`hyper`: a Hyper stream; `embedded`: in a Workspace tab).
pub fn hint_lines(hyper: bool, embedded: bool) -> Vec<String> {
    TABLE
        .iter()
        .filter(|k| !k.hint.is_empty())
        .filter(|k| k.scope == Scope::All || (hyper && k.scope == Scope::Hyper))
        .filter(|k| k.in_tab || !embedded)
        .map(|k| format!("{:<15}{}", k.keys, k.hint))
        .collect()
}

/// The list for the launcher page: `[{keys, what, scope}]`.
pub fn json() -> serde_json::Value {
    TABLE
        .iter()
        .map(|k| serde_json::json!({ "keys": k.keys, "what": k.what, "scope": k.scope.name() }))
        .collect()
}

static HINT_SHOWN_RDP: AtomicBool = AtomicBool::new(false);
static HINT_SHOWN_HYPER: AtomicBool = AtomicBool::new(false);

/// Whether this process hasn't shown the hint of this kind of session yet
/// (and from now on it has): a reconnect doesn't show it again, while a
/// Remote Desktop session that hands over to Hyper does show the Hyper keys.
pub fn first_time(hyper: bool) -> bool {
    let shown = if hyper { &HINT_SHOWN_HYPER } else { &HINT_SHOWN_RDP };
    !shown.swap(true, Ordering::Relaxed)
}

const SEEN: u8 = 1;
const CAPTURE: u8 = 2;
const KEYS_TO_HOST: u8 = 4;
const HYPER: u8 = 8;

/// The mouse lock and where the system keys go, as last seen by
/// [`toggle_notice`], and in which kind of session (0: not yet).
static TOGGLES: AtomicU8 = AtomicU8::new(0);

/// A short notice when Ctrl+Shift+M or Ctrl+Shift+K changed something since
/// the last call, which otherwise happens without a word on screen. The first
/// call, and the first after a Remote Desktop session handed over to Hyper,
/// only take note. Cheap: call it once per loop pass.
pub fn toggle_notice(capture: bool, keys_to_host: bool, hyper: bool) -> Option<&'static str> {
    let now = SEEN
        | if capture { CAPTURE } else { 0 }
        | if keys_to_host { KEYS_TO_HOST } else { 0 }
        | if hyper { HYPER } else { 0 };
    let before = TOGGLES.swap(now, Ordering::Relaxed);
    notice_for(before, now)
}

fn notice_for(before: u8, now: u8) -> Option<&'static str> {
    if before & SEEN == 0 || before == now || (before ^ now) & HYPER != 0 {
        return None;
    }
    // (Locking the mouse in also takes the system keys: the lock is the news.)
    if (before ^ now) & CAPTURE != 0 {
        return Some(if now & CAPTURE != 0 { "Mouse locked in: Ctrl+Shift+M frees it" } else { "Mouse free" });
    }
    Some(if now & KEYS_TO_HOST != 0 { "Win, Alt+Tab: the host" } else { "Win, Alt+Tab: this PC" })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One row per shortcut, each with something to say.
    #[test]
    fn the_shortcut_list_has_no_doubles() {
        for (i, k) in TABLE.iter().enumerate() {
            assert!(!k.what.is_empty(), "{}", k.keys);
            assert!(TABLE[..i].iter().all(|o| o.keys != k.keys), "{} is listed twice", k.keys);
            assert!(k.hint.is_empty() || matches!(k.scope, Scope::All | Scope::Hyper), "{}: a hint only sessions show", k.keys);
        }
    }

    /// docs/hyper.md has a table row for every shortcut.
    #[test]
    fn the_docs_list_every_shortcut() {
        let doc = include_str!("../../../docs/hyper.md");
        for k in TABLE {
            assert!(doc.contains(&format!("| {} |", k.keys)), "docs/hyper.md doesn't list {}", k.keys);
        }
    }

    #[test]
    fn each_session_hints_its_own_keys() {
        let rdp = hint_lines(false, false);
        assert_eq!(rdp.len(), 3);
        assert!(rdp.iter().all(|l| !l.starts_with("Ctrl+Shift+S")));
        let hyper = hint_lines(true, false);
        assert_eq!(hyper.len(), 7);
        assert!(hyper.iter().any(|l| l.starts_with("Ctrl+Alt+End")));
        assert!(hyper.iter().any(|l| l.starts_with("Ctrl+Shift+F")));
        // (In a Workspace tab Ctrl+Shift+F does nothing.)
        let tab = hint_lines(true, true);
        assert_eq!(tab.len(), 6);
        assert!(tab.iter().all(|l| !l.starts_with("Ctrl+Shift+F")));
        assert_eq!(hyper[0], "Ctrl+Shift+Q   quit");
    }

    #[test]
    fn the_hint_shows_once_per_kind_of_session() {
        // (The only test touching these statics.)
        assert!(first_time(false));
        assert!(!first_time(false));
        assert!(first_time(true));
        assert!(!first_time(true));
    }

    #[test]
    fn toggles_say_what_changed() {
        let s = |capture: bool, host: bool| SEEN | if capture { CAPTURE } else { 0 } | if host { KEYS_TO_HOST } else { 0 };
        assert_eq!(notice_for(0, s(false, true)), None, "the first look only takes note");
        assert_eq!(notice_for(s(false, true), s(false, true)), None);
        assert_eq!(notice_for(s(false, false), s(true, true)), Some("Mouse locked in: Ctrl+Shift+M frees it"));
        assert_eq!(notice_for(s(true, true), s(false, false)), Some("Mouse free"));
        assert_eq!(notice_for(s(false, true), s(false, false)), Some("Win, Alt+Tab: this PC"));
        assert_eq!(notice_for(s(false, false), s(false, true)), Some("Win, Alt+Tab: the host"));
        // A hand-over to Hyper (whose keys may start on this PC) isn't a toggle.
        assert_eq!(notice_for(s(false, true), s(false, false) | HYPER), None);
        assert_eq!(notice_for(s(false, false) | HYPER, s(true, true) | HYPER), Some("Mouse locked in: Ctrl+Shift+M frees it"));
    }

    #[test]
    fn the_page_gets_every_row() {
        let v = json();
        let rows = v.as_array().unwrap();
        assert_eq!(rows.len(), TABLE.len());
        assert_eq!(rows[0]["scope"], "all");
        assert!(rows.iter().any(|r| r["keys"] == "Ctrl+Alt+F12" && r["scope"] == "host"));
    }
}
