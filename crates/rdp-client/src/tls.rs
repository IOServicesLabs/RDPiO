//! Windows SChannel TLS client (SSPI) for the Enhanced RDP Security path.
//!
//! Wraps a byte stream in a Schannel-negotiated TLS session: it drives the
//! `InitializeSecurityContextW` handshake loop, then implements [`Read`]/[`Write`]
//! via `EncryptMessage`/`DecryptMessage` so the rest of the client — which is
//! generic over `Read + Write` — runs unchanged over the tunnel. The negotiated
//! server certificate is exposed via [`TlsStream::remote_cert_der`] for CredSSP
//! public-key binding (NLA).
//!
//! This is Windows-runtime code: it cannot be exercised in the Linux sandbox
//! (no SChannel), so it is written against the `windows` crate and type-checked
//! for the MSVC target, then validated on a Windows host. It is wired into the
//! connect path together with the CredSSP/NLA layer.

use core::ffi::c_void;
use std::io::{self, Read, Write};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    SEC_E_INCOMPLETE_MESSAGE, SEC_E_OK, SEC_I_CONTEXT_EXPIRED, SEC_I_CONTINUE_NEEDED,
    SEC_I_RENEGOTIATE,
};
use windows::Win32::Security::Authentication::Identity::{
    AcquireCredentialsHandleW, DecryptMessage, DeleteSecurityContext, EncryptMessage,
    FreeContextBuffer, FreeCredentialsHandle, InitializeSecurityContextW, QueryContextAttributesW,
    SecBuffer, SecBufferDesc, SecPkgContext_StreamSizes, ISC_REQ_ALLOCATE_MEMORY,
    ISC_REQ_CONFIDENTIALITY, ISC_REQ_EXTENDED_ERROR, ISC_REQ_FLAGS, ISC_REQ_INTEGRITY,
    ISC_REQ_REPLAY_DETECT, ISC_REQ_SEQUENCE_DETECT, ISC_REQ_STREAM, SCH_CREDENTIALS,
    SCH_CREDENTIALS_VERSION, SCH_CRED_MANUAL_CRED_VALIDATION, SCH_CRED_NO_DEFAULT_CREDS,
    SECBUFFER_DATA, SECBUFFER_EMPTY, SECBUFFER_EXTRA, SECBUFFER_STREAM_HEADER,
    SECBUFFER_STREAM_TRAILER, SECBUFFER_TOKEN, SECBUFFER_VERSION, SECPKG_ATTR_REMOTE_CERT_CONTEXT,
    SECPKG_ATTR_STREAM_SIZES, SECPKG_CRED_OUTBOUND,
};
use windows::Win32::Security::Credentials::SecHandle;
use windows::Win32::Security::Cryptography::{CertFreeCertificateContext, CERT_CONTEXT};

/// `targetdatarep` for SSPI: native byte order.
const SECURITY_NATIVE_DREP: u32 = 0x10;
/// How much ciphertext to pull from the socket per read.
const READ_CHUNK: usize = 16384;

fn win_err(context: &str, hr: i32) -> io::Error {
    io::Error::other(format!("{context}: 0x{hr:08X}"))
}

/// Turn a Schannel handshake failure into something a user can act on.
///
/// The case worth naming is certificate trust: RDP hosts almost always present a
/// self-signed certificate, which makes this the single most likely reason a
/// first connection fails — and a bare `0x80090325` tells nobody anything.
fn handshake_err(hr: i32) -> io::Error {
    let reason = match hr as u32 {
        0x8009_0325 => "the server's certificate is not trusted (self-signed, or an unknown CA)",
        0x8009_0327 => "the server's certificate could not be checked for revocation",
        0x8009_0328 => "the server's certificate has expired",
        0x800B_0101 => "the server's certificate has expired",
        0x800B_0109 => "the server's certificate chain ends in an untrusted root",
        0x8009_0322 | 0x800B_010F => "the server's certificate does not match the host name",
        _ => return win_err("Schannel handshake", hr),
    };
    io::Error::other(format!(
        "TLS handshake rejected: {reason} (0x{hr:08X}). RDP hosts normally present a \
         self-signed certificate — pass --insecure (-k) to accept it. The session is still \
         encrypted, but an unvalidated certificate cannot prove you reached the intended host."
    ))
}

