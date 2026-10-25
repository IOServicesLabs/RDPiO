//! UDP packet framing: a 12-byte cleartext header, then the AES-256-GCM sealed
//! body and its 16-byte tag.
//!
//! ```text
//! magic(1) version(1) kind(1) flags(1) seq(8, LE) | ciphertext | tag(16)
//! ```
//!
//! The header is authenticated as associated data. The nonce is the sender's
//! direction tag (4 bytes) followed by its packet sequence number, so the two
//! directions sharing one key never reuse a nonce. Each receiver keeps a
//! sliding replay window: a captured packet (say, a keystroke) replayed by an
//! attacker is dropped.

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};

pub const MAGIC: u8 = 0xD7;
/// The newest Hyper protocol this build speaks. Bumped on any change the
/// other end must know about; sign-in settles on the highest version both
/// speak (see [`crate::update::negotiate`]).
///
/// Compatibility rule: a build speaks the previous version too
/// ([`MIN_VERSION`] = `VERSION - 1` once there is one), so an update can
/// reach the host and its clients in any order; behaviour that differs goes
/// behind `session.version() >= N`. Builds with no version in common still
/// sign in (the auth format never changes) and move the newer build across
/// (see [`crate::update`]).
pub const VERSION: u8 = 4;
/// The oldest Hyper protocol this build speaks.
pub const MIN_VERSION: u8 = 4;
pub const HEADER_LEN: usize = 12;
pub const TAG_LEN: usize = 16;
/// Header plus tag: what every packet costs on top of its body.
pub const OVERHEAD: usize = HEADER_LEN + TAG_LEN;
/// Largest datagram we send. Tailscale's tunnel MTU is 1280 and ZeroTier's
/// underlying path is often ~1400; 1200 bytes never fragments on either (a
/// lost IP fragment loses the whole datagram).
pub const MAX_DATAGRAM: usize = 1200;
/// Largest sealed body that fits in [`MAX_DATAGRAM`].
pub const MAX_BODY: usize = MAX_DATAGRAM - OVERHEAD;

/// What a packet carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// One shard (data or parity) of an encoded video frame.
    Video = 1,
    /// One Opus packet.
    Audio = 2,
    /// Client input events.
    Input = 3,
    /// Small control messages (ping, keyframe request, feedback, bye...).
    Control = 4,
    /// The reliable channel (clipboard, cursor shapes, files).
    Reliable = 6,
    /// Moving an update between builds ([`crate::update::UpdateMsg`]); read by
    /// any protocol version.
    Update = 7,
    /// One shard of the client's camera (H.264, framed as [`Kind::Video`]
    /// is). Sent only once the host asks (`Control::Camera`), so a host that
    /// doesn't know it never gets one.
    Camera = 8,
}

impl Kind {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Video,
            2 => Self::Audio,
            3 => Self::Input,
            4 => Self::Control,
            6 => Self::Reliable,
            7 => Self::Update,
            8 => Self::Camera,
            _ => return None,
        })
    }
}

/// Which way a packet travels; part of the nonce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    HostToClient,
    ClientToHost,
}

