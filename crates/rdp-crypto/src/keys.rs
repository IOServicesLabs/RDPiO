//! Standard RDP Security key derivation (MS-RDPBCGR 5.3.5.1).
//!
//! From the 32-byte client and server randoms, derive the MAC key and the
//! directional RC4 session keys, then apply the 40/56-bit "weak key" salting
//! when the server selected a reduced strength. Built on MD5 and SHA-1.
//!
//! Exact outputs are validated against a live server (the spec's sample vectors
//! aren't reproduced here); the tests cover structure, determinism, and the
//! documented key-strength salting.

use crate::{md5::md5, sha1::sha1};

/// Server-selected encryption method (TS_UD_SC_SEC1.encryptionMethod).
pub const METHOD_40BIT: u32 = 0x01;
pub const METHOD_128BIT: u32 = 0x02;
pub const METHOD_56BIT: u32 = 0x08;
pub const METHOD_FIPS: u32 = 0x10;

/// Derived Standard RDP Security session keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionKeys {
    /// MAC signing key (16 bytes, both directions).
    pub mac_key: Vec<u8>,
    /// Client-to-server (outbound) RC4 key.
    pub client_encrypt_key: Vec<u8>,
    /// Server-to-client (inbound) RC4 key.
    pub server_decrypt_key: Vec<u8>,
}

/// SaltedHash(S, I) = MD5(S || SHA1(I || S || ClientRandom || ServerRandom)).
fn salted_hash(s: &[u8], i: &[u8], client_random: &[u8], server_random: &[u8]) -> [u8; 16] {
    let mut inner = Vec::with_capacity(i.len() + s.len() + 64);
    inner.extend_from_slice(i);
    inner.extend_from_slice(s);
    inner.extend_from_slice(client_random);
    inner.extend_from_slice(server_random);
    let sha = sha1(&inner);

    let mut outer = Vec::with_capacity(s.len() + 20);
    outer.extend_from_slice(s);
    outer.extend_from_slice(&sha);
    md5(&outer)
}

/// FinalHash(K) = MD5(K || ClientRandom || ServerRandom).
fn final_hash(k: &[u8], client_random: &[u8], server_random: &[u8]) -> [u8; 16] {
    let mut buf = Vec::with_capacity(k.len() + 64);
    buf.extend_from_slice(k);
    buf.extend_from_slice(client_random);
    buf.extend_from_slice(server_random);
    md5(&buf)
}

/// Reduce a 16-byte key to the strength the server selected.
fn adjust(method: u32, mut key: [u8; 16]) -> Vec<u8> {
    match method {
        METHOD_40BIT => {
            key[0] = 0xD1;
            key[1] = 0x26;
            key[2] = 0x9E;
            key[0..8].to_vec()
        }
        METHOD_56BIT => {
            key[0] = 0xD1;
            key[0..8].to_vec()
        }
        _ => key.to_vec(), // 128-bit (and FIPS handled elsewhere): full key
    }
}

/// Derive the session keys. `client_random` and `server_random` must be 32 bytes.
pub fn derive(client_random: &[u8], server_random: &[u8], method: u32) -> SessionKeys {
    let pre_master = [&client_random[0..24], &server_random[0..24]].concat();

    let master = [
        salted_hash(&pre_master, b"A", client_random, server_random),
        salted_hash(&pre_master, b"BB", client_random, server_random),
        salted_hash(&pre_master, b"CCC", client_random, server_random),
    ]
    .concat();

    let blob = [
        salted_hash(&master, b"X", client_random, server_random),
        salted_hash(&master, b"YY", client_random, server_random),
        salted_hash(&master, b"ZZZ", client_random, server_random),
    ]
    .concat();

    let mac_key = blob[0..16].to_vec();
    // Second 128 bits -> server's encrypt key (our inbound/decrypt key).
    let server_key = final_hash(&blob[16..32], client_random, server_random);
    // Third 128 bits -> client's encrypt key (our outbound/encrypt key).
    let client_key = final_hash(&blob[32..48], client_random, server_random);

    SessionKeys {
        mac_key,
        client_encrypt_key: adjust(method, client_key),
        server_decrypt_key: adjust(method, server_key),
    }
}

const PAD1: [u8; 40] = [0x36; 40];
const PAD2: [u8; 48] = [0x5C; 48];

