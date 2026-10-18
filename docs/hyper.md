# Hyper Accelerator

Hyper is rdpio's own low-latency stream for gaming. RDP's graphics path tops
out at 60 fps (the session's virtual display) and buffers more than a game
wants; Hyper streams the host's real monitor at its full refresh rate (120,
144, 240 Hz), encoded on the GPU, over encrypted UDP.

## Two ways to connect

| | Direct mode (recommended) | RDP handoff |
|---|---|---|
| Sign-in | A separate **Hyper password** | Your Windows credentials over RDP |
| RDP needed | No | Yes (then it steps aside) |
| Lock screen / sign-in screen / UAC | Streamed and usable | Not reachable |
| Host side | Windows service (`--hyper-service-install`) | Logon task (`--hyper-install`) |

Devices needn't run the same rdpio: sign-in settles on the newest protocol
both speak, and a host hands its newer rdpio to the devices that connect (see
[Updates and versions](#updates-and-versions)).

## The launcher

Start `rdpio.exe` with no arguments for the launcher. It has three pages:

- **Connections:** quick connect, saved connections and recent ones. Each
  saved connection has its own options:
  - how it connects: Remote Desktop, RDP + Hyper, or Hyper Direct;
  - what it's tuned for: games or work and text;
  - codec, frame rate, full screen, latency, printer and drives.

  Passwords are kept encrypted for your Windows account. A session that
  fails says why.
- **This PC:** sets this PC up as a Hyper host. It installs the service,
  sets the Hyper password, adds the virtual display, and lists the addresses
  to connect to (the Tailscale one works from anywhere). Windows asks for
  administrator permission for these steps.
- **Settings:** theme, whether the launcher stays open after connecting,
  what new connections start with, and the in-session keys.

Each session runs in its own process. `--classic-hub` opens the older window,
and rdpio falls back to it on PCs without the WebView2 runtime.

## Host setup (the gaming PC), once

The launcher's **This PC** page does this for you. Or, in an **elevated**
PowerShell, from wherever rdpio.exe is:

```powershell
.\rdpio.exe --hyper-service-install
```

It asks for a Hyper password (twice; any length), copies rdpio.exe to
`C:\Program Files\rdpio\` (the service runs that copy, which only
administrators can change), installs and starts the `rdpioHyper` service, and
allows UDP 47998 through the firewall. The service then keeps itself up to
date from GitHub (see [Updates and versions](#updates-and-versions)); after
installing a build by hand, run the install again to refresh the copy. Change the password later with
`--hyper-set-password` (it applies from the next sign-in; a laptop that saved
the old one asks again); remove the service with `--hyper-service-uninstall`
(the copy in Program Files stays). Stopping or removing the service while a
client is connected ends the stream cleanly: the host's audio and displays are
put back and the PC locks.

Optional host flags (append to the install command):

| Flag | Effect |
|---|---|
| `--hyper-mbps <N>` | Bitrate ceiling (default 80) |
| `--hyper-size <WxH>` | Stream a smaller picture (GPU-scaled), e.g. `1920x1080` over the internet |
| `--hyper-fps <N>` | Cap the refresh rate the monitor is switched to |
| `--hyper-keep-monitors` | Leave the host's other monitors on while streaming (by default only the streamed, primary one stays on, so nothing opens where the client can't see it) |
| `--hyper-port <N>` | UDP port (default 47998) |

For game audio to go to the client instead of the host's speakers, and for the
client's microphone to reach host apps, the host needs virtual audio devices:
**Steam** installs "Steam Streaming Speakers/Microphone" (used automatically);
VB-Audio CABLE works for the mic too.

## Connecting (the laptop)

```powershell
rdpio.exe --host <gaming PC> --hyper --direct --fullscreen --low-latency
```

Enter the Hyper password once; it is remembered for that host (encrypted with
your Windows account). If the PC is locked you see the lock screen in the
stream — sign in there as usual.

The keys below are shown on screen for the first seconds of every stream.

| Key | Action |
|---|---|
| Ctrl+Shift+Q | Disconnect (the host locks) |
| Ctrl+Shift+M | Lock the mouse in (relative aiming for games). Again to release |
| Ctrl+Shift+K | Win, Alt+Tab, Alt+F4, Ctrl+Esc: to the host (the default) or to the laptop (Hyper only) |
| Ctrl+Shift+S | Stats overlay: fps, latency, round trip, bitrate, loss (Hyper only; in plain RDP it reaches the remote app) |
| Ctrl+Shift+O | Office mode on/off: crisp text (see below) or game mode (Hyper only) |
| Ctrl+Alt+End | Ctrl+Alt+Del on the host (lock screen, security options) |

While the stream's window is in front, Win, Alt+Tab, Alt+F4 and Ctrl+Esc act
on the host, as if you sat at it. To use them on the laptop, press
Ctrl+Shift+K (again to give them back), or click another window. Ctrl+Alt+Del
always stays on the laptop, so there's always a way out.

**On battery** the client saves power where you won't see it:

- It decodes and draws on the GPU the screen is attached to: on a laptop
  with two GPUs, the integrated one, so the discrete GPU can switch off
  (and no frame is copied across to the screen's GPU). Plugged in, it uses
  the fastest. Plugging in or out mid-stream moves it in place, with one
  fresh picture. `RDPIO_GPU=display` or `performance` overrides it.
- On a still screen it wakes the CPU less: the 1 ms timer goes after 2 s
  with no picture and no input (back with the next key or mouse move), and
  the network thread checks in less often.
- The speakers close after 10 s of silence and open again with the next
  sound. The host sends no audio while it plays only silence (an app holding
  the device open), so they really do.

Plugged in or not, it does no work no one sees:

- **Hidden means paused.** Minimized, on another virtual desktop, the laptop's
  screen off (timed out, lid closed) or the laptop locked: the host encodes
  and sends nothing, and the client hands back what the stream held — Wi-Fi
  may nap and scan again, the screen and the PC may sleep. Seen again, the
  next picture comes at once.
- **The screen may dim on a still picture.** While the picture moves (a video,
  a game) the screen stays on, as for any video player. A still one with
  nobody at the controls dims and sleeps on the laptop's own timeout; your
  input in the window counts as using the PC.
- **Frames the screen can show, no more.** A laptop that drops from 240 Hz to
  60 on battery tells the host, which streams at 60 (never above what was
  agreed at the start) — not 240 frames a second to decode and throw away.
- **One pass to the screen.** A picture shown 1:1 with nothing over it goes
  from the decoder straight into the window's backbuffer: no copy through a
  framebuffer, no scaling pass. Where the display has an NV12 overlay plane
  (most Intel laptops), it goes there instead and the display hardware
  converts and composes it — the GPU's 3D engine and Windows' compositor stay
  idle, as for a video player. The log says which (`video on a hardware
  overlay plane`, or why not); `RDPIO_VIDEO_LAYER=0` turns the overlay off.
- **No parity copies on a clean link.** After 10 s without a lost packet,
  small frames go without forward error correction (it would double them);
  the rare loss is repaired from a long-term reference, and the first loss
  turns it back on.

The host helps too, plugged in or not:

- **Still-screen trickle (office mode).** Nobody has touched the keyboard,
  mouse or controller for 0.3 s, and the screen changes only in small ways (a
  clock, a caret, a spinner)? Frames then go out at about 12 a second instead
  of 60. The newest picture always gets through. The next input, or a big
  change such as scrolling, a video or a new page, brings the full rate back
  with the very next frame.
- **Sharpening scales with the change.** After a change, office mode
  re-encodes the still picture a few times, finer each time. A new page gets
  up to a second of that; a caret blink gets two frames.

The host's log line shows how often it trickled (`trickle_pct`), the audio
packets a second (`audio_pps`), and the GPU's graphics/encoder clocks
(`gpu_mhz`).

Decoded pictures are drawn straight from the decoder's own memory, not
copied out first (any GPU that allows it).

Client flags: `--hyper-fps <N>` (ask for this frame rate; default: your
display's), `--hyper-codec h264|hevc`, `--hyper-vsync`, `--hyper-password <PW>`.

**Codec, chosen automatically:** HEVC wherever the client's GPU decodes it:
on an NVIDIA host it encodes faster than H.264 (1.2 vs 1.35 ms a 1080p frame)
and looks better for the same bits (+0.3 to +2.3 dB at 6-25 Mbps, about a
third less bandwidth for the same picture). A client decoding on its CPU gets
H.264, the lighter of the two there. `--hyper-codec` forces either.

**Resolution, matched to the laptop:** with the **rdpio Display** driver on
the host (`drivers/rdpio-display`, installed with the service when it ships
beside rdpio.exe), the host makes a display just for the laptop:
- exactly its resolution and refresh rate, so a Surface gets 2256×1504 3:2 with
  no scaling and no bars;
- at its display scale, so text is the size the laptop's own desktop uses.

That display is the host's only one while you stream. The host's own screens
come back when you disconnect.

rdpio Display also hands each frame straight to the encoder on the GPU,
without Desktop Duplication, and sends the pointer beside the picture
instead of in it, so moving the mouse re-encodes nothing.

Until rdpio Display is signed, the open-source **Virtual Display Driver**
does the same job. It's signed, so no test mode is needed. Install it on the
host with `winget install VirtualDrivers.Virtual-Display-Driver`. rdpio uses
it whenever its own driver isn't running:
- it turns the driver on only while a device streams;
- it adds the device's exact mode to the driver's settings for that stream,
  then puts your settings back.

Without the driver, the stream is the laptop's screen size: the host scales
its desktop down to it on its GPU (never up), so the laptop decodes exactly
what it shows, 1:1.
- A laptop screen bigger than the host's desktop gets the host's full
  resolution and upscales the rest (`--upscale`).
- A desktop of another shape than the screen (16:9 on a 3:2 Surface) keeps
  its shape, with black bars. The mouse maps to the picture.

`--hyper-size` on the host overrides all of this.

**A laptop without a working GPU driver** (Device Manager shows "Microsoft
Basic Display Adapter") still gets a picture, decoded and converted on its
CPU, in H.264 (the lighter of the two there). Expect lower frame rates and
more latency than with the driver installed; the start-of-stream hint says
so.

**Office mode** (`--hyper-office`, or Ctrl+Shift+O at any time): for work
rather than games — text stays crisp, video still plays. The host encodes at
60 fps, lets a frame take a quarter second's bits (a screen of new text
arrives sharp instead of blurring and then sharpening over a second), uses
the encoder's best-quality preset, prefers HEVC (when the laptop's GPU decodes
it), runs its monitor at the laptop's resolution when the monitor offers that
exact mode (text drawn natively, nothing scaled — your resolution comes back
when you leave office mode or disconnect), and switches its font smoothing to
greyscale while streamed (ClearType's
coloured glyph edges don't survive video and are tuned for the host's own
screen; put back afterwards). Measured on a screen of 1080p office text:
first-frame luma 18 dB in game mode → 44 dB in office mode. The trade:
about 8 ms more latency (the best-quality preset: ~11 ms capture to screen
instead of ~3), and a page of new text takes a few frames longer to arrive.

**Presentation:** frames are shown the moment they're decoded (no waiting for
vblank — on a G-Sync/FreeSync display that also means no tearing).
`--hyper-vsync` waits for vblank instead.

## What carries over

Picture (H.264/HEVC), game audio (Opus), microphone (only while a host app
listens, like RDP), keyboard and mouse (incl. relative mouse), the host's
cursor shape, clipboard text, images and files (both ways, up to 4 GB),
resolution changes, and monitor consolidation. If the network drops, the
client reconnects by itself and the host keeps the session unlocked for 20 s;
closing the window locks the host immediately.

Clipboard and file copies only use what the stream leaves free: they slow
down the moment the link starts queueing, and the pointer never waits behind
them (it has its own lane).

A host whose display has gone to sleep is woken when a client connects (it
has to be on to be captured), and stays awake while streamed — as does the
client. Minimizing the client window pauses the stream (the host encodes
nothing, the laptop decodes nothing); restoring it resumes at once.

If the host's GPU driver resets or its encoder fails, the stream rebuilds
itself within a second or two; if the client's GPU resets, the client starts a
fresh renderer and rejoins. Neither ends the session.

## Over the internet (Tailscale / ZeroTier)

Connect to the host's Tailscale/ZeroTier address. Packets never exceed 1200
bytes (no fragmentation on either), lost packets are rebuilt from forward
error correction rather than resent, and the bitrate backs off as soon as
delay starts building. Prefer HEVC and consider `--hyper-size 1920x1080` and a
lower `--hyper-mbps` on the host for slow uplinks.

## Latency notes

- The host switches its monitor to the fastest refresh rate that's a whole
  multiple of the client's (240 Hz for a 120 Hz laptop, where the monitor has
  it) and back afterwards. A frame waits half a refresh on average to be
  composed and captured, so that's ~2 ms off every frame; the stream still
  runs at the client's rate, skipping every other frame evenly.
  `RDPIO_FAST_REFRESH=0` on the host keeps it at the client's rate.
- Without the rdpio Display driver, the host's monitor also takes the
  client's resolution for the stream when it offers it (1920x1080 for the
  Razer), else the closest: the mode that fills the client's screen best
  without being enlarged there, never more pixels than before and never at
  fewer frames. Fewer pixels to encode, and nothing scaled on the client.
  `RDPIO_MATCH_RESOLUTION=0` on the host keeps the resolution as it is.
- Each burst of a frame's packets goes to the kernel in one call (UDP
  segmentation offload; ~27% less send time for a keyframe on loopback),
  and the client takes the packets that queued up together in one call
  (receive coalescing). Where Windows or the adapter can't, they go one by
  one as before. Nothing changes on the wire.
- On NVIDIA GPUs the host drives the encoder (NVENC) directly rather than
  through Media Foundation: as fast (1.8 ms a 1440p frame), but a new bitrate
  takes effect at once, in place. Through Media Foundation a big change meant
  a new encoder: a ~30 ms stall and a keyframe, every 10-20 s in a live
  office session as the rate moved. Other GPUs, the "sharpest text" office
  option (Media Foundation's quality setting codes new text sharper than
  NVENC's own presets can) and `RDPIO_NVENC=0` on the host use Media
  Foundation.
- A lost frame is repaired without a keyframe where it can be: the host keeps
  a frame as a long-term reference every quarter second (two in turn), the
  client says which of those it decoded, and after a loss the host codes the
  next frame from the newest one the client has. That frame is normal-sized
  (a keyframe is several times bigger, so on a lossy link it's the one most
  likely lost again) and the picture is back after one round trip. With no
  confirmed reference, or a third repair within a second, it's a keyframe as
  before. `RDPIO_HYPER_LTR=0` on the host turns it off.
- Office mode re-sends a still picture for up to a second, each time
  sharper: the fast preset's text comes out at 33 dB, 45 dB half a second
  later and 52 dB after a second, beyond the quality preset's 42 dB (which
  costs ~8 ms on every frame).
- Frames leave from their own thread, paced on a high-resolution timer, data
  first and error correction right after — the next frame's capture and
  encode never wait on the network.
- A game that holds the GPU near 100% delays capture by roughly one of its own
  frames (Windows composites the desktop on the same GPU). Cap the game a few
  fps below the monitor's refresh rate for the lowest stream latency.
- Measured on an RTX 4080 SUPER host at 2560×1440, H.264: capture → decoded
  ≈ 3–4 ms median (encode ≈ 3.3 ms, pacing ≈ 0.15 ms on a clean link); HEVC
  adds ≈ 2 ms. Packet pacing is adaptive: tight on a clean path, spread out
  only when loss shows a Wi-Fi radio or router can't take bursts.
- HDR desktops are captured too (Windows converts them to SDR for the stream).

## Updates and versions

Devices don't need to run the same rdpio.

- **The host updates itself.** The Hyper service looks for a new release on
  GitHub every 6 hours and installs it between streams (never during one).
  It restarts on the new version within seconds. To turn this off, or to
  follow nightly builds, use the switches under This PC → Updates.
- **The host updates the devices that connect to it.** A device signing in
  with an older rdpio gets the host's copy, sent over the same encrypted
  connection:
  - When the two can still stream to each other, the copy downloads in the
    background at about 1 MB/s while you play, and installs when you close
    the window.
  - When they can't (no Hyper protocol in common), the device shows "updating
    this PC N%", installs the copy, and opens the same connection again on
    the new version. This takes a few seconds on a LAN.
- **The launcher checks GitHub when it opens** and shows a banner when a
  newer release is out. Click **Update now** to install it; rdpio then
  reopens. All of this is in Settings → Updates:
  - Check for updates automatically.
  - Update from the PCs I connect to.
  - Releases or Nightly.

  `RDPIO_NO_UPDATE=1` turns updating off for one run.
- **Updates are signed.** Each release carries `rdpio.exe.update`: the
  version, the build, the protocol range, and the exe's SHA-256, signed with
  rdpio's release key (Ed25519). rdpio installs an update only if all of
  these hold:
  - the release key signed it;
  - the exe matches the manifest;
  - it is newer than what's running.

  So no host, network or mirror can hand a device anything else, or roll it
  back to an older version. Builds you make yourself have no signature, so
  they neither offer nor accept updates.
- **The protocol rule.** Sign-in exchanges each side's rdpio version and the
  range of Hyper protocol versions it speaks, and settles on the highest one
  both share. The sign-in format and the update transfer never change, so
  any two builds from protocol 4 on can always reach each other to update.
  When the stream protocol changes:
  - the newer build keeps speaking the previous version
    (`MIN_VERSION = VERSION - 1`), so the host and devices can update in
    either order;
  - anything older gets updated as above.

Releases: push a tag `vX.Y.Z` that matches `version` in `Cargo.toml`. CI
builds, signs (repository secret `RDPIO_SIGNING_KEY`, a base64 Ed25519 seed)
and publishes `rdpio.exe` and `rdpio.exe.update` beside the zip. Every push
to `main` refreshes the nightly the same way. To sign a local build by hand
(the key in `%USERPROFILE%\.rdpio\release-signing.key`), run
`rdpio.exe --release-sign rdpio.exe` with that same exe.

## Security

- The Hyper password never crosses the network: sign-in is SPAKE2 (a
  password-authenticated key exchange), so a captured sign-in can't be
  brute-forced offline. Five wrong guesses lock the source address out, for
  longer each time; an address gets at most two sign-ins in flight at once.
- The session key is bound to everything both sides said during sign-in, so
  someone on the path who alters the client's request or the host's offer
  just breaks the sign-in.
- Every session gets a fresh AES-256-GCM key; replayed packets are dropped.
- The host stores only a hash of the password, readable by SYSTEM and
  Administrators.
- If the agent crashes mid-stream, the next one restores the audio devices and
  display layout and locks the console.
- When a client leaves, the host locks the console as the signed-in user and
  checks that it took; if it didn't, it disconnects the session instead (the
  sign-in screen shows either way). The log says which.
- Sign-in starts with an address cookie: a hello from a spoofed address gets
  nothing but a cookie, so it can't cost the host work or lock you out.
- The service install allows services to raise Ctrl+Alt+Del (the
  `SoftwareSASGeneration` policy), which Ctrl+Alt+End needs.
- Updates install only when rdpio's release key signed them (see Updates and
  versions above), whoever passed them along.

## Logs

- Host: `C:\ProgramData\rdpio\hyper-service.log`, `hyper-agent.log` (direct
  mode), `hyper-host.log` (RDP handoff's logon task)
- Client: `--log-file <path>`; a `hyper` perf line every second.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| "no answer from a Hyper host" | Service not installed/running, UDP 47998 blocked, or an older rdpio on one side. |
| "the host runs a different rdpio version" | Install the same rdpio on both PCs (then re-run the host install). |
| "wrong Hyper password" | Re-run; the saved one is cleared. |
| "refusing sign-ins … try again in N s" | Too many wrong passwords from this address. |
| "decoding on the CPU", "No GPU video decoder" | The client's GPU driver is missing — install it (the stream still works, on the CPU). |
| Choppy under heavy GPU load | Cap the game's frame rate (see Latency notes). |
