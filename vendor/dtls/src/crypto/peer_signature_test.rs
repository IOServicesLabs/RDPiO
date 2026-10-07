//! RDPiO patch tests: the peer's ServerKeyExchange signature must verify for
//! every key × hash pair a TLS 1.2 server may use. Fixtures were made with
//! OpenSSL 1.1.1 (`openssl req -x509 -newkey …` for the certificates, then
//! `openssl dgst -<hash> -sign` over `msg.bin`).

use super::*;

const MESSAGE: &[u8] = include_bytes!("testdata/msg.bin");

const RSA1024: &[u8] = include_bytes!("testdata/rsa1024.der");
const RSA2048: &[u8] = include_bytes!("testdata/rsa2048.der");
const P256: &[u8] = include_bytes!("testdata/p256.der");
const P384: &[u8] = include_bytes!("testdata/p384.der");

fn signature(key: &str, hash: HashAlgorithm) -> Vec<u8> {
    let name = match hash {
        HashAlgorithm::Sha1 => "sha1",
        HashAlgorithm::Sha256 => "sha256",
        HashAlgorithm::Sha384 => "sha384",
        HashAlgorithm::Sha512 => "sha512",
        other => panic!("no fixture for {other}"),
    };
    let path = format!(
        "{}/src/crypto/testdata/{key}.{name}.sig",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn verify(
    certificate: &[u8],
    signature: &[u8],
    algorithm: SignatureAlgorithm,
    hash: HashAlgorithm,
    message: &[u8],
    insecure: bool,
) -> Result<()> {
    verify_signature(
        message,
        &SignatureHashAlgorithm {
            hash,
            signature: algorithm,
        },
        signature,
        &[certificate.to_vec()],
        insecure,
    )
}

const HASHES: [HashAlgorithm; 4] = [
    HashAlgorithm::Sha1,
    HashAlgorithm::Sha256,
    HashAlgorithm::Sha384,
    HashAlgorithm::Sha512,
];

#[test]
fn ecdsa_verifies_every_curve_and_hash_pair() {
    for (name, cert) in [("p256", P256), ("p384", P384)] {
        for hash in HASHES {
            let sig = signature(name, hash);
            verify(cert, &sig, SignatureAlgorithm::Ecdsa, hash, MESSAGE, false)
                .unwrap_or_else(|e| panic!("{name} with {hash}: {e}"));
            assert!(
                verify(cert, &sig, SignatureAlgorithm::Ecdsa, hash, b"tampered", false).is_err(),
                "{name} with {hash} accepted a tampered message"
            );
        }
    }
}

#[test]
fn rsa_verifies_every_size_and_hash_pair() {
    for (name, cert) in [("rsa1024", RSA1024), ("rsa2048", RSA2048)] {
        for hash in HASHES {
            let sig = signature(name, hash);
            verify(cert, &sig, SignatureAlgorithm::Rsa, hash, MESSAGE, true)
                .unwrap_or_else(|e| panic!("{name} with {hash}: {e}"));
            assert!(
                verify(cert, &sig, SignatureAlgorithm::Rsa, hash, b"tampered", true).is_err(),
                "{name} with {hash} accepted a tampered message"
            );
        }
    }
}

#[test]
fn small_rsa_keys_still_need_insecure_verification() {
    let sig = signature("rsa1024", HashAlgorithm::Sha384);
    assert!(verify(RSA1024, &sig, SignatureAlgorithm::Rsa, HashAlgorithm::Sha384, MESSAGE, false).is_err());
    // Upstream always accepted 1024-bit RSA with SHA-1; that stays.
    let sig = signature("rsa1024", HashAlgorithm::Sha1);
    assert!(verify(RSA1024, &sig, SignatureAlgorithm::Rsa, HashAlgorithm::Sha1, MESSAGE, false).is_ok());
    // 2048-bit keys never need it.
    let sig = signature("rsa2048", HashAlgorithm::Sha384);
    assert!(verify(RSA2048, &sig, SignatureAlgorithm::Rsa, HashAlgorithm::Sha384, MESSAGE, false).is_ok());
}

#[test]
fn a_signature_from_another_key_is_rejected() {
    let sig = signature("p256", HashAlgorithm::Sha256);
    assert!(verify(P384, &sig, SignatureAlgorithm::Ecdsa, HashAlgorithm::Sha256, MESSAGE, false).is_err());
    let sig = signature("rsa2048", HashAlgorithm::Sha256);
    assert!(verify(RSA1024, &sig, SignatureAlgorithm::Rsa, HashAlgorithm::Sha256, MESSAGE, true).is_err());
    // Key type and signature algorithm must agree.
    let sig = signature("p256", HashAlgorithm::Sha256);
    assert!(verify(RSA2048, &sig, SignatureAlgorithm::Ecdsa, HashAlgorithm::Sha256, MESSAGE, true).is_err());
}