/// RDP non-FIPS MAC signature (MS-RDPBCGR 5.3.6.1.1): the first 8 bytes of
/// MD5(MACKey || Pad2 || SHA1(MACKey || Pad1 || len32(data) || data)).
pub fn mac_signature(mac_key: &[u8], data: &[u8]) -> [u8; 8] {
    let mut sha_in = Vec::with_capacity(mac_key.len() + 44 + data.len());
    sha_in.extend_from_slice(mac_key);
    sha_in.extend_from_slice(&PAD1);
    sha_in.extend_from_slice(&(data.len() as u32).to_le_bytes());
    sha_in.extend_from_slice(data);
    let sha = sha1(&sha_in);

    let mut md5_in = Vec::with_capacity(mac_key.len() + 48 + 20);
    md5_in.extend_from_slice(mac_key);
    md5_in.extend_from_slice(&PAD2);
    md5_in.extend_from_slice(&sha);
    let digest = md5(&md5_in);

    let mut out = [0u8; 8];
    out.copy_from_slice(&digest[0..8]);
    out
}

/// Regenerate an RC4 session key (MS-RDPBCGR 5.3.7 "Session Key Updates").
///
/// Standard RDP Security requires that, after 4096 packets have been processed
/// on a direction, the key for that direction is regenerated from the session's
/// *initial* key and its *current* key:
///   `temp = MD5(InitialKey || Pad2 || SHA1(InitialKey || Pad1 || CurrentKey))`
/// then the first `KeyLength` bytes of `temp` are RC4-encrypted with a cipher
/// keyed by those same bytes, and the reduced-strength ("weak key") fixup is
/// re-applied. `initial` and `current` are the post-[`adjust`] keys, so their
/// length already encodes the strength (8 bytes for 40/56-bit, 16 for 128-bit).
pub fn update_session_key(initial: &[u8], current: &[u8], method: u32) -> Vec<u8> {
    let len = initial.len();

    let mut sha_in = Vec::with_capacity(len + PAD1.len() + len);
    sha_in.extend_from_slice(initial);
    sha_in.extend_from_slice(&PAD1);
    sha_in.extend_from_slice(current);
    let sha = sha1(&sha_in);

    let mut md5_in = Vec::with_capacity(len + PAD2.len() + sha.len());
    md5_in.extend_from_slice(initial);
    md5_in.extend_from_slice(&PAD2);
    md5_in.extend_from_slice(&sha);
    let temp = md5(&md5_in); // 16-byte digest

    // RC4-encrypt the first `len` bytes of `temp` with a cipher keyed by those
    // same bytes — this is the new (pre-salt) directional key.
    let mut new_key = temp[..len].to_vec();
    crate::rc4::Rc4::new(&temp[..len]).apply(&mut new_key);

    // Re-apply the reduced-strength salting the server selected (no-op at 128-bit).
    match method {
        METHOD_40BIT => {
            new_key[0] = 0xD1;
            new_key[1] = 0x26;
            new_key[2] = 0x9E;
        }
        METHOD_56BIT => {
            new_key[0] = 0xD1;
        }
        _ => {}
    }
    new_key
}

/// Number of packets after which the RC4 key for a direction must be updated.
const KEY_UPDATE_INTERVAL: u32 = 4096;

/// A directional RC4 cipher for Standard RDP Security that performs the
/// mandatory session-key update every [`KEY_UPDATE_INTERVAL`] packets
/// (MS-RDPBCGR 5.3.7).
///
/// One instance owns exactly one direction's keystream and is *moved* between
/// threads (the reader owns the inbound cipher; the input sender owns the
/// outbound one), never shared — so the packet counter and the re-key stay
/// consistent without locking.
#[derive(Clone)]
pub struct SessionCipher {
    cipher: crate::rc4::Rc4,
    /// The session's first key for this direction (constant); the re-key derives
    /// from it plus the current key.
    initial: Vec<u8>,
    current: Vec<u8>,
    method: u32,
    packets: u32,
}

impl SessionCipher {
    /// Build from a derived directional key and the server-selected encryption
    /// `method` (one of the `METHOD_*` constants).
    pub fn new(key: Vec<u8>, method: u32) -> Self {
        Self {
            cipher: crate::rc4::Rc4::new(&key),
            initial: key.clone(),
            current: key,
            method,
            packets: 0,
        }
    }

