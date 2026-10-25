//! RDSTLS credential encoding for Azure Virtual Desktop / Windows 365.
//!
//! AVD/W365 reverse-connect targets negotiate RDSTLS with a server Capabilities
//! `SupportedVersions` bitmask of `0x0003` (version 1 | version 2). The AuthReq
//! itself still declares `RDSTLS_VERSION_1`, but its **Password** field is NOT
//! the relayed broker blob — it is an encrypted credential built like this
//! (matching `msrdc`, cross-checked against FreeRDP's `arm_encodeRedirectPasswd`):
//!
//! ```text
//! RedirectionPassword =
//!     UTF16( base64_crlf(
//!         RSA_pkcs1( targetServerCert.pubkey,
//!             AES256_CBC( blobKey, IV=0, PKCS7, UTF16(password)+NUL ) ) ) ) + NUL
//! ```
//!
//! where `blobKey` is the AES-256 key carried (triple-wrapped) inside
//! `redirectedAuthBlob`, and `targetServerCert` is the X.509 from
//! `redirectedServerCert`. The `Domain` field must be `"AzureAD"` and `UserName`
//! the account UPN. All the symmetric/asymmetric crypto uses Windows CNG.

/// Peel a broker `base64( UTF-16( base64( <bytes> ) ) )` field (the transport
/// wrapping used for `redirectedAuthBlob` and `redirectedServerCert`) down to the
/// innermost bytes. Mirrors FreeRDP `arm_pick_base64Utf16Field`.
pub fn peel_b64_utf16_b64(s: &str) -> Option<Vec<u8>> {
    let once = crate::arm_broker::decode_b64(s)?;
    if once.len() % 2 != 0 {
        return None;
    }
    let utf16: Vec<u16> = once
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let inner = String::from_utf16(&utf16).ok()?;
    crate::arm_broker::decode_b64(inner.trim_end_matches('\0'))
}

/// Extract the raw AES key from a peeled `redirectedAuthBlob`: a UTF-16 `"AES\0"`
/// algorithm tag followed by a `BCRYPT_KEY_DATA_BLOB` (`KDBM` magic, version,
/// `cbKeyData`, then the key). Returns the `cbKeyData`-length key.
pub fn aes_key_from_blob(blob: &[u8]) -> Option<Vec<u8>> {
    // Locate the "KDBM" magic (after the UTF-16 "AES\0" tag).
    let magic = blob.windows(4).position(|w| w == b"KDBM")?;
    let cb_off = magic + 8; // magic(4) + dwVersion(4)
    let cb = u32::from_le_bytes(blob.get(cb_off..cb_off + 4)?.try_into().ok()?) as usize;
    let key_start = cb_off + 4;
    blob.get(key_start..key_start + cb).map(<[u8]>::to_vec)
}

/// Extract the X.509 DER certificate from a peeled `redirectedServerCert` Target
/// Certificate Container: a sequence of `{type: u32, encoding: u32, size: u32,
/// data}` elements. The certificate is the `CERT_cert_file_element` (type 32).
pub fn der_from_cert_container(data: &[u8]) -> Option<Vec<u8>> {
    let mut off = 0;
    while off + 12 <= data.len() {
        let ty = u32::from_le_bytes(data[off..off + 4].try_into().ok()?);
        let size = u32::from_le_bytes(data[off + 8..off + 12].try_into().ok()?) as usize;
        off += 12;
        let elem = data.get(off..off + size)?;
        off += size;
        if ty == 32 {
            return Some(elem.to_vec());
        }
    }
    None
}

/// Standard base64 with `\r\n` inserted every 64 output characters (FreeRDP
/// `crypto_base64_encode_ex(..., withCrLf=TRUE)`).
fn base64_crlf(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut chars = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        chars.push(A[(b0 >> 2) as usize]);
        chars.push(A[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize]);
        chars.push(if chunk.len() > 1 {
            A[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize]
        } else {
            b'='
        });
        chars.push(if chunk.len() > 2 {
            A[(b2 & 0x3f) as usize]
        } else {
            b'='
        });
    }
    let mut out = String::with_capacity(chars.len() + chars.len() / 32);
    for (i, c) in chars.iter().enumerate() {
        if i != 0 && i % 64 == 0 {
            out.push('\r');
            out.push('\n');
        }
        out.push(*c as char);
    }
    out
}

