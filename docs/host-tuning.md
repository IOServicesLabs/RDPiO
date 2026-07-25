# Host-side tuning for smooth, high-fps RDP gaming

RDPiO is only the **client** (decode + present). The **encoder** is the host's
built-in Windows RDP server, so the levers that decide whether you can hit 60–120fps
— and whether motion is smooth — live in the host's OS config, not in RDPiO. This
guide lists them in priority order. Apply one at a time and measure: RDPiO logs the
achieved frame rate as `decode fps` when run with `RUST_LOG=info`.

> Context: the reference host is a rack server with a server-class CPU and **no
> iGPU** (only a basic BMC/management display adapter), so the RDP server
> software-encodes every frame on the CPU. Items tagged **[SW host]** target that
> case; **[iGPU host]** items apply to deployments whose CPU/APU has Intel
> QuickSync or AMD VCN.

---

## Choosing the right `--quality` preset

RDPiO advertises RDPGFX caps that steer the host's encoder choice. The `--quality`
flag (aliases `--gaming`, `--office`) selects the policy:

- **`--quality gaming` / `--gaming`** — latency-first, AVC420-only, render-scale
  friendly. Best for full-screen games and interactive motion on any host. The
  client GPU decodes a single H.264 stream, and the host avoids AVC444's second
  chroma stream.
- **`--quality office` / `--office`** — clarity-first. Advertises full AVC444/AVC420
  when the local client GPU can decode H.264, and falls back to AVC420-only when it
  cannot. No render-scale; use this for desktop work with small text, spreadsheets,
  and IDE windows where sub-pixel sharpness matters. Pair it with the GPO changes in
  section 4 (RemoteFX Adaptive Graphics) so static content stays crisp.
- **`--quality balanced` (default)** — probes the local GPU and advertises the richest
  caps it can decode quickly. Equivalent to `--office` on a GPU client, and to
  `--gaming` on a CPU-decode client.

For a **CPU-only host**, the biggest wins are `--gaming` + `--render-scale` (section 3).
For an **iGPU/dGPU host**, prefer `--office` so the hardware encoder is allowed to use
AVC444 4:4:4 chroma for text/video; the client GPU decodes it without becoming the
bottleneck.

---

## 1. Lift the RDP frame-rate cap — REQUIRED for >30fps  [all hosts]

By default the RDP server paces the remote session at ~30fps. No client change can
exceed that until the cap is lifted on the host. The community lever is the
`DWMFRAMEINTERVAL` value (frame interval in ms):

```cmd
reg add "HKLM\SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations" ^
  /v DWMFRAMEINTERVAL /t REG_DWORD /d 15 /f
```

- `15` ms ≈ 60fps target; try `8` for ~120fps.
- Log off and back on (or reboot) — the value is read at session start.
- **Verify:** reconnect with `RUST_LOG=info` and watch the `decode fps` line climb
  past 30 during motion. If it stays pinned at ~30, the value didn't take effect on
  your Windows build; it varies by version, so trust the measured fps, not the docs.

## 2. Steady the CPU clocks — the biggest *jitter* lever  [SW host]

Software H.264 encode time per frame swings wildly if the CPU is downclocking, and
uneven frame times are exactly the "jittery under motion" symptom. Lock the host to
flat-out:

**Windows power plan**
```powershell
powercfg /setactive 8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c   # High performance
powercfg /setacvalueindex SCHEME_CURRENT SUB_PROCESSOR PROCTHROTTLEMIN 100
powercfg /setactive SCHEME_CURRENT
```

**Server BIOS/firmware** — server-class firmware defaults to dynamic power
management that overrides Windows, so set it in firmware too. Names below are
HP RBSU's; other vendors expose equivalents:
- Power Regulator → **Static High Performance Mode**
- Workload Profile (Gen10) → **Low Latency**
- Minimum Processor Idle Power Core C-State → **No C-states**
- Energy/Performance Bias → **Maximum Performance**
- Intel Turbo Boost → **Enabled**

## 3. Cut the pixel count — cheap, scales with everything  [SW host]

Fewer pixels = proportionally less encode work, the dominant cost on a software host.
These are RDPiO client flags but the saving is the host's:

```powershell
rdpio -h <host> -u <user> -p <pw> -k --gaming --render-scale 0.66
```

- `--gaming` advertises **AVC420-only**, so the host encodes one H.264 stream, not
  AVC444's two (~half the encode work). The auxiliary chroma stream is discarded by
  the GPU decode path anyway, so there's no visible loss for games/video.
- `--render-scale 0.5–0.66` renders a smaller remote desktop (encode cost ≈ scale²)
  and upscales on the client RTX GPU. Sweep `0.5 / 0.66 / 0.8` for your
  smoothness↔sharpness sweet spot. `0.5` is the realistic path to a 120fps attempt.