    /// RC4-process one whole packet in place, regenerating the key first when
    /// [`KEY_UPDATE_INTERVAL`] packets have already been processed on this
    /// direction (checked before applying, matching the server's own schedule).
    pub fn apply_packet(&mut self, data: &mut [u8]) {
        if self.packets >= KEY_UPDATE_INTERVAL {
            self.current = update_session_key(&self.initial, &self.current, self.method);
            self.cipher = crate::rc4::Rc4::new(&self.current);
            self.packets = 0;
        }
        self.cipher.apply(data);
        self.packets += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_128bit_lengths() {
        let cr = [0xAAu8; 32];
        let sr = [0xBBu8; 32];
        let a = derive(&cr, &sr, METHOD_128BIT);
        let b = derive(&cr, &sr, METHOD_128BIT);
        assert_eq!(a, b); // deterministic
        assert_eq!(a.mac_key.len(), 16);
        assert_eq!(a.client_encrypt_key.len(), 16);
        assert_eq!(a.server_decrypt_key.len(), 16);
        assert_ne!(a.client_encrypt_key, a.server_decrypt_key);
    }

    #[test]
    fn key_strength_salting() {
        let cr = [0x11u8; 32];
        let sr = [0x22u8; 32];

        let k40 = derive(&cr, &sr, METHOD_40BIT);
        assert_eq!(k40.client_encrypt_key.len(), 8);
        assert_eq!(&k40.client_encrypt_key[0..3], &[0xD1, 0x26, 0x9E]);
        assert_eq!(&k40.server_decrypt_key[0..3], &[0xD1, 0x26, 0x9E]);

        let k56 = derive(&cr, &sr, METHOD_56BIT);
        assert_eq!(k56.client_encrypt_key.len(), 8);
        assert_eq!(k56.client_encrypt_key[0], 0xD1);
    }

    #[test]
    fn mac_signature_is_stable_and_data_dependent() {
        let key = [0x5Au8; 16];
        let a = mac_signature(&key, b"hello world");
        assert_eq!(a.len(), 8);
        assert_eq!(a, mac_signature(&key, b"hello world")); // deterministic
        assert_ne!(a, mac_signature(&key, b"hello worlD")); // data-dependent
    }

    #[test]
    fn update_session_key_is_deterministic_and_changes_the_key() {
        let initial = vec![0x11u8; 16];
        let current = initial.clone();
        let k1 = update_session_key(&initial, &current, METHOD_128BIT);
        let k2 = update_session_key(&initial, &current, METHOD_128BIT);
        assert_eq!(k1, k2); // deterministic
        assert_eq!(k1.len(), 16);
        assert_ne!(k1, initial); // actually rotates the key
        // Chaining (current advances) keeps producing fresh keys.
        let k3 = update_session_key(&initial, &k1, METHOD_128BIT);
        assert_ne!(k3, k1);
    }

    #[test]
    fn update_session_key_lengths_and_weak_key_fixup() {
        let i40 = vec![0x22u8; 8];
        let k40 = update_session_key(&i40, &i40, METHOD_40BIT);
        assert_eq!(k40.len(), 8);
        assert_eq!(&k40[0..3], &[0xD1, 0x26, 0x9E]);

        let i56 = vec![0x33u8; 8];
        let k56 = update_session_key(&i56, &i56, METHOD_56BIT);
        assert_eq!(k56.len(), 8);
        assert_eq!(k56[0], 0xD1);
    }

    #[test]
    fn session_cipher_roundtrips_across_the_rekey_boundary() {
        // A mirrored pair of ciphers (encrypt + decrypt sharing one key) must
        // stay in lockstep through the 4096-packet re-key: decrypt(encrypt(x))==x
        // for every packet, including the ones straddling the boundary.
        let key = vec![0x7Eu8; 16];
        let mut enc = SessionCipher::new(key.clone(), METHOD_128BIT);
        let mut dec = SessionCipher::new(key, METHOD_128BIT);

        for n in 0..(KEY_UPDATE_INTERVAL + 16) {
            let plain = format!("packet #{n} over the legacy RC4 stream").into_bytes();
            let mut buf = plain.clone();
            enc.apply_packet(&mut buf);
            assert_ne!(buf, plain, "ciphertext must differ from plaintext");
            dec.apply_packet(&mut buf);
            assert_eq!(buf, plain, "decrypt must recover plaintext at packet {n}");
        }
        // The re-key actually fired (counter wrapped at least once).
        assert!(enc.packets < KEY_UPDATE_INTERVAL);
    }
}