fn zero_handle() -> SecHandle {
    SecHandle {
        dwLower: 0,
        dwUpper: 0,
    }
}

/// A TLS session over an inner byte stream, negotiated with Windows SChannel.
pub struct TlsStream<S> {
    inner: S,
    cred: SecHandle,
    ctx: SecHandle,
    /// TLS target name (UTF-16, NUL-terminated) and ISC request flags, retained
    /// so a mid-stream renegotiation (`SEC_I_RENEGOTIATE` — including TLS 1.3
    /// post-handshake messages such as NewSessionTicket) can re-drive
    /// `InitializeSecurityContext` without re-deriving them.
    target: Vec<u16>,
    req: ISC_REQ_FLAGS,
    header_len: usize,
    trailer_len: usize,
    max_message: usize,
    /// Decrypted plaintext awaiting the caller.
    plain: Vec<u8>,
    plain_pos: usize,
    /// Ciphertext received from the socket but not yet decrypted.
    cipher: Vec<u8>,
    /// The peer closed the TLS session (context expired / EOF).
    closed: bool,
}

impl<S: Read + Write> TlsStream<S> {
    /// Perform the SChannel client handshake over `inner` for `server_name`.
    ///
    /// When `accept_invalid` is false (the default) Schannel validates the
    /// server certificate chain against the OS trust store and the handshake
    /// fails on an untrusted cert. When true, validation is left to the caller
    /// (effectively accepted) — needed for the self-signed certs RDP hosts
    /// commonly present, where NLA's public-key binding provides MITM defence.
    pub fn connect(mut inner: S, server_name: &str, accept_invalid: bool) -> io::Result<Self> {
        unsafe {
            // 1) Outbound Schannel credentials. NO_DEFAULT_CREDS always (we send
            //    no client cert); MANUAL_CRED_VALIDATION only when accepting
            //    invalid certs, otherwise Schannel auto-validates the chain.
            let mut flags = SCH_CRED_NO_DEFAULT_CREDS.0;
            if accept_invalid {
                flags |= SCH_CRED_MANUAL_CRED_VALIDATION.0;
            }
            tracing::info!(
                accept_invalid,
                "Schannel credentials (cert validation policy)"
            );
            let mut sc = SCH_CREDENTIALS {
                dwVersion: SCH_CREDENTIALS_VERSION,
                dwFlags: flags,
                ..Default::default()
            };
            let mut cred = zero_handle();
            AcquireCredentialsHandleW(
                PCWSTR::null(),
                windows::Win32::Security::Authentication::Identity::UNISP_NAME_W,
                SECPKG_CRED_OUTBOUND,
                None,
                Some(&mut sc as *mut _ as *const c_void),
                None,
                None,
                &mut cred,
                None,
            )
            .map_err(|e| win_err("AcquireCredentialsHandle", e.code().0))?;

            let target: Vec<u16> = server_name
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let req = ISC_REQ_FLAGS(
                ISC_REQ_CONFIDENTIALITY.0
                    | ISC_REQ_INTEGRITY.0
                    | ISC_REQ_REPLAY_DETECT.0
                    | ISC_REQ_SEQUENCE_DETECT.0
                    | ISC_REQ_STREAM.0
                    | ISC_REQ_ALLOCATE_MEMORY.0
                    | ISC_REQ_EXTENDED_ERROR.0,
            );

            let mut ctx = zero_handle();
            let ctx_ptr: *mut SecHandle = &mut ctx;
            let cred_ptr: *const SecHandle = &cred;
            let mut ctx_ready = false;
            let mut in_buf: Vec<u8> = Vec::new();
            let leftover: Vec<u8>;

            loop {
                let mut in_secs = [
                    SecBuffer {
                        cbBuffer: in_buf.len() as u32,
                        BufferType: SECBUFFER_TOKEN,
                        pvBuffer: in_buf.as_mut_ptr() as *mut c_void,
                    },
                    SecBuffer {
                        cbBuffer: 0,
                        BufferType: SECBUFFER_EMPTY,
                        pvBuffer: std::ptr::null_mut(),
                    },
                ];
                let in_desc = SecBufferDesc {
                    ulVersion: SECBUFFER_VERSION,
                    cBuffers: 2,
                    pBuffers: in_secs.as_mut_ptr(),
                };
                let mut out_secs = [SecBuffer {
                    cbBuffer: 0,
                    BufferType: SECBUFFER_TOKEN,
                    pvBuffer: std::ptr::null_mut(),
                }];
                let mut out_desc = SecBufferDesc {
                    ulVersion: SECBUFFER_VERSION,
                    cBuffers: 1,
                    pBuffers: out_secs.as_mut_ptr(),
                };
                let mut attrs = 0u32;
                let phcontext = if ctx_ready {
                    Some(ctx_ptr as *const SecHandle)
                } else {
                    None
                };
                let pinput = if ctx_ready {
                    Some(&in_desc as *const _)
                } else {
                    None
                };

                let hr = InitializeSecurityContextW(
                    Some(cred_ptr),
                    phcontext,
                    Some(target.as_ptr()),
                    req,
                    0,
                    SECURITY_NATIVE_DREP,
                    pinput,
                    0,
                    Some(ctx_ptr),
                    Some(&mut out_desc),
                    &mut attrs,
                    None,
                );
                ctx_ready = true;

                // Send any handshake token Schannel produced.
                let out = out_secs[0];
                if out.cbBuffer > 0 && !out.pvBuffer.is_null() {
                    let token = std::slice::from_raw_parts(
                        out.pvBuffer as *const u8,
                        out.cbBuffer as usize,
                    );
                    let w = inner.write_all(token);
                    let _ = FreeContextBuffer(out.pvBuffer);
                    w?;
                    inner.flush()?;
                    tracing::debug!(token = out.cbBuffer, "Schannel handshake token sent");
                }

                if hr == SEC_E_OK {
                    leftover = extra_bytes(&in_secs[1], &in_buf);
                    tracing::debug!("Schannel handshake negotiated");
                    break;
                } else if hr == SEC_I_CONTINUE_NEEDED {
                    in_buf = extra_bytes(&in_secs[1], &in_buf);
                    read_more(&mut inner, &mut in_buf)?;
                } else if hr == SEC_E_INCOMPLETE_MESSAGE {
                    read_more(&mut inner, &mut in_buf)?;
                } else {
                    // Free the partial context (if any) and the credential
                    // before bailing, so a failed handshake leaks neither.
                    let _ = DeleteSecurityContext(&ctx);
                    let _ = FreeCredentialsHandle(&cred);
                    return Err(handshake_err(hr.0));
                }
            }

            // 2) Query the negotiated record sizes for framing.
            let mut sizes = SecPkgContext_StreamSizes {
                cbHeader: 0,
                cbTrailer: 0,
                cbMaximumMessage: 0,
                cBuffers: 0,
                cbBlockSize: 0,
            };
            QueryContextAttributesW(
                ctx_ptr,
                SECPKG_ATTR_STREAM_SIZES,
                &mut sizes as *mut _ as *mut c_void,
            )
            .map_err(|e| win_err("QueryContextAttributes(STREAM_SIZES)", e.code().0))?;

            Ok(TlsStream {
                inner,
                cred,
                ctx,
                target,
                req,
                header_len: sizes.cbHeader as usize,
                trailer_len: sizes.cbTrailer as usize,
                max_message: (sizes.cbMaximumMessage as usize).max(1),
                plain: Vec::new(),
                plain_pos: 0,
                cipher: leftover,
                closed: false,
            })
        }
    }

