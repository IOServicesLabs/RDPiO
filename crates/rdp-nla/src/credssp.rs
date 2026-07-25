//! Portable CredSSP / NLA (MS-CSSP) for non-Windows hosts — the counterpart to
//! the Win32 SSPI engine in [`super::sspi`].
//!
//! It implements enough of NTLMv2 (MS-NLMP) to satisfy a Windows RDP server's
//! Network Level Authentication:
//!
//!  1. the NEGOTIATE / CHALLENGE / AUTHENTICATE token exchange with NTLMv2
//!     responses, Extended Session Security (ESS) signing + sealing, session-key
//!     exchange, and the AUTHENTICATE message-integrity check (MIC);
//!  2. the CredSSP public-key channel binding (SHA-256 over a client nonce for
//!     protocol v5+, MS-CSSP 3.1.5) that defeats a TLS man-in-the-middle;
//!  3. the sealed `TSCredentials` carrying the logon password.
//!
//! The framing ([`crate::tsrequest`]) and the bound public key
//! ([`crate::x509`]) are shared with the Windows path. NLA runs once at
//! connection setup over the already-established TLS stream and never touches a
//! per-frame path, so a from-scratch Rust implementation here has no bearing on
//! streaming performance.
//!
//! Cross-checked against the MS-NLMP §4.2.4 test vectors (see the unit tests)
//! and FreeRDP's `ntlm.c` for the AV_PAIR / MIC handling.

use std::io::{Read, Write};

use rdp_crypto::{hmac_md5, md4, md5, sha256, Rc4};

use crate::tsrequest::{password_credentials_der, TsRequest, CREDSSP_VERSION};
use crate::NlaError;

// --- NTLM NegotiateFlags (MS-NLMP 2.2.2.5) -------------------------------------
const NTLMSSP_NEGOTIATE_UNICODE: u32 = 0x0000_0001;
const NTLMSSP_REQUEST_TARGET: u32 = 0x0000_0004;
const NTLMSSP_NEGOTIATE_SIGN: u32 = 0x0000_0010;
const NTLMSSP_NEGOTIATE_SEAL: u32 = 0x0000_0020;
const NTLMSSP_NEGOTIATE_NTLM: u32 = 0x0000_0200;
const NTLMSSP_NEGOTIATE_ALWAYS_SIGN: u32 = 0x0000_8000;
const NTLMSSP_NEGOTIATE_EXTENDED_SESSIONSECURITY: u32 = 0x0008_0000;
const NTLMSSP_NEGOTIATE_TARGET_INFO: u32 = 0x0080_0000;
const NTLMSSP_NEGOTIATE_VERSION: u32 = 0x0200_0000;
const NTLMSSP_NEGOTIATE_128: u32 = 0x2000_0000;
const NTLMSSP_NEGOTIATE_KEY_EXCH: u32 = 0x4000_0000;
const NTLMSSP_NEGOTIATE_56: u32 = 0x8000_0000;

const NTLM_SIGNATURE: &[u8; 8] = b"NTLMSSP\0";

/// AV_PAIR ids we care about (MS-NLMP 2.2.2.1).
const MSV_AV_EOL: u16 = 0x0000;
const MSV_AV_TIMESTAMP: u16 = 0x0007;
const MSV_AV_FLAGS: u16 = 0x0006;
/// MsvAvFlags bit: "the client provides a MIC in the AUTHENTICATE message".
const MSV_AV_FLAGS_MIC_PRESENT: u32 = 0x0000_0002;

/// The 8-byte NTLM `Version` we advertise (MS-NLMP 2.2.2.10). Cosmetic — servers
/// key off NegotiateFlags, not this — so we report a plausible Windows 10 build.
const NTLM_VERSION: [u8; 8] = [10, 0, 0x61, 0x4a, 0x00, 0x00, 0x00, 0x0f];

/// Signing/sealing key-derivation magic constants (MS-NLMP 3.4.5.2 / 3.4.5.3);
/// the trailing NUL is part of each string.
const CLIENT_SIGNING_MAGIC: &[u8] = b"session key to client-to-server signing key magic constant\0";
const SERVER_SIGNING_MAGIC: &[u8] = b"session key to server-to-client signing key magic constant\0";
const CLIENT_SEALING_MAGIC: &[u8] = b"session key to client-to-server sealing key magic constant\0";
const SERVER_SEALING_MAGIC: &[u8] = b"session key to server-to-client sealing key magic constant\0";

