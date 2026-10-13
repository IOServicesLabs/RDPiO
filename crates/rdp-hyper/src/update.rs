//! Updates: signed release packages, and a host handing its own to a client.
//!
//! A release is `rdpio.exe` plus a [`Signed`] manifest (`rdpio.exe.update`):
//! the exe's version, build, size, SHA-256 and the Hyper protocol versions it
//! speaks, signed with the release key (Ed25519; the private half lives in the
//! release pipeline, the public half is [`RELEASE_KEY`]). Whoever passes a
//! package along — GitHub, or a host to the client signing in to it — needn't
//! be trusted: a package is installed only if its signature checks out, its
//! exe matches the manifest, and it is newer than what runs ([`newer`]; no
//! downgrades).
//!
//! On the wire, [`Kind::Update`](crate::wire::Kind::Update) packets carry
//! [`UpdateMsg`]s. Their format and the auth handshake never change, so two
//! builds without a protocol version in common still sign in and move the
//! update between them ([`Fetch`] on the client, [`Serve`] on the host).

use std::time::{Duration, Instant};

use base64::Engine as _;
use ring::{digest, signature};

/// The release key's public half (Ed25519).
pub const RELEASE_KEY: [u8; 32] = [
    0xdb, 0xe4, 0x25, 0x21, 0xb2, 0xa3, 0xaf, 0x47, 0xc6, 0xd4, 0x4b, 0xd3, 0xbd, 0xfe, 0x4e, 0x7d, 0x84, 0xd3, 0x4b, 0xd9,
    0x06, 0x57, 0x23, 0x56, 0xbf, 0x38, 0x66, 0x81, 0xc7, 0x1d, 0x2d, 0x94,
];

/// What a package is for (a Windows package is never offered to anything else).
pub const PRODUCT: &str = "rdpio-windows-x86_64";

/// The largest exe a manifest may describe (a bound on what a client allocates).
pub const MAX_SIZE: u64 = 256 << 20;

/// Bytes of the exe per [`UpdateMsg::Chunk`] (fits a sealed datagram).
pub const CHUNK: usize = 1152;

/// Chunks one [`UpdateMsg::Get`] asks for at most.
pub const MAX_GET: u16 = 256;

/// What a release is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    pub product: String,
    /// `0.3.0`.
    pub version: String,
    /// `2026.10.12.2050+9c915af` (see the build script).
    pub build: String,
    /// The Hyper protocol versions it speaks.
    pub protocol_min: u8,
    pub protocol_max: u8,
    pub size: u64,
    /// Of the exe, lowercase hex.
    pub sha256: String,
}

impl Manifest {
    /// The manifest of `exe`.
    pub fn of(exe: &[u8], version: &str, build: &str, protocols: (u8, u8)) -> Self {
        Self {
            product: PRODUCT.into(),
            version: version.into(),
            build: build.into(),
            protocol_min: protocols.0,
            protocol_max: protocols.1,
            size: exe.len() as u64,
            sha256: sha256_hex(exe),
        }
    }

    /// Whether `exe` is the file this manifest describes.
    pub fn matches(&self, exe: &[u8]) -> bool {
        exe.len() as u64 == self.size && sha256_hex(exe) == self.sha256
    }

    /// Newer than `version`/`build` (see [`newer`]).
    pub fn newer_than(&self, version: &str, build: &str) -> bool {
        newer((&self.version, &self.build), (version, build))
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    digest::digest(&digest::SHA256, data).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UpdateError {
    #[error("not an update package")]
    Malformed,
    #[error("the package's signature isn't the release key's")]
    Signature,
    #[error("the package is for {0}, not this rdpio")]
    Product(String),
    #[error("the package is bigger than any rdpio")]
    TooBig,
    #[error("the downloaded file isn't the one the package describes")]
    Mismatch,
}

/// A manifest and the release key's signature over its exact bytes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Signed {
    /// The manifest's JSON, as signed.
    pub manifest: String,
    /// Ed25519, base64.
    pub signature: String,
}

impl Signed {
    /// Sign `m` with the release key's 32-byte seed.
    pub fn sign(m: &Manifest, seed: &[u8; 32]) -> Self {
        let manifest = serde_json::to_string(m).expect("manifest");
        let key = signature::Ed25519KeyPair::from_seed_unchecked(seed).expect("ed25519 seed");
        let sig = key.sign(manifest.as_bytes());
        Self { manifest, signature: base64::engine::general_purpose::STANDARD.encode(sig.as_ref()) }
    }

