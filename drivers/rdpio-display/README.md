# rdpio Display

An indirect display driver (IddCx, UMDF 2) for the Hyper host. While a client
streams, the Hyper agent asks it for a monitor at exactly the client's
resolution and refresh rate — a Surface Laptop gets 2256×1504, a 240 Hz
laptop gets 240 Hz — and streams that monitor. Nothing is scaled and there
are no black bars. The host's own screens rest meanwhile and come back when
the stream ends.

With no stream, the driver has no monitors: Windows shows nothing extra.

## How it works

- The agent opens the device interface `GUID_DEVINTERFACE_RDPIO_DISPLAY`
  (only SYSTEM and Administrators can) and sends IOCTLs (`src/Public.h`):
  add a monitor (`IOCTL_RDPIO_DISPLAY_ADD`: size, refresh, physical size,
  render GPU), remove all, ping, info.
- Each monitor reports the client's mode as preferred, plus common smaller
  sizes so Display settings still offers choices.
- A watchdog removes the monitors when the agent stops pinging for 10 s, so
  a crashed agent never leaves a phantom monitor behind.
- The agent makes the new monitor the only active display (without saving
  that layout) and sets its scale to the client's (as the signed-in user).
  Afterwards it restores the saved layout and removes the monitor.
- **Frames go straight to the encoder.** Each frame Windows composes for the
  monitor is copied, on the GPU, into a ring of three shared textures; a
  shared section says which is newest and an event wakes the agent
  (`RDPIO_FRAMES` in `src/Public.h`, read by
  `crates/rdp-gpu/src/hyper/driver_frames.rs`). The agent converts and encodes
  from there, with no Desktop Duplication: no duplication surface, no
  duplication pacing, and frames from the lock screen and UAC prompts too.
  Repeats of an unchanged picture aren't copied.
- **The pointer travels beside the picture.** The driver sets up a hardware
  cursor, so Windows leaves the pointer out of the frames and hands over its
  shape and visibility instead. The client draws it where its own mouse is,
  and moving it re-encodes nothing.
- If the frames can't be shared (an older driver, another GPU), the agent
  duplicates the output as before. `RDPIO_DRIVER_FRAMES=0` on the host forces
  that, for comparing.
- **Nobody reading, little work.** While the agent reads frames it writes a
  tick into the shared section (`ConsumerTick`). When the tick goes stale (the
  client minimized, the stream paused) the driver takes frames off the
  swap-chain about twice a second instead of at the desktop's rate, and is
  back at full rate within 50 ms of the next read. A capture that turns to
  Desktop Duplication of the monitor sets the tick to 0, since the
  compositor's rate follows the driver's and the duplication needs it. An
  agent that doesn't write the tick (older builds) gets every frame, as
  before.
- **Windows' 10 s limit.** IddCx ends the driver host when a call into the
  driver takes more than 10 s. No callback waits unbounded: stopping a
  swap-chain thread waits at most 2 s (a thread stuck in a call to Windows,
  as during a GPU reset, is left to end by itself), the D3D device is made
  on the swap-chain thread rather than in the callback, a device that keeps
  failing backs off (0, 100, 250, 500 ms, then 1 s), and the agent's
  add/remove requests wait at most 1.5 s for one under way
  (`STATUS_DEVICE_BUSY`). No C++ exception leaves a callback.
- **Flight recorder.** The driver keeps what each of its threads is inside
  and its last 64 events (failures with their codes) in
  `Global\rdpio-display-trace` (`RDPIO_TRACE` in `src/Public.h`). The agent
  maps it while a monitor exists; if the driver stops answering it logs
  `rdpio Display driver died: ...` and `rdpio Display driver's last events:
  ...` to hyper-agent.log.

Code: `crates/rdp-client/src/virtual_display.rs` (agent side and installer).

Built on the structure of Microsoft's IddSampleDriver
(github.com/microsoft/Windows-driver-samples, MIT License), the same base the
open-source Virtual Display Driver grew from.

## Until it's signed

The agent falls back to the open-source Virtual Display Driver
(`Root\MttVDD`, signed through SignPath). It's driven through its settings
file and its control pipe, and switched on only while a stream needs it.
See `Backend::Mtt` in `virtual_display.rs`.