/// CredSSP public-key binding magic strings (MS-CSSP 3.1.5); NUL included.
const CLIENT_SERVER_HASH_MAGIC: &[u8] = b"CredSSP Client-To-Server Binding Hash\0";
const SERVER_CLIENT_HASH_MAGIC: &[u8] = b"CredSSP Server-To-Client Binding Hash\0";

fn seq(msg: &str) -> NlaError {
    NlaError::Sequence(msg.into())
}

fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// Fill `buf` with OS entropy (`/dev/urandom`). CredSSP's nonce and the
/// NTLM client challenge / exported session key must be unpredictable.
fn os_random(buf: &mut [u8]) -> Result<(), NlaError> {
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(buf)?;
    Ok(())
}

/// Current time as a Windows FILETIME (100-ns intervals since 1601-01-01 UTC).
/// Only used when the server's CHALLENGE omits a timestamp (modern Windows always
/// includes one, which we echo instead).
fn filetime_now() -> [u8; 8] {
    const UNIX_TO_FILETIME: u64 = 116_444_736_000_000_000;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let intervals = (nanos / 100) as u64 + UNIX_TO_FILETIME;
    intervals.to_le_bytes()
}

// --- NTLMv2 key/response math (MS-NLMP 3.3.2) ---------------------------------

/// `NTOWFv2 = HMAC-MD5(MD4(UTF16LE(password)), UTF16LE(UPPER(user) + domain))`.
/// The user name is upper-cased; the domain is used as-is.
fn ntowf_v2(password: &str, user: &str, domain: &str) -> [u8; 16] {
    let nt_hash = md4(&utf16le(password));
    let mut id = utf16le(&user.to_uppercase());
    id.extend_from_slice(&utf16le(domain));
    hmac_md5(&nt_hash, &id)
}

/// Build the NTLMv2 `temp` blob (MS-NLMP 3.3.2): the versioned header, the
/// timestamp and client challenge, and the server's TargetInfo AV_PAIRs.
fn build_temp(timestamp: &[u8; 8], client_challenge: &[u8; 8], target_info: &[u8]) -> Vec<u8> {
    let mut temp = Vec::with_capacity(28 + target_info.len() + 4);
    temp.extend_from_slice(&[0x01, 0x01]); // RespType=1, HiRespType=1
    temp.extend_from_slice(&[0u8; 6]); // Reserved
    temp.extend_from_slice(timestamp);
    temp.extend_from_slice(client_challenge);
    temp.extend_from_slice(&[0u8; 4]); // Reserved
    temp.extend_from_slice(target_info);
    temp.extend_from_slice(&[0u8; 4]); // Reserved
    temp
}

/// From `NTOWFv2`, the 8-byte server challenge and the `temp` blob, derive the
/// NTProofStr and the NTLMv2 session base key (MS-NLMP 3.3.2 / 3.4).
fn ntlmv2_response(ntowf: &[u8; 16], server_challenge: &[u8; 8], temp: &[u8]) -> ([u8; 16], [u8; 16]) {
    let mut buf = Vec::with_capacity(8 + temp.len());
    buf.extend_from_slice(server_challenge);
    buf.extend_from_slice(temp);
    let nt_proof = hmac_md5(ntowf, &buf);
    let session_base_key = hmac_md5(ntowf, &nt_proof);
    (nt_proof, session_base_key)
}

// --- NTLM message signing / sealing (ESS + key exchange, MS-NLMP 3.4.4.2) -----

fn derive_key(exported_session_key: &[u8; 16], magic: &[u8]) -> [u8; 16] {
    let mut buf = Vec::with_capacity(16 + magic.len());
    buf.extend_from_slice(exported_session_key);
    buf.extend_from_slice(magic);
    md5(&buf)
}

/// Seal `plaintext` into the CredSSP wire layout `[signature(16)][ciphertext]`.
/// `seal_rc4` is the *persistent* RC4 handle for this direction (its stream must
/// continue across successive messages); `seq` is the message sequence number.
fn ntlm_seal(signing_key: &[u8; 16], seal_rc4: &mut Rc4, plaintext: &[u8], seq_num: u32) -> Vec<u8> {
    // checksum = HMAC-MD5(SigningKey, seq || plaintext)[0..8]  (over the plaintext)
    let mut to_sign = Vec::with_capacity(4 + plaintext.len());
    to_sign.extend_from_slice(&seq_num.to_le_bytes());
    to_sign.extend_from_slice(plaintext);
    let checksum = hmac_md5(signing_key, &to_sign);

    // Encrypt the message, then the checksum, on the *same* continuing stream.
    let mut ciphertext = plaintext.to_vec();
    seal_rc4.apply(&mut ciphertext);
    let mut sealed_checksum = checksum[..8].to_vec();
    seal_rc4.apply(&mut sealed_checksum);

    let mut out = Vec::with_capacity(16 + ciphertext.len());
    out.extend_from_slice(&1u32.to_le_bytes()); // signature version
    out.extend_from_slice(&sealed_checksum); // 8 bytes
    out.extend_from_slice(&seq_num.to_le_bytes());
    out.extend_from_slice(&ciphertext);
    out
}

