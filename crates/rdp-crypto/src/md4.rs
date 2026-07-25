//! MD4 (RFC 1320).
//!
//! MD4 is cryptographically broken and used here for exactly one reason: it is
//! the NTLM "NT hash" primitive (`NTOWF = MD4(UTF-16LE(password))`) that NTLMv2
//! and CredSSP are defined on top of (MS-NLMP). It is never used for anything
//! that needs collision or preimage resistance.

/// Compute the MD4 digest of `input`.
pub fn md4(input: &[u8]) -> [u8; 16] {
    let (mut a0, mut b0, mut c0, mut d0) =
        (0x6745_2301u32, 0xefcd_ab89u32, 0x98ba_dcfeu32, 0x1032_5476u32);

    // Padding is identical to MD5: append 0x80, zero-pad to 56 mod 64, then the
    // 64-bit little-endian bit length.
    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let f = |x: u32, y: u32, z: u32| (x & y) | (!x & z);
    let g = |x: u32, y: u32, z: u32| (x & y) | (x & z) | (y & z);
    let h = |x: u32, y: u32, z: u32| x ^ y ^ z;

    for chunk in msg.chunks_exact(64) {
        let mut x = [0u32; 16];
        for (i, word) in x.iter_mut().enumerate() {
            *word = u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }

        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);

        // Each step rotates the (a,d,c,b) roles; a local macro keeps the RFC's
        // step table readable.
        macro_rules! step {
            ($func:expr, $a:ident, $b:ident, $c:ident, $d:ident, $k:expr, $s:expr, $add:expr) => {
                $a = $a
                    .wrapping_add($func($b, $c, $d))
                    .wrapping_add(x[$k])
                    .wrapping_add($add)
                    .rotate_left($s);
            };
        }

        // Round 1: shifts 3, 7, 11, 19; k = 0..15 in order.
        for k in (0..16).step_by(4) {
            step!(f, a, b, c, d, k, 3, 0);
            step!(f, d, a, b, c, k + 1, 7, 0);
            step!(f, c, d, a, b, k + 2, 11, 0);
            step!(f, b, c, d, a, k + 3, 19, 0);
        }

        // Round 2: shifts 3, 5, 9, 13; k = 0,4,8,12,1,5,9,13,...; +0x5A827999.
        const K2: u32 = 0x5A82_7999;
        for k in 0..4 {
            step!(g, a, b, c, d, k, 3, K2);
            step!(g, d, a, b, c, k + 4, 5, K2);
            step!(g, c, d, a, b, k + 8, 9, K2);
            step!(g, b, c, d, a, k + 12, 13, K2);
        }

        // Round 3: shifts 3, 9, 11, 15; k = 0,8,4,12,2,10,6,14,...; +0x6ED9EBA1.
        const K3: u32 = 0x6ED9_EBA1;
        for &k in &[0usize, 2, 1, 3] {
            step!(h, a, b, c, d, k, 3, K3);
            step!(h, d, a, b, c, k + 8, 9, K3);
            step!(h, c, d, a, b, k + 4, 11, K3);
            step!(h, b, c, d, a, k + 12, 15, K3);
        }

        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn rfc1320_known_answers() {
        // The complete MD4 test suite from RFC 1320, Appendix A.5.
        assert_eq!(hex(&md4(b"")), "31d6cfe0d16ae931b73c59d7e0c089c0");
        assert_eq!(hex(&md4(b"a")), "bde52cb31de33e46245e05fbdbd6fb24");
        assert_eq!(hex(&md4(b"abc")), "a448017aaf21d8525fc10ae87aa6729d");
        assert_eq!(
            hex(&md4(b"message digest")),
            "d9130a8164549fe818874806e1c7014b"
        );
        assert_eq!(
            hex(&md4(b"abcdefghijklmnopqrstuvwxyz")),
            "d79e1c308aa5bbcdeea8ed63df412da9"
        );
        assert_eq!(
            hex(&md4(
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
            )),
            "043f8582f241db351ce627e153e7f0e4"
        );
        assert_eq!(
            hex(&md4(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            )),
            "e33b4ddc9c38f2199c3e7b164fcc0536"
        );
    }

    #[test]
    fn nt_hash_of_password() {
        // The NTLM "NT hash" of "Password" (MS-NLMP 4.2.1) is MD4 of its UTF-16LE
        // encoding. This is the exact input NTLMv2 is built on.
        let utf16: Vec<u8> = "Password".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(hex(&md4(&utf16)), "a4f49c406510bdcab6824ee7c30fd852");
    }
}
