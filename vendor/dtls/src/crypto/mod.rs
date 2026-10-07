#[cfg(test)]
mod crypto_test;
#[cfg(test)]
mod peer_signature_test;

pub mod crypto_cbc;
pub mod crypto_ccm;
pub mod crypto_chacha20;
pub mod crypto_gcm;
pub mod padding;

use std::convert::TryFrom;
use std::sync::Arc;

use der_parser::oid;
use der_parser::oid::Oid;

use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::server::danger::ClientCertVerifier;

use rcgen::{generate_simple_self_signed, CertifiedKey, KeyPair};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, Ed25519KeyPair};

use crate::curve::named_curve::*;
use crate::error::*;
use crate::record_layer::record_layer_header::*;
use crate::signature_hash_algorithm::{HashAlgorithm, SignatureAlgorithm, SignatureHashAlgorithm};

/// A X.509 certificate(s) used to authenticate a DTLS connection.
#[derive(Clone, PartialEq, Debug)]
pub struct Certificate {
    /// DER-encoded certificates.
    pub certificate: Vec<CertificateDer<'static>>,
    /// Private key.
    pub private_key: CryptoPrivateKey,
}

impl Certificate {
    /// Generate a self-signed certificate.
    ///
    /// See [`rcgen::generate_simple_self_signed`].
    pub fn generate_self_signed(subject_alt_names: impl Into<Vec<String>>) -> Result<Self> {
        let CertifiedKey { cert, key_pair } =
            generate_simple_self_signed(subject_alt_names).unwrap();
        Ok(Certificate {
            certificate: vec![cert.der().to_owned()],
            private_key: CryptoPrivateKey::try_from(&key_pair)?,
        })
    }

    /// Generate a self-signed certificate with the given algorithm.
    ///
    /// See [`rcgen::Certificate::from_params`].
    pub fn generate_self_signed_with_alg(
        subject_alt_names: impl Into<Vec<String>>,
        alg: &'static rcgen::SignatureAlgorithm,
    ) -> Result<Self> {
        let params = rcgen::CertificateParams::new(subject_alt_names).unwrap();
        let key_pair = rcgen::KeyPair::generate_for(alg).unwrap();
        let cert = params.self_signed(&key_pair).unwrap();

        Ok(Certificate {
            certificate: vec![cert.der().to_owned()],
            private_key: CryptoPrivateKey::try_from(&key_pair)?,
        })
    }

    /// Parses a certificate from the ASCII PEM format.
    #[cfg(feature = "pem")]
    pub fn from_pem(pem_str: &str) -> Result<Self> {
        let mut pems = pem::parse_many(pem_str).map_err(|e| Error::InvalidPEM(e.to_string()))?;
        if pems.len() < 2 {
            return Err(Error::InvalidPEM(format!(
                "expected at least two PEM blocks, got {}",
                pems.len()
            )));
        }
        if pems[0].tag() != "PRIVATE_KEY" {
            return Err(Error::InvalidPEM(format!(
                "invalid tag (expected: 'PRIVATE_KEY', got: '{}')",
                pems[0].tag()
            )));
        }

        let keypair = KeyPair::try_from(pems[0].contents())
            .map_err(|e| Error::InvalidPEM(format!("can't decode keypair: {e}")))?;

        let mut rustls_certs = Vec::new();
        for p in pems.drain(1..) {
            if p.tag() != "CERTIFICATE" {
                return Err(Error::InvalidPEM(format!(
                    "invalid tag (expected: 'CERTIFICATE', got: '{}')",
                    p.tag()
                )));
            }
            rustls_certs.push(CertificateDer::from(p.contents().to_vec()));
        }

        Ok(Certificate {
            certificate: rustls_certs,
            private_key: CryptoPrivateKey::try_from(&keypair)?,
        })
    }