impl Direction {
    fn tag(self) -> [u8; 4] {
        match self {
            Self::HostToClient => *b"H2C\0",
            Self::ClientToHost => *b"C2H\0",
        }
    }
    fn reverse(self) -> Self {
        match self {
            Self::HostToClient => Self::ClientToHost,
            Self::ClientToHost => Self::HostToClient,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OpenError {
    #[error("not a hyper packet")]
    NotOurs,
    #[error("unsupported version {0}")]
    Version(u8),
    #[error("unknown packet kind {0}")]
    Kind(u8),
    #[error("authentication failed")]
    Auth,
    #[error("replayed or too old")]
    Replay,
}

/// Packets this far behind the newest one accepted are still accepted once
/// (several threads share a session, so wire order isn't sequence order).
const REPLAY_WINDOW: u64 = 1024;
const WINDOW_WORDS: usize = (REPLAY_WINDOW / 64) as usize;

/// One endpoint's half of the encrypted session: seals what it sends and
/// opens (and replay-checks) what it receives.
pub struct Session {
    key: LessSafeKey,
    /// The protocol version agreed at sign-in.
    version: u8,
    send_dir: Direction,
    send_seq: u64,
    /// Highest sequence number accepted so far, and a bitmap of the
    /// [`REPLAY_WINDOW`] below it (bit `i` = `recv_top - i` seen).
    recv_top: u64,
    recv_window: [u64; WINDOW_WORDS],
    recv_any: bool,
}

impl Session {
    /// `key` is the 32-byte session key from the RDP-side handshake.
    pub fn new(key: &[u8; 32], send_dir: Direction) -> Self {
        Self::with_version(key, send_dir, VERSION)
    }

    /// A session speaking protocol `version` (as agreed at sign-in).
    pub fn with_version(key: &[u8; 32], send_dir: Direction, version: u8) -> Self {
        let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).expect("32-byte key"));
        Self { key, version, send_dir, send_seq: 0, recv_top: 0, recv_window: [0; WINDOW_WORDS], recv_any: false }
    }

    /// The protocol version this session speaks.
    pub fn version(&self) -> u8 {
        self.version
    }

    /// Seal `body` into a datagram of `kind`.
    pub fn seal(&mut self, kind: Kind, body: &[u8]) -> Vec<u8> {
        self.send_seq += 1;
        let seq = self.send_seq;
        let mut out = Vec::with_capacity(HEADER_LEN + body.len() + TAG_LEN);
        out.extend_from_slice(&[MAGIC, self.version, kind as u8, 0]);
        out.extend_from_slice(&seq.to_le_bytes());
        let header: [u8; HEADER_LEN] = out[..HEADER_LEN].try_into().unwrap();
        let mut sealed = body.to_vec();
        self.key
            .seal_in_place_append_tag(nonce(self.send_dir, seq), Aad::from(header), &mut sealed)
            .expect("seal");
        out.extend_from_slice(&sealed);
        out
    }

    /// Authenticate and decrypt a received datagram.
    pub fn open(&mut self, datagram: &[u8]) -> Result<(Kind, Vec<u8>), OpenError> {
        let (kind, seq) = self.check(datagram)?;
        let header: [u8; HEADER_LEN] = datagram[..HEADER_LEN].try_into().unwrap();
        let mut body = datagram[HEADER_LEN..].to_vec();
        let plain_len = self
            .key
            .open_in_place(nonce(self.send_dir.reverse(), seq), Aad::from(header), &mut body)
            .map_err(|_| OpenError::Auth)?
            .len();
        body.truncate(plain_len);
        self.accept(seq);
        Ok((kind, body))
    }

    /// [`Session::open`] without a copy: decrypts within `datagram` (a
    /// receive buffer) and returns the body as part of it.
    pub fn open_in_place<'a>(&mut self, datagram: &'a mut [u8]) -> Result<(Kind, &'a [u8]), OpenError> {
        let (kind, seq) = self.check(datagram)?;
        let (header, body) = datagram.split_at_mut(HEADER_LEN);
        let header: [u8; HEADER_LEN] = (&*header).try_into().unwrap();
        let plain = self
            .key
            .open_in_place(nonce(self.send_dir.reverse(), seq), Aad::from(header), body)
            .map_err(|_| OpenError::Auth)?;
        self.accept(seq);
        Ok((kind, plain))
    }

    /// The header's checks, before decrypting: kind and sequence number.
    fn check(&self, datagram: &[u8]) -> Result<(Kind, u64), OpenError> {
        if datagram.len() < OVERHEAD || datagram[0] != MAGIC {
            return Err(OpenError::NotOurs);
        }
        let kind = Kind::from_u8(datagram[2]).ok_or(OpenError::Kind(datagram[2]))?;
        // Update packets cross versions; everything else is in the session's.
        if datagram[1] != self.version && kind != Kind::Update {
            return Err(OpenError::Version(datagram[1]));
        }
        let seq = u64::from_le_bytes(datagram[4..12].try_into().unwrap());
        if !self.fresh(seq) {
            return Err(OpenError::Replay);
        }
        Ok((kind, seq))
    }

