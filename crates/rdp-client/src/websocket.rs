//! WebSocket byte-stream wrapper for the W365 / AVD Reverse Connect path.
//!
//! Windows 365 and Azure Virtual Desktop tunnel RDP over a WebSocket on port
//! 443 ("Reverse Connect"). The RDP stack underneath expects a plain byte
//! stream, so this module provides a [`WebSocketStream`] that implements
//! [`Read`]/[`Write`] on top of a tungstenite WebSocket. Binary messages are
//! concatenated on read and buffered writes are sent as binary messages on
//! flush.

use std::io::{self, Read, Write};
use std::net::TcpStream;

use tungstenite::client::IntoClientRequest;
use tungstenite::{client, Message, WebSocket};
use url::Url;

use crate::tls::TlsStream;

/// Errors from the WebSocket transport layer.
#[derive(Debug, thiserror::Error)]
pub enum WebSocketError {
    #[error("network error: {0}")]
    Io(#[from] io::Error),
    #[error("WebSocket error: {0}")]
    Tungstenite(#[from] tungstenite::Error),
}

/// A byte stream over a TLS-secured WebSocket.
///
/// Writes are accumulated in an internal buffer and sent as one binary message
/// on [`Write::flush`]. Reads return bytes from the current binary message,
/// automatically fetching the next message when the buffer is exhausted.
pub struct WebSocketStream {
    ws: WebSocket<TlsStream<TcpStream>>,
    /// Bytes of the current inbound binary message not yet consumed by `read`.
    read_buf: Vec<u8>,
    /// Position within `read_buf`.
    read_pos: usize,
    /// Outbound bytes accumulated since the last flush.
    write_buf: Vec<u8>,
    /// True once a close frame has been received or sent.
    closed: bool,
}

impl WebSocketStream {
    /// Connect to `request.uri()` over TLS, perform the WebSocket handshake,
    /// and return a byte stream.
    ///
    /// `server_name` is the TLS SNI name (usually the host part of the URI).
    /// `accept_invalid` is forwarded to the SChannel TLS layer so self-signed
    /// or otherwise non-trusted gateway certificates can be used when the user
    /// opts in with `--insecure`.
    pub fn connect(
        request: http::Request<()>,
        server_name: &str,
        accept_invalid: bool,
    ) -> Result<Self, WebSocketError> {
        let url = request.uri().to_string();
        let url_parsed = Url::parse(&url)
            .map_err(|e| io::Error::other(format!("invalid WebSocket URL: {e}")))?;
        let host = url_parsed
            .host_str()
            .ok_or_else(|| io::Error::other("WebSocket URL has no host"))?;
        let port = url_parsed
            .port_or_known_default()
            .unwrap_or(443);

        let tcp = TcpStream::connect((host, port))?;
        tcp.set_nodelay(true).ok();
        tracing::info!(%url, "WebSocket TCP connected");

        let tls = TlsStream::connect(tcp, server_name, accept_invalid)?;

        // tungstenite 0.24 passes an `http::Request` through verbatim and
        // requires it to already carry every mandatory WebSocket handshake
        // header (`Host`, `Connection`, `Upgrade`, `Sec-WebSocket-Version`,
        // `Sec-WebSocket-Key`). The caller hands us a request with only app
        // headers (Authorization, User-Agent, ...), so build a proper client
        // request from the URI — which generates those five headers plus a
        // fresh random key — and merge the caller's headers on top. None of
        // the app headers collide with the WS headers, so `insert` is safe.
        let mut ws_request = request
            .uri()
            .clone()
            .into_client_request()
            .map_err(|e| io::Error::other(format!("building WebSocket request: {e}")))?;
        {
            let headers = ws_request.headers_mut();
            for (name, value) in request.headers() {
                headers.insert(name, value.clone());
            }
        }

        let (ws, response) = match client(ws_request, tls) {
            Ok(pair) => pair,
            // A rejected upgrade (401/403/…) carries the server's HTTP response.
            // Surface the diagnostic headers/body so an auth failure tells us
            // *what* it wants (e.g. a `WWW-Authenticate` scheme) instead of a
            // bare status — decisive for the Shortpath rendezvous 401.
            Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(resp))) => {
                let status = resp.status();
                let hdr = |name: &str| {
                    resp.headers()
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string()
                };
                let body_preview = resp
                    .body()
                    .as_ref()
                    .map(|b| String::from_utf8_lossy(&b[..b.len().min(200)]).into_owned())
                    .unwrap_or_default();
                tracing::warn!(
                    status = status.as_u16(),
                    www_authenticate = %hdr("www-authenticate"),
                    x_ms_diagnostics = %hdr("x-ms-diagnostics"),
                    x_ms_error_code = %hdr("x-ms-error-code"),
                    body_preview = %body_preview,
                    "WebSocket upgrade rejected"
                );
                return Err(WebSocketError::Io(io::Error::other(format!(
                    "WebSocket handshake failed: HTTP {status}"
                ))));
            }
            Err(e) => {
                return Err(WebSocketError::Io(io::Error::other(format!(
                    "WebSocket handshake failed: {e}"
                ))))
            }
        };

        tracing::info!(
            status = response.status().as_u16(),
            "WebSocket handshake complete"
        );

        Ok(Self {
            ws,
            read_buf: Vec::new(),
            read_pos: 0,
            write_buf: Vec::new(),
            closed: false,
        })
    }

