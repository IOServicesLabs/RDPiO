# RDPiO — architecture notes

Short write-up of how the repo is laid out, what the crate targets are, and the
exact code paths that establish an RDP connection. Written as the reference for
adding a session-manager GUI on top of the existing client.

> **TL;DR** — RDPiO is **both** a library workspace and a binary project: a
> Cargo workspace of **10 library crates** (`rdp-asn1`, `rdp-crypto`,
> `rdp-pdu`, `rdp-core`, `rdp-nla`, `rdp-channels`, `rdp-graphics`, `rdp-gpu`,
> `rdp-webrtc`) plus **one binary — `rdpio` — which already exists** in
> `crates/rdp-client` (`[[bin]] name = "rdpio", path = "src/main.rs"`).
> The connection is established through `rdp_core::ClientConfig` +
> `rdp_core::Connector` (protocol state machine), `transport::connect`
> (TCP + X.224 negotiation), and `connect::establish_reconnect` /
> `session::activate` (TLS/NLA + activation) — details below.

## Repo layout and crate targets

The repo is a Cargo **workspace** (resolver `2`, edition `2021`, rust-version
`1.77`) of **10 crates** under `crates/`. There is no top-level `src/` — all
code lives in the per-crate `src/` directories.

| Crate | Target | Platform | Responsibility |
| --- | --- | --- | --- |
| `rdp-asn1` | library | any | Minimal BER/PER/DER codec (MCS/GCC, CredSSP) |
| `rdp-crypto` | library | any | RC4, RSA, MD4/MD5/SHA1/SHA256, HMAC, RDP key derivation |
| `rdp-pdu` | library | any | All wire PDU types + encode/decode |
| `rdp-core` | library | any | Sans-I/O connection state machine (`Connector`, `ClientConfig`, `Credentials`, `Phase`) |
| `rdp-nla` | library | any | CredSSP/NLA — SSPI on Windows, portable NTLMv2 elsewhere |
| `rdp-channels` | library | any | Static MCS channels + DRDYNVC; cliprdr, rdpsnd, rdpdr, disp, audio-input, camera, RDPEMT |
| `rdp-graphics` | library | any | EGFX command parsing, bitmap codecs, AVC420/444 reconstruction, DVC demuxer |
| `rdp-gpu` | library | Windows* | D3D11/D3D12 device + swapchain, H.264 decode/encode, present (`*` has a stub `Backend` so Linux compiles) |
| `rdp-client` | **binary `rdpio`** (`src/main.rs`) | any | Transport, event loop, device backends — **the only binary in the workspace** |
| `rdp-webrtc` | library | any | Teams "Optimized" WebRTC redirector + a webrtc-rs engine (Windows-gated dep) |

**A binary already exists**: `rdpio`, declared in
`crates/rdp-client/Cargo.toml` (`[[bin]] name = "rdpio", path = "src/main.rs"`).
Every other crate is a plain library. The workspace has no root package of its
own (no root `src/main.rs`, no root `[package]`), so builds target `-p rdp-client`.

## Connection API entry points

There are three layers, each with its own entry point:

### 1. Protocol core — `rdp-core` (sans-I/O, platform-neutral)

- `rdp_core::ClientConfig` (`crates/rdp-core/src/lib.rs:63`) — the central
  connection parameter struct: `hostname`, `port`, `width`/`height`,
  `credentials: Credentials`, `allow_legacy_fallback`, `allow_invalid_certificate`,
  `drive_paths`, `monitors`, `force_legacy`, `keyboard_layout`, `color_depth`,
  `enable_rfx`, `load_balance_info`, `redirected_session_id`,
  `reverse_connect: Option<ReverseConnectConfig>`, `shortpath`, `multitransport`.
  Implements `Default` (port 3389, 1920×1080).
- `rdp_core::Credentials` (`:139`) — `domain`, `username`, `password` (Debug is
  redacted).
- `rdp_core::Connector::new(ClientConfig)` (`:212`) — drives the X.224
  negotiation (`initial_request()`, `handle_negotiation_response(...)`),
  advertising TLS+CredSSP unless `force_legacy`; exposes
  `selected_protocol()` / `requested_protocols()` and `Phase`.
- `rdp_core::split_domain_user(domain, user)` (`:172`) — splits `DOMAIN\user` /
  `.\user` / UPN into the separate domain/user fields RDP needs.

### 2. Client connect stack — `rdp-client` (transport + activation)

- **`transport::connect(&ClientConfig) -> Result<(TcpStream, Connector, SecurityProtocol), NegotiateError>`**
  (`crates/rdp-client/src/transport.rs:122`) — opens TCP and completes X.224
  security negotiation, transparently falling back to legacy Standard RDP
  Security when the server refuses TLS. Cross-platform (plain `std::net`).