/// Reverse of [`ntlm_seal`] for the peer's direction: decrypt and verify the
/// `[signature(16)][ciphertext]` blob, returning the plaintext.
fn ntlm_unseal(
    signing_key: &[u8; 16],
    seal_rc4: &mut Rc4,
    blob: &[u8],
    seq_num: u32,
) -> Result<Vec<u8>, NlaError> {
    if blob.len() < 16 {
        return Err(seq("sealed message too short"));
    }
    let (sig, ciphertext) = blob.split_at(16);

    let mut plaintext = ciphertext.to_vec();
    seal_rc4.apply(&mut plaintext);

    let mut to_sign = Vec::with_capacity(4 + plaintext.len());
    to_sign.extend_from_slice(&seq_num.to_le_bytes());
    to_sign.extend_from_slice(&plaintext);
    let checksum = hmac_md5(signing_key, &to_sign);
    let mut sealed_checksum = checksum[..8].to_vec();
    seal_rc4.apply(&mut sealed_checksum);

    let version_ok = sig[0..4] == 1u32.to_le_bytes();
    let checksum_ok = sig[4..12] == sealed_checksum[..];
    let seq_ok = sig[12..16] == seq_num.to_le_bytes();
    if !version_ok || !checksum_ok || !seq_ok {
        return Err(seq("sealed message signature mismatch"));
    }
    Ok(plaintext)
}

/// The four ESS message-protection keys plus the two persistent RC4 handles.
struct MessageCrypto {
    client_signing: [u8; 16],
    server_signing: [u8; 16],
    client_seal: Rc4,
    server_seal: Rc4,
}

impl MessageCrypto {
    fn new(exported_session_key: &[u8; 16]) -> Self {
        let client_sealing = derive_key(exported_session_key, CLIENT_SEALING_MAGIC);
        let server_sealing = derive_key(exported_session_key, SERVER_SEALING_MAGIC);
        Self {
            client_signing: derive_key(exported_session_key, CLIENT_SIGNING_MAGIC),
            server_signing: derive_key(exported_session_key, SERVER_SIGNING_MAGIC),
            client_seal: Rc4::new(&client_sealing),
            server_seal: Rc4::new(&server_sealing),
        }
    }

    fn seal(&mut self, plaintext: &[u8], seq_num: u32) -> Vec<u8> {
        ntlm_seal(&self.client_signing, &mut self.client_seal, plaintext, seq_num)
    }

    fn unseal(&mut self, blob: &[u8], seq_num: u32) -> Result<Vec<u8>, NlaError> {
        ntlm_unseal(&self.server_signing, &mut self.server_seal, blob, seq_num)
    }
}

// --- NTLM messages ------------------------------------------------------------

/// Build the NEGOTIATE_MESSAGE (MS-NLMP 2.2.1.1). Domain and workstation are
/// left empty (the server does not use them at this stage).
fn build_negotiate(flags: u32) -> Vec<u8> {
    let mut m = Vec::with_capacity(40);
    m.extend_from_slice(NTLM_SIGNATURE);
    m.extend_from_slice(&1u32.to_le_bytes()); // MessageType = NEGOTIATE
    m.extend_from_slice(&flags.to_le_bytes());
    m.extend_from_slice(&[0u8; 8]); // DomainNameFields (empty)
    m.extend_from_slice(&[0u8; 8]); // WorkstationFields (empty)
    m.extend_from_slice(&NTLM_VERSION);
    m
}

/// The fields we need out of a CHALLENGE_MESSAGE (MS-NLMP 2.2.1.2).
struct Challenge {
    flags: u32,
    server_challenge: [u8; 8],
    target_info: Vec<u8>,
}