    /// Serializes the certificate (including the private key) in PKCS#8 format in PEM.
    #[cfg(feature = "pem")]
    pub fn serialize_pem(&self) -> String {
        let mut data = vec![pem::Pem::new(
            "PRIVATE_KEY".to_string(),
            self.private_key.serialized_der.clone(),
        )];
        for rustls_cert in &self.certificate {
            data.push(pem::Pem::new(
                "CERTIFICATE".to_string(),
                rustls_cert.as_ref(),
            ));
        }
        pem::encode_many(&data)
    }
}

pub(crate) fn value_key_message(
    client_random: &[u8],
    server_random: &[u8],
    public_key: &[u8],
    named_curve: NamedCurve,
) -> Vec<u8> {
    let mut server_ecdh_params = vec![0u8; 4];
    server_ecdh_params[0] = 3; // named curve
    server_ecdh_params[1..3].copy_from_slice(&(named_curve as u16).to_be_bytes());
    server_ecdh_params[3] = public_key.len() as u8;

    let mut plaintext = vec![];
    plaintext.extend_from_slice(client_random);
    plaintext.extend_from_slice(server_random);
    plaintext.extend_from_slice(&server_ecdh_params);
    plaintext.extend_from_slice(public_key);

    plaintext
}

/// Either ED25519, ECDSA or RSA keypair.
#[derive(Debug)]
pub enum CryptoPrivateKeyKind {
    Ed25519(Ed25519KeyPair),
    Ecdsa256(EcdsaKeyPair),
    Rsa256(ring::rsa::KeyPair),
}

/// Private key.
#[derive(Debug)]
pub struct CryptoPrivateKey {
    /// Keypair.
    pub kind: CryptoPrivateKeyKind,
    /// DER-encoded keypair.
    pub serialized_der: Vec<u8>,
}

impl PartialEq for CryptoPrivateKey {
    fn eq(&self, other: &Self) -> bool {
        if self.serialized_der != other.serialized_der {
            return false;
        }

        matches!(
            (&self.kind, &other.kind),
            (
                CryptoPrivateKeyKind::Rsa256(_),
                CryptoPrivateKeyKind::Rsa256(_)
            ) | (
                CryptoPrivateKeyKind::Ecdsa256(_),
                CryptoPrivateKeyKind::Ecdsa256(_)
            ) | (
                CryptoPrivateKeyKind::Ed25519(_),
                CryptoPrivateKeyKind::Ed25519(_)
            )
        )
    }
}

impl Clone for CryptoPrivateKey {
    fn clone(&self) -> Self {
        match self.kind {
            CryptoPrivateKeyKind::Ed25519(_) => CryptoPrivateKey {
                kind: CryptoPrivateKeyKind::Ed25519(
                    Ed25519KeyPair::from_pkcs8_maybe_unchecked(&self.serialized_der).unwrap(),
                ),
                serialized_der: self.serialized_der.clone(),
            },
            CryptoPrivateKeyKind::Ecdsa256(_) => CryptoPrivateKey {
                kind: CryptoPrivateKeyKind::Ecdsa256(
                    EcdsaKeyPair::from_pkcs8(
                        &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING,
                        &self.serialized_der,
                        &SystemRandom::new(),
                    )
                    .unwrap(),
                ),
                serialized_der: self.serialized_der.clone(),
            },
            CryptoPrivateKeyKind::Rsa256(_) => CryptoPrivateKey {
                kind: CryptoPrivateKeyKind::Rsa256(
                    ring::rsa::KeyPair::from_pkcs8(&self.serialized_der).unwrap(),
                ),
                serialized_der: self.serialized_der.clone(),
            },
        }
    }
}

impl TryFrom<&KeyPair> for CryptoPrivateKey {
    type Error = Error;

    fn try_from(key_pair: &KeyPair) -> Result<Self> {
        Self::from_key_pair(key_pair)
    }
}

