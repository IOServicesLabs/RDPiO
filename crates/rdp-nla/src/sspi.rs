//! CredSSP / NLA over the Win32 SSPI `Negotiate` package (MS-CSSP).
//!
//! Drives the full client handshake over an already-established (TLS) stream:
//! the SPNEGO/NTLM/Kerberos token loop (`InitializeSecurityContextW`), the
//! public-key authentication echo (sealed with `EncryptMessage`, verified for
//! v5+ via the SHA-256 nonce binding), and finally the sealed `TSCredentials`.
//!
//! Windows-runtime FFI: type-checked against the windows crate for MSVC and
//! validated on a Windows host. The seal/unseal buffer layout, sequence
//! numbering, and v5+ hash construction mirror the reference CredSSP client.

use core::ffi::c_void;
use std::io::{Read, Write};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{SEC_E_OK, SEC_I_CONTINUE_NEEDED};
use windows::Win32::Security::Authentication::Identity::{
    AcquireCredentialsHandleW, DecryptMessage, DeleteSecurityContext, EncryptMessage,
    FreeContextBuffer, FreeCredentialsHandle, InitializeSecurityContextW, QueryContextAttributesW,
    SecBuffer, SecBufferDesc, SecPkgContext_Sizes, ISC_REQ_ALLOCATE_MEMORY,
    ISC_REQ_CONFIDENTIALITY, ISC_REQ_FLAGS, ISC_REQ_MUTUAL_AUTH, ISC_REQ_REPLAY_DETECT,
    ISC_REQ_SEQUENCE_DETECT, NEGOSSP_NAME_W, SECBUFFER_DATA, SECBUFFER_TOKEN, SECBUFFER_VERSION,
    SECPKG_ATTR_SIZES, SECPKG_CRED_OUTBOUND,
};
use windows::Win32::Security::Credentials::SecHandle;
use windows::Win32::System::Rpc::{SEC_WINNT_AUTH_IDENTITY_UNICODE, SEC_WINNT_AUTH_IDENTITY_W};

use crate::tsrequest::{password_credentials_der, TsRequest, CREDSSP_VERSION};
use crate::NlaError;

const SECURITY_NATIVE_DREP: u32 = 0x10;

/// `CredSSP Client-To-Server Binding Hash\0` (the trailing NUL is included).
const CLIENT_SERVER_HASH_MAGIC: &[u8] = b"CredSSP Client-To-Server Binding Hash\0";
/// `CredSSP Server-To-Client Binding Hash\0`.
const SERVER_CLIENT_HASH_MAGIC: &[u8] = b"CredSSP Server-To-Client Binding Hash\0";

fn sspi(hr: i32) -> NlaError {
    NlaError::Sspi(hr)
}

fn zero_handle() -> SecHandle {
    SecHandle {
        dwLower: 0,
        dwUpper: 0,
    }
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// A cryptographically-secure 32-byte CredSSP client nonce from the OS CSPRNG
/// (`BCryptGenRandom`). The nonce feeds the public-key channel binding, so it
/// must be unpredictable; if the OS RNG fails we log loudly and fall back to a
/// weak time-seeded value rather than abort.
fn nonce32() -> [u8; 32] {
    use windows::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    let mut out = [0u8; 32];
    // windows 0.62 takes the algorithm handle as `Option`; `None` selects the
    // system-preferred RNG (paired with BCRYPT_USE_SYSTEM_PREFERRED_RNG).
    let status = unsafe {
        BCryptGenRandom(
            None,
            &mut out,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status.0 == 0 {
        return out;
    }
    tracing::error!("BCryptGenRandom failed; CredSSP nonce is WEAK (not secure)");
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    for chunk in out.chunks_mut(8) {
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        chunk.copy_from_slice(&z.to_le_bytes());
    }
    out
}

/// Decrement a little-endian integer in place (for the legacy pubkey+1 echo).
fn decrement_le(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        if *b > 0 {
            *b -= 1;
            break;
        }
        *b = 0xFF;
    }
}

/// Largest TSRequest element we'll read. Real CredSSP messages (nego tokens,
/// pubKeyAuth, sealed credentials) are a few KB; the cap stops a hostile or
/// corrupt pre-auth server from making us allocate a multi-gigabyte buffer.
const MAX_DER_ELEMENT: usize = 256 * 1024;

/// Read exactly one DER element (tag + length + content) from the stream.
fn read_der_element<S: Read>(stream: &mut S) -> Result<Vec<u8>, NlaError> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head)?;
    let mut out = vec![head[0], head[1]];
    let content_len = if head[1] < 0x80 {
        head[1] as usize
    } else {
        let n = (head[1] & 0x7f) as usize;
        if n == 0 || n > 4 {
            return Err(NlaError::Sequence("invalid DER length".into()));
        }
        let mut lb = vec![0u8; n];
        stream.read_exact(&mut lb)?;
        out.extend_from_slice(&lb);
        lb.iter().fold(0usize, |acc, &b| (acc << 8) | b as usize)
    };
    if content_len > MAX_DER_ELEMENT {
        return Err(NlaError::Sequence("DER element too large".into()));
    }
    let mut body = vec![0u8; content_len];
    stream.read_exact(&mut body)?;
    out.extend_from_slice(&body);
    Ok(out)
}

