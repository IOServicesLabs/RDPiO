# RDPiO

A from-scratch, GPU-accelerated Remote Desktop (RDP) client written in Rust —
built for **business work and gaming at the same time**.

RDPiO implements the RDP protocol stack from the wire up — no third-party RDP
library. On Windows it decodes H.264 on the GPU (Direct3D 11), spans multiple
monitors, redirects your devices (drives, clipboard, audio, mic, camera,
printer), and can ride a low-latency UDP side-band transport.

---

## Quick start

Copy-paste one of these. Swap `192.168.1.50` / `alice` / the password for your
own — the only required argument is `--host`.

**Windows** — download the latest build and connect:

```powershell
irm https://github.com/IOServicesLabs/RDPiO/releases/download/nightly/rdpio-windows-x86_64.zip -OutFile rdpio.zip
Expand-Archive rdpio.zip -DestinationPath . -Force
.\rdpio-windows-x86_64\rdpio.exe --host 192.168.1.50 --user alice --password 'hunter2' --insecure
```

**Linux** — same, but headless (it runs the protocol and logs frames; no window yet):

```bash
curl -L https://github.com/IOServicesLabs/RDPiO/releases/download/nightly/rdpio-linux-x86_64.tar.gz | tar xz
RUST_LOG=info ./rdpio-linux-x86_64/rdpio --host 192.168.1.50 --user alice --password 'hunter2'
```

**From source** — any platform, needs [Rust](https://rustup.rs) 1.77+ (and the
MSVC toolchain on Windows):

```bash
git clone https://github.com/IOServicesLabs/RDPiO
cd RDPiO
cargo build --release -p rdp-client
./target/release/rdpio --host 192.168.1.50 --user alice --password 'hunter2' --insecure
```

`--insecure` accepts a self-signed certificate, which is what a stock Windows
host presents. Drop it once the host has a trusted cert.

> **CPU requirement:** builds target `x86-64-v3` (AVX2), so they need a 2013-or-
> newer CPU — Intel Haswell / AMD Excavator or later. The pixel-decode loops are
> the client's main CPU cost and AVX2 doubles their vector width, which is worth
> more than supporting hardware that predates the D3D11 GPU decoding this client
> already assumes. To build for an older machine, override the baseline:
> `RUSTFLAGS="-C target-cpu=x86-64" cargo build --release -p rdp-client`.

Beyond a plain connection, these two cover most of what people actually want:

```powershell
# Business: all your monitors, a shared folder, and the local printer
rdpio --host 192.168.1.50 --user alice --password 'hunter2' --insecure --multimon --drive C:\Shared --printer

# Gaming: latency-first, and render at 66% so the host encodes far fewer pixels
rdpio --host 192.168.1.50 --user alice --password 'hunter2' --insecure --gaming --render-scale 0.66
```

