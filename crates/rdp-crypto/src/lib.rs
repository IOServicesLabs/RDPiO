//! Pure-Rust crypto primitives for the Standard RDP Security layer
//! (MS-RDPBCGR 5.3): RC4, MD5, and SHA-1, with the RDP key derivation and RSA
//! public-key encryption built on top (added next). No third-party crates — the
//! algorithms are implemented from scratch and checked against standard vectors.

#![forbid(unsafe_code)]

pub mod bignum;
pub mod keys;
pub mod md4;
pub mod md5;
pub mod rc4;
pub mod rsa;
pub mod sha1;
pub mod sha256;

pub use bignum::BigUint;
pub use keys::SessionCipher;
pub use md4::md4;
pub use md5::{hmac_md5, md5};
pub use rc4::Rc4;
pub use sha1::{hmac_sha1, sha1};
pub use sha256::sha256;
