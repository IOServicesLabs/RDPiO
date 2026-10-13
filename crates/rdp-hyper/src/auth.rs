//! Direct-mode sign-in: the Hyper password, without RDP or Windows credentials.
//!
//! SPAKE2 (a password-authenticated key exchange) runs over the password's
//! PBKDF2 secret: each side proves it knows the secret without sending it, an
//! eavesdropper learns nothing they could brute-force offline, and an active
//! attacker gets exactly one guess per exchange — which [`Lockout`] rations.
//! The SPAKE2 key and a hash of both messages as sent (so tampering with the
//! cleartext want/offer makes the keys differ) give the session's AES-GCM
//! key; the client's first sealed packet is the proof (a wrong password can't
//! seal one the host can open). The challenge echoes the hello's nonce, so a
//! late challenge for an earlier attempt is ignored, not mistaken.
//!
//! Auth packets are the only cleartext ones: the usual 12-byte header with
//! kind [`AUTH_KIND`] and a sequence number of 0, then the body.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use ring::{digest, hkdf, hmac, pbkdf2};
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::wire::{HEADER_LEN, MAGIC, VERSION};

/// The wire kind of cleartext auth packets.
pub const AUTH_KIND: u8 = 5;

const PBKDF2_ROUNDS: u32 = 200_000;
const PASSWORD_SALT: &[u8] = b"rdpio hyper password v1";
const ID_CLIENT: &[u8] = b"rdpio hyper client";
const ID_HOST: &[u8] = b"rdpio hyper host";

/// The secret both sides run SPAKE2 over. The host stores only this (never the
/// password); the client derives it from what the user types.
pub fn password_secret(password: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        std::num::NonZeroU32::new(PBKDF2_ROUNDS).unwrap(),
        PASSWORD_SALT,
        password.as_bytes(),
        &mut out,
    );
    out
}

/// What the client wants from the stream (sent in the clear with its hello).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Want {
    pub codecs: Vec<String>,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    /// How many monitors the client has (the host consolidates to match).
    #[serde(default = "one")]
    pub monitors: u32,
    /// Stream at the client's screen size (`width`x`height`): the host scales
    /// its desktop down to fit it — never up; a screen bigger than the host's
    /// desktop upscales on the client.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fit: bool,
    /// Start in office mode (see `Control::Mode`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub office: bool,
    /// Office mode with the encoder's fast preset (see `Control::Mode`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fast_text: bool,
    /// The host's address cookie, echoed back (see [`CookieJar`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookie: Option<String>,
    /// The client screen's display scale in percent (150 = 150 %), 0 if
    /// unknown: a host that makes a display for the client sets it the same,
    /// so text is as big as on the client's own desktop.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub scale: u32,
    /// The client's rdpio (`0.2.0`) and build, for updates.
    #[serde(default)]
    pub app: String,
    #[serde(default)]
    pub build: String,
    /// The Hyper protocol versions it speaks, oldest and newest (builds from
    /// before this field spoke 3 only).
    #[serde(default = "protocol_3")]
    pub protocols: (u8, u8),
}

fn one() -> u32 {
    1
}

fn protocol_3() -> (u8, u8) {
    (3, 3)
}

fn three() -> u8 {
    3
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

/// What the host will stream.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Offer {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: String,
    /// The protocol version the session speaks; 0 when the two builds have
    /// none in common — then the session only moves an update (see `update`).
    #[serde(default = "three")]
    pub protocol: u8,
    /// The host's rdpio and build.
    #[serde(default)]
    pub app: String,
    #[serde(default)]
    pub build: String,
    /// The host has a newer rdpio for the client, signed, to fetch over the
    /// session ([`crate::update`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub update: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMsg {
    Hello { nonce: [u8; 16], spake: Vec<u8>, want: Want },
    /// `echo` is the nonce of the hello it answers.
    Challenge { nonce: [u8; 16], echo: [u8; 16], spake: Vec<u8>, offer: Offer },
    /// Too many failures from this address; try again after this long.
    Denied { retry_after_s: u16 },
    /// Host → client: prove you receive at your address — say hello again
    /// with this cookie.
    Cookie { cookie: [u8; 16] },
}

