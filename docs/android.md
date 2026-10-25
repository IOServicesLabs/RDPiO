# rdpio for Android

The Hyper stream from your PC, on an Android tablet or phone (Android 11+,
arm64). It's the same protocol and the same stream as the desktop client's
`--hyper --direct`: the PC encodes with NVENC, the tablet decodes in hardware
(MediaCodec, low-latency mode) straight onto the screen, and sound plays
through AAudio's low-latency path.

## On the PC (once)

The Android app signs in with the **Hyper password** (direct mode): no
Windows credentials, no RDP. See [hyper.md](hyper.md) for the details.

1. Install rdpio **of the same version as the app** (both ends speak the same
   protocol build).
2. As administrator: `rdpio --hyper-service-install` (the service that streams
   the console), then `rdpio --hyper-set-password`.
3. UDP 47998 must reach the PC (the install opens it in Windows Firewall). Over
   the internet, use Tailscale or ZeroTier and connect to the PC's address
   there.

## On the tablet

Install the APK (`adb install -r app-release.apk`, or copy it over and open
it). Enter the PC's name or address and the Hyper password, then **Connect**.
The connection is remembered; tap it next time.

Options per PC:

- **Office mode:** crisp text at 60 fps instead of the lowest latency. Switch
  live with Ctrl+Shift+O.
- **Frame rate:** your screen's fastest by default (144 Hz on a OnePlus Pad 2).
- **Resolution:** your screen's own by default. With the rdpio Display driver
  the PC makes a monitor exactly this size; otherwise it scales its desktop
  down to fit (never up).
- **Text size on the PC:** the Windows display scale for that monitor.
- **Codec:** HEVC where the tablet decodes it, else H.264.

## Windows 365

The **Windows 365** card signs in with your work account (Microsoft's own
sign-in page) and lists your Cloud PCs; tap one to connect. The Cloud PC's
sign-in (RDSTLS) needs the account's password once: it's kept sealed with the
Android Keystore if you choose, and forgotten when the Cloud PC refuses it.
This is the desktop client's RDP stack (EGFX with H.264 decoded by MediaCodec,
sound, clipboard, touch), with the tablet's mic and camera redirected to the
Cloud PC's apps. The Cloud PC's desktop is the screen's full resolution
at a Windows display scale that matches the tablet's (200% on a OnePlus
Pad 2); Menu → **Text size (Windows 365)** changes it, live. If your
organisation's Conditional Access requires a
managed (enrolled) device, the sign-in is refused: rdpio doesn't pretend to
be another device.

## Sessions

A Hyper session and a Windows 365 one can be open together. **Switch
session** in the menu (or Ctrl+Shift+Tab) flips between them at once; the
hidden one stays connected, paused (no decoding, no picture sent). **Other
PCs** in the menu goes back to the connect screen without disconnecting.
Disconnecting one shows the other.

Sessions stay connected while the app is in the background (a notification
says so). Swiping the app away in Recents closes them.

## Efficiency

Always on:

- **Hyper:** datagrams received in batches (`recvmmsg`, with UDP GRO where
  the network coalesces a frame's packets) and decrypted where they landed
  (no copy or allocation per packet); packet and frame buffers reused (the
  decoder hands each frame's back); a lost packet rebuilt by solving for the
  missing pieces only, in NEON; the decoder driven by MediaCodec's own
  callbacks (asynchronous mode), not polled; packets go straight into their
  frame's buffer, tracked in a fixed ring rather than a map. The decoder is
  made while signing in, from Qualcomm's low-latency variant by name, and
  survives a rotation or a trip to the background (parked off-screen).
- **The build:** one optimised unit for the whole Rust core (fat LTO) for
  ARMv8.2 (every Snapdragon since the 845: single-instruction atomics, dot
  product, FP16); the Kotlin side shrunk by R8 (2.3 MB of dex to 0.15 MB)
  and compiled ahead of time from a baseline profile.
- **Windows 365:** NEON YUV → RGBA (1.6 ms a 1080p frame on the big core), only
  the changed rectangles copied to each screen buffer (typing: ~18× fewer
  bytes; scrolls move pixels in place), a non-blocking socket; the bulk
  decompressor and ClearCodec decoder table-driven (1.6-5× faster).
- **Both:** each frame's work reported to Android (ADPF performance hints);
  few wakeups when the picture is still, a session is paused or the app is
  in the background (the screen's own checks drop from 10 to 4 a second when
  nobody's touching it); the stats line logged every 10 s, not every second.
- **Game mode:** unbuffered touch and mouse input (each event at once);
  the receive, decode and picture threads kept on the big cores.
  Menu → Game resolution (100 / 75 / 50%): the PC sends fewer pixels, and
  the tablet makes them full size again with **Snapdragon Game Super
  Resolution** on the GPU (Qualcomm's single-pass upscaler for Adreno: a
  Lanczos-like filter with adaptive sharpening; the stats line shows its
  sizes and cost). Menu → Upscaler → Display hands the scaling to the
  display's own scaler instead (no GPU work, softer).
- **Office resolution** (100 / 75 / 50 / 33%, Hyper only): the same for
  office mode, for a weak connection: at 50% the PC encodes a quarter of the
  pixels, so a page of text costs about a quarter of the data, and the
  upscaler keeps it readable.
- **Auto resolution** (the first choice of both, and what a tablet that
  never picked a size uses): full size to start, then the size follows the
  connection. The measure is bits per pixel per frame (frame rates above 60
  counted as 60): the PC's target bitrate, or, while the link is visibly
  behind (latency over 250 ms or a round trip over 500 ms: frames queue on
  the way), what actually arrives (5 s average). Down after 3 s behind with
  under 0.03 bpp (office: 0.012) to the largest size with some room; up a
  step after 10 s keeping up with the next size still above 0.06 bpp
  (office: 0.025); at most one change per 10 s (each is an encoder restart
  and a keyframe on the PC). An up that has to be taken back within 30 s
  doubles the next up's wait (to 160 s at most) and caps the next ups near
  what arrived, for 5 minutes: a PC target that ignores the queue isn't
  believed twice. Never down while the link keeps up, so a LAN stays at full
  size. Each change is logged (`auto resolution: 50% (2.9 Mbps, 0.030 bpp;
  was 75% at 0.014 bpp; …)`) and shown for 3 s. On the relayed link measured
  in October (2.9 of 15 Mbps arriving, 4.7 s behind, game mode at 75%) it
  goes to 50% after 3 s.
- **After a stall** (a relayed TCP path delivers a second's frames at
  once): every frame is decoded (the next ones refer to it) but only the
  newest is shown — the decoder's pictures behind a newer one are let go
  unshown, each shown one is stamped with the display's clock so the display
  drops one it hasn't shown when a newer one lands before the next refresh,
  and the upscaler latches past queued pictures to the newest. The stats
  line counts them (`stale` on the stream's line and the upscaler's). And
  such a clump is decoded, not thrown away: frames waiting for the decoder
  are dropped for a fresh keyframe only when that backlog hasn't shrunk in
  300 ms (a decoder slower than the stream) or is over 2 s of frames — a
  keyframe asked for over a link carrying a few Mbit/s takes seconds and
  arrives as the next clump.
- **Sound:** the desktop's jitter buffer: held at the depth the network's
  jitter needs (10 ms on a calm link), with a burst or the two clocks' drift
  played away 0.5% faster or slower (inaudible) instead of lingering as
  delay. In game mode the output opens exclusive (MMAP: the DSP reads the
  buffer directly) at one burst, a burst more each time it glitches; after
  10 s of quiet it pauses rather than closes, so the next sound starts at
  once. The mic goes in 10 ms frames in game mode (voice chat), 20 ms in
  office mode (calls), Opus at complexity 5 (a quarter less CPU, WebRTC's
  value for phones). The stats line's `audio` row shows the buffer, its
  target, the device's own latency, underruns and the path. A Cloud PC's
  sound goes through the same jitter buffer, held at 60 ms (RDPSND sends it
  in bursts).
- **Office mode and Windows 365** (60 fps anyway): the display at 60 Hz
  (120 while a mouse or trackpad moves, for a smooth pointer, back to 60
  1.5 s after it stops), input batched to the display's frames (and sent
  from the app's own thread, without waking another), the decoder clocked for the rate, packets taken
  off the socket in bunches (a 1 ms rest after each batch: half the receive
  thread's CPU) and Wi-Fi allowed to doze between bursts. (Not the power-saving sound path:
  it measured 263 ms of its own delay, a video's sound visibly late.)
- **Heat:** from Android's "moderate" thermal status (or a forecast of
  throttling) the PC streams at most 90 fps, from "severe" 60, until the
  tablet cools.

The stats line (Ctrl+Shift+S) shows the app's CPU (% of one core), wakeups
and input events a second, the decode time, the upscaler's CPU and GPU time
a frame, and the thermal status and headroom.

### On the PC's own network

Through Tailscale every packet is encrypted twice, and Tailscale's own app
spends about three quarters of a core of the tablet at 24 Mbit/s. So the PC
tells the tablet its local addresses when it signs in; the next time the
tablet is on one of those networks it connects there directly ("streaming
over the local network" in the log), and falls back to the address you
gave if the PC doesn't answer. The stream is encrypted end to end either
way. Needs the same rdpio on the PC.

### Switches for testing

Each optimisation can be turned off on a connected tablet, for an A/B
comparison, with `adb shell setprop debug.rdpio.<name> 0` (back on with
`1`; read within a second, unless noted):

| Switch | Off means |
|--------|-----------|
| `bigcores` | no thread placement (or `1` to force it in office mode) |
| `asynccodec` | the decoder polled (the decoder is remade) |
| `longwait` | the decoder's output asked for every 8 ms |
| `directinput` | input handed to the control thread |
| `framebufs` | a new buffer for every frame |
| `udpgro` | one datagram per receive slot |
| `rxnap` | no rest between receive batches in office mode |
| `audioinline` | sound decoded on its own thread |
| `opuscx5` | the mic at Opus's default complexity |
| `pointerhz` | 120 Hz whenever a mouse is attached |
| `lanpath` | always the address given (read when connecting) |
| `keepdecoder` | the decoder remade when the screen's Surface changes (rotation, split screen, back from the background) |
| `qtilowlat` | Qualcomm's decoder found by Android's default lookup, not its `.low_latency` variant |
| `prewarm` | the decoder made only once the PC's first frame arrives |
| `adpfspike` | Android not warned before a keyframe or a recovery frame |
| `adpfeco` | office mode's performance hints not marked power-efficient |
| `invalidate` | the GPU writes the upscaler's old frame back to memory every frame |
| `upscalehint` | no performance hints for the upscaler's GPU thread |
| `cursorcache` | each PC cursor shape rebuilt every time it appears |
| `uinotify` | the screen's own checks polled (10 a second, 4 idle) instead of woken by the core |
| `micinline` | the mic's sender woken on every audio burst, not once a frame |
| `autores` | Auto resolution stays where it is (full size from the start) |
| `newest` | every decoded picture shown, in turn, after a stall (the decoder's and the upscaler's) |
| `newestsf` | the decoder's pictures stamped with the PC's capture time, as before: the display shows each queued one a refresh in turn (only the drop of pictures already behind a newer one stays) |
| `burstok` | a backlog of more than ~50 ms of frames thrown away at once for a keyframe, even a clump off a stalled link |
| `stageshards` | (`1`) each packet copied twice on its way into the frame (read when connecting) |
| `av1` | (`1`) AV1 asked for first, on a host that encodes it (read when connecting) |
| `perflog` | (`1`) the stats line logged every second |
| `inputdup` | every unacknowledged input event in every packet, resent every 15 ms (not the newest few, a key-up repeated 5 ms later, everything once a round trip) |
| `opusfec` | no Opus in-band FEC: the mic sends none, and a lost frame of the PC's sound is only concealed |
| `catchup` | a sound backlog cut abruptly at the cap (a click) and played away at 0.5% below it (10 s for 45 ms), not cut out after half a second with a matched 5 ms crossfade; the stats line's `cut` is how much was cut |
| `fastreconnect` | a stream taken for dead only after 6 s of silence, sign-ins retried after 0.3 s then every 2 s (not probed from 1.5 s, dropped at ~3 s, retried from 250 ms doubling) |

The stats line ends with `path: LAN`, `direct` or `relay ord` once the PC
says how it reaches the tablet (Tailscale's relay: every packet over TCP via a
server elsewhere; the log also says "Tailscale is relaying this connection").
A reconnect logs "signed in again N ms after the drop" and then "first picture
N ms after the sign-in began", counted from the drop.

## Input

Touch, a trackpad and a mouse all work at the same time:

| Device | What it does |
|---|---|
| Fingers on the screen | **Windows touch**: tap, flick to scroll, press and hold, pinch, edge swipes. Menu → "Fingers: touch / mouse" makes them a mouse instead (tap = click, hold = right click, drag = drag, two fingers = scroll, two-finger tap = right click). |
| Trackpad (keyboard cover) | Android's pointer, drawn with the PC's cursor. Click and tap-to-click, right click, **two-finger scroll** (smooth, like a precision touchpad), **pinch to zoom** (Ctrl + wheel). |
| Mouse | Pointer, all five buttons, the wheel (and tilt). |
| Ctrl+Shift+M | Locks the mouse to the stream for games (raw relative motion, no acceleration). Press again to free it. |
| Hardware keyboard | See below. |
| On-screen keyboard | Menu → Keyboard. Typed as characters, so any language works. |

Shortcuts: **Ctrl+Shift+Q** disconnect, **Ctrl+Shift+S** stats, **Ctrl+Shift+O**
game/office, **Ctrl+Shift+M** lock the mouse. The round button over the stream
(or the back gesture, or **Shift+Esc** on a keyboard) opens the menu; drag the
button anywhere and it stays on that side next time. A keyboard's Esc always
goes to the PC, also from keyboards that send it as Android's Back key.

### Hardware keyboards

Two modes (Menu → "Keys: PC / tablet layout"; remembered):

- **PC layout** (the default, Windows App's "scancode" mode): every key goes
  by its position, as if the keyboard were plugged into the PC. The PC's
  keyboard layout applies, and so do games, shortcuts, and keys held down.
  Keys skip the tablet's input method, so it can't autocorrect or swallow them.
- **Tablet layout** ("unicode" mode): character keys type the character they
  make on the tablet (its layout, dead keys, AltGr), and an input method can
  compose (Japanese, Chinese, Korean). Shortcuts (with Ctrl, Alt or Win),
  arrows, Enter, Tab, Backspace and the keypad still go as keys. Use it when
  the tablet's keyboard layout isn't installed on the PC.

Android keeps some combinations for itself: Alt+Tab, the Windows/Meta key
and its shortcuts. To send them to the PC, open the menu and choose **Alt+Tab
to PC**, then turn on **"rdpio: PC shortcuts"** in Android's accessibility
settings. It's an accessibility service because only those see a keyboard's
keys before Android's own shortcuts do. It reads no screen content, and it
only takes keys while a stream is in front. If the switch is greyed out (an
APK installed from a file), go to App info → ⋮ → **Allow restricted
settings** first. The same menu item turns the capture off and on again.

Without it, these stand in, as in Windows App:

| Press | The PC gets |
|---|---|
| Ctrl+Alt+End or Ctrl+Alt+Del | Ctrl+Alt+Del (the secure screen) |
| Alt+Page Up | Alt+Tab (hold Alt, press again to step) |
| Alt+Page Down | Alt+Shift+Tab |
| Alt+Home | Start |

Also mapped: Japanese (Ro, Yen, Henkan, Muhenkan, Kana, Zenkaku), Korean
(Hangul, Hanja), Brazilian (keypad comma, the extra / key), ISO's extra key,
F13–F24, Pause, media keys. A modifier remapped in Android's keyboard settings
(Caps Lock as Ctrl, say) stays remapped on the PC. The keyboard's volume keys
set the tablet's volume.

Text copied on either side lands on the other's clipboard.

## Building

Requirements: the Android SDK with an NDK and CMake (Android Studio's SDK
manager, or `sdkmanager "ndk;27.1.12297006" "cmake;3.22.1"`), JDK 17+, and
for the Rust core `rustup target add aarch64-linux-android` and
`cargo install cargo-ndk`.

```sh
cd android
./gradlew assembleRelease        # also builds the Rust core (build-rust.sh)
adb install -r app/build/outputs/apk/release/app-release.apk
```

On Windows, Gradle runs `build-rust.sh` with Git's bash
(`-Prdpio.bash=<path>` picks another). libopus is built from source with CMake
and Ninja (the SDK's CMake has both).

Logs: `adb logcat -s rdpio` (a `perf:` line every 10 s while a stream plays,
and at once on a loss or a bitrate cut: fps, latency p50/p95, round trip,
bitrate, lost frames).

Third-party: `app/src/main/assets/sgsr1_shader_mobile_edge_direction.frag` is
Snapdragon Game Super Resolution 1, © Qualcomm Innovation Center, Inc., under
the BSD 3-Clause licence (`assets/sgsr-LICENSE.txt`, shipped in the app).