## Build

Needs Visual Studio 2022 with C++, and the Windows Driver Kit 10.0.26100.

```powershell
.\build.ps1             # out\rdpio-display.dll, .inf, .cat
.\build.ps1 -TestSign   # also signs them with a self-signed test certificate
```

## Install

Put the `out` folder beside `rdpio.exe` and name it `rdpio-display`. Then
`rdpio --hyper-service-install` installs the driver with the service. You can
also use **This PC → Exact-resolution display → Install** in the launcher, or
`rdpio --hyper-display-install` from an elevated prompt.
`--hyper-display-uninstall` removes it.

### Signing

Windows loads only signed drivers.

- **Development (test-signed):** turn on test signing (from an elevated
  prompt, `bcdedit /set testsigning on`) and restart. It needs Secure Boot off
  and shows a "Test Mode" watermark. The installer trusts the test
  certificate in `rdpio-test.cer`.
- **Release:** sign through Microsoft's Hardware Dev Center (attestation
  signing). That needs an EV code-signing certificate and a Partner Center
  account. SignPath's free programme for open-source projects is another
  route. A release-signed driver installs on any Windows 11 PC with no test
  mode.

## Updating a test-signed install, and going back

For a PC that already runs a test-signed rdpio Display (test signing on).
Every step that installs runs in an **elevated** PowerShell. Do it at the PC,
or from a session that isn't on the rdpio Display monitor: reinstalling the
driver ends a stream that is on it.

1. **Keep the build you have.** Copy the output of the build now installed
   somewhere durable first (a temporary folder can be cleared at any time),
   e.g.

   ```powershell
   Copy-Item -Recurse <folder with the current out\ files> "$env:USERPROFILE\Documents\rdpio-display-2026-10-11"
   ```

2. **Build** (not elevated is fine), with a higher version so the two are
   told apart:

   ```powershell
   cd drivers\rdpio-display
   .\build.ps1 -TestSign -Version 1.0.1.0
   ```

   `-TestSign` reuses the "rdpio Test Signing" certificate in your
   CurrentUser\My store if there is one (then it's trusted already), and
   writes `out\rdpio-test.cer`.

3. **Install** (elevated). The installer trusts `rdpio-test.cer` and forces
   the package onto the device, newer or not:

   ```powershell
   New-Item -ItemType Directory -Force "C:\Program Files\rdpio\rdpio-display" | Out-Null
   Copy-Item .\out\rdpio-display.dll, .\out\rdpio-display.inf, .\out\rdpio-display.cat, .\out\rdpio-test.cer "C:\Program Files\rdpio\rdpio-display\" -Force
   & "C:\Program Files\rdpio\rdpio.exe" --hyper-display-install
   ```

   The `rdpio-display` folder beside `rdpio.exe` is also what the Hyper agent
   reinstalls when it has to repair a failed device: keep it at the build you
   want.

4. **Check.** `Get-PnpDevice -InstanceId 'ROOT\DISPLAY\0001'` says `OK`, and
   `Get-PnpDeviceProperty -InstanceId 'ROOT\DISPLAY\0001' -KeyName DEVPKEY_Device_DriverVersion`
   says `1.0.1.0`. Connect: hyper-agent.log has `virtual monitor added` and
   `frames come straight from the rdpio Display driver`.

**Going back** (elevated): put the kept build in place and install it the same
way (the installer forces the older package), then take the newer one out of
the driver store so Windows never picks it again:

```powershell
Copy-Item "$env:USERPROFILE\Documents\rdpio-display-2026-10-11\rdpio-display.*", "$env:USERPROFILE\Documents\rdpio-display-2026-10-11\rdpio-test.cer" "C:\Program Files\rdpio\rdpio-display\" -Force
& "C:\Program Files\rdpio\rdpio.exe" --hyper-display-install
pnputil /enum-drivers   # find the oemNN.inf whose Original Name is rdpio-display.inf and Driver Version 1.0.1.0
pnputil /delete-driver oemNN.inf
```

`DEVPKEY_Device_DriverVersion` then says `1.0.0.0` (dated 10/11/2026). If the
device shows a problem afterwards, `pnputil /restart-device "ROOT\DISPLAY\0001"`
starts it again.