impl AuthMsg {
    /// A whole datagram (header + body).
    pub fn to_datagram(&self) -> Vec<u8> {
        let mut o = vec![MAGIC, VERSION, AUTH_KIND, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let (tag, nonce, spake, json) = match self {
            Self::Hello { nonce, spake, want } => {
                o.push(1);
                (1u8, nonce, spake, serde_json::to_vec(want).unwrap())
            }
            Self::Challenge { nonce, echo, spake, offer } => {
                o.push(2);
                o.extend_from_slice(echo);
                (2u8, nonce, spake, serde_json::to_vec(offer).unwrap())
            }
            Self::Denied { retry_after_s } => {
                o.push(3);
                o.extend_from_slice(&retry_after_s.to_le_bytes());
                return o;
            }
            Self::Cookie { cookie } => {
                o.push(4);
                o.extend_from_slice(cookie);
                return o;
            }
        };
        let _ = tag;
        o.extend_from_slice(nonce);
        o.extend_from_slice(&(spake.len() as u16).to_le_bytes());
        o.extend_from_slice(spake);
        o.extend_from_slice(&json);
        o
    }

    /// Parse a datagram, if it is an auth packet.
    pub fn parse(d: &[u8]) -> Option<Self> {
        if d.len() <= HEADER_LEN || d[0] != MAGIC || d[1] < FROZEN_SINCE || d[2] != AUTH_KIND {
            return None;
        }
        let b = &d[HEADER_LEN..];
        match b[0] {
            3 => Some(Self::Denied { retry_after_s: u16::from_le_bytes(b.get(1..3)?.try_into().ok()?) }),
            4 => Some(Self::Cookie { cookie: b.get(1..17)?.try_into().ok()? }),
            tag @ (1 | 2) => {
                // A challenge carries the echoed hello nonce first.
                let (echo, b) = if tag == 2 { (Some(b.get(1..17)?), &b[16..]) } else { (None, b) };
                let nonce: [u8; 16] = b.get(1..17)?.try_into().ok()?;
                let len = u16::from_le_bytes(b.get(17..19)?.try_into().ok()?) as usize;
                let spake = b.get(19..19 + len)?.to_vec();
                let json = b.get(19 + len..)?;
                Some(match echo {
                    None => Self::Hello { nonce, spake, want: serde_json::from_slice(json).ok()? },
                    Some(e) => Self::Challenge {
                        nonce,
                        echo: e.try_into().ok()?,
                        spake,
                        offer: serde_json::from_slice(json).ok()?,
                    },
                })
            }
            _ => None,
        }
    }
}

/// Is this datagram an auth packet (of any protocol version)?
pub fn is_auth(d: &[u8]) -> bool {
    d.len() > HEADER_LEN && d[0] == MAGIC && d[2] == AUTH_KIND
}

/// Auth packets keep one format from this protocol version on (the header's
/// version byte is the sender's newest protocol; the hello and offer carry
/// the ranges), so any two builds since can sign in to each other.
pub const FROZEN_SINCE: u8 = 4;

/// An auth packet from a build older than [`FROZEN_SINCE`] (which can't
/// sign in to this one, nor take an update from it): its version. Each side
/// reports these instead of going silent.
pub fn foreign_version(d: &[u8]) -> Option<u8> {
    (is_auth(d) && d[1] < FROZEN_SINCE).then(|| d[1])
}

/// Equal, in time independent of where they differ.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn random<const N: usize>() -> [u8; N] {
    use ring::rand::SecureRandom;
    let mut b = [0u8; N];
    ring::rand::SystemRandom::new().fill(&mut b).expect("system RNG");
    b
}