/// Encode the RDSTLS Password field for AVD/W365: AES-encrypt the UTF-16 password
/// with the broker's AES key, RSA-encrypt that with the target cert's public key,
/// then base64+UTF-16 wrap it. Returns the exact bytes for the AuthReq Password.
pub fn encode_redirect_password(
    cert_der: &[u8],
    aes_key: &[u8],
    password: &str,
) -> Result<Vec<u8>, String> {
    // Plaintext: UTF-16LE(password) + null terminator.
    let mut wpass: Vec<u8> = password.encode_utf16().flat_map(u16::to_le_bytes).collect();
    wpass.extend_from_slice(&[0, 0]);

    let ciphered = aes256_cbc_encrypt(aes_key, &wpass)?;
    let rsa_out = rsa_pkcs1_encrypt(cert_der, &ciphered)?;
    let b64 = base64_crlf(&rsa_out);

    // RedirectionPassword: UTF-16LE(base64 string) + null terminator.
    let mut out: Vec<u8> = b64.encode_utf16().flat_map(u16::to_le_bytes).collect();
    out.extend_from_slice(&[0, 0]);
    Ok(out)
}

#[cfg(windows)]
fn aes256_cbc_encrypt(key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Cryptography::{
        BCryptCloseAlgorithmProvider, BCryptDestroyKey, BCryptEncrypt,
        BCryptGenerateSymmetricKey, BCryptOpenAlgorithmProvider, BCryptSetProperty,
        BCRYPT_AES_ALGORITHM, BCRYPT_ALG_HANDLE, BCRYPT_BLOCK_PADDING, BCRYPT_CHAINING_MODE,
        BCRYPT_HANDLE, BCRYPT_KEY_HANDLE, BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS,
    };

    // CNG chaining-mode property value: the wide string "ChainingModeCBC\0".
    let cbc: Vec<u16> = "ChainingModeCBC".encode_utf16().chain(std::iter::once(0)).collect();
    let cbc_bytes =
        unsafe { std::slice::from_raw_parts(cbc.as_ptr().cast::<u8>(), cbc.len() * 2) };

    unsafe {
        let mut halg = BCRYPT_ALG_HANDLE::default();
        BCryptOpenAlgorithmProvider(
            &mut halg,
            BCRYPT_AES_ALGORITHM,
            PCWSTR::null(),
            BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
        )
        .ok()
        .map_err(|e| format!("BCryptOpenAlgorithmProvider(AES): {e}"))?;

        let result = (|| {
            BCryptSetProperty(BCRYPT_HANDLE(halg.0), BCRYPT_CHAINING_MODE, cbc_bytes, 0)
                .ok()
                .map_err(|e| format!("BCryptSetProperty(CBC): {e}"))?;

            let mut hkey = BCRYPT_KEY_HANDLE::default();
            BCryptGenerateSymmetricKey(halg, &mut hkey, None, key, 0)
                .ok()
                .map_err(|e| format!("BCryptGenerateSymmetricKey: {e}"))?;

            let mut iv = [0u8; 16];
            let mut out = vec![0u8; plaintext.len() + 16];
            let mut result_len = 0u32;
            let status = BCryptEncrypt(
                hkey,
                Some(plaintext),
                None,
                Some(&mut iv),
                Some(&mut out),
                &mut result_len,
                BCRYPT_BLOCK_PADDING,
            );
            let _ = BCryptDestroyKey(hkey);
            status.ok().map_err(|e| format!("BCryptEncrypt(AES-CBC): {e}"))?;
            out.truncate(result_len as usize);
            Ok::<Vec<u8>, String>(out)
        })();

        let _ = BCryptCloseAlgorithmProvider(halg, 0);
        result
    }
}

