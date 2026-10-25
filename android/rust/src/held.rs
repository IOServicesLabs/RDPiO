//! What the app holds down (released when it loses focus: the key-ups go
//! elsewhere), and the stand-ins for the keys Android keeps for itself.
//! Shared by the Hyper and the Windows 365 sessions.

use std::collections::{BTreeMap, BTreeSet};

use rdp_hyper::input::{InputEvent, TouchPhase};

#[derive(Default)]
pub struct Held {
    keys: BTreeSet<(u16, bool)>,
    buttons: BTreeSet<u8>,
    fingers: BTreeSet<u8>,
    /// Keys pressed as something else (see [`Held::remap`]): what a repeat
    /// of the key sends, and what its release sends.
    remapped: BTreeMap<(u16, bool), (Vec<InputEvent>, Vec<InputEvent>)>,
    /// Releases not passed on (an Alt already lifted for Alt+Home).
    swallow: BTreeSet<(u16, bool)>,
}

impl Held {
    /// What to send for `ev` (remapped), noting what it holds or lets go.
    pub fn apply(&mut self, ev: InputEvent) -> Vec<InputEvent> {
        let out = self.remap(ev);
        for ev in &out {
            match *ev {
                InputEvent::Key { scancode, extended, down: true } => {
                    self.keys.insert((scancode, extended));
                }
                InputEvent::Key { scancode, extended, down: false } => {
                    self.keys.remove(&(scancode, extended));
                }
                InputEvent::Button { button, down: true } => {
                    self.buttons.insert(button);
                }
                InputEvent::Button { button, down: false } => {
                    self.buttons.remove(&button);
                }
                InputEvent::Touch { id, phase: TouchPhase::Down, .. } => {
                    self.fingers.insert(id);
                }
                InputEvent::Touch { id, phase: TouchPhase::Up | TouchPhase::Cancel, .. } => {
                    self.fingers.remove(&id);
                }
                _ => {}
            }
        }
        out
    }

    /// Releases for everything held.
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        let keys = std::mem::take(&mut self.keys).into_iter().map(|(scancode, extended)| InputEvent::Key { scancode, extended, down: false });
        let buttons = std::mem::take(&mut self.buttons).into_iter().map(|button| InputEvent::Button { button, down: false });
        let fingers = std::mem::take(&mut self.fingers).into_iter().map(|id| InputEvent::Touch { id, phase: TouchPhase::Cancel, x: 0, y: 0 });
        let out = keys.chain(buttons).chain(fingers).collect();
        self.remapped.clear();
        self.swallow.clear();
        out
    }

    /// A key as the PC gets it. Android keeps Alt+Tab, the Windows key and
    /// some Meta shortcuts for itself, so these stand in for them (Windows
    /// App's own): Ctrl+Alt+End or Ctrl+Alt+Del = Ctrl+Alt+Del, Alt+Page Up =
    /// Alt+Tab, Alt+Page Down = Alt+Shift+Tab, Alt+Home = Start.
    fn remap(&mut self, ev: InputEvent) -> Vec<InputEvent> {
        let InputEvent::Key { scancode, extended, down } = ev else { return vec![ev] };
        let key = (scancode, extended);
        let k = |scancode, extended, down| InputEvent::Key { scancode, extended, down };
        if !down {
            if let Some((_, up)) = self.remapped.remove(&key) {
                return up;
            }
            if self.swallow.remove(&key) {
                return Vec::new();
            }
            return vec![ev];
        }
        if let Some((again, _)) = self.remapped.get(&key) {
            // Held (Android's key repeat): Alt+Page Up steps through the windows.
            return again.clone();
        }
        let held = |sc: u16| self.keys.iter().any(|k| k.0 == sc);
        let (ctrl, alt, shift) = (held(0x1D), held(0x38), held(0x2A) || held(0x36));
        let (now, again, up) = match key {
            // End or Delete (or the keypad's) with Ctrl+Alt.
            (0x4F | 0x53, _) if ctrl && alt => (vec![InputEvent::SecureAttention], Vec::new(), Vec::new()),
            (0x49, true) if alt && !ctrl => (vec![k(0x0F, false, true)], vec![k(0x0F, false, true)], vec![k(0x0F, false, false)]),
            (0x51, true) if alt && !ctrl => {
                let tab = vec![k(0x0F, false, true)];
                if shift {
                    (tab.clone(), tab, vec![k(0x0F, false, false)])
                } else {
                    (vec![k(0x2A, false, true), k(0x0F, false, true)], tab, vec![k(0x0F, false, false), k(0x2A, false, false)])
                }
            }
            (0x47, true) if alt && !ctrl => {
                // Lift Alt first (Alt+Win isn't Start), with a Ctrl tap so
                // the lone Alt doesn't open the app's menu bar.
                let mut now = vec![k(0x1D, false, true), k(0x1D, false, false)];
                let alts: Vec<_> = self.keys.iter().copied().filter(|k| k.0 == 0x38).collect();
                for (sc, ext) in alts {
                    now.push(k(sc, ext, false));
                    self.swallow.insert((sc, ext));
                }
                now.extend([k(0x5B, true, true), k(0x5B, true, false)]);
                (now, Vec::new(), Vec::new())
            }
            _ => return vec![ev],
        };
        self.remapped.insert(key, (again, up));
        now
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(scancode: u16, extended: bool, down: bool) -> InputEvent {
        InputEvent::Key { scancode, extended, down }
    }

    #[test]
    fn alt_page_up_is_alt_tab_and_releases_cleanly() {
        let mut h = Held::default();
        h.apply(key(0x38, false, true));
        assert_eq!(h.apply(key(0x49, true, true)), vec![key(0x0F, false, true)]);
        assert_eq!(h.apply(key(0x49, true, true)), vec![key(0x0F, false, true)]);
        assert_eq!(h.apply(key(0x49, true, false)), vec![key(0x0F, false, false)]);
        assert_eq!(h.release_all(), vec![key(0x38, false, false)]);
    }
}