- The client upscales with **Catmull-Rom bicubic** by default — sharp on text/UI
  without the ringing an AI *video* upscaler produces on non-video content. For a
  *full-screen* game (no desktop text on screen), add `--upscale vsr` to switch to
  NVIDIA RTX Video Super Resolution, which is tuned for exactly that motion content.
  `--upscale bilinear` is the soft-but-clean fallback.

## 4. Fix "muddy static under motion" — use region-based graphics  [all hosts]

Symptom: a video plays clear, but the *surrounding* desktop (text, UI) goes muddy
while anything moves, and stays muddy. Cause: the host is in **full-screen H.264**
mode — the whole desktop is one H.264 stream, so under motion the encoder raises the
quantizer for the entire frame and the static areas freeze at that low quality (H.264
skip-blocks hold the muddy version; there is no refinement pass). That mode is what
"Prioritize H.264/AVC 444 graphics mode" forces.

Fix: **disable** that GPO so the server uses **RemoteFX Adaptive Graphics** instead —
static text/UI via near-lossless RemoteFX **Progressive** (which refines to crisp and
*stays* crisp), and only the video region via H.264. RDPiO decodes **both** (Progressive
via WireToSurface2, H.264 via the GPU path), so this just works, and it is the single
biggest *quality* win.

- gpedit: *Computer Config → Admin Templates → Windows Components → Remote Desktop
  Services → RD Session Host → Remote Session Environment →* **Prioritize H.264/AVC
  444 graphics mode for Remote Desktop connections** = **Disabled** (or Not Configured).
- Registry: `HKLM\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services` →
  `AVC444ModePreferred` (DWORD) = `0`.

Bonus: with static content now on cheap Progressive (not H.264), the host's encode
load drops enough that you can usually run **native resolution** (drop
`--render-scale`) — which also removes the "everything looks big / soft upscale"
problem. Keep `--gaming` for the low-latency present; the video region still uses
single-stream H.264.

**Verify (RUST_LOG=info):** you should now see *both* `EGFX WireToSurface2 (progressive)
received` (static/text) and `EGFX WireToSurface1 codec in use codec_id="0x000b"`
(video). Text should stay sharp during motion. If the video's colour looks soft and
the host has CPU headroom, A/B `--force-avc444` for 4:4:4 chroma on the video region.

## 5. Hardware H.264 encode — biggest win where it exists  [iGPU host]

A GPU-less server host has no iGPU, so this does **not** apply to it — but for any host
whose CPU/APU has Intel QuickSync or AMD VCN, moving encode onto the iGPU offloads the
CPU entirely and is the single biggest smoothness win:

- gpedit, same *Remote Session Environment* node:
  - **Use hardware graphics adapters for all Remote Desktop Services sessions** = Enabled
  - **Configure H.264/AVC hardware encoding for Remote Desktop connections** = Enabled
- Registry equivalents under `HKLM\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services`:
  `bEnumerateHWBeforeSW` = `1`, `AVCHardwareEncodePreferred` = `1`.
- Needs a GPU driver that exposes an AVC encoder and a session bound to that GPU. The
  RDPiO client-side GPU decode fix is vendor-agnostic, so Intel/AMD *client* GPUs
  decode fine too.

## 6. Network

On a lossless direct link (e.g. a multi-gigabit LAN), keep TCP and **do not** pass `--udp`: its
FEC/retransmit is pure overhead with no loss to recover. `TCP_NODELAY` is always on.

---

## Reality check for a GPU-less server host

With no iGPU and only a BMC display adapter, the remote session's desktop composition (DWM)
runs in software (WARP) on the CPU, on top of software H.264 encode. **60fps at a
reduced render-scale is realistic; sustained 120fps in pure software is unlikely.**
Treat `--render-scale 0.5` + `DWMFRAMEINTERVAL=8` as the 120fps experiment, and a
discrete GPU (or an iGPU-equipped host) as the real path beyond 60.

## Optional micro-tweak

RDPiO already sends the balanced experience flags (no wallpaper / window-drag contents
/ menu animations) on every connect, which is where nearly all the per-frame
experience saving is. Disabling the cursor shadow and cursor blink on top of that is
negligible for jitter, so RDPiO does not bother; set it in the host's session settings
if you want it.

## Verification checklist (from the RDPiO `info` log)

1. No `ignoring unknown argument` → you're running a current build.
2. `--gaming: advertising AVC420-only caps`, and `gfx_caps` without `655362` (the v10
   / AVC444 capset).
3. `EGFX WireToSurface1 codec in use codec_id="0x000b"`, picture paints (not frozen).
4. `DXVA GPU H.264 decoder created` **without** a following
   `DXVA decode failed; falling back to CPU` — the client GPU is decoding.
5. `decode fps` rises past 30 after step 1 (DWMFRAMEINTERVAL), and motion steadies
   after step 2 (power). Sweep `--render-scale` and watch the same line.
