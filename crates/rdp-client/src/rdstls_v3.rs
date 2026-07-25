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
fn aes256_cbc_encrypt(_key: &[u8], _plaintext: &[u8]) -> Result<Vec<u8>, String> {
    Err("RDSTLS v3 credential encoding requires Windows CNG".into())
}

#[cfg(not(windows))]
fn rsa_pkcs1_encrypt(_cert_der: &[u8], _data: &[u8]) -> Result<Vec<u8>, String> {
    Err("RDSTLS v3 credential encoding requires Windows CNG".into())
}

#[cfg(test)]
mod tests {
    use super::*;

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
