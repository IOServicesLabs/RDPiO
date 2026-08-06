# NOTES.md — Repository inspection & build-out plan

Date: 2026-08-06
Inspected by: iteration-1 agent (steps: `step-1-inspect`, `step-2-init-module`)

## 1. Current state

The repository is **RDPiO**, a from-scratch, GPU-accelerated RDP client. Today
it is an entirely **Rust** project: a Cargo workspace (`resolver = "2"`,
edition 2021, rust-version 1.77) of **10 library crates** under `crates/`
(`rdp-asn1`, `rdp-crypto`, `rdp-pdu`, `rdp-core`, `rdp-nla`, `rdp-channels`,
`rdp-graphics`, `rdp-gpu`, `rdp-client`, `rdp-webrtc`) plus one binary
(`rdpio`, in `crates/rdp-client`).

- Git branch checked out: `swarm/add-a-nice-ui` (head `08164fd`, "swarm: green
  iteration 1"); `main` also present locally and on `origin`.
- Go toolchain: **not installed** on the host at inspection time (verified
  with `find`); a Go 1.22.12 SDK (`/root/go-sdk/go/bin`) was used from
  iteration 1 onward to author and verify the Go build-out. No Go files,
  no `go.mod`, no `go.sum` existed in the tree before this work.
- TODOs: `grep -rn -E 'TODO|FIXME|XXX'` over `README.md`, `PORTING.md`,
  `docs/`, and `crates/**/*.rs` returned **no matches**. The remaining work is
  tracked as *stages* in `PORTING.md` (Stage 4 — W365 on Linux, Stage 5 —
  interactive client), not as code TODOs.

## 2. Complete file listing

All files tracked in the working tree (155 files). `./.git/` internals and
`./target/` build artifacts are excluded (VCS metadata / generated output).

```
.
├── .cargo/config.toml
├── .gitattributes
├── .github/workflows/release.yml
├── .gitignore
├── Cargo.lock
├── Cargo.toml                     # workspace root (10 crates)
├── LICENSE
├── PORTING.md                     # Linux port stage tracker (Stages 4–5 open)
├── README.md                      # project README (CLI docs, quick start)
├── docs/
│   ├── architecture.md
│   └── host-tuning.md
├── run-rdpio.ps1
├── rust-toolchain.toml
├── tune-rdpio-host.ps1
├── tune-rdpio-network.ps1
└── crates/
    ├── rdp-asn1/
    │   ├── Cargo.toml
    │   └── src/{der.rs, lib.rs}
    ├── rdp-channels/
    │   ├── Cargo.toml
    │   └── src/{audio_input.rs, camera.rs, cliprdr.rs, disp.rs, drdynvc.rs,
    │           emt.rs, lib.rs, rdpei.rs, rdpdr.rs, rdpsnd.rs, serial.rs, svc.rs}
    ├── rdp-client/
    │   ├── Cargo.toml
    │   ├── build.rs
    │   └── src/{allocator.rs, arm_broker.rs, audio.rs, clipboard.rs,
    │           cloud_pc_picker.rs, congestion.rs, connect.rs, connbar.rs,
    │           crash.rs, feed.rs, gateway.rs, iocp.rs, main.rs, metrics.rs,
    │           mf_camera.rs, mic.rs, nano_ffi.rs, net_listener.rs, net_wait.rs,
    │           password_cache.rs, printer.rs, prompt.rs, rendezvous.rs,
    │           reverse_connect.rs, rdstls_auth.rs, rdstls_v3.rs, rng.rs,
    │           session.rs, stun.rs, tls.rs, tls_rustls.rs, token_cache.rs,
    │           transport.rs, udp.rs, w365.rs, webrtc_addin.rs, webrtc_devices.rs,
    │           webrtc_native.rs, webrtc_turn.rs, webview_auth.rs, websocket.rs,
    │           window.rs}
    ├── rdp-core/
    │   ├── Cargo.toml
    │   └── src/lib.rs
    ├── rdp-crypto/
    │   ├── Cargo.toml
    │   └── src/{bignum.rs, keys.rs, lib.rs, md4.rs, md5.rs, rc4.rs, rsa.rs,
    │           sha1.rs, sha256.rs}
    ├── rdp-gpu/
    │   ├── Cargo.toml
    │   └── src/{backend.rs, d3d11.rs, d3d12.rs, h264.rs, lib.rs, stub.rs}
    ├── rdp-graphics/
    │   ├── Cargo.toml
    │   ├── examples/{cc_replay.rs, prog_replay.rs}
    │   └── src/{avc.rs, bitmap.rs, channel.rs, clearcodec.rs, egfx.rs, lib.rs,
    │           pointer.rs, pool.rs, progressive.rs, redirect.rs, rfx.rs,
    │           surface.rs, yuv.rs, zgfx.rs}
    ├── rdp-nla/
    │   ├── Cargo.toml
    │   └── src/{credssp.rs, lib.rs, sspi.rs, tsrequest.rs, x509.rs}
    ├── rdp-pdu/
    │   ├── Cargo.toml
    │   └── src/{autodetect.rs, capabilities.rs, errinfo.rs, fastpath.rs,
    │           finalization.rs, gcc.rs, gfx.rs, input.rs, license.rs, lib.rs,
    │           logon.rs, mcs.rs, multitransport.rs, rdpudp.rs, rdstls.rs,
    │           redirection.rs, security.rs, x224.rs}
    └── rdp-webrtc/
        ├── Cargo.toml
        ├── src/{bridge.rs, capture.rs, devices.rs, dispatch.rs, engine.rs,
        │       framing.rs, ice.rs, lib.rs, objects.rs, presentation.rs, rpc.rs,
        │       session.rs}
        └── tests/
            ├── dispatch_replay.rs
            ├── engine_replay.rs
            ├── fixtures/{audio_only_answer.sdp, teams_call.wrtc}
            ├── presentation_replay.rs
            └── replay.rs
```

File counts by extension (excluding `.git/` and `target/`): 128 `.rs`,
13 `.toml` (12 `Cargo.toml` + `rust-toolchain.toml`), 4 `.md`, 3 `.ps1`,
3 extension-less (`LICENSE`, `.gitignore`, `.gitattributes`), 1 `.lock`
(`Cargo.lock`), 1 `.yml`, 1 `.sdp`, 1 `.wrtc`. Total: **155 files**.

## 3. Module path decision

No Go module definition exists anywhere in the tree (no `go.mod` / `go.sum`;
nothing to migrate). Per the plan, the Go module will be initialized with:

- **Module path:** `github.com/IOServicesLabs/RDPiO`
- **Go version:** 1.22 (toolchain `go1.22.12` used to author and verify)
- **Dependencies:** `github.com/sirupsen/logrus` (logging),
  `github.com/stretchr/testify` (test assertions)

The Rust workspace (`Cargo.toml`) and the new Go module (`go.mod`) coexist at
the repo root; `cargo` ignores `.go` files and the Go toolchain ignores
`Cargo.toml`/`crates/`, so there is no interference.

## 4. Component status (Go build-out plan)

All components of the Go build-out are now **complete** (steps 1–7):

| # | Component | Status | Notes |
| --- | --- | --- | --- |
| 1 | `go.mod` / `go.sum` | **DONE** (step-2) | module `github.com/IOServicesLabs/RDPiO`, `go 1.22.12`; logrus v1.9.4, testify v1.11.1 (+ `golang.org/x/sys` v0.13.0 transitive); `go build ./...` exits 0 |
| 2 | `config` package (`Config` struct + `Load()`, env vars `RDP_PROXY_PORT`/`RDP_TARGET_HOST`/`RDP_TARGET_PORT`, `config_test.go`) | **DONE** (step-3) | defaults 3389 / 127.0.0.1 / 3389; invalid/empty env falls back to defaults; 6 unit tests |
| 3 | `proxy` package (`Proxy`, `NewProxy`, `HandleConnection`, `Run`, `Wait`, `proxy_test.go` with in-process TCP echo server) | **DONE** (step-4) | bidirectional piping with clean half-close and connection shutdown; `Wait()` (added step-5) tracks in-flight connections via `sync.WaitGroup`; 6 unit tests |
| 4 | `main.go` + `health` package (TCP listener on `:ProxyPort`, proxy run loop, HTTP `/healthz` → `{"status":"ok"}` on `HEALTH_PORT`/8080, SIGINT/SIGTERM graceful shutdown, logrus logging) | **DONE** (step-5) | `startServers()`/`service.Shutdown()` factored out for testability; live smoke test passed: both servers start, `curl /healthz` returns `{"status":"ok"}`, SIGINT exits 0 cleanly; 4 health unit tests |
| 5 | Integration tests (`main_test.go`, end-to-end health + forwarding) | **DONE** (step-6) | 6 integration tests in `package main`: healthz over real HTTP, end-to-end TCP forwarding through a temporary echo target, shutdown closes both listeners, shutdown waits for in-flight connections, `HEALTH_PORT` parsing, health-port conflict; race-detector clean |
| 6 | Final quality pass (`go mod tidy`, `go build`, `go vet`, `gofmt -l`, `go test ./...`) | **DONE** (step-7) | all green, see §6 |
| 7 | `NOTES.md` | this file | final state recorded here |

## 5. Final Go file layout (at repo root)

```
go.mod                          # module github.com/IOServicesLabs/RDPiO (go 1.22)
go.sum
NOTES.md                        # this file
main.go                         # wiring, health server, graceful shutdown
main_test.go                    # integration tests (package main)
config/
    config.go                   # Config{ProxyPort, TargetHost, TargetPort}; Load()
    config_test.go
proxy/
    proxy.go                    # Proxy, NewProxy, HandleConnection, Run, Wait
    proxy_test.go
health/
    health.go                   # Handler(): GET /healthz -> {"status":"ok"}
    health_test.go
```

The Rust workspace (`Cargo.toml` + `crates/`) and the Go module coexist at the
repo root; neither toolchain interferes with the other.

## 6. Final verification (step-7)

Run with Go `1.22.12` (`/root/go-sdk/go/bin`); cargo gates run with the
workspace toolchain from `rust-toolchain.toml`.

- `go mod tidy` — no changes required (dependency set already minimal:
  logrus + testify, both direct).
- `go build ./...` — OK, no errors.
- `go vet ./...` — OK, no findings.
- `gofmt -l .` — no files listed (all Go sources gofmt-clean).
- `go test ./...` — all pass: `main` (6 tests + 5 subtests), `config` (6),
  `health` (4), `proxy` (6).
- `go test -race -count=1 ./...` — all pass (no data races, including the
  proxy `WaitGroup` shutdown path).
- `cargo check --workspace --all-targets` — exit 0 (pre-existing dead-code
  warnings only, unrelated to the Go work).
- `cargo test` — all crates pass (440+ tests, 0 failures).
- Live smoke test (step-5 acceptance): service starts both listeners;
  `curl localhost:8080/healthz` → `HTTP/1.1 200 OK`, body `{"status":"ok"}`;
  SIGINT terminates the process with exit code 0 after graceful shutdown
  ("shutdown signal received" → "listener closed" → "shutdown complete").

**Repository is ready for commit.**
