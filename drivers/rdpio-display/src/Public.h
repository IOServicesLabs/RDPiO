// rdpio Display: the interface between the driver and the rdpio Hyper agent.
//
// The agent opens the device interface below (SYSTEM and Administrators
// only, see the INF) and asks for a virtual monitor at the client's exact
// resolution and refresh rate. Monitors go away on IOCTL_RDPIO_DISPLAY_REMOVE,
// or by themselves when the agent stops pinging (any IOCTL counts) for
// RDPIO_DISPLAY_WATCHDOG_MS — a crashed agent never leaves a phantom
// monitor behind.
//
// Each monitor also shares its frames (RDPIO_FRAMES below): the driver copies
// every composed frame into a ring of shared GPU textures, so the agent
// encodes them without Desktop Duplication, and the pointer travels beside
// the picture instead of in it.
//
// Mirrored in crates/rdp-client/src/virtual_display.rs: keep both in step.

#pragma once

#include <guiddef.h>

// {6D3B1A7E-2C41-4F0E-9A51-3E8B7C215D94}
DEFINE_GUID(GUID_DEVINTERFACE_RDPIO_DISPLAY, 0x6d3b1a7e, 0x2c41, 0x4f0e, 0x9a, 0x51, 0x3e, 0x8b, 0x7c, 0x21, 0x5d, 0x94);

#define RDPIO_DISPLAY_INTERFACE_VERSION 1 // (frame sharing is found by name)
#define RDPIO_DISPLAY_MAX_MONITORS 4
#define RDPIO_DISPLAY_WATCHDOG_MS 10000

#define RDPIO_DISPLAY_IOCTL(n) CTL_CODE(FILE_DEVICE_UNKNOWN, 0x900 + (n), METHOD_BUFFERED, FILE_ANY_ACCESS)

// out: RDPIO_DISPLAY_INFO
#define IOCTL_RDPIO_DISPLAY_INFO RDPIO_DISPLAY_IOCTL(0)
// in: RDPIO_DISPLAY_MODE; out (optional): RDPIO_DISPLAY_ADDED
#define IOCTL_RDPIO_DISPLAY_ADD RDPIO_DISPLAY_IOCTL(1)
// no buffers: every monitor departs
#define IOCTL_RDPIO_DISPLAY_REMOVE RDPIO_DISPLAY_IOCTL(2)
// no buffers: the agent's still there (resets the watchdog)
#define IOCTL_RDPIO_DISPLAY_PING RDPIO_DISPLAY_IOCTL(3)

#pragma pack(push, 4)

typedef struct _RDPIO_DISPLAY_INFO {
    UINT32 InterfaceVersion;
    UINT32 MaxMonitors;
    UINT32 Monitors;     // connected now
    UINT32 AdapterReady; // 1 once IddCx finished setting the adapter up
} RDPIO_DISPLAY_INFO;

typedef struct _RDPIO_DISPLAY_MODE {
    UINT32 Width;        // pixels, 640..7680
    UINT32 Height;       // pixels, 480..4320
    UINT32 RefreshHz;    // 24..500
    UINT32 WidthMm;      // the client's physical screen (0: unknown)
    UINT32 HeightMm;     //   (Windows picks a default scale from it)
    UINT32 Flags;        // RDPIO_DISPLAY_FLAG_*
    LUID RenderAdapter;  // the GPU to render this monitor on (0: any)
} RDPIO_DISPLAY_MODE;

// Replace the monitors there are with this one (else: add a monitor).
#define RDPIO_DISPLAY_FLAG_REPLACE 0x1

typedef struct _RDPIO_DISPLAY_ADDED {
    UINT32 MonitorId;      // also the EDID serial number
    UINT32 ConnectorIndex;
} RDPIO_DISPLAY_ADDED;

#pragma pack(pop)