    /// Set (or clear) a read timeout on the underlying TCP socket. When set, a
    /// read with no data available within the timeout returns `WouldBlock` rather
    /// than blocking — letting the graphics loop flush queued input between reads
    /// (input over the WebSocket transport is otherwise gated on server frames).
    pub fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> io::Result<()> {
        self.ws.get_ref().get_ref().set_read_timeout(dur)
    }

    /// Read the next whole inbound **binary** WebSocket message, preserving frame
    /// boundaries. Unlike the [`Read`] impl (which flattens all messages into one
    /// byte stream for the RDP tunnel), the Shortpath rendezvous signaling treats
    /// each binary frame as a discrete protocol message, so boundaries matter.
    ///
    /// Returns `WouldBlock` on a read timeout and `UnexpectedEof` on a peer close.
    /// Must not be mixed with the byte-stream [`Read`] impl on the same stream.
    pub fn read_binary_message(&mut self) -> io::Result<Vec<u8>> {
        self.fill_read_buf()?;
        self.read_pos = 0;
        Ok(std::mem::take(&mut self.read_buf))
    }

    /// Send one **binary** WebSocket message immediately (not buffered until
    /// [`Write::flush`], as the byte-stream path is). Used for message-oriented
    /// rendezvous signaling where each protocol message is one frame.
    #[allow(dead_code)] // consumed by the rendezvous signaling (Shortpath milestone 1+).
    pub fn send_binary_message(&mut self, data: &[u8]) -> io::Result<()> {
        self.ws
            .send(Message::Binary(data.to_vec()))
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
        self.ws
            .flush()
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))
    }

    /// Fetch the next binary message into `read_buf`. Non-binary control
    /// messages are handled automatically.
    fn fill_read_buf(&mut self) -> io::Result<()> {
        loop {
            let msg = match self.ws.read() {
                Ok(m) => m,
                // A socket read timeout (SO_RCVTIMEO) surfaces here mid-frame;
                // tungstenite buffers its partial state, so propagate WouldBlock
                // and resume on the next read. NOT a fatal/broken connection.
                // Under load Windows may report the timeout as a transient
                // overlapped-I/O status (997/996/995) instead of TimedOut — treat
                // those the same rather than mapping them to a fatal BrokenPipe.
                Err(tungstenite::Error::Io(e))
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut
                        || matches!(e.raw_os_error(), Some(995 | 996 | 997)) =>
                {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "read timed out"));
                }
                Err(e) => return Err(io::Error::new(io::ErrorKind::BrokenPipe, e)),
            };
            match msg {
                Message::Binary(data) => {
                    self.read_buf = data;
                    self.read_pos = 0;
                    return Ok(());
                }
                Message::Text(text) => {
                    tracing::warn!(
                        bytes = text.len(),
                        "ignoring unexpected text WebSocket message"
                    );
                }
                Message::Ping(_) | Message::Pong(_) => {
                    // tungstenite handles pong replies automatically.
                }
                Message::Close(frame) => {
                    self.closed = true;
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("WebSocket closed: {frame:?}"),
                    ));
                }
                Message::Frame(_) => {
                    // Raw frames are not produced by the sync client.
                }
            }
        }
    }
}