    fn fresh(&self, seq: u64) -> bool {
        if !self.recv_any || seq > self.recv_top {
            return true;
        }
        let back = self.recv_top - seq;
        back < REPLAY_WINDOW && self.recv_window[(back / 64) as usize] & (1u64 << (back % 64)) == 0
    }

    fn accept(&mut self, seq: u64) {
        if !self.recv_any {
            self.recv_any = true;
            self.recv_top = seq;
            self.recv_window = [0; WINDOW_WORDS];
        } else if seq > self.recv_top {
            self.shift(seq - self.recv_top);
            self.recv_top = seq;
        }
        let back = self.recv_top - seq;
        self.recv_window[(back / 64) as usize] |= 1u64 << (back % 64);
    }

    /// Age the window by `n` (bit `i` moves to `i + n`).
    fn shift(&mut self, n: u64) {
        if n >= REPLAY_WINDOW {
            self.recv_window = [0; WINDOW_WORDS];
            return;
        }
        let (words, bits) = ((n / 64) as usize, (n % 64) as u32);
        for w in (0..WINDOW_WORDS).rev() {
            let at = |i: usize| self.recv_window.get(i).copied().unwrap_or(0);
            let mut v = if w >= words { at(w - words) << bits } else { 0 };
            if bits > 0 && w > words {
                v |= at(w - words - 1) >> (64 - bits);
            }
            self.recv_window[w] = v;
        }
    }
}

fn nonce(dir: Direction, seq: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(&dir.tag());
    n[4..].copy_from_slice(&seq.to_le_bytes());
    Nonce::assume_unique_for_key(n)
}