More recipes in [Run it](#run-it); every flag is listed under [Options](#options).

---

## What you can run today

| | Windows | Linux |
| --- | --- | --- |
| **Binary** | `rdpio.exe` | `rdpio` |
| **What it does** | Full interactive client — window, GPU decode, input, device redirection | **Headless**: connects, authenticates, activates, and logs decoded rectangles. No window yet |
| Connect to a Windows host (TCP) | ✅ | ✅ |
| TLS + NLA/CredSSP | ✅ (SChannel/SSPI) | ✅ (rustls + portable CredSSP) |
| Protocol stack + codecs | ✅ | ✅ |
| Window, input, GPU decode/present | ✅ | ⛔ not yet |
| Drives, clipboard, audio, mic, camera, printer | ✅ | ⛔ not yet |
| Windows 365 / AVD sign-in | ✅ | ⛔ not yet |
| Teams "Optimized" (WebRTC redirect) | ✅ | ⛔ not yet |

Linux is a staged port: the protocol core is portable and already runs there.
Rendering, input, and device backends are the remaining work — see
[`PORTING.md`](PORTING.md).

## Run it

Builds come from the [Releases](../../releases) page: every push to `main`
refreshes the `nightly` prerelease with a Windows `.zip` and a Linux `.tar.gz`,
and `v*` tags get permanent releases.

```text
rdpio --host HOST [--port N] [--user U] [--domain D] [--password P] [options]
```

**A domain session** — multi-monitor, with a shared folder, printer, and a
download folder for files copied out of the session:

```powershell
rdpio --host 192.168.1.50 --user alice --domain CORP --password 'hunter2' `
      --multimon --drive C:\Shared --printer --clipboard-dir C:\Downloads
```

**Gaming / low latency** — cheaper host encode plus client-side GPU upscaling.
Best single knob if motion is choppy. `--upscale fsr` (AMD FidelityFX SR 1.0,
runs on any GPU) reconstructs game imagery best; the default `bicubic` is the
safer pick for mixed desktop + game use:

```powershell
rdpio --host 192.168.1.50 --user alice --password 'hunter2' --insecure `
      --gaming --render-scale 0.66 --upscale fsr
```

Render-scale also works across monitors — the host renders a smaller spanned
desktop and the client upscales each monitor's slice back to its native panel:

```powershell
rdpio --host 192.168.1.50 --user alice --password 'hunter2' --insecure `
      --multimon --gaming --render-scale 0.66 --upscale fsr
```

**A Windows 365 Cloud PC** — signs in through the browser and lists your Cloud
PCs to pick from:

```powershell
rdpio --w365
```

**Teams "Optimized"** — run Teams A/V on the client instead of the Cloud PC:

```powershell
rdpio --w365 --teams
```

**A saved connection file**, or a workspace feed on a non-default endpoint
(`--feed` overrides `https://rdweb.wvd.microsoft.com/api/arm/feeddiscovery`):

```powershell
rdpio --rdp-file .\CloudPC.rdp
rdpio --feed https://rdweb.wvd.microsoftonline.us/api/arm/feeddiscovery
```

**Diagnose a session** — mirror the log to a file (`RUST_LOG` sets the level):

```powershell
$env:RUST_LOG = 'info'
rdpio --host 192.168.1.50 --user alice --password 'hunter2' --insecure --log-file rdpio.log
```

On Linux the same commands run headless (connection, TLS, NLA, activation, and
decoded rectangles are logged) — set `RUST_LOG=info` to see the frame log.

> **If gaming feels choppy, the host is usually the bottleneck**, not RDPiO —
> the encoder is the host's built-in RDP server. Lifting its default ~30fps cap
> and steadying CPU clocks are OS-side changes. See
> [`docs/host-tuning.md`](docs/host-tuning.md). RDPiO logs the achieved
> `decode fps` at `RUST_LOG=info` so you can measure each change.

### Helper scripts (Windows)

`run-rdpio.ps1` wraps the two common presets, so you don't have to remember the
flag combinations:

```powershell
.\run-rdpio.ps1 -Mode gaming -RdpHost 192.168.1.50 -User .\alice -Password 'hunter2'
.\run-rdpio.ps1 -Mode office -RdpHost 192.168.1.50 -User .\alice -Password 'hunter2'
```

Two more apply the OS-side tuning from [`docs/host-tuning.md`](docs/host-tuning.md).
Both need an **elevated** shell and support `-WhatIf`, which prints every change
without making it — always dry-run them first:

```powershell
# On the RDP HOST: encoder/graphics policy (run elevated)
.\tune-rdpio-host.ps1 -WhatIf
.\tune-rdpio-host.ps1

# On either end: TCP, NIC offload/RSS, and QoS DSCP marking for rdpio.exe
.\tune-rdpio-network.ps1 -WhatIf
.\tune-rdpio-network.ps1 -LatencyProfile gaming
```

`-LatencyProfile` takes `gaming`, `throughput` (default), or `wifi`. Add
`-EnableJumboFrames` only on a wired LAN — it hurts on Wi-Fi.

## Options

Unknown flags are ignored with a warning (there is no `--help` yet), so a typo
is silent — check the startup log if a flag seems to have no effect.

**Connection**

| Flag | Effect |
| --- | --- |
| `--host`, `-h` / `--port` | Server address (default port 3389) |
| `--user` `-u`, `--domain` `-d`, `--password` `-p` | Credentials (password redacted in logs) |
| `--insecure`, `-k` | Accept a self-signed / untrusted TLS server certificate |
| `--legacy` | Force Standard RDP Security (RC4), skipping TLS/NLA |
| `--keyboard-layout ID` | Keyboard layout ID (decimal or `0x…`) |
| `--log-file PATH`, `--log PATH` | Also write the log to a file |

**Display** *(Windows)*

| Flag | Effect |
| --- | --- |
| `--multimon`, `-m` | Span the desktop across all local monitors |
| `--per-monitor`, `--multimon-windows` | Span all monitors, but one borderless window per monitor so remote windows respect the seams |
| `--fullscreen`, `-f` | Borderless fullscreen on the primary monitor |
| `--width N`, `--height N`, `--size WxH` | Requested desktop size (ignored under `--multimon`/`--fullscreen`) |
| `--bpp 16\|24\|32` | Session color depth |

**Performance**

| Flag | Effect |
| --- | --- |
| `--quality gaming\|office\|balanced` | Latency vs clarity preset. `--gaming` and `--office` are shorthands. All presets advertise the same AVC420 codec caps; what they change is presentation (office pins vsync, 1:1 rendering and bicubic) |
| `--low-latency` | Present with tearing (no vsync) for absolute-minimum latency. Opt-in only — no preset implies it, because the shear is very visible on scrolling text |
| `--render-scale F`, `--scale F` | Render at fraction `F` (0.4–1.0) of the window and upscale on the client GPU — far fewer pixels for the host to encode; `0.66` ≈ 1080p→720p. Works under `--multimon`/`--per-monitor` too: every monitor edge scales through one consistent map (seams stay seams) and each monitor's slice is upscaled to its native panel |
| `--upscale vsr\|bicubic\|fsr\|nearest\|bilinear` | Upscaler for `--render-scale`. **`bicubic`** (default) is sharp without the text ringing AI upscalers cause on UI; **`fsr`** is AMD FidelityFX Super Resolution 1.0 (EASU+RCAS, runs as a shader on **any** GPU — best edge reconstruction for game content); **`vsr`** engages the driver AI video SR — NVIDIA RTX Video Super Resolution or Intel VPE Super Resolution (best for full-screen video; AMD has no driver SR — use `fsr`); **`nearest`** is pixel-perfect at integer ratios (e.g. `0.5`); **`bilinear`** is soft but artifact-free. Aliases: `--vsr`, `--no-vsr`, `--fsr` |
| `--sharpen F` | RCAS adaptive-sharpen strength 0–1 after the upscale (or at 1:1), composable with every `--upscale` mode. Defaults: on (`0.9`) for `fsr`, off otherwise; `--sharpen 0` disables. Counteracts H.264 4:2:0 softness without amplifying compression noise |
| `--pace FPS`, `--smooth FPS` | Present on an even cadence (≤ `FPS`), always the newest frame, to smooth jittery motion for a few ms of latency. Default off |
| `--force-avc444`, `--force-avc` | Advertise full 4:4:4 chroma. Only the CPU decode path reconstructs the extra chroma stream — the GPU (DXVA) path renders its 4:2:0 main view and logs the discard — so this costs the host double encode for a benefit only off-GPU |
| `--no-avc` | Advertise no-AVC caps so the server uses ClearCodec/planar/progressive instead of H.264 |
| `--cpu-yuv` | Force CPU YCbCr→RGB (if GPU-decoded colors look wrong) |
| `--backend d3d11\|d3d12` | GPU backend (default d3d11) |
| `--udp` | Enable the experimental UDP side-band transport (falls back to TCP) |
| `--udp-debug` | Log decoded RDP-UDP datagrams for diagnostics (implies `--udp`) |

**Redirection** *(Windows)*

| Flag | Effect |
| --- | --- |
| `--drive PATH`, `-D` | Share a local folder or drive root as a redirected drive. **Repeatable** (`--drive C:\ --drive D:\Media`), and `--drive all` shares every mounted drive letter — mapped network drives included. Each appears in the session under its drive letter or folder name |
| `--printer` | Redirect the local default printer |
| `--clipboard-dir DIR` | Stage files copied in the session under DIR instead of a temp folder. Clipboard file copy works **without** this flag; set it if you want the pasted files to persist (temp staging is deleted when the copy is superseded or the session ends) |
| `--teams`, `--webrtc` | Teams "Optimized" A/V by hosting Microsoft's WebRTC add-in |
| `--teams-native`, `--webrtc-native` | Teams "Optimized" through RDPiO's own WebRTC engine |

**Windows 365 / AVD** *(Windows)*

| Flag | Effect |
| --- | --- |
| `--w365` | Sign in to Windows 365 and pick a Cloud PC |
| `--feed URL` | Connect via a workspace feed URL |
| `--rdp-file PATH` | Connect using a downloaded `.rdp` file |
| `--tenant ID`, `--client-id ID` | Override the Entra tenant / app registration |
| `--w365-device-code` | Use the device-code sign-in flow instead of the browser |
| `--w365-relogin`, `--w365-logout` | Clear the cached token and sign in again |
| `--forget-password` | Drop the cached Cloud PC password |
| `--shortpath` | Probe the W365 Shortpath UDP rendezvous (diagnostic) |

**Diagnostics**

| Flag | Effect |
| --- | --- |
| `--replay-gfx PATH` | Replay a captured EGFX stream offline, no server (Windows) |
| `--no-seed` | Decode ClearCodec tiles from black to isolate seed/persistence artifacts |

## Features

| Area | What's implemented |
| --- | --- |
| **Display** | Multi-monitor spanning, per-monitor windows, borderless fullscreen, server-driven dynamic resize (MS-RDPEDISP), render-scale on every layout (single, spanned, per-monitor) |
| **Graphics** | GPU H.264 decode via the D3D11 video processor (CPU fallback); RDPGFX/EGFX pipeline; AVC420 + AVC444; ClearCodec, progressive, planar, RemoteFX; client upscalers — AMD FSR 1.0 (EASU+RCAS, any GPU), Catmull-Rom bicubic, NVIDIA RTX VSR / Intel VPE SR, nearest — plus a composable RCAS sharpen pass, on both D3D11 and D3D12 |
| **Latency** | UDP side-band transport: RDP-UDP handshake + TLS-over-UDP + RDPEMT tunnel, retransmission and ACK vectors, automatic TCP fallback |
| **Drives** | Folder and whole-drive redirection (MS-RDPEFS), any number at once, mapped network drives included |
| **Clipboard** | Text and images (`CF_DIB`) both ways; file and folder copy both ways — streamed to disk (any size), fetched lazily on paste, staging cleaned up automatically |
| **Audio** | Speaker output (MS-RDPEA) and microphone input (MS-RDPEAI) |
| **Camera** | Webcam redirection (MS-RDPECAM) via Media Foundation, H.264 encoded on-device (NV12 fallback) |
| **Printer** | Local printer redirection (MS-RDPEPC) via the Win32 spooler |
| **Input** | Keyboard scancodes, gaming-mouse side buttons + tilt wheel, Unicode/IME text, lock-key sync |
| **Security** | Standard RDP Security (RC4/RSA/MAC) **and** Enhanced (TLS + NLA/CredSSP) |
| **Cloud** | Windows 365 / AVD sign-in, workspace feeds, Cloud PC picker, RDSTLS, Reverse Connect |
| **Teams** | "Optimized" A/V — both hosting Microsoft's add-in and a native WebRTC engine |
| **Resilience** | Auto-reconnect (ARC cookies), Deactivate-All reactivation, licensing |

## How it works

### The "from scratch" boundary

"From scratch" means the *protocol* is ours. It does **not** mean
reimplementing TLS, Kerberos, or an H.264 codec — those stay on the OS:

| Concern | Windows | Linux |
| --- | --- | --- |
| RDP protocol (every PDU) | Our crates | Our crates |
| TLS | SChannel | rustls |
| NTLM/Kerberos | SSPI `Negotiate` | Our portable NTLMv2 CredSSP |
| H.264 decode/encode | Media Foundation / DXVA | *(pending)* |
| YCbCr→RGB | D3D11 video processor, or our CPU fallback | our CPU fallback |
| Windowing / spooler / clipboard | Win32 | *(pending)* |

### Workspace layout

| Crate | Platform | Responsibility |
| --- | --- | --- |
| `rdp-asn1` | any | Minimal BER/PER/DER codec (MCS/GCC, CredSSP) |
| `rdp-crypto` | any | RC4, RSA, MD4/MD5/SHA1/SHA256, HMAC, RDP key derivation |
| `rdp-pdu` | any | All wire PDU types + encode/decode (X.224, MCS, GCC, security, capabilities, input, gfx, logon, licensing, multitransport, RDP-UDP) |
| `rdp-core` | any | Sans-I/O connection state machine |
| `rdp-channels` | any | Static MCS channels + DRDYNVC; cliprdr, rdpsnd, rdpdr, disp, audio-input, camera, RDPEMT |
| `rdp-graphics` | any | EGFX command parsing, bitmap codecs, AVC420/444 reconstruction, the DVC demuxer |
| `rdp-webrtc` | any | The `webrtc.1` Teams-redirector protocol + a webrtc-rs engine |
| `rdp-nla` | any | CredSSP/NLA — SSPI on Windows, portable NTLMv2 elsewhere |
| `rdp-gpu` | Windows | D3D11/D3D12 device + swapchain, H.264 decode/encode, color conversion, present |
| `rdp-client` | any | Transport (TCP/TLS/UDP), event loop, device backends — the `rdpio` binary |

The `any` crates are sans-I/O and compile/test on any host, which keeps the
protocol logic unit-testable off-Windows.

### Connection sequence

`rdp-core` drives the standard MS-RDPBCGR sequence over whichever transport the
server negotiates:

1. **X.224** connection request/confirm — advertise TLS/NLA, fall back to
   Standard RDP Security.
2. **TLS** upgrade, then **CredSSP/NLA** when the server requires it.
3. **MCS** Connect-Initial/Response carrying the **GCC** conference data
   (including the `CS_MONITOR` layout for multi-monitor).
4. Erect-Domain, Attach-User, per-channel Channel-Join.
5. Licensing, **Demand-Active / Confirm-Active** capability exchange.
6. Synchronize / Control / Font finalization → **Active**.
7. Fast-path output (bitmaps) and the RDPGFX dynamic channel (H.264) flow;
   DRDYNVC multiplexes graphics, display control, audio-input, and camera.

A server that offers multitransport triggers the optional UDP path: RDP-UDP
handshake → TLS-over-UDP → RDPEMT tunnel → RDPGFX over UDP, with automatic
fallback to TCP on any failure.

## Development

```bash
cargo test --workspace                                    # 456 tests
cargo build --release -p rdp-client                        # the binary
cargo clippy --target x86_64-pc-windows-msvc -p rdp-client # Windows lint from any host
```

Cross-compiling a Linux binary from Windows needs a Linux C toolchain for
`ring`; [`PORTING.md`](PORTING.md) covers the `cargo-zigbuild` route. CI
(`.github/workflows/release.yml`) builds and tests both platforms on every push
and publishes the artifacts.

## License

Licensed under either of MIT or Apache-2.0 at your option.

## DEVELOPED IN THE USA