impl CryptoPrivateKey {
    pub fn from_key_pair(key_pair: &KeyPair) -> Result<Self> {
        let serialized_der = key_pair.serialize_der();
        if key_pair.is_compatible(&rcgen::PKCS_ED25519) {
            Ok(CryptoPrivateKey {
                kind: CryptoPrivateKeyKind::Ed25519(
                    Ed25519KeyPair::from_pkcs8_maybe_unchecked(&serialized_der)
                        .map_err(|e| Error::Other(e.to_string()))?,
                ),
                serialized_der,
            })
        } else if key_pair.is_compatible(&rcgen::PKCS_ECDSA_P256_SHA256) {
            Ok(CryptoPrivateKey {
                kind: CryptoPrivateKeyKind::Ecdsa256(
                    EcdsaKeyPair::from_pkcs8(
                        &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING,
                        &serialized_der,
                        &SystemRandom::new(),
                    )
                    .map_err(|e| Error::Other(e.to_string()))?,
                ),
                serialized_der,
            })
        } else if key_pair.is_compatible(&rcgen::PKCS_RSA_SHA256) {
            Ok(CryptoPrivateKey {
                kind: CryptoPrivateKeyKind::Rsa256(
                    ring::rsa::KeyPair::from_pkcs8(&serialized_der)
                        .map_err(|e| Error::Other(e.to_string()))?,
                ),
                serialized_der,
            })
        } else {
            Err(Error::Other("Unsupported key_pair".to_owned()))
        }
    }
}

// If the client provided a "signature_algorithms" extension, then all
// certificates provided by the server MUST be signed by a
// hash/signature algorithm pair that appears in that extension
//
// https://tools.ietf.org/html/rfc5246#section-7.4.2
pub(crate) fn generate_key_signature(
    client_random: &[u8],
    server_random: &[u8],
    public_key: &[u8],
    named_curve: NamedCurve,
    private_key: &CryptoPrivateKey, /*, hash_algorithm: HashAlgorithm*/
) -> Result<Vec<u8>> {
    let msg = value_key_message(client_random, server_random, public_key, named_curve);
    let signature = match &private_key.kind {
        CryptoPrivateKeyKind::Ed25519(kp) => kp.sign(&msg).as_ref().to_vec(),
        CryptoPrivateKeyKind::Ecdsa256(kp) => {
            let system_random = SystemRandom::new();
            kp.sign(&system_random, &msg)
                .map_err(|e| Error::Other(e.to_string()))?
                .as_ref()
                .to_vec()
        }
        CryptoPrivateKeyKind::Rsa256(kp) => {
            let system_random = SystemRandom::new();
            let mut signature = vec![0; kp.public().modulus_len()];
            kp.sign(
                &ring::signature::RSA_PKCS1_SHA256,
                &system_random,
                &msg,
                &mut signature,
            )
            .map_err(|e| Error::Other(e.to_string()))?;

            signature
        }
    };

    Ok(signature)
}

// add OID_ED25519 which is not defined in x509_parser
pub const OID_ED25519: Oid<'static> = oid!(1.3.101 .112);
pub const OID_ECDSA: Oid<'static> = oid!(1.2.840 .10045 .2 .1);