fn parse_challenge(msg: &[u8]) -> Result<Challenge, NlaError> {
    // Fixed header runs through the TargetInfoFields at offset 40..48.
    if msg.len() < 48 || &msg[0..8] != NTLM_SIGNATURE {
        return Err(seq("malformed NTLM CHALLENGE"));
    }
    if u32::from_le_bytes(msg[8..12].try_into().unwrap()) != 2 {
        return Err(seq("NTLM message is not a CHALLENGE"));
    }
    let flags = u32::from_le_bytes(msg[20..24].try_into().unwrap());
    let mut server_challenge = [0u8; 8];
    server_challenge.copy_from_slice(&msg[24..32]);

    let ti_len = u16::from_le_bytes(msg[40..42].try_into().unwrap()) as usize;
    let ti_off = u32::from_le_bytes(msg[44..48].try_into().unwrap()) as usize;
    let target_info = if ti_len == 0 {
        Vec::new()
    } else {
        msg.get(ti_off..ti_off + ti_len)
            .ok_or_else(|| seq("CHALLENGE TargetInfo out of bounds"))?
            .to_vec()
    };
    Ok(Challenge {
        flags,
        server_challenge,
        target_info,
    })
}

/// Parse AV_PAIRs into `(id, value)` tuples, stopping at (and dropping) the EOL.
fn parse_av_pairs(mut data: &[u8]) -> Vec<(u16, Vec<u8>)> {
    let mut out = Vec::new();
    while data.len() >= 4 {
        let id = u16::from_le_bytes([data[0], data[1]]);
        let len = u16::from_le_bytes([data[2], data[3]]) as usize;
        if id == MSV_AV_EOL {
            break;
        }
        if data.len() < 4 + len {
            break;
        }
        out.push((id, data[4..4 + len].to_vec()));
        data = &data[4 + len..];
    }
    out
}

/// Serialize AV_PAIRs and append the EOL terminator.
fn serialize_av_pairs(pairs: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (id, val) in pairs {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&(val.len() as u16).to_le_bytes());
        out.extend_from_slice(val);
    }
    out.extend_from_slice(&MSV_AV_EOL.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// The server's TargetInfo, adjusted for the response: set the MsvAvFlags MIC
/// bit (adding the pair if absent) so the server validates our MIC. Returns the
/// rewritten AV_PAIR bytes and the timestamp to bind (the server's if present).
fn response_target_info(server_target_info: &[u8]) -> (Vec<u8>, [u8; 8]) {
    let mut pairs = parse_av_pairs(server_target_info);

    let mut timestamp = [0u8; 8];
    let mut have_timestamp = false;
    for (id, val) in &pairs {
        if *id == MSV_AV_TIMESTAMP && val.len() == 8 {
            timestamp.copy_from_slice(val);
            have_timestamp = true;
        }
    }
    if !have_timestamp {
        timestamp = filetime_now();
    }

    // OR the MIC-present bit into MsvAvFlags, creating the pair if needed.
    if let Some((_, val)) = pairs.iter_mut().find(|(id, _)| *id == MSV_AV_FLAGS) {
        let mut flags = if val.len() == 4 {
            u32::from_le_bytes([val[0], val[1], val[2], val[3]])
        } else {
            0
        };
        flags |= MSV_AV_FLAGS_MIC_PRESENT;
        *val = flags.to_le_bytes().to_vec();
    } else {
        pairs.push((MSV_AV_FLAGS, MSV_AV_FLAGS_MIC_PRESENT.to_le_bytes().to_vec()));
    }

    (serialize_av_pairs(&pairs), timestamp)
}

/// AUTHENTICATE_MESSAGE payload field descriptor (Len, MaxLen, BufferOffset).
fn field(len: usize, offset: usize) -> [u8; 8] {
    let mut f = [0u8; 8];
    f[0..2].copy_from_slice(&(len as u16).to_le_bytes());
    f[2..4].copy_from_slice(&(len as u16).to_le_bytes());
    f[4..8].copy_from_slice(&(offset as u32).to_le_bytes());
    f
}

/// Assemble the AUTHENTICATE_MESSAGE (MS-NLMP 2.2.1.3) with a zeroed MIC field;
/// the caller fills the MIC in place once it can hash all three messages.
#[allow(clippy::too_many_arguments)]
fn build_authenticate(
    flags: u32,
    domain: &str,
    user: &str,
    lm_response: &[u8],
    nt_response: &[u8],
    encrypted_session_key: &[u8],
) -> Vec<u8> {
    let domain_b = utf16le(domain);
    let user_b = utf16le(user);

    // Fixed header (through the MIC) is 88 bytes; payload follows.
    const HEADER: usize = 88;
    let mut payload = Vec::new();
    let dom_off = HEADER;
    payload.extend_from_slice(&domain_b);
    let user_off = HEADER + payload.len();
    payload.extend_from_slice(&user_b);
    let ws_off = HEADER + payload.len(); // workstation: empty
    let lm_off = HEADER + payload.len();
    payload.extend_from_slice(lm_response);
    let nt_off = HEADER + payload.len();
    payload.extend_from_slice(nt_response);
    let key_off = HEADER + payload.len();
    payload.extend_from_slice(encrypted_session_key);

    let mut m = Vec::with_capacity(HEADER + payload.len());
    m.extend_from_slice(NTLM_SIGNATURE);
    m.extend_from_slice(&3u32.to_le_bytes()); // MessageType = AUTHENTICATE
    m.extend_from_slice(&field(lm_response.len(), lm_off));
    m.extend_from_slice(&field(nt_response.len(), nt_off));
    m.extend_from_slice(&field(domain_b.len(), dom_off));
    m.extend_from_slice(&field(user_b.len(), user_off));
    m.extend_from_slice(&field(0, ws_off));
    m.extend_from_slice(&field(encrypted_session_key.len(), key_off));
    m.extend_from_slice(&flags.to_le_bytes());
    m.extend_from_slice(&NTLM_VERSION);
    m.extend_from_slice(&[0u8; 16]); // MIC (filled by the caller)
    debug_assert_eq!(m.len(), HEADER);
    m.extend_from_slice(&payload);
    m
}

/// Offset of the 16-byte MIC field within the AUTHENTICATE message.
const AUTH_MIC_OFFSET: usize = 72;

// --- Read one DER element off the wire ----------------------------------------

const MAX_DER_ELEMENT: usize = 256 * 1024;

fn read_der_element<S: Read>(stream: &mut S) -> Result<Vec<u8>, NlaError> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head)?;
    let mut out = vec![head[0], head[1]];
    let content_len = if head[1] < 0x80 {
        head[1] as usize
    } else {
        let n = (head[1] & 0x7f) as usize;
        if n == 0 || n > 4 {
            return Err(seq("invalid DER length"));
        }
        let mut lb = vec![0u8; n];
        stream.read_exact(&mut lb)?;
        out.extend_from_slice(&lb);
        lb.iter().fold(0usize, |acc, &b| (acc << 8) | b as usize)
    };
    if content_len > MAX_DER_ELEMENT {
        return Err(seq("DER element too large"));
    }
    let mut body = vec![0u8; content_len];
    stream.read_exact(&mut body)?;
    out.extend_from_slice(&body);
    Ok(out)
}