/// The session key from the SPAKE2 key and the transcript: both datagrams
/// exactly as sent, so altering anything in either (the want, the offer, a
/// nonce) leaves the two sides with different keys.
fn session_key(spake_key: &[u8], hello: &[u8], challenge: &[u8]) -> [u8; 32] {
    let mut t = digest::Context::new(&digest::SHA256);
    t.update(&(hello.len() as u32).to_le_bytes());
    t.update(hello);
    t.update(challenge);
    let transcript = t.finish();
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, transcript.as_ref()).extract(spake_key);
    let mut key = [0u8; 32];
    prk.expand(&[b"rdpio hyper session v2"], hkdf::HKDF_SHA256)
        .and_then(|okm| okm.fill(&mut key))
        .expect("hkdf");
    key
}

/// Client half: create the hello, then turn the host's challenge into a key.
pub struct ClientAuth {
    state: Spake2<Ed25519Group>,
    nonce: [u8; 16],
    /// The hello datagram as sent (half the transcript).
    hello: Vec<u8>,
}

impl ClientAuth {
    /// Start a sign-in: the state, and the hello datagram to send.
    pub fn start(secret: &[u8; 32], want: Want) -> (Self, Vec<u8>) {
        let (state, spake) = Spake2::<Ed25519Group>::start_a(
            &Password::new(secret),
            &Identity::new(ID_CLIENT),
            &Identity::new(ID_HOST),
        );
        let nonce = random();
        let hello = AuthMsg::Hello { nonce, spake, want }.to_datagram();
        (Self { state, nonce, hello: hello.clone() }, hello)
    }

    /// Whether a parsed challenge answers this attempt's hello (a late one
    /// for an earlier attempt doesn't).
    pub fn answers(&self, msg: &AuthMsg) -> bool {
        matches!(msg, AuthMsg::Challenge { echo, .. } if ct_eq(echo, &self.nonce))
    }

    /// The session key from the host's challenge datagram (correct only if
    /// the passwords matched and nobody altered either message — the host's
    /// first reply to a sealed packet is the confirmation).
    pub fn finish(self, challenge: &[u8]) -> Option<[u8; 32]> {
        let msg = AuthMsg::parse(challenge)?;
        if !self.answers(&msg) {
            return None;
        }
        let AuthMsg::Challenge { spake, .. } = msg else { return None };
        let k = self.state.finish(&spake).ok()?;
        Some(session_key(&k, &self.hello, challenge))
    }
}

/// Host half: answer a hello datagram with a challenge datagram and the
/// candidate session key.
pub fn host_respond(secret: &[u8; 32], hello: &[u8], offer: Offer) -> Option<(Vec<u8>, [u8; 32])> {
    let AuthMsg::Hello { nonce: echo, spake: hello_spake, .. } = AuthMsg::parse(hello)? else { return None };
    let (state, spake) = Spake2::<Ed25519Group>::start_b(
        &Password::new(secret),
        &Identity::new(ID_CLIENT),
        &Identity::new(ID_HOST),
    );
    let k = state.finish(&hello_spake).ok()?;
    let challenge = AuthMsg::Challenge { nonce: random(), echo, spake, offer }.to_datagram();
    let key = session_key(&k, hello, &challenge);
    Some((challenge, key))
}

/// Stateless proof that a client receives packets at its source address (the
/// idea of DTLS's HelloVerifyRequest / QUIC's Retry). A hello without a valid
/// cookie gets only a cookie back — no SPAKE2 work, no state, no lockout entry
/// — so spoofed hellos can neither burn the host's CPU nor lock a real client
/// out by impersonating its address.
pub struct CookieJar {
    key: hmac::Key,
}

/// Cookies are minted per 30 s bucket; the current and previous are accepted.
const COOKIE_BUCKET_S: u64 = 30;

impl CookieJar {
    pub fn new() -> Self {
        Self { key: hmac::Key::new(hmac::HMAC_SHA256, &random::<32>()) }
    }