// RDPiO patch: pick the verifier from the peer certificate's actual key, not
// from the hash alone. Upstream mapped ecdsa+sha256 to the P-256 verifier and
// ecdsa+sha384 to the P-384 one, and had no RSA+SHA-384 verifier for keys under
// 2048 bits — but TLS 1.2 lets a server sign with any listed hash whatever its
// key, so a P-256 key signing with SHA-384 (or RSA-1024 with SHA-384) failed the
// handshake with a bare `ring::error::Unspecified`. Teams' media servers vary
// in exactly this way, so calls to some of them never got past DTLS.
fn verify_signature(
    message: &[u8],
    hash_algorithm: &SignatureHashAlgorithm,
    remote_key_signature: &[u8],
    raw_certificates: &[Vec<u8>],
    insecure_verification: bool,
) -> Result<()> {
    use x509_parser::public_key::PublicKey;

    if raw_certificates.is_empty() {
        return Err(Error::ErrLengthMismatch);
    }

    let (_, certificate) = x509_parser::parse_x509_certificate(&raw_certificates[0])
        .map_err(|e| Error::Other(e.to_string()))?;
    let spki = &certificate.tbs_certificate.subject_pki;
    let key_bytes: &[u8] = spki.subject_public_key.data.as_ref();
    let hash = hash_algorithm.hash;

    let (key, outcome) = match (hash_algorithm.signature, spki.parsed()) {
        (SignatureAlgorithm::Ed25519, _) => (
            "Ed25519".to_owned(),
            ring_verify(&ring::signature::ED25519, key_bytes, message, remote_key_signature),
        ),
        (SignatureAlgorithm::Ecdsa, Ok(PublicKey::EC(point))) => {
            let bits = point.key_size();
            (
                format!("EC P-{bits}"),
                verify_ecdsa(bits, key_bytes, hash, message, remote_key_signature),
            )
        }
        (SignatureAlgorithm::Rsa, Ok(PublicKey::RSA(rsa))) => {
            let modulus = strip_leading_zeros(rsa.modulus);
            let bits = bit_length(modulus);
            (
                format!("RSA {bits}-bit"),
                verify_rsa(
                    bits,
                    key_bytes,
                    modulus,
                    rsa.exponent,
                    hash,
                    message,
                    remote_key_signature,
                    insecure_verification,
                ),
            )
        }
        (signature, parsed) => (
            format!("{parsed:?}"),
            Err(Error::Other(format!(
                "certificate key does not match the {signature:?} signature"
            ))),
        ),
    };

    match outcome {
        Ok(()) => {
            log::debug!("DTLS peer signature verified: {key} key, {hash} hash");
            Ok(())
        }
        Err(err) => {
            log::warn!(
                "DTLS peer signature check failed: {key} key, {:?} with {hash} hash, {}-byte signature: {err}",
                hash_algorithm.signature,
                remote_key_signature.len()
            );
            Err(err)
        }
    }
}

fn ring_verify(
    algorithm: &'static dyn ring::signature::VerificationAlgorithm,
    public_key: &[u8],
    message: &[u8],
    signature: &[u8],
) -> Result<()> {
    ring::signature::UnparsedPublicKey::new(algorithm, public_key)
        .verify(message, signature)
        .map_err(|e| Error::Other(e.to_string()))
}

/// ECDSA over P-256/P-384 with whichever hash the peer chose. ring covers the
/// four P-256/P-384 × SHA-256/SHA-384 pairs for uncompressed points; any other
/// hash, or a compressed point, is verified prehashed with RustCrypto.
fn verify_ecdsa(
    bits: usize,
    point: &[u8],
    hash: HashAlgorithm,
    message: &[u8],
    signature: &[u8],
) -> Result<()> {
    use ring::signature as rs;
    let ring_algorithm: Option<&'static dyn rs::VerificationAlgorithm> = match (bits, hash) {
        (256, HashAlgorithm::Sha256) => Some(&rs::ECDSA_P256_SHA256_ASN1),
        (256, HashAlgorithm::Sha384) => Some(&rs::ECDSA_P256_SHA384_ASN1),
        (384, HashAlgorithm::Sha256) => Some(&rs::ECDSA_P384_SHA256_ASN1),
        (384, HashAlgorithm::Sha384) => Some(&rs::ECDSA_P384_SHA384_ASN1),
        _ => None,
    };
    if let (Some(algorithm), Some(4)) = (ring_algorithm, point.first()) {
        return ring_verify(algorithm, point, message, signature);
    }
    // A digest shorter than the field (SHA-1 on P-384) is left-padded: the same
    // integer under SEC 1 §4.1.4, and the length RustCrypto requires.
    let mut digest = message_digest(hash, message)?;
    let field_bytes = bits.div_ceil(8);
    if digest.len() < field_bytes {
        let mut padded = vec![0u8; field_bytes - digest.len()];
        padded.extend_from_slice(&digest);
        digest = padded;
    }
    let err = |e: &dyn std::fmt::Display| Error::Other(e.to_string());
    match bits {
        256 => {
            use p256::ecdsa::signature::hazmat::PrehashVerifier;
            let key = p256::ecdsa::VerifyingKey::from_sec1_bytes(point).map_err(|e| err(&e))?;
            let sig = p256::ecdsa::Signature::from_der(signature).map_err(|e| err(&e))?;
            key.verify_prehash(&digest, &sig).map_err(|e| err(&e))
        }
        384 => {
            use p384::ecdsa::signature::hazmat::PrehashVerifier;
            let key = p384::ecdsa::VerifyingKey::from_sec1_bytes(point).map_err(|e| err(&e))?;
            let sig = p384::ecdsa::Signature::from_der(signature).map_err(|e| err(&e))?;
            key.verify_prehash(&digest, &sig).map_err(|e| err(&e))
        }
        _ => Err(Error::ErrKeySignatureVerifyUnimplemented),
    }
}

