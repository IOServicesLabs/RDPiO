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
