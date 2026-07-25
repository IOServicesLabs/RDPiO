//! RSA public-key encryption for Standard RDP Security (MS-RDPBCGR 5.3.4).
//!
//! The client random is encrypted with the server's RSA public key. The RDP
//! Proprietary Certificate stores the modulus and public exponent little-endian,
//! and the encrypted result is transmitted little-endian, padded to the modulus
//! length. (Standard RDP Security uses raw textbook RSA — no OAEP padding.)

use crate::bignum::BigUint;

/// Encrypt `message_le` with the RSA public key (`modulus_le`, `exponent_le`,
/// all little-endian), returning the little-endian ciphertext padded to the
/// modulus length.
pub fn encrypt_le(message_le: &[u8], modulus_le: &[u8], exponent_le: &[u8]) -> Vec<u8> {
    let m = BigUint::from_bytes_le(message_le);
    let n = BigUint::from_bytes_le(modulus_le);
    let e = BigUint::from_bytes_le(exponent_le);
    m.modpow(&e, &n).to_bytes_le(modulus_le.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn textbook_rsa_little_endian() {
        // n=3233 (0x0CA1), e=17 (0x11), m=65 (0x41) -> c=2790 (0x0AE6).
        let c = encrypt_le(&[0x41], &[0xA1, 0x0C], &[0x11]);
        assert_eq!(c, [0xE6, 0x0A]); // 2790 little-endian, padded to modulus length
    }
}