/// RSA PKCS#1 v1.5. Keys of 2048 bits and up go to ring. Smaller keys (Teams'
/// media servers use 1024-bit RSA) need `insecure_verification` — except SHA-1,
/// which upstream always accepted from 1024 bits — and are checked by hand,
/// because ring has no sub-2048-bit verifier for SHA-384.
#[allow(clippy::too_many_arguments)]
fn verify_rsa(
    bits: usize,
    der_public_key: &[u8],
    modulus: &[u8],
    exponent: &[u8],
    hash: HashAlgorithm,
    message: &[u8],
    signature: &[u8],
    insecure_verification: bool,
) -> Result<()> {
    use ring::signature as rs;
    if bits >= 2048 {
        let algorithm: &'static dyn rs::VerificationAlgorithm = match hash {
            HashAlgorithm::Sha1 => &rs::RSA_PKCS1_2048_8192_SHA1_FOR_LEGACY_USE_ONLY,
            HashAlgorithm::Sha256 => &rs::RSA_PKCS1_2048_8192_SHA256,
            HashAlgorithm::Sha384 => &rs::RSA_PKCS1_2048_8192_SHA384,
            HashAlgorithm::Sha512 => &rs::RSA_PKCS1_2048_8192_SHA512,
            _ => return Err(Error::ErrKeySignatureVerifyUnimplemented),
        };
        return ring_verify(algorithm, der_public_key, message, signature);
    }
    let legacy_ok = bits >= 1024 && hash == HashAlgorithm::Sha1;
    if !(insecure_verification || legacy_ok) {
        return Err(Error::Other(format!(
            "{bits}-bit RSA key is below 2048 bits (insecure verification is off)"
        )));
    }
    rsa_pkcs1v15_verify(modulus, exponent, hash, message, signature)
}