- **`connect::establish_reconnect(&mut ClientConfig, Option<&ReconnectCookie>) -> Result<Established, Box<dyn Error>>`**
  (`crates/rdp-client/src/connect.rs:129`, `#[cfg(windows)]`) — the full
  Windows connect path: TCP → (optional) TLS via SChannel → CredSSP/NLA
  (`rdp_nla::sspi::authenticate`) → `session::activate`, following up to 3
  server redirections (`apply_redirection`). Returns `Established { transport,
  session, control, input_tcp, protocol }` where `Transport` is
  `Tcp | Tls(Box<TlsStream<TcpStream>>) | WebSocket | WebSocketTls`. A
  non-Windows TLS/NLA variant of the same sequence lives in `main.rs`'s
  `run_connect` / `headless_run` using rustls + portable CredSSP.
- **`session::activate<S: Read + Write>(stream, &ClientConfig, SecurityProtocol, Option<&ReconnectCookie>) -> Result<ActiveSession, ActivateError>`**
  (`crates/rdp-client/src/session.rs:486`) — runs the activation sequence to
  the Active state (basic settings/GCC, channels, licensing, Demand-Active,
  finalization).
- **`session::run_session<S, F: FrameSink>(stream, &mut ActiveSession, &mut F)`**
  (`session.rs:1643`) / `run_graphics_session` (`:1947`) / `pump_once`
  (`:1471`) — the post-activation session loops. `FrameSink` (`:1357`) is the
  trait a UI implements to receive `blit`/`present`/`cursor` events.

### 3. Binary entry — `crates/rdp-client/src/main.rs`

- `main()` parses CLI args (`Args::from_env()`), initializes tracing, then
  dispatches:
  - `--host <host>` (or `--w365` / `--feed`) → **Windows:** `win::run_connected(&args)`
    (window + D3D11 paint, session worker thread); **other platforms:**
    `run_connect(&args)` (headless: negotiate, activate, log decoded rectangles).
  - No args on Windows → `win::run()` — the idle "M0" 1280×720 window with a
    D3D11 swapchain (the closest thing to a GUI today).
  - No args elsewhere → prints usage, exits 2.
- **`config_from_args(&Args) -> ClientConfig`** (`main.rs:405`) is the bridge
  from CLI to the connect stack: splits logon names via `rdp_core::split_domain_user`,
  applies resolution clamping, quality presets, etc. It is `pub(crate)`-ish
  (module-private) — a GUI shell would need it exposed or a parallel builder.

## CLI argument handling

- **No external arg-parsing crate.** `Args` (`main.rs:599`, ~50 fields) is
  filled by a hand-rolled `while let Some(flag) = it.next()` loop in
  `Args::from_env()`. Unknown flags are ignored with a warning; there is a
  `print_help()` for `--help`/`--usage`/`-?`/`/?` and `--version` (note `-h`
  means `--host`, kept for compatibility).
- Connection-relevant flags: `--host`, `--port` (default 3389), `--user`,
  `--domain`, `--password`, `--insecure`, `--legacy`, `--keyboard-layout`,
  `--bpp`, `--width/--height/--size`; display/perf/redirection flags are
  Windows-only features; W365/AVD flags drive `--feed`, `--rdp-file`, `--w365`.

## Existing config/session storage

There is **no saved-session profile store yet** (no `sessions.json`, no
`SessionStore`). What exists today, all under **`%LOCALAPPDATA%\rdpio\`**
(Windows; `LOCALAPPDATA` unset → temp dir):

- **Reconnect cookies** — `rdpio_reconnect_<host-hash>.cookie`
  (`reconnect_cookie_path` / `save_reconnect_cookie` / `load_reconnect_cookie`,
  `main.rs:364`–`398`).
- **OAuth token cache** — `token_cache.rs` (`%LOCALAPPDATA%\rdpio\`, DPAPI).
- **W365 password cache** — `password_cache.rs`
  (`%LOCALAPPDATA%\rdpio\w365_password.bin`, DPAPI-encrypted).

A GUI session store should follow the same `%LOCALAPPDATA%\rdpio` convention on
Windows (the planned `%APPDATA%\RDPiO`/`~/.config/rdpio` split is noted in the
GUI plan; the existing code consistently uses `LOCALAPPDATA\rdpio`).

## State of the branch

Working branch is **`swarm/add-a-nice-ui`**, based on commit
`d973d0b` ("Merge branch 'main' of https://github.com/IOServicesLabs/RDPiO") —
i.e. current upstream `main` with no diverging commits. `Cargo.lock` is
committed (v3). Build baseline: `x86-64-v3` (AVX2) via
`.cargo/config.toml` `rustflags` per target; `rust-toolchain.toml` pins stable
with the minimal profile.

## Implications for a GUI layer

- Add the GUI as a second binary target in `crates/rdp-client/Cargo.toml`
  (`src/bin/gui.rs`) or a new crate; the workspace root has no package.
- Reuse the connect stack via `rdp_core::ClientConfig` + `transport::connect`
  + `session::activate`; the Windows interactive path (`win::run_connected`)
  is deeply tied to `Args`, so a GUI should build a `ClientConfig` directly.
- The `FrameSink` trait is the seam for painting decoded frames; connection
  progress/errors come back as `Result` from `establish_reconnect` /
  `session::activate`, which are blocking and should run on a worker thread.
- `rdp-client` currently has no `lib.rs` — code is private to the `rdpio`
  binary, so shared GUI+CLI helpers (e.g. `config_from_args`) would need to be
  extracted into a library module or duplicated.