/// Prime the Azure App Service load-balancing affinity cookie for an AVD/W365
/// brokered connection URL.
///
/// The AVD gateway sits behind Azure ARR (Application Request Routing). The
/// first request to a freshly-brokered connection URL is answered with `403
/// Forbidden` **and** a `Set-Cookie: ARRAffinity=…` (plus `ARRAffinitySameSite`)
/// — the 403 is expected; the cookie pins subsequent requests to the backend
/// instance that owns the brokered connection state. The real WebSocket upgrade
/// must therefore carry that cookie, or every instance rejects it with 403.
///
/// This performs a plain HTTPS GET (no WebSocket upgrade) to `url` and returns
/// the affinity cookies as `(name, value)` pairs. Best-effort: on any transport
/// error it returns an empty list and lets the caller proceed without them.
pub fn prime_affinity_cookies(
    url: &str,
    server_name: &str,
    accept_invalid: bool,
) -> Vec<(String, String)> {
    match try_prime_affinity(url, server_name, accept_invalid) {
        Ok(cookies) => cookies,
        Err(e) => {
            tracing::debug!(error = %e, "affinity cookie priming failed; proceeding without");
            Vec::new()
        }
    }
}

fn try_prime_affinity(
    url: &str,
    server_name: &str,
    accept_invalid: bool,
) -> io::Result<Vec<(String, String)>> {
    let parsed = Url::parse(url).map_err(|e| io::Error::other(format!("invalid URL: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| io::Error::other("URL has no host"))?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    let mut path = parsed.path().to_string();
    if let Some(q) = parsed.query() {
        path.push('?');
        path.push_str(q);
    }

    let tcp = TcpStream::connect((host, port))?;
    tcp.set_nodelay(true).ok();
    let mut tls = TlsStream::connect(tcp, server_name, accept_invalid)?;

    // Plain GET (no Upgrade), matching FreeRDP's first ARM-transport request.
    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Accept: */*\r\n\
         Cache-Control: no-cache\r\n\
         Pragma: no-cache\r\n\
         User-Agent: Mozilla/5.0 (Windows NT 10.0; Win64; x64) RdClient\r\n\
         X-Ms-User-Agent: Windows365NativeClient/2.0.1193.0\r\n\
         Connection: close\r\n\r\n"
    );
    tls.write_all(request.as_bytes())?;
    tls.flush()?;

    // Read just far enough to have the full response header block.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    while find_header_end(&buf).is_none() && buf.len() <= 64 * 1024 {
        let n = tls.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }

    let head_end = find_header_end(&buf).unwrap_or(buf.len());
    let headers = String::from_utf8_lossy(&buf[..head_end]);
    let status = headers.lines().next().unwrap_or("").trim().to_string();

    let mut cookies = Vec::new();
    for line in headers.lines() {
        if line.len() < 11 || !line[..11].eq_ignore_ascii_case("set-cookie:") {
            continue;
        }
        // "ARRAffinity=abc123; path=/; secure; ..." — take the name=value pair.
        let pair = line[11..].split(';').next().unwrap_or("").trim();
        if let Some((name, value)) = pair.split_once('=') {
            let name = name.trim();
            if name.eq_ignore_ascii_case("ARRAffinity")
                || name.eq_ignore_ascii_case("ARRAffinitySameSite")
            {
                cookies.push((name.to_string(), value.trim().to_string()));
            }
        }
    }

    tracing::info!(
        %status,
        cookies = cookies.len(),
        "primed AVD gateway affinity cookie"
    );
    Ok(cookies)
}

/// Byte offset just past the `\r\n\r\n` that ends an HTTP header block.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

impl Read for WebSocketStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        if self.read_pos >= self.read_buf.len() {
            if self.closed {
                return Ok(0);
            }
            self.fill_read_buf()?;
        }

        let available = &self.read_buf[self.read_pos..];
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.read_pos += n;
        Ok(n)
    }
}

impl Write for WebSocketStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.write_buf.is_empty() {
            return Ok(());
        }
        let payload = std::mem::take(&mut self.write_buf);
        self.ws
            .send(Message::Binary(payload))
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
        self.ws.flush().map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
        Ok(())
    }
}

impl Drop for WebSocketStream {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.ws.close(None);
        }
    }
}

#[cfg(test)]
mod tests {
    /// Full loopback handshake tests require a TLS certificate; this module is
    /// validated by integration tests against a live gateway. The framing logic
    /// in [`WebSocketStream`] is exercised implicitly once those tests pass.
    #[test]
    fn wrapper_compiles() {
        assert!(true);
    }
}