/// RSASSA-PKCS1-v1_5 verification (RFC 8017 §8.2.2): `s^e mod n` must equal
/// `00 01 FF…FF 00 || DigestInfo(hash(message))`.
fn rsa_pkcs1v15_verify(
    modulus: &[u8],
    exponent: &[u8],
    hash: HashAlgorithm,
    message: &[u8],
    signature: &[u8],
) -> Result<()> {
    use num_bigint::BigUint;
    // DER DigestInfo prefixes (RFC 8017 §9.2 note 1).
    let prefix: &[u8] = match hash {
        HashAlgorithm::Sha1 => &[
            0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04,
            0x14,
        ],
        HashAlgorithm::Sha256 => &[
            0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x01, 0x05, 0x00, 0x04, 0x20,
        ],
        HashAlgorithm::Sha384 => &[
            0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x02, 0x05, 0x00, 0x04, 0x30,
        ],
        HashAlgorithm::Sha512 => &[
            0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x03, 0x05, 0x00, 0x04, 0x40,
        ],
        _ => return Err(Error::ErrKeySignatureVerifyUnimplemented),
    };
    let digest = message_digest(hash, message)?;
    let mismatch = || Error::Other("RSA PKCS#1 v1.5 signature mismatch".to_owned());

    let k = modulus.len();
    let t_len = prefix.len() + digest.len();
    // Keys under 512 bits are never acceptable; the encoding needs ≥ 8 bytes of padding.
    if k < 64 || k < t_len + 11 || signature.len() != k {
        return Err(mismatch());
    }
    let n = BigUint::from_bytes_be(modulus);
    let e = BigUint::from_bytes_be(exponent);
    let s = BigUint::from_bytes_be(signature);
    if s >= n || e.bits() < 2 {
        return Err(mismatch());
    }
    let m = s.modpow(&e, &n).to_bytes_be();
    let mut encoded = vec![0u8; k.saturating_sub(m.len())];
    encoded.extend_from_slice(&m);

    let mut expected = Vec::with_capacity(k);
    expected.extend_from_slice(&[0x00, 0x01]);
    expected.resize(k - t_len - 1, 0xff);
    expected.push(0x00);
    expected.extend_from_slice(prefix);
    expected.extend_from_slice(&digest);
    if encoded == expected {
        Ok(())
    } else {
        Err(mismatch())
    }
}

fn message_digest(hash: HashAlgorithm, message: &[u8]) -> Result<Vec<u8>> {
    use sha2::Digest;
    Ok(match hash {
        HashAlgorithm::Sha1 => sha1::Sha1::digest(message).to_vec(),
        HashAlgorithm::Sha256 => sha2::Sha256::digest(message).to_vec(),
        HashAlgorithm::Sha384 => sha2::Sha384::digest(message).to_vec(),
        HashAlgorithm::Sha512 => sha2::Sha512::digest(message).to_vec(),
        _ => return Err(Error::ErrKeySignatureVerifyUnimplemented),
    })
}

fn strip_leading_zeros(bytes: &[u8]) -> &[u8] {
    let first = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    &bytes[first..]
}

/// Bit length of a big-endian unsigned integer without leading zero bytes.
fn bit_length(bytes: &[u8]) -> usize {
    match bytes.first() {
        Some(&b) => (bytes.len() - 1) * 8 + (8 - b.leading_zeros() as usize),
        None => 0,
    }
}

pub(crate) fn verify_key_signature(
    message: &[u8],
    hash_algorithm: &SignatureHashAlgorithm,
    remote_key_signature: &[u8],
    raw_certificates: &[Vec<u8>],
    insecure_verification: bool,
) -> Result<()> {
    verify_signature(
        message,
        hash_algorithm,
        remote_key_signature,
        raw_certificates,
        insecure_verification,
    )
}

// If the server has sent a CertificateRequest message, the client MUST send the Certificate
// message.  The ClientKeyExchange message is now sent, and the content
// of that message will depend on the public key algorithm selected
// between the ClientHello and the ServerHello.  If the client has sent
// a certificate with signing ability, a digitally-signed
// CertificateVerify message is sent to explicitly verify possession of
// the private key in the certificate.
// https://tools.ietf.org/html/rfc5246#section-7.3
pub(crate) fn generate_certificate_verify(
    handshake_bodies: &[u8],
    private_key: &CryptoPrivateKey, /*, hashAlgorithm hashAlgorithm*/
) -> Result<Vec<u8>> {
    let signature = match &private_key.kind {
        CryptoPrivateKeyKind::Ed25519(kp) => kp.sign(handshake_bodies).as_ref().to_vec(),
        CryptoPrivateKeyKind::Ecdsa256(kp) => {
            let system_random = SystemRandom::new();
            kp.sign(&system_random, handshake_bodies)
                .map_err(|e| Error::Other(e.to_string()))?
                .as_ref()
                .to_vec()
        }
        CryptoPrivateKeyKind::Rsa256(kp) => {
            let system_random = SystemRandom::new();
            let mut signature = vec![0; kp.public().modulus_len()];
            kp.sign(
                &ring::signature::RSA_PKCS1_SHA256,
                &system_random,
                handshake_bodies,
                &mut signature,
            )
            .map_err(|e| Error::Other(e.to_string()))?;

            signature
        }
    };

    Ok(signature)
}

