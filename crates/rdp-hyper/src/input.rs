//! Client input → host, over lossy UDP without retransmission delay.
//!
//! Every event gets a sequence number. Each input packet carries the events
//! the host hasn't acknowledged yet, oldest first, so a lost packet's events
//! ride the next one — a dropped key-up can never leave a key stuck — and the
//! host applies each sequence number exactly once, in order, never skipping
//! one.

/// One input event, in host-neutral terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// A key by hardware scancode (`extended` = the E0 prefix).
    Key { scancode: u16, extended: bool, down: bool },
    /// Absolute pointer position, normalised to 0..=65535 across the stream.
    MouseAbs { x: u16, y: u16 },
    /// Relative pointer motion in device counts.
    MouseRel { dx: i16, dy: i16 },
    /// 0 left, 1 right, 2 middle, 3 back (X1), 4 forward (X2).
    Button { button: u8, down: bool },
    /// Wheel notch in WHEEL_DELTA units; `horizontal` for tilt.
    Wheel { delta: i16, horizontal: bool },
    /// A Unicode character with no scancode (IME text).
    Char { code: u16, down: bool },
    /// Ctrl+Alt+Del — can't be typed as keys; the host raises it itself.
    SecureAttention,
}

impl InputEvent {
    fn write(&self, out: &mut Vec<u8>) {
        match *self {
            Self::Key { scancode, extended, down } => {
                out.push(1);
                out.extend_from_slice(&scancode.to_le_bytes());
                out.push(extended as u8 | (down as u8) << 1);
            }
            Self::MouseAbs { x, y } => {
                out.push(2);
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            Self::MouseRel { dx, dy } => {
                out.push(3);
                out.extend_from_slice(&dx.to_le_bytes());
                out.extend_from_slice(&dy.to_le_bytes());
            }
            Self::Button { button, down } => {
                out.push(4);
                out.push(button);
                out.push(down as u8);
            }
            Self::Wheel { delta, horizontal } => {
                out.push(5);
                out.extend_from_slice(&delta.to_le_bytes());
                out.push(horizontal as u8);
            }
            Self::Char { code, down } => {
                out.push(6);
                out.extend_from_slice(&code.to_le_bytes());
                out.push(down as u8);
            }
            Self::SecureAttention => out.push(7),
        }
    }

    fn read(b: &[u8]) -> Option<(Self, usize)> {
        let u16_at = |i: usize| b.get(i..i + 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
        Some(match *b.first()? {
            1 => {
                let f = *b.get(3)?;
                (Self::Key { scancode: u16_at(1)?, extended: f & 1 != 0, down: f & 2 != 0 }, 4)
            }
            2 => (Self::MouseAbs { x: u16_at(1)?, y: u16_at(3)? }, 5),
            3 => (Self::MouseRel { dx: u16_at(1)? as i16, dy: u16_at(3)? as i16 }, 5),
            4 => (Self::Button { button: *b.get(1)?, down: *b.get(2)? != 0 }, 3),
            5 => (Self::Wheel { delta: u16_at(1)? as i16, horizontal: *b.get(3)? != 0 }, 4),
            6 => (Self::Char { code: u16_at(1)?, down: *b.get(3)? != 0 }, 4),
            7 => (Self::SecureAttention, 1),
            _ => return None,
        })
    }
}

/// Most events carried in one packet: the largest event is 5 bytes after a
/// 5-byte header, within [`crate::wire::MAX_BODY`] (and the count is a byte).
pub const MAX_PER_PACKET: usize = (crate::wire::MAX_BODY - 5) / 5;
const _: () = assert!(MAX_PER_PACKET <= 255);

/// Client side: numbers events and builds packets of everything unacknowledged.
#[derive(Default)]
pub struct InputSender {
    /// `(seq, event)` not yet acknowledged by the host, oldest first.
    pending: Vec<(u32, InputEvent)>,
    next_seq: u32,
    /// Highest sequence number already put in a packet.
    sent: u32,
}

impl InputSender {
    pub fn new() -> Self {
        Self { pending: Vec::new(), next_seq: 1, sent: 0 }
    }

    /// Queue an event. Consecutive relative moves are merged (the host only
    /// needs their sum), as are absolute moves (only the last counts), while
    /// they are still unsent.
    pub fn push(&mut self, ev: InputEvent) {
        let sent = self.sent;
        let unsent = |seq: u32| seq.wrapping_sub(sent) as i32 > 0;
        if let (InputEvent::MouseRel { dx, dy }, Some((seq, InputEvent::MouseRel { dx: px, dy: py }))) =
            (ev, self.pending.last_mut())
        {
            if unsent(*seq) {
                if let (Some(x), Some(y)) = (px.checked_add(dx), py.checked_add(dy)) {
                    *px = x;
                    *py = y;
                    return;
                }
            }
        }
        if let (InputEvent::MouseAbs { .. }, Some((seq, last @ InputEvent::MouseAbs { .. }))) =
            (ev, self.pending.last_mut())
        {
            if unsent(*seq) {
                *last = ev;
                return;
            }
        }
        self.pending.push((self.next_seq, ev));
        self.next_seq = self.next_seq.wrapping_add(1);
    }

    /// The host has applied everything up to and including `seq`.
    pub fn ack(&mut self, seq: u32) {
        self.pending.retain(|(s, _)| s.wrapping_sub(seq) as i32 > 0);
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Packet bodies for the unacknowledged events, oldest first, at most
    /// `max_packets` of them (anything beyond waits for acks to free room).
    /// Empty if nothing is pending.
    pub fn packets(&mut self, max_packets: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for events in self.pending.chunks(MAX_PER_PACKET).take(max_packets) {
            let (first, _) = events[0];
            let mut body = Vec::with_capacity(5 + events.len() * 5);
            body.extend_from_slice(&first.to_le_bytes());
            body.push(events.len() as u8);
            for (_, e) in events {
                e.write(&mut body);
            }
            out.push(body);
            let (last, _) = events[events.len() - 1];
            if last.wrapping_sub(self.sent) as i32 > 0 {
                self.sent = last;
            }
        }
        out
    }

    /// The first packet of [`Self::packets`].
    pub fn packet(&mut self) -> Option<Vec<u8>> {
        self.packets(1).pop()
    }
}

/// Host side: applies each sequence number once, in order.
#[derive(Default)]
pub struct InputReceiver {
    /// Highest sequence number applied.
    applied: u32,
}

impl InputReceiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Highest sequence number applied (sent back as the ack).
    pub fn applied(&self) -> u32 {
        self.applied
    }

    /// Events from `body` not applied before, in order. Stops at a gap (a
    /// packet carrying newer events than the next one due): those come again,
    /// after the missing ones.
    pub fn receive(&mut self, body: &[u8]) -> Vec<InputEvent> {
        let Some(head) = body.get(..5) else {
            return Vec::new();
        };
        let first = u32::from_le_bytes(head[..4].try_into().unwrap());
        let count = head[4] as usize;
        let mut out = Vec::new();
        let mut pos = 5;
        for i in 0..count {
            let Some((ev, used)) = InputEvent::read(&body[pos..]) else {
                break;
            };
            pos += used;
            let seq = first.wrapping_add(i as u32);
            match seq.wrapping_sub(self.applied) as i32 {
                1 => {
                    out.push(ev);
                    self.applied = seq;
                }
                d if d > 1 => break, // a gap: never skip an event
                _ => {}              // applied already
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: InputEvent = InputEvent::Key { scancode: 0x1e, extended: false, down: true };
    const A_UP: InputEvent = InputEvent::Key { scancode: 0x1e, extended: false, down: false };

    #[test]
    fn every_event_kind_round_trips() {
        let all = [
            InputEvent::Key { scancode: 0x48, extended: true, down: true },
            InputEvent::MouseAbs { x: 65535, y: 12 },
            InputEvent::MouseRel { dx: -300, dy: 7 },
            InputEvent::Button { button: 4, down: false },
            InputEvent::Wheel { delta: -120, horizontal: true },
            InputEvent::Char { code: 0x263A, down: true },
            InputEvent::SecureAttention,
        ];
        let mut tx = InputSender::new();
        for e in all {
            tx.push(e);
        }
        let mut rx = InputReceiver::new();
        assert_eq!(rx.receive(&tx.packet().unwrap()), all);
    }

    #[test]
    fn a_lost_packet_is_covered_by_the_next() {
        let mut tx = InputSender::new();
        let mut rx = InputReceiver::new();
        tx.push(A);
        let _lost = tx.packet().unwrap();
        tx.push(A_UP);
        assert_eq!(rx.receive(&tx.packet().unwrap()), vec![A, A_UP]);
        // A duplicate delivers nothing new; acking trims the sender.
        assert!(rx.receive(&tx.packet().unwrap()).is_empty());
        tx.ack(rx.applied());
        assert!(!tx.has_pending());
        assert!(tx.packet().is_none());
    }

    #[test]
    fn unsent_mouse_motion_is_merged() {
        let mut tx = InputSender::new();
        tx.push(InputEvent::MouseRel { dx: 3, dy: -1 });
        tx.push(InputEvent::MouseRel { dx: 4, dy: 2 });
        let mut rx = InputReceiver::new();
        assert_eq!(rx.receive(&tx.packet().unwrap()), vec![InputEvent::MouseRel { dx: 7, dy: 1 }]);
        // Already sent → a new event, not merged into the old one.
        tx.push(InputEvent::MouseRel { dx: 1, dy: 1 });
        assert_eq!(rx.receive(&tx.packet().unwrap()), vec![InputEvent::MouseRel { dx: 1, dy: 1 }]);
    }

    #[test]
    fn a_burst_bigger_than_a_packet_is_sent_oldest_first_and_complete() {
        let mut tx = InputSender::new();
        let mut rx = InputReceiver::new();
        let keys: Vec<InputEvent> =
            (0..600).map(|i| InputEvent::Key { scancode: i as u16, extended: false, down: i % 2 == 0 }).collect();
        for &k in &keys {
            tx.push(k);
        }
        let mut got = Vec::new();
        while tx.has_pending() {
            for p in tx.packets(2) {
                got.extend(rx.receive(&p));
            }
            tx.ack(rx.applied());
        }
        assert_eq!(got, keys);
    }

    #[test]
    fn a_lost_key_up_is_never_skipped() {
        let mut tx = InputSender::new();
        let mut rx = InputReceiver::new();
        tx.push(A);
        rx.receive(&tx.packet().unwrap());
        tx.ack(rx.applied());
        tx.push(A_UP);
        // An outage while the mouse keeps moving: every packet is lost.
        for i in 0..300 {
            tx.push(InputEvent::MouseRel { dx: 1, dy: i as i16 });
            let _lost = tx.packets(2);
        }
        // The first packets through still start with the key-up.
        let mut got = Vec::new();
        for p in tx.packets(2) {
            got.extend(rx.receive(&p));
        }
        assert_eq!(got.first(), Some(&A_UP));
        assert!(got.len() > 1);
    }

    #[test]
    fn a_packet_past_a_gap_applies_nothing() {
        let mut tx = InputSender::new();
        let mut rx = InputReceiver::new();
        for i in 0..(MAX_PER_PACKET + 10) {
            tx.push(InputEvent::Key { scancode: i as u16, extended: false, down: true });
        }
        let ps = tx.packets(2);
        assert_eq!(ps.len(), 2);
        assert!(rx.receive(&ps[1]).is_empty(), "the first packet was lost");
        assert_eq!(rx.receive(&ps[0]).len(), MAX_PER_PACKET);
        assert_eq!(rx.receive(&ps[1]).len(), 10);
    }

    #[test]
    fn truncated_packets_apply_what_parses() {
        let mut tx = InputSender::new();
        tx.push(A);
        tx.push(A_UP);
        let p = tx.packet().unwrap();
        let mut rx = InputReceiver::new();
        assert_eq!(rx.receive(&p[..p.len() - 1]), vec![A]);
    }
}