/// SSPI seal/unseal over the established context, tracking the (possibly
/// shrunk) security trailer and reusing it as the decrypt split point.
struct Sealer {
    ctx: *const SecHandle,
    trailer: u32,
}

impl Sealer {
    /// Seal `plaintext` → `[signature][ciphertext]` (CredSSP wire layout).
    unsafe fn seal(&mut self, plaintext: &[u8], seq: u32) -> Result<Vec<u8>, NlaError> {
        let mut buf = vec![0u8; self.trailer as usize + plaintext.len()];
        buf[self.trailer as usize..].copy_from_slice(plaintext);
        let base = buf.as_mut_ptr();
        let mut secs = [
            SecBuffer {
                cbBuffer: self.trailer,
                BufferType: SECBUFFER_TOKEN,
                pvBuffer: base as *mut c_void,
            },
            SecBuffer {
                cbBuffer: plaintext.len() as u32,
                BufferType: SECBUFFER_DATA,
                pvBuffer: base.add(self.trailer as usize) as *mut c_void,
            },
        ];
        let desc = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: 2,
            pBuffers: secs.as_mut_ptr(),
        };
        let hr = EncryptMessage(self.ctx, 0, &desc, seq);
        if hr != SEC_E_OK {
            return Err(sspi(hr.0));
        }
        let tok = secs[0].cbBuffer as usize;
        let dat = secs[1].cbBuffer as usize;
        if (tok as u32) < self.trailer {
            // Signature shorter than the reserved trailer: close the gap and
            // adopt the real signature length for subsequent decrypts.
            buf.copy_within(self.trailer as usize..self.trailer as usize + dat, tok);
            self.trailer = tok as u32;
        }
        buf.truncate(tok + dat);
        Ok(buf)
    }

    /// Unseal a `[signature][ciphertext]` blob, returning the plaintext.
    unsafe fn unseal(&mut self, blob: &[u8], seq: u32) -> Result<Vec<u8>, NlaError> {
        if blob.len() < self.trailer as usize {
            return Err(NlaError::Sequence("sealed message too small".into()));
        }
        tracing::debug!(
            blob_len = blob.len(),
            trailer = self.trailer,
            data_len = blob.len() - self.trailer as usize,
            seq,
            "unsealing server message"
        );
        let mut token = blob[..self.trailer as usize].to_vec();
        let mut data = blob[self.trailer as usize..].to_vec();
        let mut secs = [
            SecBuffer {
                cbBuffer: token.len() as u32,
                BufferType: SECBUFFER_TOKEN,
                pvBuffer: token.as_mut_ptr() as *mut c_void,
            },
            SecBuffer {
                cbBuffer: data.len() as u32,
                BufferType: SECBUFFER_DATA,
                pvBuffer: data.as_mut_ptr() as *mut c_void,
            },
        ];
        let desc = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: 2,
            pBuffers: secs.as_mut_ptr(),
        };
        // Pass a real pfQOP out-param: the reference CredSSP client supplies one,
        // and some NTLM/Negotiate providers write through it unconditionally
        // (a NULL there surfaces as SEC_E_INTERNAL_ERROR, 0x80090304).
        let mut qop = 0u32;
        let hr = DecryptMessage(self.ctx, &desc, seq, Some(&mut qop));
        if hr != SEC_E_OK {
            return Err(sspi(hr.0));
        }
        data.truncate(secs[1].cbBuffer as usize);
        Ok(data)
    }
}

/// Owns the SSPI credential and security-context handles so they're freed on
/// *every* exit path (success or any early error), not just the happy path.
struct Handles {
    cred: SecHandle,
    ctx: SecHandle,
    /// Set once `InitializeSecurityContextW` has produced a context to delete.
    ctx_ready: bool,
}

impl Drop for Handles {
    fn drop(&mut self) {
        unsafe {
            if self.ctx_ready {
                let _ = DeleteSecurityContext(&self.ctx);
            }
            let _ = FreeCredentialsHandle(&self.cred);
        }
    }
}