#[cfg(windows)]
fn rsa_pkcs1_encrypt(cert_der: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::Security::Cryptography::{
        BCryptDestroyKey, BCryptEncrypt, CertCreateCertificateContext,
        CertFreeCertificateContext, CryptImportPublicKeyInfoEx2, BCRYPT_KEY_HANDLE,
        BCRYPT_PAD_PKCS1, CRYPT_IMPORT_PUBLIC_KEY_FLAGS, X509_ASN_ENCODING,
    };

    unsafe {
        let ctx = CertCreateCertificateContext(X509_ASN_ENCODING, cert_der);
        if ctx.is_null() {
            return Err("CertCreateCertificateContext failed (bad target cert DER)".into());
        }

        let result = (|| {
            let info = &(*(*ctx).pCertInfo).SubjectPublicKeyInfo as *const _;
            let mut hkey = BCRYPT_KEY_HANDLE::default();
            CryptImportPublicKeyInfoEx2(
                X509_ASN_ENCODING,
                info,
                CRYPT_IMPORT_PUBLIC_KEY_FLAGS(0),
                None,
                &mut hkey,
            )
            .map_err(|e| format!("CryptImportPublicKeyInfoEx2: {e}"))?;

            let mut out = vec![0u8; 1024];
            let mut result_len = 0u32;
            let status = BCryptEncrypt(
                hkey,
                Some(data),
                None,
                None,
                Some(&mut out),
                &mut result_len,
                BCRYPT_PAD_PKCS1,
            );
            let _ = BCryptDestroyKey(hkey);
            status.ok().map_err(|e| format!("BCryptEncrypt(RSA-PKCS1): {e}"))?;
            out.truncate(result_len as usize);
            Ok::<Vec<u8>, String>(out)
        })();

        let _ = CertFreeCertificateContext(Some(ctx));
        result
    }
}

#[cfg(not(windows))]
use portable::{aes256_cbc_encrypt, rsa_pkcs1_encrypt};

/// The same two operations without CNG (Android, Linux): RustCrypto AES and
/// the crate's own bignum RSA. Built on Windows too so tests can hold them
/// against CNG.
#[cfg(any(not(windows), test))]
mod portable {
    use rdp_asn1::der;
    use rdp_crypto::BigUint;

    /// AES-256-CBC, IV 0, PKCS#7 padding (CNG's BCRYPT_BLOCK_PADDING).
    pub(super) fn aes256_cbc_encrypt(key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
        use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        let enc = cbc::Encryptor::<aes::Aes256>::new_from_slices(key, &[0u8; 16])
            .map_err(|_| format!("AES-256 key is {} bytes, not 32", key.len()))?;
        Ok(enc.encrypt_padded_vec_mut::<Pkcs7>(plaintext))
    }

    /// RSA PKCS#1 v1.5 (type 2) with the certificate's public key; the result
    /// is big-endian and as long as the modulus, like CNG's.
    pub(super) fn rsa_pkcs1_encrypt(cert_der: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
        let spki = rdp_nla::x509::extract_public_key(cert_der)
            .map_err(|e| format!("target cert public key: {e:?}"))?;
        let (n, e) = rsa_key(&spki).ok_or("target cert key isn't RSA")?;
        let mut random = vec![0u8; n.len()];
        if !crate::rng::fill(&mut random) {
            return Err("no randomness for RSA padding".into());
        }
        rsa_pkcs1_encrypt_with(&n, &e, data, &random)
    }

    /// RSAPublicKey ::= SEQUENCE { modulus INTEGER, publicExponent INTEGER },
    /// both big-endian without the sign byte.
    fn rsa_key(spki: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
        let mut input = spki;
        let mut seq = der::expect(&mut input, der::TAG_SEQUENCE).ok()?;
        let n = der::expect(&mut seq, der::TAG_INTEGER).ok()?;
        let e = der::expect(&mut seq, der::TAG_INTEGER).ok()?;
        let strip = |v: &[u8]| {
            let first = v.iter().position(|&b| b != 0).unwrap_or(v.len());
            v[first..].to_vec()
        };
        Some((strip(n), strip(e)))
    }