    /// The public key belonging to a release key seed.
    pub fn public_key(seed: &[u8; 32]) -> [u8; 32] {
        use signature::KeyPair as _;
        let key = signature::Ed25519KeyPair::from_seed_unchecked(seed).expect("ed25519 seed");
        key.public_key().as_ref().try_into().expect("32-byte key")
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(self).expect("signed manifest")
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, UpdateError> {
        serde_json::from_slice(bytes).map_err(|_| UpdateError::Malformed)
    }

    /// The manifest, if the release key signed it and it is for this product.
    pub fn verify(&self) -> Result<Manifest, UpdateError> {
        self.verify_with(&RELEASE_KEY)
    }

    pub fn verify_with(&self, key: &[u8; 32]) -> Result<Manifest, UpdateError> {
        let sig = base64::engine::general_purpose::STANDARD.decode(&self.signature).map_err(|_| UpdateError::Malformed)?;
        signature::UnparsedPublicKey::new(&signature::ED25519, key)
            .verify(self.manifest.as_bytes(), &sig)
            .map_err(|_| UpdateError::Signature)?;
        let m: Manifest = serde_json::from_str(&self.manifest).map_err(|_| UpdateError::Malformed)?;
        if m.product != PRODUCT {
            return Err(UpdateError::Product(m.product));
        }
        if m.size > MAX_SIZE {
            return Err(UpdateError::TooBig);
        }
        Ok(m)
    }
}

/// Whether release `a` (version, build) is newer than `b`: by version
/// (numerically, `0.10.0` > `0.9.0`), then by build time — so a nightly built
/// later is newer than an earlier one of the same version. A build stamp is
/// `YYYY.MM.DD.HHMM+commit`; only the time part counts.
pub fn newer(a: (&str, &str), b: (&str, &str)) -> bool {
    let va = version_parts(a.0);
    let vb = version_parts(b.0);
    if va != vb {
        return va > vb;
    }
    build_time(a.1) > build_time(b.1)
}

fn version_parts(v: &str) -> Vec<u64> {
    let core = v.trim().trim_start_matches('v').split(['-', '+']).next().unwrap_or("");
    let mut parts: Vec<u64> = core.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    while parts.len() < 3 {
        parts.push(0);
    }
    parts
}

fn build_time(b: &str) -> Vec<u64> {
    let time = b.split(['+', ' ', '(']).next().unwrap_or("");
    // Anything that isn't a stamp (a local "unknown") sorts first.
    time.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// The highest Hyper protocol version both ranges include.
pub fn negotiate(ours: (u8, u8), theirs: (u8, u8)) -> Option<u8> {
    let hi = ours.1.min(theirs.1);
    let lo = ours.0.max(theirs.0);
    (lo <= hi).then_some(hi)
}

/// [`Kind::Update`](crate::wire::Kind::Update) messages. Frozen: every rdpio
/// from protocol 4 on reads them the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateMsg {
    /// Client → host: send the signed manifest.
    GetManifest,
    /// Host → client: its [`Signed`] manifest's bytes.
    Manifest(Vec<u8>),
    /// Client → host: chunks `first..first + count`.
    Get { first: u32, count: u16 },
    /// Host → client: one chunk of the exe.
    Chunk { index: u32, data: Vec<u8> },
    /// Host → client: it has no package to give.
    Unavailable,
    /// Client → host: done (got it, or gave up).
    Done,
}

impl UpdateMsg {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut o = Vec::new();
        match self {
            Self::GetManifest => o.push(1),
            Self::Manifest(b) => {
                o.push(2);
                o.extend_from_slice(b);
            }
            Self::Get { first, count } => {
                o.push(3);
                o.extend_from_slice(&first.to_le_bytes());
                o.extend_from_slice(&count.to_le_bytes());
            }
            Self::Chunk { index, data } => {
                o.push(4);
                o.extend_from_slice(&index.to_le_bytes());
                o.extend_from_slice(data);
            }
            Self::Unavailable => o.push(5),
            Self::Done => o.push(6),
        }
        o
    }

