//! Cryptographically-secure random bytes from the OS.
//!
//! Used for the Standard RDP Security client random and (indirectly) anywhere
//! unpredictability matters. On Windows this is `BCryptGenRandom` with the
//! system-preferred RNG; elsewhere it reads `/dev/urandom`. Returns whether the
//! fill succeeded so callers can fail loudly rather than proceed with weak bytes.

/// Fill `buf` with cryptographically-secure random bytes. Returns `false` if the
/// OS RNG was unavailable (the caller must then treat the bytes as untrusted).
#[cfg(windows)]
pub fn fill(buf: &mut [u8]) -> bool {
    use windows::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    // `None` algorithm handle + USE_SYSTEM_PREFERRED_RNG = the OS CSPRNG.
    let status = unsafe { BCryptGenRandom(None, buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    status.0 == 0 // STATUS_SUCCESS
}

#[cfg(not(windows))]
pub fn fill(buf: &mut [u8]) -> bool {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_and_varies() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        assert!(fill(&mut a));
        assert!(fill(&mut b));
        // Two CSPRNG draws are astronomically unlikely to match or be all-zero.
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 32]);
    }
}