/// Run CredSSP/NLA to completion over `stream` (already TLS-protected),
/// authenticating with `domain`/`username`/`password` against `spn` (e.g.
/// `"TERMSRV/host"`). `server_cert_der` is the TLS server certificate, from
/// which the bound public key is extracted.
pub fn authenticate<S: Read + Write>(
    stream: &mut S,
    spn: &str,
    server_cert_der: &[u8],
    domain: &str,
    username: &str,
    password: &str,
) -> Result<(), NlaError> {
    let public_key = crate::x509::extract_public_key(server_cert_der)?;
    tracing::info!(spn, public_key_len = public_key.len(), "starting CredSSP/NLA");

    unsafe {
        // 1) Negotiate credentials carrying the user's identity.
        let mut user_w = utf16(username);
        let mut domain_w = utf16(domain);
        let mut pass_w = utf16(password);
        let identity = SEC_WINNT_AUTH_IDENTITY_W {
            User: user_w.as_mut_ptr(),
            UserLength: user_w.len() as u32,
            Domain: domain_w.as_mut_ptr(),
            DomainLength: domain_w.len() as u32,
            Password: pass_w.as_mut_ptr(),
            PasswordLength: pass_w.len() as u32,
            Flags: SEC_WINNT_AUTH_IDENTITY_UNICODE,
        };
        let mut cred = zero_handle();
        AcquireCredentialsHandleW(
            PCWSTR::null(),
            NEGOSSP_NAME_W,
            SECPKG_CRED_OUTBOUND,
            None,
            Some(&identity as *const _ as *const c_void),
            None,
            None,
            &mut cred,
            None,
        )
        .map_err(|e| sspi(e.code().0))?;

        // From here on, the credential (and, once created, the context) are
        // owned by `handles`, whose Drop frees them on every return path below.
        let mut handles = Handles {
            cred,
            ctx: zero_handle(),
            ctx_ready: false,
        };

        let target: Vec<u16> = spn.encode_utf16().chain(std::iter::once(0)).collect();
        let req = ISC_REQ_FLAGS(
            ISC_REQ_CONFIDENTIALITY.0
                | ISC_REQ_REPLAY_DETECT.0
                | ISC_REQ_SEQUENCE_DETECT.0
                | ISC_REQ_MUTUAL_AUTH.0
                | ISC_REQ_ALLOCATE_MEMORY.0,
        );

        let ctx_ptr: *mut SecHandle = &mut handles.ctx;
        let cred_ptr: *const SecHandle = &handles.cred;
        let mut input_token: Vec<u8> = Vec::new();
        let mut negotiated_version = CREDSSP_VERSION;
        let final_token: Vec<u8>;

        // 2) SPNEGO/NTLM/Kerberos token loop.
        loop {
            let mut in_sec = [SecBuffer {
                cbBuffer: input_token.len() as u32,
                BufferType: SECBUFFER_TOKEN,
                pvBuffer: input_token.as_mut_ptr() as *mut c_void,
            }];
            let in_desc = SecBufferDesc {
                ulVersion: SECBUFFER_VERSION,
                cBuffers: 1,
                pBuffers: in_sec.as_mut_ptr(),
            };
            let mut out_sec = [SecBuffer {
                cbBuffer: 0,
                BufferType: SECBUFFER_TOKEN,
                pvBuffer: std::ptr::null_mut(),
            }];
            let mut out_desc = SecBufferDesc {
                ulVersion: SECBUFFER_VERSION,
                cBuffers: 1,
                pBuffers: out_sec.as_mut_ptr(),
            };
            let mut attrs = 0u32;
            let hr = InitializeSecurityContextW(
                Some(cred_ptr),
                if handles.ctx_ready {
                    Some(ctx_ptr as *const SecHandle)
                } else {
                    None
                },
                Some(target.as_ptr()),
                req,
                0,
                SECURITY_NATIVE_DREP,
                if handles.ctx_ready {
                    Some(&in_desc as *const _)
                } else {
                    None
                },
                0,
                Some(ctx_ptr),
                Some(&mut out_desc),
                &mut attrs,
                None,
            );
            handles.ctx_ready = true;

            let o = out_sec[0];
            let token = if o.cbBuffer > 0 && !o.pvBuffer.is_null() {
                let s = std::slice::from_raw_parts(o.pvBuffer as *const u8, o.cbBuffer as usize)
                    .to_vec();
                let _ = FreeContextBuffer(o.pvBuffer);
                s
            } else {
                Vec::new()
            };

            if hr == SEC_E_OK {
                final_token = token;
                break;
            } else if hr == SEC_I_CONTINUE_NEEDED {
                let mut req_pdu = TsRequest {
                    version: CREDSSP_VERSION,
                    ..Default::default()
                };
                if !token.is_empty() {
                    req_pdu.nego_tokens = vec![token];
                }
                stream.write_all(&req_pdu.to_der())?;
                stream.flush()?;

                let resp = TsRequest::from_der(&read_der_element(stream)?)?;
                if let Some(ec) = resp.error_code {
                    if ec != 0 {
                        return Err(NlaError::Sequence(format!("server error 0x{ec:08X}")));
                    }
                }
                negotiated_version = negotiated_version.min(resp.version);
                input_token = resp.nego_tokens.into_iter().next().unwrap_or_default();
                tracing::debug!(
                    server_version = resp.version,
                    in_token = input_token.len(),
                    "CredSSP negotiation round"
                );
            } else {
                return Err(sspi(hr.0));
            }
        }
        tracing::info!(negotiated_version, "SSPI Negotiate handshake complete");

        // 3) Message sizes for sealing.
        let mut sizes = SecPkgContext_Sizes {
            cbMaxToken: 0,
            cbMaxSignature: 0,
            cbBlockSize: 0,
            cbSecurityTrailer: 0,
        };
        QueryContextAttributesW(
            ctx_ptr,
            SECPKG_ATTR_SIZES,
            &mut sizes as *mut _ as *mut c_void,
        )
        .map_err(|e| sspi(e.code().0))?;
        tracing::debug!(
            cb_max_token = sizes.cbMaxToken,
            cb_max_signature = sizes.cbMaxSignature,
            cb_block_size = sizes.cbBlockSize,
            cb_security_trailer = sizes.cbSecurityTrailer,
            "SSPI context sizes"
        );
        let mut sealer = Sealer {
            ctx: ctx_ptr,
            trailer: sizes.cbSecurityTrailer,
        };

        // 4) Public-key authentication (anti-MITM channel binding).
        let nonce = nonce32();
        let use_hash = negotiated_version >= 5;
        let pub_key_auth = if use_hash {
            let mut h = Vec::with_capacity(CLIENT_SERVER_HASH_MAGIC.len() + 32 + public_key.len());
            h.extend_from_slice(CLIENT_SERVER_HASH_MAGIC);
            h.extend_from_slice(&nonce);
            h.extend_from_slice(&public_key);
            sealer.seal(&rdp_crypto::sha256(&h), 0)?
        } else {
            sealer.seal(&public_key, 0)?
        };

        let mut pk_req = TsRequest {
            version: CREDSSP_VERSION,
            pub_key_auth: Some(pub_key_auth),
            ..Default::default()
        };
        if !final_token.is_empty() {
            pk_req.nego_tokens = vec![final_token];
        }
        if use_hash {
            pk_req.client_nonce = Some(nonce);
        }
        stream.write_all(&pk_req.to_der())?;
        stream.flush()?;
        tracing::debug!(
            use_hash,
            sealed_len = pk_req.pub_key_auth.as_ref().map(|p| p.len()).unwrap_or(0),
            final_token_len = pk_req.nego_tokens.first().map(|t| t.len()).unwrap_or(0),
            trailer_after_seal = sealer.trailer,
            "sent pubKeyAuth (public-key channel binding)"
        );

        // 5) Verify the server's public-key echo.
        let resp = TsRequest::from_der(&read_der_element(stream)?)?;
        if let Some(ec) = resp.error_code {
            if ec != 0 {
                return Err(NlaError::Sequence(format!("server error 0x{ec:08X}")));
            }
        }
        tracing::debug!(
            error_code = ?resp.error_code,
            nego_tokens = resp.nego_tokens.len(),
            pub_key_auth_len = resp.pub_key_auth.as_ref().map(|p| p.len()).unwrap_or(0),
            "server response to pubKeyAuth"
        );
        let server_pk = resp
            .pub_key_auth
            .ok_or_else(|| NlaError::Sequence("server omitted pubKeyAuth".into()))?;
        let decrypted = sealer.unseal(&server_pk, 0)?;
        if use_hash {
            let mut h = Vec::with_capacity(SERVER_CLIENT_HASH_MAGIC.len() + 32 + public_key.len());
            h.extend_from_slice(SERVER_CLIENT_HASH_MAGIC);
            h.extend_from_slice(&nonce);
            h.extend_from_slice(&public_key);
            if decrypted != rdp_crypto::sha256(&h) {
                return Err(NlaError::PubKeyMismatch);
            }
        } else {
            let mut d = decrypted;
            decrement_le(&mut d);
            if d != public_key {
                return Err(NlaError::PubKeyMismatch);
            }
        }

        tracing::info!("server public-key echo verified");

        // 6) Send the sealed credentials (next sequence number).
        let creds = password_credentials_der(domain, username, password);
        let auth_info = sealer.seal(&creds, 1)?;
        let ai_req = TsRequest {
            version: CREDSSP_VERSION,
            auth_info: Some(auth_info),
            ..Default::default()
        };
        stream.write_all(&ai_req.to_der())?;
        stream.flush()?;
        tracing::info!("CredSSP credentials sent; NLA complete");

        // `handles` (cred + ctx) is freed by its Drop as this scope unwinds.
        Ok(())
    }
}