    fn mint(&self, addr: SocketAddr, bucket: u64) -> [u8; 16] {
        let mut msg = Vec::with_capacity(32);
        match addr.ip() {
            IpAddr::V4(v4) => msg.extend_from_slice(&v4.octets()),
            IpAddr::V6(v6) => msg.extend_from_slice(&v6.octets()),
        }
        msg.extend_from_slice(&addr.port().to_le_bytes());
        msg.extend_from_slice(&bucket.to_le_bytes());
        let tag = hmac::sign(&self.key, &msg);
        tag.as_ref()[..16].try_into().unwrap()
    }

    /// A cookie for `addr` at time `now_s` (seconds, any monotonic origin).
    pub fn make(&self, addr: SocketAddr, now_s: u64) -> [u8; 16] {
        self.mint(addr, now_s / COOKIE_BUCKET_S)
    }

    pub fn check(&self, addr: SocketAddr, cookie: &[u8; 16], now_s: u64) -> bool {
        let b = now_s / COOKIE_BUCKET_S;
        [b, b.saturating_sub(1)].iter().any(|&b| ct_eq(&self.mint(addr, b), cookie))
    }
}

impl Default for CookieJar {
    fn default() -> Self {
        Self::new()
    }
}

/// Hex helpers for carrying the cookie in [`Want`].
pub fn cookie_to_hex(c: &[u8; 16]) -> String {
    c.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn cookie_from_hex(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Failed-attempt accounting per source address. Five strikes block the
/// address for 30 s, doubling with each further failure (capped at an hour);
/// a success clears it. Once [`MAX_TRACKED`] addresses are blocked at the
/// same time, further addresses share one overflow entry, so thousands of
/// sources can't make a full table a free pass for fresh ones.
#[derive(Default)]
pub struct Lockout {
    by_ip: HashMap<IpAddr, Strikes>,
    overflow: Strikes,
}

type Strikes = (u32, Option<Instant>);

fn strike(e: &mut Strikes, now: Instant) {
    e.0 += 1;
    if e.0 >= FREE_FAILURES {
        let n = (e.0 - FREE_FAILURES).min(10);
        e.1 = Some(now + (FIRST_BLOCK * 2u32.pow(n)).min(MAX_BLOCK));
    }
}

const FREE_FAILURES: u32 = 5;
/// Addresses tracked at most.
const MAX_TRACKED: usize = 4096;
const FIRST_BLOCK: Duration = Duration::from_secs(30);
const MAX_BLOCK: Duration = Duration::from_secs(3600);

impl Lockout {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Err(remaining)` if `ip` is blocked.
    pub fn check(&self, ip: IpAddr, now: Instant) -> Result<(), Duration> {
        let e = self.by_ip.get(&ip).or((self.by_ip.len() >= MAX_TRACKED).then_some(&self.overflow));
        match e {
            Some((_, Some(until))) if *until > now => Err(*until - now),
            _ => Ok(()),
        }
    }

    pub fn fail(&mut self, ip: IpAddr, now: Instant) {
        // Bounded: keep only addresses currently blocked once the table is big.
        if self.by_ip.len() >= MAX_TRACKED && !self.by_ip.contains_key(&ip) {
            self.by_ip.retain(|_, (_, until)| until.is_some_and(|u| u > now));
            if self.by_ip.len() >= MAX_TRACKED {
                strike(&mut self.overflow, now);
                return;
            }
        }
        strike(self.by_ip.entry(ip).or_insert((0, None)), now);
    }

    pub fn succeed(&mut self, ip: IpAddr) {
        self.by_ip.remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Direction, Kind, Session};

    /// `fit` is left out unless set, and a hello without it reads as unset.
    #[test]
    fn fit_is_optional_on_the_wire() {
        let plain = serde_json::to_string(&want()).unwrap();
        assert!(!plain.contains("fit"), "{plain}");
        let fitted = Want { fit: true, ..want() };
        let json = serde_json::to_string(&fitted).unwrap();
        assert_eq!(serde_json::from_str::<Want>(&json).unwrap(), fitted);
        let old = r#"{"codecs":["h264"],"width":1920,"height":1080,"refresh_hz":144}"#;
        assert!(!serde_json::from_str::<Want>(old).unwrap().fit);
    }

    fn want() -> Want {
        Want {
            codecs: vec!["h264".into()],
            width: 1920,
            height: 1080,
            refresh_hz: 144,
            monitors: 1,
            app: "0.2.0".into(),
            protocols: (4, 4),
            ..Default::default()
        }
    }

    /// Hellos and offers from builds before versions were exchanged read as
    /// protocol 3 (so they get no update and no stream, not a broken one).
    #[test]
    fn handshakes_without_versions_read_as_protocol_3() {
        let old = r#"{"codecs":["h264"],"width":1920,"height":1080,"refresh_hz":144}"#;
        assert_eq!(serde_json::from_str::<Want>(old).unwrap().protocols, (3, 3));
        let old = r#"{"width":1,"height":1,"fps":60,"codec":"h264"}"#;
        let o = serde_json::from_str::<Offer>(old).unwrap();
        assert_eq!((o.protocol, o.update), (3, false));
    }

    #[test]
    fn cookies_bind_to_the_address_and_expire() {
        let jar = CookieJar::new();
        let a: SocketAddr = "10.0.0.5:5000".parse().unwrap();
        let b: SocketAddr = "10.0.0.5:5001".parse().unwrap();
        let c = jar.make(a, 1000);
        assert!(jar.check(a, &c, 1000));
        assert!(jar.check(a, &c, 1000 + 30), "previous bucket still accepted");
        assert!(!jar.check(a, &c, 1000 + 90), "expired");
        assert!(!jar.check(b, &c, 1000), "other port");
        assert!(!CookieJar::new().check(a, &c, 1000), "other host key");
        assert_eq!(cookie_from_hex(&cookie_to_hex(&c)), Some(c));
        let d = AuthMsg::Cookie { cookie: c }.to_datagram();
        assert_eq!(AuthMsg::parse(&d), Some(AuthMsg::Cookie { cookie: c }));
    }
    fn offer() -> Offer {
        Offer { width: 2560, height: 1440, fps: 144, codec: "h264".into(), protocol: 4, app: "0.2.0".into(), ..Default::default() }
    }

    /// Run a sign-in; `tamper` may alter the challenge datagram in flight.
    fn handshake_with(client_pw: &str, host_pw: &str, tamper: impl Fn(&mut Vec<u8>)) -> bool {
        let (client, hello) = ClientAuth::start(&password_secret(client_pw), want());
        let AuthMsg::Hello { want: w, .. } = AuthMsg::parse(&hello).unwrap() else { panic!() };
        assert_eq!(w, want());
        let (mut challenge, host_key) = host_respond(&password_secret(host_pw), &hello, offer()).unwrap();
        let AuthMsg::Challenge { offer: o, .. } = AuthMsg::parse(&challenge).unwrap() else { panic!() };
        assert_eq!(o, offer());
        tamper(&mut challenge);
        let Some(client_key) = client.finish(&challenge) else { return false };
        // The proof: the client's first sealed packet opens only with a matching key.
        let mut c = Session::new(&client_key, Direction::ClientToHost);
        let mut h = Session::new(&host_key, Direction::HostToClient);
        h.open(&c.seal(Kind::Control, b"ping")).is_ok()
    }

    fn handshake(client_pw: &str, host_pw: &str) -> bool {
        handshake_with(client_pw, host_pw, |_| {})
    }

    #[test]
    fn a_tampered_offer_breaks_the_key() {
        assert!(!handshake_with("correct horse battery", "correct horse battery", |c| {
            let at = c.windows(4).position(|w| w == b"h264").unwrap();
            c[at..at + 4].copy_from_slice(b"hevc");
        }));
    }

    #[test]
    fn a_challenge_for_another_attempt_is_ignored() {
        let s = password_secret("pw");
        let (first, hello1) = ClientAuth::start(&s, want());
        let (second, _hello2) = ClientAuth::start(&s, want());
        let (late, _) = host_respond(&s, &hello1, offer()).unwrap();
        let late_msg = AuthMsg::parse(&late).unwrap();
        assert!(first.answers(&late_msg));
        assert!(!second.answers(&late_msg));
        assert!(second.finish(&late).is_none());
    }

    #[test]
    fn other_versions_are_recognised() {
        let mut d = AuthMsg::Denied { retry_after_s: 0 }.to_datagram();
        assert_eq!(foreign_version(&d), None);
        // A newer build's auth packets read the same (the format is frozen)…
        d[1] = VERSION + 1;
        assert_eq!(foreign_version(&d), None);
        assert_eq!(AuthMsg::parse(&d), Some(AuthMsg::Denied { retry_after_s: 0 }));
        // …a build from before that can't sign in.
        d[1] = 3;
        assert_eq!(foreign_version(&d), Some(3));
        assert_eq!(AuthMsg::parse(&d), None);
        assert_eq!(foreign_version(&[1]), None, "junk");
    }

    #[test]
    fn a_full_lockout_table_still_blocks_fresh_addresses() {
        let mut l = Lockout::new();
        let t = Instant::now();
        for i in 0..MAX_TRACKED as u32 {
            let ip = IpAddr::from((0x0a00_0000u32 + i).to_be_bytes());
            for _ in 0..FREE_FAILURES {
                l.fail(ip, t);
            }
        }
        let fresh: IpAddr = "192.168.7.7".parse().unwrap();
        for _ in 0..FREE_FAILURES {
            l.fail(fresh, t);
        }
        assert!(l.check(fresh, t).is_err());
    }

    #[test]
    fn the_right_password_yields_a_shared_key() {
        assert!(handshake("correct horse battery", "correct horse battery"));
    }

    #[test]
    fn a_wrong_password_cannot_seal_a_packet() {
        assert!(!handshake("correct horse battery", "Correct horse battery"));
    }

    #[test]
    fn each_session_gets_a_fresh_key() {
        let s = password_secret("pw");
        let (c1, h1) = ClientAuth::start(&s, want());
        let (c2, h2) = ClientAuth::start(&s, want());
        let (_, k1) = host_respond(&s, &h1, offer()).unwrap();
        let (_, k2) = host_respond(&s, &h2, offer()).unwrap();
        assert_ne!(k1, k2);
        drop((c1, c2));
    }

    #[test]
    fn auth_packets_are_recognised_and_denials_round_trip() {
        let d = AuthMsg::Denied { retry_after_s: 90 }.to_datagram();
        assert!(is_auth(&d));
        assert_eq!(AuthMsg::parse(&d), Some(AuthMsg::Denied { retry_after_s: 90 }));
        let mut sealed = Session::new(&[1; 32], Direction::ClientToHost);
        assert!(!is_auth(&sealed.seal(Kind::Control, b"x")));
    }

    #[test]
    fn repeated_failures_lock_an_address_out_and_success_clears_it() {
        let mut l = Lockout::new();
        let ip: IpAddr = "10.0.0.5".parse().unwrap();
        let t = Instant::now();
        for _ in 0..4 {
            l.fail(ip, t);
            assert!(l.check(ip, t).is_ok());
        }
        l.fail(ip, t);
        assert!(l.check(ip, t).is_err());
        assert!(l.check(ip, t + Duration::from_secs(31)).is_ok());
        l.fail(ip, t);
        assert!(l.check(ip, t + Duration::from_secs(31)).is_err(), "second block is longer");
        assert!(l.check("10.0.0.6".parse().unwrap(), t).is_ok(), "other addresses unaffected");
        l.succeed(ip);
        assert!(l.check(ip, t).is_ok());
    }
}