    /// The underlying transport, for setting socket options (e.g. a read
    /// timeout) without disturbing the TLS session state above it.
    pub fn get_ref(&self) -> &S {
        &self.inner
    }

    /// The DER-encoded server certificate (for CredSSP public-key binding).
    pub fn remote_cert_der(&self) -> Option<Vec<u8>> {
        unsafe {
            let mut cert: *mut CERT_CONTEXT = std::ptr::null_mut();
            QueryContextAttributesW(
                &self.ctx,
                SECPKG_ATTR_REMOTE_CERT_CONTEXT,
                &mut cert as *mut _ as *mut c_void,
            )
            .ok()?;
            if cert.is_null() {
                return None;
            }
            let c = &*cert;
            let der =
                std::slice::from_raw_parts(c.pbCertEncoded, c.cbCertEncoded as usize).to_vec();
            let _ = CertFreeCertificateContext(Some(cert));
            Some(der)
        }
    }

    /// Decrypt more ciphertext into `self.plain`; sets `closed` on clean EOF.
    fn fill(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        unsafe {
            loop {
                if self.cipher.is_empty() && !read_more(&mut self.inner, &mut self.cipher)? {
                    self.closed = true;
                    return Ok(());
                }
                let mut data = std::mem::take(&mut self.cipher);
                let mut secs = [
                    SecBuffer {
                        cbBuffer: data.len() as u32,
                        BufferType: SECBUFFER_DATA,
                        pvBuffer: data.as_mut_ptr() as *mut c_void,
                    },
                    empty_buffer(),
                    empty_buffer(),
                    empty_buffer(),
                ];
                let desc = SecBufferDesc {
                    ulVersion: SECBUFFER_VERSION,
                    cBuffers: 4,
                    pBuffers: secs.as_mut_ptr(),
                };
                let hr = DecryptMessage(&self.ctx, &desc, 0, None);

                if hr == SEC_E_OK || hr == SEC_I_RENEGOTIATE {
                    let mut extra_len = 0usize;
                    for s in &secs {
                        if s.cbBuffer == 0 || s.pvBuffer.is_null() {
                            continue;
                        }
                        if s.BufferType == SECBUFFER_DATA {
                            let slice = std::slice::from_raw_parts(
                                s.pvBuffer as *const u8,
                                s.cbBuffer as usize,
                            );
                            self.plain.extend_from_slice(slice);
                        } else if s.BufferType == SECBUFFER_EXTRA {
                            // Points into `data`'s own tail (the unconsumed
                            // suffix of the input) — remember the length and
                            // reuse the allocation below instead of cloning a
                            // fresh Vec per record.
                            extra_len = s.cbBuffer as usize;
                        }
                    }
                    if hr == SEC_I_RENEGOTIATE {
                        // The peer wants to renegotiate. On TLS 1.3 this is how
                        // Schannel surfaces post-handshake messages the server
                        // sends right after the handshake (NewSessionTicket,
                        // KeyUpdate) — extremely common with Azure gateways. The
                        // leftover `SECBUFFER_EXTRA` holds the handshake record;
                        // feed it back through InitializeSecurityContext to
                        // consume it, then treat whatever remains as ciphertext.
                        let extra = data[data.len() - extra_len.min(data.len())..].to_vec();
                        self.cipher = continue_handshake(
                            &mut self.inner,
                            &self.cred,
                            &mut self.ctx,
                            &self.target,
                            self.req,
                            extra,
                        )?;
                    } else {
                        // Shift the leftover to the front of `data` and keep the
                        // allocation as the cipher buffer (one bounded memmove
                        // instead of an alloc + copy + free per TLS record).
                        let n = extra_len.min(data.len());
                        let start = data.len() - n;
                        data.copy_within(start.., 0);
                        data.truncate(n);
                        self.cipher = data;
                    }
                    if !self.plain.is_empty() {
                        return Ok(());
                    }
                    // Decrypted to nothing yet (e.g. a lone post-handshake
                    // record): keep looping to fetch the actual payload.
                } else if hr == SEC_E_INCOMPLETE_MESSAGE {
                    // Need a longer record — restore and read more.
                    self.cipher = data;
                    if !read_more(&mut self.inner, &mut self.cipher)? {
                        self.closed = true;
                        return Ok(());
                    }
                } else if hr == SEC_I_CONTEXT_EXPIRED {
                    self.closed = true;
                    return Ok(());
                } else {
                    return Err(win_err("DecryptMessage", hr.0));
                }
            }
        }
    }
}