pub(crate) fn verify_certificate_verify(
    handshake_bodies: &[u8],
    hash_algorithm: &SignatureHashAlgorithm,
    remote_key_signature: &[u8],
    raw_certificates: &[Vec<u8>],
    insecure_verification: bool,
) -> Result<()> {
    verify_signature(
        handshake_bodies,
        hash_algorithm,
        remote_key_signature,
        raw_certificates,
        insecure_verification,
    )
}

pub(crate) fn load_certs(raw_certificates: &[Vec<u8>]) -> Result<Vec<CertificateDer<'static>>> {
    if raw_certificates.is_empty() {
        return Err(Error::ErrLengthMismatch);
    }

    let mut certs = vec![];
    for raw_cert in raw_certificates {
        let cert = CertificateDer::from(raw_cert.to_vec());
        certs.push(cert);
    }

    Ok(certs)
}

pub(crate) fn verify_client_cert(
    raw_certificates: &[Vec<u8>],
    cert_verifier: &Arc<dyn ClientCertVerifier>,
) -> Result<Vec<CertificateDer<'static>>> {
    let chains = load_certs(raw_certificates)?;

    let (end_entity, intermediates) = chains
        .split_first()
        .ok_or(Error::ErrClientCertificateRequired)?;

    match cert_verifier.verify_client_cert(
        end_entity,
        intermediates,
        rustls::pki_types::UnixTime::now(),
    ) {
        Ok(_) => {}
        Err(err) => return Err(Error::Other(err.to_string())),
    };

    Ok(chains)
}

pub(crate) fn verify_server_cert(
    raw_certificates: &[Vec<u8>],
    cert_verifier: &Arc<dyn ServerCertVerifier>,
    server_name: &str,
) -> Result<Vec<CertificateDer<'static>>> {
    let chains = load_certs(raw_certificates)?;
    let server_name = match ServerName::try_from(server_name) {
        Ok(server_name) => server_name,
        Err(err) => return Err(Error::Other(err.to_string())),
    };

    let (end_entity, intermediates) = chains
        .split_first()
        .ok_or(Error::ErrServerMustHaveCertificate)?;
    match cert_verifier.verify_server_cert(
        end_entity,
        intermediates,
        &server_name,
        &[],
        rustls::pki_types::UnixTime::now(),
    ) {
        Ok(_) => {}
        Err(err) => return Err(Error::Other(err.to_string())),
    };

    Ok(chains)
}

pub(crate) fn generate_aead_additional_data(h: &RecordLayerHeader, payload_len: usize) -> Vec<u8> {
    let mut additional_data = vec![0u8; 13];
    // SequenceNumber MUST be set first
    // we only want uint48, clobbering an extra 2 (using uint64, rust doesn't have uint48)
    additional_data[..8].copy_from_slice(&h.sequence_number.to_be_bytes());
    additional_data[..2].copy_from_slice(&h.epoch.to_be_bytes());
    additional_data[8] = h.content_type as u8;
    additional_data[9] = h.protocol_version.major;
    additional_data[10] = h.protocol_version.minor;
    additional_data[11..].copy_from_slice(&(payload_len as u16).to_be_bytes());

    additional_data
}

#[cfg(test)]
mod test {
    #[cfg(feature = "pem")]
    use super::*;

    #[cfg(feature = "pem")]
    #[test]
    fn test_certificate_serialize_pem_and_from_pem() -> crate::error::Result<()> {
        let cert = Certificate::generate_self_signed(vec!["webrtc.rs".to_owned()])?;

        let pem = cert.serialize_pem();
        let loaded_cert = Certificate::from_pem(&pem)?;

        assert_eq!(loaded_cert, cert);

        Ok(())
    }
}
