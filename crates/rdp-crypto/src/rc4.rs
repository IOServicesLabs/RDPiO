//! RC4 stream cipher (used for Standard RDP Security bulk encryption).

/// RC4 keystream generator / cipher state.
#[derive(Clone)]
pub struct Rc4 {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    /// Initialise the cipher with `key` (the key-scheduling algorithm).
    pub fn new(key: &[u8]) -> Self {
        assert!(!key.is_empty(), "RC4 key must not be empty");
        let mut s = [0u8; 256];
        for (idx, byte) in s.iter_mut().enumerate() {
            *byte = idx as u8;
        }
        let mut j = 0u8;
        for i in 0..256 {
            j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
            s.swap(i, j as usize);
        }
        Self { s, i: 0, j: 0 }
    }

    /// XOR `data` in place with the next keystream bytes (encrypt = decrypt).
    pub fn apply(&mut self, data: &mut [u8]) {
        for byte in data {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.s[self.i as usize]);
            self.s.swap(self.i as usize, self.j as usize);
            let k =
                self.s[(self.s[self.i as usize].wrapping_add(self.s[self.j as usize])) as usize];
            *byte ^= k;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keystream_cipher(key: &[u8], plaintext: &[u8]) -> Vec<u8> {
        let mut data = plaintext.to_vec();
        Rc4::new(key).apply(&mut data);
        data
    }

    #[test]
    fn rc4_known_vectors() {
        // Classic RC4 test vectors (key + plaintext -> ciphertext).
        assert_eq!(
            keystream_cipher(b"Key", b"Plaintext"),
            [0xBB, 0xF3, 0x16, 0xE8, 0xD9, 0x40, 0xAF, 0x0A, 0xD3]
        );
        assert_eq!(
            keystream_cipher(b"Wiki", b"pedia"),
            [0x10, 0x21, 0xBF, 0x04, 0x20]
        );
        assert_eq!(
            keystream_cipher(b"Secret", b"Attack at dawn"),
            [0x45, 0xA0, 0x1F, 0x64, 0x5F, 0xC3, 0x5B, 0x38, 0x35, 0x52, 0x54, 0x4B, 0x9B, 0xF5]
        );
    }

    #[test]
    fn rc4_roundtrips() {
        let key = b"session-key";
        let msg = b"the quick brown fox";
        let ct = keystream_cipher(key, msg);
        assert_ne!(ct, msg);
        assert_eq!(keystream_cipher(key, &ct), msg);
    }
}