#[inline]
fn empty_buffer() -> SecBuffer {
    SecBuffer {
        cbBuffer: 0,
        BufferType: SECBUFFER_EMPTY,
        pvBuffer: std::ptr::null_mut(),
    }
}

/// Copy the trailing `SECBUFFER_EXTRA` bytes (unconsumed input) out of `buf`.
fn extra_bytes(extra: &SecBuffer, buf: &[u8]) -> Vec<u8> {
    if extra.BufferType == SECBUFFER_EXTRA && extra.cbBuffer > 0 {
        let n = (extra.cbBuffer as usize).min(buf.len());
        buf[buf.len() - n..].to_vec()
    } else {
        Vec::new()
    }
}

/// Drive `InitializeSecurityContext` to completion for an already-established
/// context, used when `DecryptMessage` reports `SEC_I_RENEGOTIATE`.
///
/// `initial` is the leftover `SECBUFFER_EXTRA` from `DecryptMessage` — the start
/// of the renegotiation/post-handshake record. Any handshake tokens Schannel
/// produces are written back to `inner`. Returns the ciphertext bytes that
/// remained past the renegotiation (to be decrypted as application data).
unsafe fn continue_handshake<S: Read + Write>(
    inner: &mut S,
    cred: &SecHandle,
    ctx: &mut SecHandle,
    target: &[u16],
    req: ISC_REQ_FLAGS,
    initial: Vec<u8>,
) -> io::Result<Vec<u8>> {
    let ctx_ptr: *mut SecHandle = ctx;
    let cred_ptr: *const SecHandle = cred;
    let mut in_buf = initial;

    loop {
        let mut in_secs = [
            SecBuffer {
                cbBuffer: in_buf.len() as u32,
                BufferType: SECBUFFER_TOKEN,
                pvBuffer: in_buf.as_mut_ptr() as *mut c_void,
            },
            empty_buffer(),
        ];
        let in_desc = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: 2,
            pBuffers: in_secs.as_mut_ptr(),
        };
        let mut out_secs = [SecBuffer {
            cbBuffer: 0,
            BufferType: SECBUFFER_TOKEN,
            pvBuffer: std::ptr::null_mut(),
        }];
        let mut out_desc = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: 1,
            pBuffers: out_secs.as_mut_ptr(),
        };
        let mut attrs = 0u32;

        let hr = InitializeSecurityContextW(
            Some(cred_ptr),
            Some(ctx_ptr as *const SecHandle),
            Some(target.as_ptr()),
            req,
            0,
            SECURITY_NATIVE_DREP,
            Some(&in_desc as *const _),
            0,
            Some(ctx_ptr),
            Some(&mut out_desc),
            &mut attrs,
            None,
        );

        let out = out_secs[0];
        if out.cbBuffer > 0 && !out.pvBuffer.is_null() {
            let token =
                std::slice::from_raw_parts(out.pvBuffer as *const u8, out.cbBuffer as usize);
            let w = write_all_riding_wouldblock(inner, token);
            let _ = FreeContextBuffer(out.pvBuffer);
            w?;
            inner.flush()?;
            tracing::debug!(token = out.cbBuffer, "Schannel renegotiation token sent");
        }

        if hr == SEC_E_OK {
            tracing::debug!("Schannel renegotiation complete");
            return Ok(extra_bytes(&in_secs[1], &in_buf));
        } else if hr == SEC_I_CONTINUE_NEEDED {
            in_buf = extra_bytes(&in_secs[1], &in_buf);
            if !read_more_riding_wouldblock(inner, &mut in_buf)? {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
        } else if hr == SEC_E_INCOMPLETE_MESSAGE {
            if !read_more_riding_wouldblock(inner, &mut in_buf)? {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
        } else {
            return Err(win_err("Schannel renegotiation", hr.0));
        }
    }
}

