//! Copies on the LAN over a TCP stream of their own, beside the Hyper link.
//!
//! Over the link's reliable lane a copy went as fast as a congestion window
//! written to keep out of the video's way let it — 1.2 KB datagrams each
//! sealed on its own, and every rise in delay over a Wi-Fi radio read as a
//! queue: 60-270 Mbit/s depending on the stream's state, where the radio
//! carries more. TCP gets the kernel's congestion control, window tuning and
//! the adapter's offloads, with no tuning of ours to get wrong.
//!
//! The host listens on the Hyper port's number (TCP) and hands the client a
//! fresh key over the (sealed) link; the client connects and says hello under
//! it. Each record is a [`Message`](crate::messages::Message)'s bytes, sealed
//! with AES-256-GCM: `u32` little-endian length, then ciphertext and tag. The
//! nonce is the direction and a record counter, so a replayed, reordered or
//! reflected record fails to open.

use std::io::{self, Read, Write};

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};

pub const KEY_LEN: usize = 32;

/// What the client says first: it holds the key.
pub const HELLO: &[u8] = b"rdpio bulk hello 1";

/// Largest record opened (a 1 MB file chunk and its header, with room).
pub const MAX_RECORD: usize = 4 * 1024 * 1024;

const TAG_LEN: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    HostToClient = 1,
    ClientToHost = 2,
}

fn nonce(dir: Dir, counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[0] = dir as u8;
    n[4..].copy_from_slice(&counter.to_le_bytes());
    Nonce::assume_unique_for_key(n)
}

fn key(k: &[u8; KEY_LEN]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&AES_256_GCM, k).expect("a 32-byte key"))
}

/// A fresh key for a session's stream.
pub fn new_key() -> [u8; KEY_LEN] {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut k = [0u8; KEY_LEN];
    SystemRandom::new().fill(&mut k).expect("the system's random source");
    k
}

/// Seals records going one way.
pub struct Sealer {
    key: LessSafeKey,
    dir: Dir,
    counter: u64,
    buf: Vec<u8>,
}

impl Sealer {
    pub fn new(k: &[u8; KEY_LEN], dir: Dir) -> Self {
        Self { key: key(k), dir, counter: 0, buf: Vec::new() }
    }

    /// Seal `plain` as one record and write it.
    pub fn write(&mut self, w: &mut impl Write, plain: &[u8]) -> io::Result<()> {
        if plain.len() + TAG_LEN > MAX_RECORD {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "record too big"));
        }
        self.buf.clear();
        self.buf.extend_from_slice(&((plain.len() + TAG_LEN) as u32).to_le_bytes());
        self.buf.extend_from_slice(plain);
        let mut body = self.buf.split_off(4);
        self.key
            .seal_in_place_append_tag(nonce(self.dir, self.counter), Aad::empty(), &mut body)
            .map_err(|_| io::Error::other("seal"))?;
        self.counter += 1;
        self.buf.extend_from_slice(&body);
        w.write_all(&self.buf)
    }
}

/// Opens records coming one way.
pub struct Opener {
    key: LessSafeKey,
    dir: Dir,
    counter: u64,
}

impl Opener {
    pub fn new(k: &[u8; KEY_LEN], dir: Dir) -> Self {
        Self { key: key(k), dir, counter: 0 }
    }

    /// The next record's plaintext; an error ends the stream (a record that
    /// doesn't open is an attacker's or a bug's — never skipped).
    pub fn read(&mut self, r: &mut impl Read) -> io::Result<Vec<u8>> {
        let mut len = [0u8; 4];
        r.read_exact(&mut len)?;
        let len = u32::from_le_bytes(len) as usize;
        if !(TAG_LEN..=MAX_RECORD).contains(&len) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "record length"));
        }
        let mut body = vec![0u8; len];
        r.read_exact(&mut body)?;
        let plain = self
            .key
            .open_in_place(nonce(self.dir, self.counter), Aad::empty(), &mut body)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "record didn't open"))?
            .len();
        self.counter += 1;
        body.truncate(plain);
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_in_order() {
        let k = new_key();
        let (mut s, mut o) = (Sealer::new(&k, Dir::ClientToHost), Opener::new(&k, Dir::ClientToHost));
        let mut wire = Vec::new();
        for m in [&b"hello"[..], &[], &vec![7u8; 1 << 20]] {
            s.write(&mut wire, m).unwrap();
        }
        let mut r = &wire[..];
        assert_eq!(o.read(&mut r).unwrap(), b"hello");
        assert_eq!(o.read(&mut r).unwrap(), b"");
        assert_eq!(o.read(&mut r).unwrap(), vec![7u8; 1 << 20]);
        assert!(o.read(&mut r).is_err(), "end of stream");
    }

    #[test]
    fn wrong_key_direction_or_order_fails() {
        let k = new_key();
        let mut wire = Vec::new();
        let mut s = Sealer::new(&k, Dir::HostToClient);
        s.write(&mut wire, b"one").unwrap();
        s.write(&mut wire, b"two").unwrap();
        // Another key.
        assert!(Opener::new(&new_key(), Dir::HostToClient).read(&mut &wire[..]).is_err());
        // Reflected back the other way.
        assert!(Opener::new(&k, Dir::ClientToHost).read(&mut &wire[..]).is_err());
        // The second record first (a replay or a reorder).
        let first_len = 4 + 3 + TAG_LEN;
        assert!(Opener::new(&k, Dir::HostToClient).read(&mut &wire[first_len..]).is_err());
        // Tampered.
        let mut bad = wire.clone();
        bad[6] ^= 1;
        assert!(Opener::new(&k, Dir::HostToClient).read(&mut &bad[..]).is_err());
    }

    #[test]
    fn a_silly_length_is_refused_before_reading_it() {
        let wire = (MAX_RECORD as u32 + 1).to_le_bytes();
        let e = Opener::new(&new_key(), Dir::HostToClient).read(&mut &wire[..]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }
}
