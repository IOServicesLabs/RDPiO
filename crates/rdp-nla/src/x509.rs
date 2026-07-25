//! Minimal X.509 navigation: pull the certificate's public key out for CredSSP.
//!
//! CredSSP's public-key authentication hashes (v5+) or encrypts (legacy) the
//! server's *`subjectPublicKey`* — the BIT STRING contents of the
//! `SubjectPublicKeyInfo`, i.e. the raw key (for RSA, the PKCS#1 `RSAPublicKey`
//! DER). This matches OpenSSL `i2d_PublicKey`, which the reference CredSSP client
//! uses. It is **not** the whole `SubjectPublicKeyInfo` (`i2d_PUBKEY`): hashing
//! the algorithm-identifier wrapper too makes the server's channel-binding check
//! fail and it drops the connection right after pubKeyAuth.
//!
//! ```text
//! Certificate ::= SEQUENCE { tbsCertificate SEQUENCE { ... }, sigAlg, sig }
//! TBSCertificate ::= SEQUENCE {
//!   version [0] EXPLICIT INTEGER OPTIONAL, serialNumber INTEGER,
//!   signature SEQUENCE, issuer SEQUENCE, validity SEQUENCE, subject SEQUENCE,
//!   subjectPublicKeyInfo SEQUENCE { algorithm SEQUENCE, subjectPublicKey BIT STRING }, ... }
//! ```

use rdp_asn1::der;
use rdp_asn1::Asn1Error;

/// Extract the raw `subjectPublicKey` (the BIT STRING contents of the
/// `SubjectPublicKeyInfo`) from a DER X.509 certificate — the value CredSSP's
/// public-key binding operates on.
pub fn extract_public_key(cert_der: &[u8]) -> Result<Vec<u8>, Asn1Error> {
    let mut outer = cert_der;
    let cert_body = der::expect(&mut outer, der::TAG_SEQUENCE)?;
    let mut cb = cert_body;
    let tbs = der::expect(&mut cb, der::TAG_SEQUENCE)?;
    let mut t = tbs;

    // First element is either the optional [0] version or the serialNumber.
    let (tag0, _) = der::read_element(&mut t)?;
    if tag0 == der::context_tag(0) {
        let _ = der::read_element(&mut t)?; // serialNumber
    }
    // Skip signature, issuer, validity, subject.
    for _ in 0..4 {
        let _ = der::read_element(&mut t)?;
    }
    // subjectPublicKeyInfo ::= SEQUENCE { algorithm SEQUENCE, subjectPublicKey BIT STRING }
    let spki = der::expect(&mut t, der::TAG_SEQUENCE)?;
    let mut s = spki;
    let _algorithm = der::expect(&mut s, der::TAG_SEQUENCE)?; // AlgorithmIdentifier — skipped
    let bit_string = der::expect(&mut s, der::TAG_BIT_STRING)?;
    // A BIT STRING value begins with a byte counting unused trailing bits (0 for
    // a key); the remainder is the key material (the PKCS#1 RSAPublicKey for RSA,
    // the EC point for EC) — exactly what `i2d_PublicKey` yields.
    if bit_string.is_empty() {
        return Err(Asn1Error::UnexpectedEof);
    }
    Ok(bit_string[1..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a SubjectPublicKeyInfo `SEQUENCE { algorithm, subjectPublicKey }`,
    /// with `key` carried verbatim inside the BIT STRING (0 unused bits).
    fn spki(algorithm_contents: &[u8], key: &[u8]) -> Vec<u8> {
        let mut bit_string = vec![der::TAG_BIT_STRING];
        let mut content = vec![0x00]; // unused-bits count
        content.extend_from_slice(key);
        der::encode_length(content.len(), &mut bit_string);
        bit_string.extend_from_slice(&content);
        der::sequence(&[der::sequence(algorithm_contents), bit_string].concat())
    }

    fn synthetic_cert(with_version: bool, spki: &[u8]) -> Vec<u8> {
        let mut tbs = Vec::new();
        if with_version {
            tbs.extend(der::context(0, &der::integer(2))); // version v3
        }
        tbs.extend(der::integer(0x1234)); // serialNumber
        tbs.extend(der::sequence(&der::integer(0))); // signature alg
        tbs.extend(der::sequence(&[])); // issuer
        tbs.extend(der::sequence(&[])); // validity
        tbs.extend(der::sequence(&[])); // subject
        tbs.extend_from_slice(spki); // subjectPublicKeyInfo
        der::sequence(
            &[
                der::sequence(&tbs),
                der::sequence(&der::integer(0)), // signatureAlgorithm
                der::octet_string(&[0xAB, 0xCD]), // signatureValue (stand-in)
            ]
            .concat(),
        )
    }

    #[test]
    fn extracts_raw_public_key_without_version() {
        let key = [0x55, 0x66, 0x77, 0x88, 0x99];
        let cert = synthetic_cert(false, &spki(&der::integer(0), &key));
        assert_eq!(extract_public_key(&cert).unwrap(), key);
    }

    #[test]
    fn extracts_raw_public_key_with_version() {
        // A more realistic key body: a PKCS#1-style SEQUENCE { modulus, exp }.
        let key = der::sequence(&[der::integer(0x00C0_FFEE), der::integer(65537)].concat());
        let cert = synthetic_cert(true, &spki(&der::integer(0), &key));
        // We get back exactly the BIT STRING contents (no algorithm wrapper, no
        // unused-bits byte) — i.e. the raw key.
        assert_eq!(extract_public_key(&cert).unwrap(), key);
    }

    #[test]
    fn rejects_garbage() {
        assert!(extract_public_key(&[0x01, 0x02, 0x03]).is_err());
    }
}