    /// The padding and exponentiation, with the padding's random bytes given
    /// (zeros in it are skipped).
    pub(super) fn rsa_pkcs1_encrypt_with(
        n: &[u8],
        e: &[u8],
        data: &[u8],
        random: &[u8],
    ) -> Result<Vec<u8>, String> {
        let k = n.len();
        if data.len() + 11 > k {
            return Err(format!("{} bytes don't fit a {}-bit RSA key", data.len(), k * 8));
        }
        // EM = 00 02 PS 00 M, PS at least 8 non-zero random bytes.
        let ps_len = k - 3 - data.len();
        let mut ps = random.iter().copied().filter(|&b| b != 0);
        let mut em = Vec::with_capacity(k);
        em.extend_from_slice(&[0, 2]);
        for _ in 0..ps_len {
            // (Out of non-zero randomness: a fixed byte keeps it valid.)
            em.push(ps.next().unwrap_or(0x5A));
        }
        em.push(0);
        em.extend_from_slice(data);

        let c = BigUint::from_bytes_be(&em).modpow(&BigUint::from_bytes_be(e), &BigUint::from_bytes_be(n));
        let mut out = c.to_bytes_le(k);
        out.reverse();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_aes_pads_like_cng() {
        // PKCS#7 always adds a block's worth at most, a whole one on a boundary.
        let key: Vec<u8> = (0..32u8).collect();
        let out = portable::aes256_cbc_encrypt(&key, b"").unwrap();
        assert_eq!(out.len(), 16, "empty input still gets a full padding block");
        let out = portable::aes256_cbc_encrypt(&key, &[7u8; 16]).unwrap();
        assert_eq!(out.len(), 32);
        assert!(portable::aes256_cbc_encrypt(&key[..16], b"x").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn portable_aes_matches_cng() {
        let key: Vec<u8> = (0..32u8).map(|i| i.wrapping_mul(37)).collect();
        for len in [0usize, 1, 15, 16, 17, 34, 100] {
            let msg: Vec<u8> = (0..len as u8).collect();
            assert_eq!(
                portable::aes256_cbc_encrypt(&key, &msg).unwrap(),
                aes256_cbc_encrypt(&key, &msg).unwrap(),
                "len {len}"
            );
        }
    }

    /// A throwaway 1024-bit test key: encrypt with the portable code, decrypt
    /// with d, and find the PKCS#1 frame.
    #[test]
    fn portable_rsa_round_trips() {
        use rdp_crypto::BigUint;
        let n = hex(N_HEX);
        let d = hex(D_HEX);
        let msg = b"rdstls password blob";
        let random: Vec<u8> = (0..200u32).map(|i| (i * 7 + 3) as u8).collect();
        let c = portable::rsa_pkcs1_encrypt_with(&n, &[1, 0, 1], msg, &random).unwrap();
        assert_eq!(c.len(), n.len());
        let m = BigUint::from_bytes_be(&c).modpow(&BigUint::from_bytes_be(&d), &BigUint::from_bytes_be(&n));
        let mut em = m.to_bytes_le(n.len());
        em.reverse();
        assert_eq!(&em[..2], &[0, 2]);
        let zero = em[2..].iter().position(|&b| b == 0).unwrap() + 2;
        assert!(zero >= 10, "at least 8 padding bytes");
        assert_eq!(&em[zero + 1..], msg);
        assert!(portable::rsa_pkcs1_encrypt_with(&n, &[1, 0, 1], &[0u8; 120], &random).is_err());
    }

    /// Decrypt with the test key and unwrap PKCS#1 v1.5 type 2.
    fn rsa_open(c: &[u8]) -> Vec<u8> {
        use rdp_crypto::BigUint;
        let n = hex(N_HEX);
        let m = BigUint::from_bytes_be(c).modpow(&BigUint::from_bytes_be(&hex(D_HEX)), &BigUint::from_bytes_be(&n));
        let mut em = m.to_bytes_le(n.len());
        em.reverse();
        assert_eq!(&em[..2], &[0, 2]);
        let zero = em[2..].iter().position(|&b| b == 0).unwrap() + 2;
        em[zero + 1..].to_vec()
    }

    #[test]
    fn portable_rsa_reads_the_certificate() {
        let cert = hex(CERT_HEX);
        let a = portable::rsa_pkcs1_encrypt(&cert, b"hello").unwrap();
        let b = portable::rsa_pkcs1_encrypt(&cert, b"hello").unwrap();
        assert_ne!(a, b, "random padding");
        assert_eq!(rsa_open(&a), b"hello");
    }

    #[cfg(windows)]
    #[test]
    fn cng_rsa_opens_the_same_way() {
        // The reference: CNG's output unwraps identically, so the portable
        // padding and byte order match what the Windows build sends.
        let cert = hex(CERT_HEX);
        let c = rsa_pkcs1_encrypt(&cert, b"hello").unwrap();
        assert_eq!(c.len(), 128);
        assert_eq!(rsa_open(&c), b"hello");
    }

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    const N_HEX: &str = "
        b8418881963a5679d690cf50b320a80a7288c07f8e1c3f94d463dd3429b94245da055019a28ab9276f1814f037b1ced1
        7f563a2794630c6f6acbb49a0a2b09881c2aa1acae6d70defa5d767a17e16c6c4fdbd5a8a65e3f67a555945e7d338c5d
        998046b2f38f1f7431641df9bde1de10108ef94fd85858153373efd66972d9f5
    ";
    const D_HEX: &str = "
        b7ebe260c81e4f77aaf03da0e64d30afcd08286eaae213ad721c850fae9e5842f58fdc16daa2f2932457bb7ed5794d62
        dd62ab061b1e8ba923c756ed743ce57320492e8c6f2e30333a91f547189c3644258e1923ceccd0a35753c856b61c3458
        2166c28ab311f413444b8d33bc49bfb53e1fb3a4021f9346a4243e1592bc9e79
    ";
    /// A self-signed certificate for the same key.
    const CERT_HEX: &str = "
        308201a030820109a003020102020101300d06092a864886f70d01010b050030163114301206035504030c0b72647374
        6c732d74657374301e170d3236303130313030303030305a170d3336303130313030303030305a301631143012060355
        04030c0b726473746c732d7465737430819f300d06092a864886f70d010101050003818d0030818902818100b8418881
        963a5679d690cf50b320a80a7288c07f8e1c3f94d463dd3429b94245da055019a28ab9276f1814f037b1ced17f563a27
        94630c6f6acbb49a0a2b09881c2aa1acae6d70defa5d767a17e16c6c4fdbd5a8a65e3f67a555945e7d338c5d998046b2
        f38f1f7431641df9bde1de10108ef94fd85858153373efd66972d9f50203010001300d06092a864886f70d01010b0500
        038181005b70250dd54895480e3eb21785f461988fb7b6021b641227e92fee6db634bd797f00a5e42569687db77da6c3
        999905116ab29ec9aa85e7c43d6178ac6f25183231a057225503804a03449f4b0e7d336f2d97aa27ea0f0f0d294d0a47
        722b7adf45830ace32c25d8a0b76854df4110451d108549ba322dde0ee0475a998dff55b
    ";

    #[test]
    fn base64_crlf_matches_known_vector() {
        assert_eq!(base64_crlf(b""), "");
        assert_eq!(base64_crlf(b"f"), "Zg==");
        assert_eq!(base64_crlf(b"foobar"), "Zm9vYmFy");
        // 48 input bytes -> 64 base64 chars -> no CRLF yet (break is *between* lines).
        assert_eq!(base64_crlf(&[0u8; 48]).len(), 64);
        // 49 input bytes -> 68 base64 chars -> one CRLF after the 64th.
        assert!(base64_crlf(&[0u8; 49]).contains("\r\n"));
    }

    #[test]
    fn aes_key_from_blob_finds_kdbm_key() {
        // "AES\0" (UTF-16) + KDBM + ver + cb=16 + 16-byte key.
        let mut blob = Vec::new();
        blob.extend_from_slice(&[0x41, 0, 0x45, 0, 0x53, 0, 0, 0]); // "AES\0" UTF-16
        blob.extend_from_slice(b"KDBM");
        blob.extend_from_slice(&1u32.to_le_bytes());
        blob.extend_from_slice(&16u32.to_le_bytes());
        blob.extend_from_slice(&[0xAB; 16]);
        assert_eq!(aes_key_from_blob(&blob), Some(vec![0xAB; 16]));
    }

    #[test]
    fn der_from_container_picks_cert_element() {
        let der = b"\x30\x82fake-der";
        let mut c = Vec::new();
        c.extend_from_slice(&32u32.to_le_bytes()); // CERT_cert_file_element
        c.extend_from_slice(&1u32.to_le_bytes()); // ASN.1 DER
        c.extend_from_slice(&(der.len() as u32).to_le_bytes());
        c.extend_from_slice(der);
        assert_eq!(der_from_cert_container(&c).as_deref(), Some(&der[..]));
    }
}