    pub fn parse(b: &[u8]) -> Option<Self> {
        let (&op, rest) = b.split_first()?;
        Some(match op {
            1 => Self::GetManifest,
            2 => Self::Manifest(rest.to_vec()),
            3 => Self::Get {
                first: u32::from_le_bytes(rest.get(0..4)?.try_into().ok()?),
                count: u16::from_le_bytes(rest.get(4..6)?.try_into().ok()?),
            },
            4 => Self::Chunk { index: u32::from_le_bytes(rest.get(0..4)?.try_into().ok()?), data: rest.get(4..)?.to_vec() },
            5 => Self::Unavailable,
            6 => Self::Done,
            _ => return None,
        })
    }
}

/// The host's side: answers [`UpdateMsg`]s from its package.
pub struct Serve {
    signed: Vec<u8>,
    exe: Vec<u8>,
    pub manifest: Manifest,
}

impl Serve {
    /// A package to hand out: checked like one received (a host never passes
    /// on what a client would refuse).
    pub fn new(signed: Vec<u8>, exe: Vec<u8>) -> Result<Self, UpdateError> {
        let manifest = Signed::parse(&signed)?.verify()?;
        if !manifest.matches(&exe) {
            return Err(UpdateError::Mismatch);
        }
        Ok(Self { signed, exe, manifest })
    }

    #[cfg(test)]
    fn new_with(signed: Vec<u8>, exe: Vec<u8>, key: &[u8; 32]) -> Result<Self, UpdateError> {
        let manifest = Signed::parse(&signed)?.verify_with(key)?;
        if !manifest.matches(&exe) {
            return Err(UpdateError::Mismatch);
        }
        Ok(Self { signed, exe, manifest })
    }