/// `write_all` that tolerates a non-blocking socket. The graphics worker puts
/// the socket into event-notification mode (`WSAEventSelect`, which makes it
/// non-blocking), so a full send buffer surfaces as `WouldBlock` instead of
/// blocking. A TLS record must go out whole — abandoning one mid-write
/// desynchronizes the stream — so ride the condition out with a short sleep
/// until the buffer drains (bounded; a peer that stops reading for this long
/// is a dead connection).
fn write_all_riding_wouldblock<S: Write>(inner: &mut S, mut buf: &[u8]) -> io::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !buf.is_empty() {
        match inner.write(buf) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut =>
            {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "send buffer stayed full while writing a TLS record",
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// [`read_more`], but riding out `WouldBlock`/timeouts from a non-blocking or
/// timeout-bounded socket. Renegotiation cannot be suspended midway (its state
/// lives on this call's stack), so block here — bounded — until the peer's
/// next record arrives.
fn read_more_riding_wouldblock<S: Read>(inner: &mut S, buf: &mut Vec<u8>) -> io::Result<bool> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match read_more(inner, buf) {
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut =>
            {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "peer stalled mid TLS renegotiation",
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            other => return other,
        }
    }
}

/// Read one chunk from `inner`, appending to `buf` (directly into its tail —
/// no intermediate stack buffer copy). Returns false on EOF.
fn read_more<S: Read>(inner: &mut S, buf: &mut Vec<u8>) -> io::Result<bool> {
    let old = buf.len();
    buf.resize(old + READ_CHUNK, 0);
    let n = match inner.read(&mut buf[old..]) {
        Ok(n) => n,
        // Under heavy inbound load a socket read that hits its SO_RCVTIMEO
        // deadline can surface as a transient *overlapped-I/O* status —
        // ERROR_IO_PENDING (997), WSA_IO_INCOMPLETE (996) or
        // ERROR_OPERATION_ABORTED (995) — instead of the usual WSAETIMEDOUT.
        // These mean "no data yet", not a broken connection, so normalize them
        // to WouldBlock; the timeout-aware layers above then resume on the next
        // poll instead of tearing the session down (which froze W365 under lots
        // of on-screen motion).
        Err(e) if matches!(e.raw_os_error(), Some(995 | 996 | 997)) => {
            buf.truncate(old);
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "socket read timed out"));
        }
        Err(e) => {
            buf.truncate(old);
            return Err(e);
        }
    };
    buf.truncate(old + n);
    if n == 0 {
        return Ok(false);
    }
    Ok(true)
}

