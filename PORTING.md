# RDPiO Linux port

RDPiO is a Windows-native RDP/W365 client. This tracks the staged port to Linux.
The **portable protocol core** (`rdp-pdu`, `rdp-core`, `rdp-crypto`, `rdp-channels`,
`rdp-graphics`, `rdp-asn1`) is pure sans-I/O Rust and already cross-compiles.
Everything platform-specific lives behind `#[cfg(windows)]` / `#[cfg(unix)]` seams.

## Cross-compiling from Windows (dev/CI)

`ring` (via rustls) needs a Linux C toolchain. Use zig:

```
cargo install cargo-zigbuild
# zig 0.13 on PATH (e.g. C:\Users\<you>\zig)
# NOTE: the zig/lld linker mangles paths with spaces, so build into a space-free
# target dir when the checkout path contains spaces:
CARGO_TARGET_DIR=/c/rdpiotgt cargo zigbuild --target x86_64-unknown-linux-gnu -p rdp-client
```

On a real Linux host, a plain `cargo build` works (gcc is standard, no space issue).

## Stages

- [x] **Stage 1 — compile headless on Linux.** DONE. Gated Windows-only deps
  (`webview2-com`) and the W365/UI/platform modules (`websocket`, `reverse_connect`,
  `rdstls_auth`, `rdstls_v3`, `webview_auth`, `cloud_pc_picker`, `net_listener`)
  behind `cfg(windows)`; added a `Backend` stub to `rdp-gpu`; made the `AtomicU32`
  import unconditional. Produces a real `x86_64-unknown-linux-gnu` ELF binary that
  runs the protocol stack headless (TCP; the `#[cfg(not(windows))] run_connect`
  path). Windows build unaffected.
- [x] **Stage 2 — rustls TLS.** DONE. `tls_rustls.rs` provides a non-Windows
  `tls::TlsStream<S>` (rustls 0.23 + webpki-roots; `--insecure` → accept-any
  verifier) matching the SChannel API (`connect`, Read/Write, `get_ref`,
  `remote_cert_der`). `run_connect` now wraps the negotiated socket in TLS when the
  server selects SSL (Enhanced RDP Security) and runs activation headlessly; HYBRID
  (NLA) still warns (Stage 3). Builds for Linux and Windows.
- [x] **Stage 3 — NLA / CredSSP on Linux.** DONE. Lets the headless build connect
  to standard Windows RDP servers (which select NLA/HYBRID). `rdp-nla` already had
  portable `TSRequest` framing (`tsrequest.rs`) and public-key extraction
  (`x509.rs`); only the Win32 SSPI engine (`sspi.rs`) was Windows-only. Added a
  portable `credssp.rs` (`#[cfg(not(windows))]`): NTLMv2 (NEGOTIATE/CHALLENGE/
  AUTHENTICATE, NTLMv2 response, Extended Session Security sign+seal, key exchange,
  MIC) + the CredSSP public-key channel binding (SHA-256 nonce, v5+) + sealed
  `TSCredentials`. Added `md4` to `rdp-crypto` (the NT-hash primitive; everything
  else — MD5/HMAC-MD5/RC4/SHA-256 — was already in-tree). `run_connect` now wraps
  the socket in TLS for **both** SSL and HYBRID, runs `credssp::authenticate` over
  the tunnel for HYBRID (mirroring the Windows `connect.rs` path,
  `spn = TERMSRV/<host>`), then activates. Validated offline against the MS-NLMP
  §4.2.4 NTLMv2 test vectors + a seal/unseal round-trip (no Windows APIs, no new
  external crates). NLA is a one-time connection handshake, off every per-frame
  path, so this does not affect streaming performance.
- [ ] **Stage 4 — W365 on Linux.** Port the RDSTLS v3 credential (CNG AES/RSA/cert
  → RustCrypto: `aes`, `cbc`, `rsa`, `x509-cert`), the token/password caches
  (DPAPI → an encrypted file or libsecret), and auth (WebView2 → system browser +
  loopback redirect).
- [ ] **Stage 5 — interactive client.** Rendering (D3D11 → `wgpu`), window/input
  (Win32 → `winit`), H.264 decode (Media Foundation → `ffmpeg`/VA-API), audio
  (WASAPI → `cpal`/PipeWire).

## Notes

- `rdp-gpu` is Windows-only with a non-Windows stub already, so it links (no-op) on
  Linux; Stage 4 replaces the stub with a real backend.
- `main.rs` already has a `#[cfg(not(windows))]` `run_connect` headless path.