// --- The CredSSP handshake ----------------------------------------------------

/// Run CredSSP/NLA to completion over `stream` (already TLS-protected),
/// authenticating `domain`/`username`/`password`. `server_cert_der` is the TLS
/// server certificate, whose public key is bound into the exchange. `spn` (e.g.
/// `"TERMSRV/host"`) is accepted for signature parity with the Windows path; the
/// pure-NTLM engine does not send a target-name AV pair (default RDP servers
/// don't require Extended Protection for Authentication).
pub fn authenticate<S: Read + Write>(
    stream: &mut S,
    spn: &str,
    server_cert_der: &[u8],
    domain: &str,
    username: &str,
    password: &str,
) -> Result<(), NlaError> {
    let public_key = crate::x509::extract_public_key(server_cert_der)?;
    tracing::info!(
        spn,
        public_key_len = public_key.len(),
        "starting CredSSP/NLA (portable NTLMv2)"
    );

    // 1) NEGOTIATE.
    let negotiate_flags = NTLMSSP_NEGOTIATE_UNICODE
        | NTLMSSP_REQUEST_TARGET
        | NTLMSSP_NEGOTIATE_SIGN
        | NTLMSSP_NEGOTIATE_SEAL
        | NTLMSSP_NEGOTIATE_NTLM
        | NTLMSSP_NEGOTIATE_ALWAYS_SIGN
        | NTLMSSP_NEGOTIATE_EXTENDED_SESSIONSECURITY
        | NTLMSSP_NEGOTIATE_VERSION
        | NTLMSSP_NEGOTIATE_128
        | NTLMSSP_NEGOTIATE_KEY_EXCH
        | NTLMSSP_NEGOTIATE_56;
    let negotiate_msg = build_negotiate(negotiate_flags);
    let req = TsRequest::with_nego_token(negotiate_msg.clone());
    stream.write_all(&req.to_der())?;
    stream.flush()?;

    // 2) CHALLENGE.
    let resp = TsRequest::from_der(&read_der_element(stream)?)?;
    if let Some(ec) = resp.error_code {
        if ec != 0 {
            return Err(seq(&format!("server error 0x{ec:08X}")));
        }
    }
    let negotiated_version = CREDSSP_VERSION.min(resp.version);
    let challenge_msg = resp
        .nego_tokens
        .into_iter()
        .next()
        .ok_or_else(|| seq("server omitted NTLM CHALLENGE"))?;
    let challenge = parse_challenge(&challenge_msg)?;
    tracing::debug!(
        server_version = resp.version,
        negotiated_version,
        challenge_flags = format_args!("0x{:08X}", challenge.flags),
        target_info_len = challenge.target_info.len(),
        "received NTLM CHALLENGE"
    );

    // 3) Compute the NTLMv2 response.
    let ntowf = ntowf_v2(password, username, domain);
    let (response_ti, timestamp) = response_target_info(&challenge.target_info);
    let mut client_challenge = [0u8; 8];
    os_random(&mut client_challenge)?;
    let temp = build_temp(&timestamp, &client_challenge, &response_ti);
    let (nt_proof, session_base_key) =
        ntlmv2_response(&ntowf, &challenge.server_challenge, &temp);

    let mut nt_response = Vec::with_capacity(16 + temp.len());
    nt_response.extend_from_slice(&nt_proof);
    nt_response.extend_from_slice(&temp);
    // With a timestamp bound in the response the LM field is unused (MS-NLMP
    // 3.1.5.1.2); send the canonical 24 zero bytes.
    let lm_response = [0u8; 24];

    // KeyExchangeKey == SessionBaseKey for NTLMv2 (MS-NLMP 3.4.5.1).
    let use_key_exch = challenge.flags & NTLMSSP_NEGOTIATE_KEY_EXCH != 0;
    let mut exported_session_key = [0u8; 16];
    let encrypted_session_key: Vec<u8> = if use_key_exch {
        os_random(&mut exported_session_key)?;
        let mut enc = exported_session_key.to_vec();
        Rc4::new(&session_base_key).apply(&mut enc);
        enc
    } else {
        exported_session_key = session_base_key;
        Vec::new()
    };

    let mut auth_flags = NTLMSSP_NEGOTIATE_UNICODE
        | NTLMSSP_REQUEST_TARGET
        | NTLMSSP_NEGOTIATE_SIGN
        | NTLMSSP_NEGOTIATE_SEAL
        | NTLMSSP_NEGOTIATE_NTLM
        | NTLMSSP_NEGOTIATE_ALWAYS_SIGN
        | NTLMSSP_NEGOTIATE_EXTENDED_SESSIONSECURITY
        | NTLMSSP_NEGOTIATE_TARGET_INFO
        | NTLMSSP_NEGOTIATE_VERSION
        | NTLMSSP_NEGOTIATE_128
        | NTLMSSP_NEGOTIATE_56;
    if use_key_exch {
        auth_flags |= NTLMSSP_NEGOTIATE_KEY_EXCH;
    }

    let mut authenticate_msg = build_authenticate(
        auth_flags,
        domain,
        username,
        &lm_response,
        &nt_response,
        &encrypted_session_key,
    );

    // 4) MIC = HMAC-MD5(ExportedSessionKey, NEGOTIATE || CHALLENGE || AUTHENTICATE)
    // with the AUTHENTICATE MIC field zeroed (MS-NLMP 3.1.5.1.2 / 3.2.5.1.2).
    let mut mic_input =
        Vec::with_capacity(negotiate_msg.len() + challenge_msg.len() + authenticate_msg.len());
    mic_input.extend_from_slice(&negotiate_msg);
    mic_input.extend_from_slice(&challenge_msg);
    mic_input.extend_from_slice(&authenticate_msg);
    let mic = hmac_md5(&exported_session_key, &mic_input);
    authenticate_msg[AUTH_MIC_OFFSET..AUTH_MIC_OFFSET + 16].copy_from_slice(&mic);

    // Derive the ESS message-protection keys / RC4 handles.
    let mut crypto = MessageCrypto::new(&exported_session_key);

    // 5) Public-key channel binding, sent alongside the AUTHENTICATE token.
    let use_hash = negotiated_version >= 5;
    let nonce = if use_hash {
        let mut n = [0u8; 32];
        os_random(&mut n)?;
        Some(n)
    } else {
        None
    };
    let pub_key_auth = if let Some(nonce) = &nonce {
        let mut h = Vec::with_capacity(CLIENT_SERVER_HASH_MAGIC.len() + 32 + public_key.len());
        h.extend_from_slice(CLIENT_SERVER_HASH_MAGIC);
        h.extend_from_slice(nonce);
        h.extend_from_slice(&public_key);
        crypto.seal(&sha256(&h), 0)
    } else {
        crypto.seal(&public_key, 0)
    };

    let pk_req = TsRequest {
        version: CREDSSP_VERSION,
        nego_tokens: vec![authenticate_msg],
        pub_key_auth: Some(pub_key_auth),
        client_nonce: nonce,
        ..Default::default()
    };
    stream.write_all(&pk_req.to_der())?;
    stream.flush()?;
    tracing::debug!(use_hash, "sent AUTHENTICATE + pubKeyAuth");

    // 6) Verify the server's sealed public-key echo.
    let resp = TsRequest::from_der(&read_der_element(stream)?)?;
    if let Some(ec) = resp.error_code {
        if ec != 0 {
            return Err(seq(&format!("server rejected authentication: 0x{ec:08X}")));
        }
    }
    let server_pk = resp
        .pub_key_auth
        .ok_or_else(|| seq("server omitted pubKeyAuth echo"))?;
    let decrypted = crypto.unseal(&server_pk, 0)?;
    if let Some(nonce) = &nonce {
        let mut h = Vec::with_capacity(SERVER_CLIENT_HASH_MAGIC.len() + 32 + public_key.len());
        h.extend_from_slice(SERVER_CLIENT_HASH_MAGIC);
        h.extend_from_slice(nonce);
        h.extend_from_slice(&public_key);
        if decrypted != sha256(&h) {
            return Err(NlaError::PubKeyMismatch);
        }
    } else {
        // Legacy echo: the server returns the public key incremented by one.
        let mut expected = public_key.clone();
        for b in expected.iter_mut() {
            let (v, carry) = b.overflowing_add(1);
            *b = v;
            if !carry {
                break;
            }
        }
        if decrypted != expected {
            return Err(NlaError::PubKeyMismatch);
        }
    }
    tracing::info!("server public-key echo verified");

    // 7) Sealed credentials (next sequence number).
    let creds = password_credentials_der(domain, username, password);
    let auth_info = crypto.seal(&creds, 1);
    let ai_req = TsRequest {
        version: CREDSSP_VERSION,
        auth_info: Some(auth_info),
        ..Default::default()
    };
    stream.write_all(&ai_req.to_der())?;
    stream.flush()?;
    tracing::info!("CredSSP credentials sent; NLA complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// MS-NLMP §4.2.4 — the NTLMv2 worked example (the values here are the
    /// widely-reproduced, stable ones: NTOWFv2, NTProofStr, SessionBaseKey and
    /// the key-exchange output). Reproducing them exercises MD4, HMAC-MD5, the
    /// `temp` layout and RC4 keying end to end.
    #[test]
    fn ms_nlmp_ntlmv2_vectors() {
        let ntowf = ntowf_v2("Password", "User", "Domain");
        assert_eq!(hex(&ntowf), "0c868a403bfd7a93a3001ef22ef02e3f");

        // §4.2.4.1.3 TargetInfo: NbDomainName "Domain", NbComputerName "Server".
        let mut target_info = Vec::new();
        target_info.extend_from_slice(&[0x02, 0x00, 0x0c, 0x00]);
        target_info.extend_from_slice(&utf16le("Domain"));
        target_info.extend_from_slice(&[0x01, 0x00, 0x0c, 0x00]);
        target_info.extend_from_slice(&utf16le("Server"));
        target_info.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // EOL

        let timestamp = [0u8; 8];
        let client_challenge = [0xaau8; 8];
        let temp = build_temp(&timestamp, &client_challenge, &target_info);

        let server_challenge = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let (nt_proof, session_base_key) = ntlmv2_response(&ntowf, &server_challenge, &temp);
        assert_eq!(hex(&nt_proof), "68cd0ab851e51c96aabc927bebef6a1c");
        assert_eq!(hex(&session_base_key), "8de40ccadbc14a82f15cb0ad0de95ca3");

        // §4.2.4.2 key exchange: EncryptedRandomSessionKey =
        // RC4(KeyExchangeKey=SessionBaseKey, RandomSessionKey = 0x55 * 16).
        let mut encrypted = [0x55u8; 16].to_vec();
        Rc4::new(&session_base_key).apply(&mut encrypted);
        assert_eq!(hex(&encrypted), "c5dad2544fc9799094ce1ce90bc9d03e");
    }

    /// Seal then unseal on matching keys must round-trip and validate the
    /// signature — this pins the ESS sign/seal wiring (order of operations,
    /// continuing RC4 stream, sequence number in the signature).
    #[test]
    fn seal_roundtrips_and_verifies() {
        let esk = [0x55u8; 16];
        let signing = derive_key(&esk, CLIENT_SIGNING_MAGIC);
        let sealing = derive_key(&esk, CLIENT_SEALING_MAGIC);
        let mut enc = Rc4::new(&sealing);
        let mut dec = Rc4::new(&sealing);

        let msg = b"the quick brown fox jumps over the lazy dog";
        let blob = ntlm_seal(&signing, &mut enc, msg, 0);
        assert_eq!(blob.len(), 16 + msg.len());
        assert_ne!(&blob[16..], &msg[..]); // actually encrypted
        let out = ntlm_unseal(&signing, &mut dec, &blob, 0).unwrap();
        assert_eq!(out, msg);
    }

    #[test]
    fn unseal_rejects_tampering() {
        let esk = [0x42u8; 16];
        let signing = derive_key(&esk, CLIENT_SIGNING_MAGIC);
        let sealing = derive_key(&esk, CLIENT_SEALING_MAGIC);
        let mut enc = Rc4::new(&sealing);
        let mut dec = Rc4::new(&sealing);

        let mut blob = ntlm_seal(&signing, &mut enc, b"secret", 0);
        let last = blob.len() - 1;
        blob[last] ^= 0xff; // flip a ciphertext bit
        assert!(ntlm_unseal(&signing, &mut dec, &blob, 0).is_err());
    }

    #[test]
    fn wrong_sequence_number_rejected() {
        let esk = [0x11u8; 16];
        let signing = derive_key(&esk, CLIENT_SIGNING_MAGIC);
        let sealing = derive_key(&esk, CLIENT_SEALING_MAGIC);
        let mut enc = Rc4::new(&sealing);
        let mut dec = Rc4::new(&sealing);

        let blob = ntlm_seal(&signing, &mut enc, b"payload", 0);
        assert!(ntlm_unseal(&signing, &mut dec, &blob, 1).is_err());
    }

    #[test]
    fn av_pairs_roundtrip_and_mic_flag_added() {
        // TargetInfo with only a timestamp; response must gain a MsvAvFlags pair
        // with the MIC bit set, and expose the timestamp for binding.
        let mut ti = Vec::new();
        ti.extend_from_slice(&[0x07, 0x00, 0x08, 0x00]); // MsvAvTimestamp, len 8
        ti.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef, 0x00, 0x11, 0x22, 0x33]);
        ti.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // EOL

        let (out, timestamp) = response_target_info(&ti);
        assert_eq!(timestamp, [0xde, 0xad, 0xbe, 0xef, 0x00, 0x11, 0x22, 0x33]);
        let pairs = parse_av_pairs(&out);
        let flags = pairs
            .iter()
            .find(|(id, _)| *id == MSV_AV_FLAGS)
            .expect("MsvAvFlags present");
        assert_eq!(flags.1, MSV_AV_FLAGS_MIC_PRESENT.to_le_bytes());
        // The original timestamp pair survives.
        assert!(pairs.iter().any(|(id, _)| *id == MSV_AV_TIMESTAMP));
    }

    #[test]
    fn authenticate_message_layout() {
        // Header is exactly 88 bytes; MIC field is at offset 72 and starts zeroed.
        let m = build_authenticate(0, "D", "U", &[0u8; 24], &[1u8; 40], &[2u8; 16]);
        assert_eq!(&m[0..8], NTLM_SIGNATURE);
        assert_eq!(u32::from_le_bytes(m[8..12].try_into().unwrap()), 3);
        assert_eq!(&m[AUTH_MIC_OFFSET..AUTH_MIC_OFFSET + 16], &[0u8; 16]);
        // NtChallengeResponse (40 bytes of 0x01) is present in the payload.
        let nt_off = u32::from_le_bytes(m[24..28].try_into().unwrap()) as usize;
        assert_eq!(&m[nt_off..nt_off + 40], &[1u8; 40]);
    }
}