/// A fresh random 32-byte session key.
pub fn random_key() -> [u8; 32] {
    use ring::rand::SecureRandom;
    let mut k = [0u8; 32];
    ring::rand::SystemRandom::new().fill(&mut k).expect("system RNG");
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Session, Session) {
        let key = random_key();
        (Session::new(&key, Direction::HostToClient), Session::new(&key, Direction::ClientToHost))
    }

    #[test]
    fn round_trips_both_ways() {
        let (mut host, mut client) = pair();
        let p = host.seal(Kind::Video, b"frame bytes");
        assert_eq!(client.open(&p).unwrap(), (Kind::Video, b"frame bytes".to_vec()));
        let p = client.seal(Kind::Input, b"keys");
        assert_eq!(host.open(&p).unwrap(), (Kind::Input, b"keys".to_vec()));
    }

    #[test]
    fn tampering_and_wrong_keys_are_rejected() {
        let (mut host, mut client) = pair();
        let mut p = host.seal(Kind::Control, b"hello");
        let last = p.len() - 1;
        p[last] ^= 1;
        assert_eq!(client.open(&p), Err(OpenError::Auth));
        // The header is authenticated too: relabelling the kind fails.
        let mut p = host.seal(Kind::Control, b"hello");
        p[2] = Kind::Input as u8;
        assert_eq!(client.open(&p), Err(OpenError::Auth));
        let mut stranger = Session::new(&random_key(), Direction::ClientToHost);
        let p = host.seal(Kind::Control, b"hello");
        assert_eq!(stranger.open(&p), Err(OpenError::Auth));
    }

    #[test]
    fn only_update_packets_cross_protocol_versions() {
        let key = random_key();
        let mut newer = Session::with_version(&key, Direction::HostToClient, VERSION + 1);
        let mut older = Session::with_version(&key, Direction::ClientToHost, VERSION);
        let p = newer.seal(Kind::Control, b"x");
        assert_eq!(older.open(&p), Err(OpenError::Version(VERSION + 1)));
        let p = newer.seal(Kind::Update, b"chunk");
        assert_eq!(older.open(&p).unwrap(), (Kind::Update, b"chunk".to_vec()));
    }

    #[test]
    fn opens_in_place_like_open() {
        let (mut host, mut client) = pair();
        let mut p = host.seal(Kind::Video, b"frame bytes");
        assert_eq!(client.open_in_place(&mut p).unwrap(), (Kind::Video, &b"frame bytes"[..]));
        // (Replay-checked the same way.)
        let mut again = host.seal(Kind::Video, b"x");
        let copy = again.clone();
        client.open_in_place(&mut again).unwrap();
        assert_eq!(client.open(&copy), Err(OpenError::Replay));
    }

    /// A/B: `cargo test -p rdp-hyper --release -- --ignored --nocapture open_bench`.
    #[test]
    #[ignore]
    fn open_bench() {
        let (mut host, mut client) = pair();
        let body = [7u8; MAX_BODY];
        let packets: Vec<Vec<u8>> = (0..200_000).map(|_| host.seal(Kind::Video, &body)).collect();
        let t = std::time::Instant::now();
        let mut n = 0usize;
        for p in &packets[..100_000] {
            n += client.open(p).unwrap().1.len();
        }
        let copy = t.elapsed();
        let mut bufs: Vec<Vec<u8>> = packets[100_000..].to_vec();
        let t = std::time::Instant::now();
        for p in &mut bufs {
            n += client.open_in_place(p).unwrap().1.len();
        }
        let in_place = t.elapsed();
        println!("open: {:.0} ns/packet, open_in_place: {:.0} ns/packet ({n} bytes)", copy.as_nanos() as f64 / 1e5, in_place.as_nanos() as f64 / 1e5);
    }

    #[test]
    fn a_packet_cannot_be_reflected_back_to_its_sender() {
        let (mut host, _) = pair();
        let p = host.seal(Kind::Control, b"hello");
        assert_eq!(host.open(&p), Err(OpenError::Auth));
    }

    #[test]
    fn replays_are_dropped_but_reordering_is_fine() {
        let (mut host, mut client) = pair();
        let a = host.seal(Kind::Video, b"a");
        let b = host.seal(Kind::Video, b"b");
        let c = host.seal(Kind::Video, b"c");
        assert!(client.open(&c).is_ok());
        assert!(client.open(&a).is_ok(), "late but new");
        assert_eq!(client.open(&a), Err(OpenError::Replay));
        assert!(client.open(&b).is_ok());
        assert_eq!(client.open(&c), Err(OpenError::Replay));
    }

    #[test]
    fn the_replay_window_spans_a_thousand_packets() {
        let (mut host, mut client) = pair();
        let late = host.seal(Kind::Audio, b"late");
        let mut burst: Vec<Vec<u8>> = (0..1000).map(|_| host.seal(Kind::Video, b"v")).collect();
        for p in &burst {
            assert!(client.open(p).is_ok());
        }
        assert!(client.open(&late).is_ok(), "1000 behind is still accepted once");
        assert_eq!(client.open(&late), Err(OpenError::Replay));
        // Every packet of the burst stays a replay after more traffic.
        for _ in 0..10 {
            burst.push(host.seal(Kind::Video, b"v"));
            assert!(client.open(burst.last().unwrap()).is_ok());
        }
        for p in &burst[20..] {
            assert_eq!(client.open(p), Err(OpenError::Replay));
        }
        // Too old to tell: refused.
        let ancient = host.seal(Kind::Audio, b"old");
        for _ in 0..REPLAY_WINDOW + 5 {
            assert!(client.open(&host.seal(Kind::Video, b"v")).is_ok());
        }
        assert_eq!(client.open(&ancient), Err(OpenError::Replay));
    }

    #[test]
    fn a_full_body_fits_the_datagram_budget() {
        let (mut host, _) = pair();
        assert_eq!(host.seal(Kind::Video, &[0u8; MAX_BODY]).len(), MAX_DATAGRAM);
    }
}