    /// The replies to `msg` (bodies to seal as `Kind::Update`).
    pub fn answer(&self, msg: &UpdateMsg) -> Vec<Vec<u8>> {
        match msg {
            UpdateMsg::GetManifest => vec![UpdateMsg::Manifest(self.signed.clone()).to_bytes()],
            UpdateMsg::Get { first, count } => {
                let chunks = self.exe.len().div_ceil(CHUNK) as u64;
                let end = (*first as u64 + (*count).min(MAX_GET) as u64).min(chunks);
                (*first as u64..end)
                    .map(|i| {
                        let at = i as usize * CHUNK;
                        let data = self.exe[at..(at + CHUNK).min(self.exe.len())].to_vec();
                        UpdateMsg::Chunk { index: i as u32, data }.to_bytes()
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }
}

/// How a [`Fetch`] stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchState {
    /// Waiting for the manifest.
    Asking,
    /// Downloading `version`: `have` of `size` bytes.
    Downloading { version: String, have: u64, size: u64 },
    /// All there and checked.
    Done,
    /// Nothing to install (why).
    Failed(String),
}

/// A finished download: the signed manifest's bytes and the exe.
pub struct Package {
    pub signed: Vec<u8>,
    pub manifest: Manifest,
    pub exe: Vec<u8>,
}

/// A chunk not here this long after it was asked for is asked for again.
const RESEND_AFTER: Duration = Duration::from_millis(400);
/// The manifest is asked for this often until it comes.
const MANIFEST_EVERY: Duration = Duration::from_millis(300);

/// The client's side: drives the download (call [`poll`](Self::poll) often,
/// feed it every update message) and checks what arrives.
pub struct Fetch {
    our: (String, String),
    key: [u8; 32],
    signed: Option<Vec<u8>>,
    manifest: Option<Manifest>,
    data: Vec<u8>,
    /// Per chunk: received, or when it was last asked for.
    have: Vec<bool>,
    asked: Vec<Option<Instant>>,
    missing: usize,
    cursor: usize,
    manifest_asked: Option<Instant>,
    state: FetchState,
    package: Option<Package>,
}

impl Fetch {
    /// A download of a package newer than `version`/`build` (what runs here).
    pub fn new(version: &str, build: &str) -> Self {
        Self::with_key(version, build, RELEASE_KEY)
    }

    fn with_key(version: &str, build: &str, key: [u8; 32]) -> Self {
        Self {
            our: (version.into(), build.into()),
            key,
            signed: None,
            manifest: None,
            data: Vec::new(),
            have: Vec::new(),
            asked: Vec::new(),
            missing: 0,
            cursor: 0,
            manifest_asked: None,
            state: FetchState::Asking,
            package: None,
        }
    }

    pub fn state(&self) -> &FetchState {
        &self.state
    }

    /// Whether it's over (done or failed): nothing more to ask.
    pub fn finished(&self) -> bool {
        matches!(self.state, FetchState::Done | FetchState::Failed(_))
    }

    /// The package, once [`FetchState::Done`] (once).
    pub fn take(&mut self) -> Option<Package> {
        self.package.take()
    }

    /// The fraction downloaded (0..=1).
    pub fn progress(&self) -> f32 {
        match &self.state {
            FetchState::Downloading { have, size, .. } if *size > 0 => *have as f32 / *size as f32,
            FetchState::Done => 1.0,
            _ => 0.0,
        }
    }

    /// What to send now: at most `budget` chunks asked for (the caller's
    /// rate), plus anything overdue.
    pub fn poll(&mut self, now: Instant, budget: usize) -> Vec<UpdateMsg> {
        if self.finished() {
            return Vec::new();
        }
        if self.manifest.is_none() {
            if self.manifest_asked.is_none_or(|t| now.duration_since(t) >= MANIFEST_EVERY) {
                self.manifest_asked = Some(now);
                return vec![UpdateMsg::GetManifest];
            }
            return Vec::new();
        }
        let n = self.have.len();
        let mut out = Vec::new();
        let mut asked = 0usize;
        let mut run: Option<(u32, u16)> = None;
        let mut scanned = 0;
        // From the cursor round to it: the first chunks still missing that
        // were never asked for, or were asked for too long ago.
        while scanned < n && asked < budget {
            let i = (self.cursor + scanned) % n;
            scanned += 1;
            let due = !self.have[i] && self.asked[i].is_none_or(|t| now.duration_since(t) >= RESEND_AFTER);
            if due {
                self.asked[i] = Some(now);
                asked += 1;
                match &mut run {
                    Some((first, count)) if *first as usize + *count as usize == i && *count < MAX_GET => *count += 1,
                    _ => {
                        if let Some((first, count)) = run.take() {
                            out.push(UpdateMsg::Get { first, count });
                        }
                        run = Some((i as u32, 1));
                    }
                }
            }
        }
        if let Some((first, count)) = run {
            out.push(UpdateMsg::Get { first, count });
        }
        self.cursor = (self.cursor + scanned) % n.max(1);
        out
    }

    /// Take in a message from the host.
    pub fn on_msg(&mut self, msg: UpdateMsg) {
        if self.finished() {
            return;
        }
        match msg {
            UpdateMsg::Unavailable => self.state = FetchState::Failed("the host has no update to give".into()),
            UpdateMsg::Manifest(bytes) if self.manifest.is_none() => {
                let m = match Signed::parse(&bytes).and_then(|s| s.verify_with(&self.key)) {
                    Ok(m) => m,
                    Err(e) => {
                        self.state = FetchState::Failed(e.to_string());
                        return;
                    }
                };
                if !m.newer_than(&self.our.0, &self.our.1) {
                    self.state = FetchState::Failed(format!("the host's rdpio ({} {}) isn't newer", m.version, m.build));
                    return;
                }
                let chunks = (m.size as usize).div_ceil(CHUNK);
                self.data = vec![0; m.size as usize];
                self.have = vec![false; chunks];
                self.asked = vec![None; chunks];
                self.missing = chunks;
                self.state = FetchState::Downloading { version: m.version.clone(), have: 0, size: m.size };
                self.signed = Some(bytes);
                self.manifest = Some(m);
                if chunks == 0 {
                    self.finish();
                }
            }
            UpdateMsg::Chunk { index, data } => {
                let Some(m) = &self.manifest else { return };
                let i = index as usize;
                if i >= self.have.len() || self.have[i] {
                    return;
                }
                let at = i * CHUNK;
                let want = (m.size as usize - at).min(CHUNK);
                if data.len() != want {
                    return;
                }
                self.data[at..at + want].copy_from_slice(&data);
                self.have[i] = true;
                self.missing -= 1;
                if let FetchState::Downloading { have, .. } = &mut self.state {
                    *have += want as u64;
                }
                if self.missing == 0 {
                    self.finish();
                }
            }
            _ => {}
        }
    }

    fn finish(&mut self) {
        let m = self.manifest.clone().expect("manifest");
        if !m.matches(&self.data) {
            self.state = FetchState::Failed(UpdateError::Mismatch.to_string());
            return;
        }
        self.package = Some(Package { signed: self.signed.take().unwrap_or_default(), manifest: m, exe: std::mem::take(&mut self.data) });
        self.state = FetchState::Done;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: [u8; 32] = [7; 32];

    fn package(version: &str, build: &str, exe: &[u8]) -> Vec<u8> {
        Signed::sign(&Manifest::of(exe, version, build, (4, 4)), &SEED).to_bytes()
    }

    #[test]
    fn versions_order_by_number_then_build_time() {
        assert!(newer(("0.10.0", ""), ("0.9.9", "")));
        assert!(newer(("1.0.0", ""), ("0.99.0", "2099.01.01.0000")));
        assert!(!newer(("0.2.0", "2026.10.12.2050+abc"), ("0.2.0", "2026.10.12.2050+def")));
        assert!(newer(("0.2.0", "2026.10.12.2051+abc"), ("0.2.0", "2026.10.12.2050+def")));
        assert!(newer(("0.2.0", "2026.10.13.0000+a"), ("0.2.0", "unknown")));
        assert!(!newer(("0.2.0", "2026.10.12.2050"), ("0.2.1", "2020.01.01.0000")));
        assert!(newer(("v0.3", ""), ("0.2.9", "")));
    }

    #[test]
    fn protocols_meet_at_the_highest_both_speak() {
        assert_eq!(negotiate((4, 6), (5, 9)), Some(6));
        assert_eq!(negotiate((4, 4), (4, 4)), Some(4));
        assert_eq!(negotiate((4, 4), (5, 5)), None);
        assert_eq!(negotiate((3, 3), (4, 6)), None);
    }

    #[test]
    fn only_the_release_key_signs() {
        let exe = b"an exe".to_vec();
        let signed = Signed::parse(&package("0.3.0", "", &exe)).unwrap();
        let key = Signed::public_key(&SEED);
        assert_eq!(signed.verify_with(&key).unwrap().version, "0.3.0");
        // Another key's signature, or an edited manifest, fails.
        assert_eq!(signed.verify_with(&Signed::public_key(&[8; 32])), Err(UpdateError::Signature));
        let mut edited = signed.clone();
        edited.manifest = edited.manifest.replace("0.3.0", "9.9.9");
        assert_eq!(edited.verify_with(&key), Err(UpdateError::Signature));
        // (The real key isn't the test one.)
        assert_eq!(signed.verify(), Err(UpdateError::Signature));
    }

    #[test]
    fn messages_round_trip() {
        for m in [
            UpdateMsg::GetManifest,
            UpdateMsg::Manifest(b"{}".to_vec()),
            UpdateMsg::Get { first: 70_000, count: 256 },
            UpdateMsg::Chunk { index: 3, data: vec![1, 2, 3] },
            UpdateMsg::Unavailable,
            UpdateMsg::Done,
        ] {
            assert_eq!(UpdateMsg::parse(&m.to_bytes()), Some(m));
        }
        assert_eq!(UpdateMsg::parse(&[]), None);
        assert_eq!(UpdateMsg::parse(&[3, 1]), None);
        // The biggest message fits a sealed datagram.
        assert!(UpdateMsg::Chunk { index: 0, data: vec![0; CHUNK] }.to_bytes().len() <= crate::wire::MAX_BODY);
    }

    /// Host and client over a lossy, reordering link.
    fn transfer(exe: &[u8], client_version: &str, loss_every: usize) -> (Fetch, usize) {
        let key = Signed::public_key(&SEED);
        let serve = Serve::new_with(package("0.3.0", "2026.10.20.1200+x", exe), exe.to_vec(), &key).unwrap();
        let mut fetch = Fetch::with_key(client_version, "2026.10.12.2050+y", key);
        let mut now = Instant::now();
        let mut sent = 0usize;
        for _round in 0..10_000 {
            if fetch.finished() {
                break;
            }
            let mut replies = Vec::new();
            for msg in fetch.poll(now, 64) {
                replies.extend(serve.answer(&msg));
            }
            replies.reverse();
            for r in replies {
                sent += 1;
                if loss_every > 0 && sent % loss_every == 0 {
                    continue;
                }
                fetch.on_msg(UpdateMsg::parse(&r).unwrap());
            }
            now += Duration::from_millis(50);
        }
        (fetch, sent)
    }

    #[test]
    fn a_package_arrives_whole_despite_loss() {
        let exe: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let (mut fetch, _) = transfer(&exe, "0.2.0", 5);
        assert_eq!(fetch.state(), &FetchState::Done);
        let p = fetch.take().unwrap();
        assert_eq!(p.exe, exe);
        assert_eq!(p.manifest.version, "0.3.0");
    }

    #[test]
    fn nothing_older_or_the_same_is_taken() {
        let (fetch, sent) = transfer(b"exe", "0.3.1", 0);
        assert!(matches!(fetch.state(), FetchState::Failed(_)), "{:?}", fetch.state());
        assert!(sent <= 2, "only the manifest moved");
    }

    #[test]
    fn a_corrupt_chunk_fails_the_download() {
        let key = Signed::public_key(&SEED);
        let exe = vec![5u8; CHUNK * 3];
        let serve = Serve::new_with(package("0.3.0", "", &exe), exe.clone(), &key).unwrap();
        let mut fetch = Fetch::with_key("0.2.0", "", key);
        let now = Instant::now();
        for m in fetch.poll(now, 8) {
            for r in serve.answer(&m) {
                fetch.on_msg(UpdateMsg::parse(&r).unwrap());
            }
        }
        for m in fetch.poll(now, 8) {
            for r in serve.answer(&m) {
                let mut msg = UpdateMsg::parse(&r).unwrap();
                if let UpdateMsg::Chunk { index: 1, data } = &mut msg {
                    data[0] ^= 1;
                }
                fetch.on_msg(msg);
            }
        }
        assert_eq!(fetch.state(), &FetchState::Failed(UpdateError::Mismatch.to_string()));
        assert!(fetch.take().is_none());
    }

    #[test]
    fn a_host_wont_serve_a_package_that_isnt_its_exe() {
        let key = Signed::public_key(&SEED);
        assert_eq!(Serve::new_with(package("0.3.0", "", b"one"), b"two".to_vec(), &key).err(), Some(UpdateError::Mismatch));
    }
}