impl<S: Read + Write> Read for TlsStream<S> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.plain_pos >= self.plain.len() {
            self.plain.clear();
            self.plain_pos = 0;
            self.fill()?;
        }
        let avail = self.plain.len() - self.plain_pos;
        let n = avail.min(out.len());
        out[..n].copy_from_slice(&self.plain[self.plain_pos..self.plain_pos + n]);
        self.plain_pos += n;
        Ok(n)
    }
}

impl<S: Read + Write> Write for TlsStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        unsafe {
            for chunk in buf.chunks(self.max_message) {
                let mut rec = vec![0u8; self.header_len + chunk.len() + self.trailer_len];
                rec[self.header_len..self.header_len + chunk.len()].copy_from_slice(chunk);
                let base = rec.as_mut_ptr();
                let mut secs = [
                    SecBuffer {
                        cbBuffer: self.header_len as u32,
                        BufferType: SECBUFFER_STREAM_HEADER,
                        pvBuffer: base as *mut c_void,
                    },
                    SecBuffer {
                        cbBuffer: chunk.len() as u32,
                        BufferType: SECBUFFER_DATA,
                        pvBuffer: base.add(self.header_len) as *mut c_void,
                    },
                    SecBuffer {
                        cbBuffer: self.trailer_len as u32,
                        BufferType: SECBUFFER_STREAM_TRAILER,
                        pvBuffer: base.add(self.header_len + chunk.len()) as *mut c_void,
                    },
                    empty_buffer(),
                ];
                let desc = SecBufferDesc {
                    ulVersion: SECBUFFER_VERSION,
                    cBuffers: 4,
                    pBuffers: secs.as_mut_ptr(),
                };
                let hr = EncryptMessage(&self.ctx, 0, &desc, 0);
                if hr != SEC_E_OK {
                    return Err(win_err("EncryptMessage", hr.0));
                }
                let total = secs[0].cbBuffer as usize
                    + secs[1].cbBuffer as usize
                    + secs[2].cbBuffer as usize;
                write_all_riding_wouldblock(&mut self.inner, &rec[..total])?;
            }
            self.inner.flush()?;
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<S> Drop for TlsStream<S> {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteSecurityContext(&self.ctx);
            let _ = FreeCredentialsHandle(&self.cred);
        }
    }
}
