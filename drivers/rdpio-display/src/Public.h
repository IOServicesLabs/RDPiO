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
    UINT32 Reserved;
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