// ---------------------------------------------------------------- frames
//
// Per monitor, named in the global namespace (the driver runs in session 0,
// the agent in the user's), SYSTEM and Administrators only:
//   Global\rdpio-display-<MonitorId>                  section: RDPIO_FRAMES
//   Global\rdpio-display-<MonitorId>-ready            auto-reset event: a new
//                                                      frame or pointer change
//   Global\rdpio-display-<MonitorId>-<Generation>-<Slot>
//                                                      the textures (shared NT
//                                                      handles, keyed mutex)
// Each texture has a keyed mutex used with key 0 by both sides: the driver
// skips a slot the agent holds, and never writes the newest frame's slot.
// Both groups of fields are seqlocks: the lock is odd while the writer
// changes them; a reader retries if it was odd or changed meanwhile.

#define RDPIO_FRAMES_VERSION 1
#define RDPIO_FRAMES_SLOTS 3
#define RDPIO_FRAMES_NO_SLOT 0xFFFFFFFFu
#define RDPIO_CURSOR_MAX 256

#pragma pack(push, 8)

typedef struct _RDPIO_FRAMES {
    UINT32 Version;          // RDPIO_FRAMES_VERSION
    UINT32 Generation;       // the textures' names; 0 = none yet
    volatile LONG FrameLock;
    UINT32 Width;            // the textures
    UINT32 Height;
    UINT32 Format;           // DXGI_FORMAT
    LUID Adapter;            // the GPU they live on
    UINT32 Slot;             // the newest frame (RDPIO_FRAMES_NO_SLOT: none yet)
    // Written by the agent (GetTickCount64() / 250, never 0) while it reads
    // frames. 0 (an older agent, or one that maps the section read-only):
    // the driver hands over every frame at once, as it always did. A stale
    // value: nobody reads, so the driver takes frames off the swap-chain at
    // a few per second until the agent writes again.
    volatile UINT32 ConsumerTick;
    UINT64 FrameNumber;      // frames published so far
    UINT64 PresentQpc;       // when the newest one was presented (QPC)

    volatile LONG CursorLock;
    UINT32 HardwareCursor;   // 1: the pointer isn't in the frames; it's here
    UINT32 CursorVisible;
    UINT32 ShapeId;          // changes with the shape
    UINT32 ShapeType;        // IDDCX_CURSOR_SHAPE_TYPE: 1 masked colour, 2 alpha
    UINT32 ShapeWidth;
    UINT32 ShapeHeight;
    UINT32 ShapePitch;
    UINT32 XHot;
    UINT32 YHot;
    BYTE Shape[RDPIO_CURSOR_MAX * RDPIO_CURSOR_MAX * 4];
} RDPIO_FRAMES;

#pragma pack(pop)

// ---------------------------------------------------------------- trace
//
// A flight recorder, for when Windows ends the driver's host (IddCx's
// watchdog does that when a call into or out of the driver takes over 10 s,
// and leaves nothing behind that says which). One per driver host, named
//   Global\rdpio-display-trace                        section: RDPIO_TRACE
// The agent maps it while a monitor exists and logs it when the driver dies:
// the mapping keeps the section after the host is gone. Optional: a driver
// without it (older builds) has nothing to read. A new host reuses a
// section the agent still holds, and starts it over with Boot + 1.
//
// InCall: what each kind of driver thread is inside right now (Code 0:
// nothing). Ring: the last RDPIO_TRACE_RING events (starts, failures with
// their HRESULT/NTSTATUS in Arg), Next counting every one ever added.
// Plain memory writes, no locks: a reader may see one entry half written.

#define RDPIO_TRACE_VERSION 1
#define RDPIO_TRACE_RING 64

// InCall slots.
#define RDPIO_TRACE_SLOT_CALLBACK 0   // OS calls: PnP, power, IOCTLs, modes
#define RDPIO_TRACE_SLOT_SWAPCHAIN 1  // OS calls: assign/unassign swap-chain
#define RDPIO_TRACE_SLOT_FRAMES 2     // a monitor's swap-chain thread
#define RDPIO_TRACE_SLOT_WATCHDOG 3   // the agent-ping watchdog thread
#define RDPIO_TRACE_SLOTS 4

// Codes (crates/rdp-client/src/virtual_display.rs names them).
#define RDPIO_TRACE_HOST_START 1          // Arg: Boot
#define RDPIO_TRACE_DEVICE_ADD 2
#define RDPIO_TRACE_D0_ENTRY 3
#define RDPIO_TRACE_IOCTL_INFO 4
#define RDPIO_TRACE_IOCTL_ADD 5
#define RDPIO_TRACE_IOCTL_REMOVE 6
#define RDPIO_TRACE_IOCTL_PING 7
#define RDPIO_TRACE_ASSIGN_SWAPCHAIN 8    // Arg: monitor id
#define RDPIO_TRACE_UNASSIGN_SWAPCHAIN 9  // Arg: monitor id
#define RDPIO_TRACE_PARSE_DESCRIPTION 10
#define RDPIO_TRACE_DEFAULT_MODES 11
#define RDPIO_TRACE_TARGET_MODES 12
#define RDPIO_TRACE_COMMIT_MODES 13
#define RDPIO_TRACE_ADAPTER_INIT 14
#define RDPIO_TRACE_D3D_INIT 20           // Arg: HRESULT on failure
#define RDPIO_TRACE_SET_DEVICE 21
#define RDPIO_TRACE_ACQUIRE_BUFFER 22
#define RDPIO_TRACE_PUBLISH 23
#define RDPIO_TRACE_FINISHED_FRAME 24
#define RDPIO_TRACE_DELETE_SWAPCHAIN 25
#define RDPIO_TRACE_SETUP_CURSOR 26
#define RDPIO_TRACE_QUERY_CURSOR 27
#define RDPIO_TRACE_STOP_THREAD 28        // waiting for a swap-chain thread
#define RDPIO_TRACE_DEVICE_REMOVED 29     // Arg: GetDeviceRemovedReason
#define RDPIO_TRACE_BACKOFF 30            // Arg: ms
#define RDPIO_TRACE_SHARE_TEXTURES 31     // Arg: HRESULT on failure
#define RDPIO_TRACE_MONITOR_CREATE 40
#define RDPIO_TRACE_MONITOR_ARRIVAL 41
#define RDPIO_TRACE_MONITOR_DEPARTURE 42
#define RDPIO_TRACE_WATCHDOG_REMOVE 43
#define RDPIO_TRACE_EXCEPTION 50          // Arg: the code it happened in
#define RDPIO_TRACE_BUSY 51               // Arg: the IOCTL's code
#define RDPIO_TRACE_DETACHED 52           // a thread left running; Arg: monitor id
#define RDPIO_TRACE_NO_READER 53          // Arg: 1 throttled, 0 full rate again

#pragma pack(push, 8)

typedef struct _RDPIO_TRACE_CALL {
    volatile LONG Code;      // 0: not in a call
    UINT32 Arg;
    UINT64 StartMs;          // GetTickCount64() when it began
} RDPIO_TRACE_CALL;

typedef struct _RDPIO_TRACE_EVENT {
    UINT64 TickMs;           // GetTickCount64()
    UINT32 Code;
    UINT32 Arg;
} RDPIO_TRACE_EVENT;

typedef struct _RDPIO_TRACE {
    UINT32 Version;          // RDPIO_TRACE_VERSION
    UINT32 Boot;             // driver hosts that used this section
    volatile LONG Next;      // events added so far
    UINT32 ProcessId;        // the driver host
    RDPIO_TRACE_CALL InCall[RDPIO_TRACE_SLOTS];
    RDPIO_TRACE_EVENT Ring[RDPIO_TRACE_RING];
} RDPIO_TRACE;

#pragma pack(pop)
