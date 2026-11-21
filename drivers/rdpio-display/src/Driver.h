// rdpio Display — an indirect display driver (IddCx, UMDF 2) whose monitors
// exist only while an rdpio Hyper stream wants one, at exactly the client's
// resolution and refresh rate, and hand their frames and pointer straight to
// the Hyper agent (Public.h, RDPIO_FRAMES).
//
// Built on the structure of Microsoft's IddSampleDriver
// (github.com/microsoft/Windows-driver-samples, MIT License), the same base
// the open-source Virtual Display Driver grew from.

#pragma once

#define NOMINMAX
#include <windows.h>
#include <bugcodes.h>
#include <wudfwdm.h>
#include <wdf.h>
#include <iddcx.h>

#include <dxgi1_5.h>
#include <d3d11_2.h>
#include <avrt.h>
#include <wrl.h>

#include <atomic>
#include <memory>
#include <mutex>
#include <vector>

#include "Public.h"

namespace Microsoft::WRL::Wrappers
{
    typedef HandleT<HandleTraits::HANDLENullTraits> Thread;
}

namespace rdpio
{
    struct Mode
    {
        DWORD Width;
        DWORD Height;
        DWORD Hz;
    };

    /// What a monitor offers: the client's mode first (preferred), then
    /// common sizes no larger, so Display settings still has choices.
    std::vector<Mode> ModesFor(const RDPIO_DISPLAY_MODE& Wanted);

    /// A 128-byte EDID 1.4 naming the monitor "rdpio Display", with `Serial`
    /// (how ParseMonitorDescription finds the monitor's modes again) and the
    /// client's physical size.
    void BuildEdid(BYTE (&Edid)[128], UINT32 Serial, const RDPIO_DISPLAY_MODE& Wanted);

    /// A D3D11 device on the render adapter IddCx hands a swap-chain to.
    struct Direct3DDevice
    {
        explicit Direct3DDevice(LUID AdapterLuid) : AdapterLuid(AdapterLuid) {}
        HRESULT Init();

        LUID AdapterLuid;
        Microsoft::WRL::ComPtr<IDXGIFactory5> DxgiFactory;
        Microsoft::WRL::ComPtr<IDXGIAdapter1> Adapter;
        Microsoft::WRL::ComPtr<ID3D11Device> Device;
        Microsoft::WRL::ComPtr<ID3D11DeviceContext> DeviceContext;
    };

    /// The flight recorder (Public.h, RDPIO_TRACE): made once per driver
    /// host. Everything below works (and does nothing) without it.
    void TraceOpen() noexcept;
    /// Add an event to the ring.
    void TraceEvent(UINT32 Code, UINT32 Arg) noexcept;
    /// An event only when `hr` is a failure.
    inline void TraceFailed(UINT32 Code, HRESULT hr) noexcept
    {
        if (FAILED(hr))
            TraceEvent(Code, (UINT32)hr);
    }

    /// Marks a thread as inside call `Code` (in its InCall slot) for as long
    /// as it lives; what the slot said before comes back afterwards, so calls
    /// nest.
    class TraceCall
    {
    public:
        TraceCall(UINT32 Slot, UINT32 Code, UINT32 Arg = 0) noexcept;
        ~TraceCall();
        TraceCall(const TraceCall&) = delete;
        TraceCall& operator=(const TraceCall&) = delete;

    private:
        UINT32 m_Slot;
        LONG m_Code;
        LONG m_SavedCode = 0;
        UINT32 m_SavedArg = 0;
        UINT64 m_SavedStart = 0;
    };

    /// The back-off before a swap-chain is given up after its D3D device
    /// failed `Failures` times in a row (1: the first): none the first time,
    /// so a mode change is as quick as ever, then growing to a second.
    DWORD DeviceBackoffMs(UINT32 Failures) noexcept;

    /// Whether an agent's ConsumerTick (Public.h) says it stopped reading:
    /// non-zero and more than 2 s older than `NowTick` (both in 250 ms units,
    /// wrapping).
    bool ConsumerGone(UINT32 ConsumerTick, UINT32 NowTick) noexcept;

    /// What a monitor shares with the Hyper agent (Public.h, RDPIO_FRAMES):
    /// the section and its event. The textures belong to the swap-chain
    /// thread that fills them (FrameRing). Each swap-chain thread writes the
    /// section under its own epoch: once a newer thread has taken over
    /// (Reset), an older one that was left to finish writes nothing.
    class FrameShare
    {
    public:
        explicit FrameShare(UINT32 MonitorId);
        ~FrameShare();
        FrameShare(const FrameShare&) = delete;
        FrameShare& operator=(const FrameShare&) = delete;

        UINT32 Id() const { return m_Id; }
        /// Whether the section exists (else the agent duplicates the output).
        bool Shares() const { return m_View != nullptr; }
        /// The frames so far are forgotten and older writers shut out.
        /// Returns the epoch of the next writer.
        UINT32 Reset();
        /// A new name for a set of textures, never used before.
        UINT32 NextGeneration() { return ++m_Generation; }
        /// New textures (`Generation`), with no frame in them yet.
        bool Announce(UINT32 Epoch, UINT32 Generation, const D3D11_TEXTURE2D_DESC& Desc, LUID Adapter);
        /// The slot of the newest frame (the agent may be about to read it).
        UINT32 NewestSlot() const;
        /// `Slot` holds the newest frame: tell the agent.
        bool Publish(UINT32 Epoch, UINT32 Slot, UINT64 PresentQpc);
        void SetHardwareCursor(UINT32 Epoch, bool On);
        /// `Shape` (with `Data`) only when the shape changed.
        void PublishCursor(UINT32 Epoch, bool Visible, const IDDCX_CURSOR_SHAPE_INFO* Shape, const BYTE* Data);
        /// An agent that says it reads frames (ConsumerTick) stopped.
        bool NoReader() const;

        /// The monitor's D3D device failed again; the back-off to wait.
        DWORD NoteDeviceFailure() { return DeviceBackoffMs(++m_DeviceFailures); }
        /// A frame went through: the device is fine.
        void NoteFrame() { m_DeviceFailures = 0; }

    private:
        bool BeginWrite(UINT32 Epoch);
        void EndWrite() { m_Writing.store(0, std::memory_order_release); }

        UINT32 m_Id;
        HANDLE m_Section = nullptr;
        RDPIO_FRAMES* m_View = nullptr;
        HANDLE m_Ready = nullptr;
        std::atomic<UINT32> m_Epoch{0};
        // One writer of the section at a time (memory writes only, never
        // held across a call): a thread left to finish may still be in one.
        std::atomic<LONG> m_Writing{0};
        // Starts at the time (a monitor id can come again after a restart,
        // with the old textures maybe still open somewhere).
        std::atomic<UINT32> m_Generation{(UINT32)(GetTickCount64() / 1000) << 8};
        std::atomic<UINT32> m_DeviceFailures{0};
        bool m_CursorVisible = false; // (written under BeginWrite)
    };

    /// A swap-chain thread's ring of GPU textures the agent opens by name.
    /// Frames never leave the GPU. Made, used and let go on that thread only.
    class FrameRing
    {
    public:
        FrameRing(std::shared_ptr<FrameShare> Share, UINT32 Epoch) : m_Share(std::move(Share)), m_Epoch(Epoch) {}
        ~FrameRing() { Release(); }
        FrameRing(const FrameRing&) = delete;
        FrameRing& operator=(const FrameRing&) = delete;

        /// Copy `Surface` into a free texture and tell the agent. True when
        /// the agent was told.
        bool Publish(ID3D11Device* Device, ID3D11DeviceContext* Context, ID3D11Texture2D* Surface, UINT64 PresentQpc, LUID Adapter);

    private:
        bool MakeTextures(ID3D11Device* Device, const D3D11_TEXTURE2D_DESC& Like, LUID Adapter);
        void Release();

        struct Slot
        {
            Microsoft::WRL::ComPtr<ID3D11Texture2D> Texture;
            Microsoft::WRL::ComPtr<IDXGIKeyedMutex> Mutex;
            HANDLE Shared = nullptr; // keeps the name alive
        };

        std::shared_ptr<FrameShare> m_Share;
        UINT32 m_Epoch;
        Slot m_Slots[RDPIO_FRAMES_SLOTS];
        ID3D11Device* m_TexturesDevice = nullptr; // (compared only)
        D3D11_TEXTURE2D_DESC m_Desc = {};
        UINT32 m_Next = 0;
    };

    /// Takes the frames Windows composes for a monitor off its swap-chain and
    /// shares them with the agent; with the pointer, which the OS leaves out
    /// of the frames once the hardware cursor is set up.
    ///
    /// Its thread owns a reference to it, so it can outlive its monitor's
    /// interest: an OS callback stops it with a bounded wait (Stop), and a
    /// thread stuck in a call to Windows (a GPU stall) is left to finish by
    /// itself. Such a thread makes no further call to Windows, writes nothing
    /// shared, and leaves its swap-chain to IddCx.
    class SwapChainProcessor
    {
    public:
        /// Start the thread for `hSwapChain`. Null when it couldn't start;
        /// the swap-chain is deleted then, and the OS makes a new one.
        static std::shared_ptr<SwapChainProcessor> Start(IDDCX_SWAPCHAIN hSwapChain, IDDCX_MONITOR Monitor,
            std::shared_ptr<FrameShare> Share, UINT32 Epoch, LUID RenderAdapter, HANDLE NewFrameEvent);
        /// Never waits.
        ~SwapChainProcessor();
        SwapChainProcessor(const SwapChainProcessor&) = delete;
        SwapChainProcessor& operator=(const SwapChainProcessor&) = delete;

        /// Ask the thread to end and wait up to `Ms` for it. False: it's
        /// still in a call to Windows and was left to end by itself.
        bool Stop(DWORD Ms);

    private:
        SwapChainProcessor(IDDCX_SWAPCHAIN hSwapChain, IDDCX_MONITOR Monitor, std::shared_ptr<FrameShare> Share, UINT32 Epoch,
            LUID RenderAdapter, HANDLE NewFrameEvent);
        static DWORD CALLBACK RunThread(LPVOID Argument);
        void Run();
        /// True when the D3D device failed (not the swap-chain).
        bool RunCore(Direct3DDevice& Device);
        void SetupCursor();
        void QueryCursor();
        bool Stopping() const { return m_State.load() != Running; }

        enum : LONG
        {
            Running,
            StopAsked,
            Detached, // Stop gave up waiting: the thread leaves the swap-chain alone
            Ended,    // the thread ended by itself or as asked
        };

        IDDCX_SWAPCHAIN m_hSwapChain;
        IDDCX_MONITOR m_Monitor;
        std::shared_ptr<FrameShare> m_Share;
        UINT32 m_Epoch;
        LUID m_RenderAdapter;
        HANDLE m_hAvailableBufferEvent;
        std::atomic<LONG> m_State{Running};
        Microsoft::WRL::Wrappers::Thread m_hThread;
        Microsoft::WRL::Wrappers::Event m_hTerminateEvent; // manual reset
        Microsoft::WRL::Wrappers::Event m_hCursorEvent;
        bool m_HardwareCursor = false;
        DWORD m_LastShapeId = 0;
        NTSTATUS m_CursorFailure = 0; // the last one traced
        std::vector<BYTE> m_CursorShape;
    };

    class MonitorContext
    {
    public:
        MonitorContext(IDDCX_MONITOR Monitor, UINT32 Id, std::vector<Mode> Modes)
            : m_Monitor(Monitor), m_Id(Id), m_Modes(std::move(Modes)), m_Share(std::make_shared<FrameShare>(Id)) {}
        /// Stops the swap-chain thread (bounded).
        ~MonitorContext();

        void AssignSwapChain(IDDCX_SWAPCHAIN SwapChain, LUID RenderAdapter, HANDLE NewFrameEvent);
        void UnassignSwapChain();

        IDDCX_MONITOR Monitor() const { return m_Monitor; }
        UINT32 Id() const { return m_Id; }
        const std::vector<Mode>& Modes() const { return m_Modes; }

    private:
        void StopProcessor() noexcept;

        IDDCX_MONITOR m_Monitor;
        UINT32 m_Id;
        std::vector<Mode> m_Modes;
        std::shared_ptr<FrameShare> m_Share;
        std::shared_ptr<SwapChainProcessor> m_Processor;
    };

    class DeviceContext
    {
    public:
        explicit DeviceContext(WDFDEVICE WdfDevice);
        /// Call only once StopWatchdog said the thread ended.
        ~DeviceContext() = default;

        /// Stop the watchdog thread, waiting up to `Ms`. False: it's still in
        /// a call to Windows (this context must then never be freed).
        bool StopWatchdog(DWORD Ms);

        void InitAdapter();
        void AdapterReady(bool Ready);

        /// STATUS_DEVICE_BUSY when another addition or removal holds things
        /// up for too long.
        NTSTATUS Add(const RDPIO_DISPLAY_MODE& Wanted, RDPIO_DISPLAY_ADDED* Added);
        NTSTATUS RemoveAll();
        void Ping();
        RDPIO_DISPLAY_INFO Info();

        /// The modes of the monitor with this EDID serial (empty if none).
        std::vector<Mode> ModesForSerial(UINT32 Serial);
        /// The modes of this monitor.
        std::vector<Mode> ModesForMonitor(IDDCX_MONITOR Monitor);

    private:
        static DWORD CALLBACK WatchdogThread(LPVOID Argument);
        void Watchdog();
        /// (`TraceSlot`: the calling thread's, for the flight recorder.)
        void RemoveAllLocked(UINT32 TraceSlot);

        WDFDEVICE m_WdfDevice;
        IDDCX_ADAPTER m_Adapter = nullptr;
        std::atomic<bool> m_Ready{false};

        // m_Lock serializes adding and removing (held across IddCx calls);
        // the agent's requests wait for it a bounded time, the watchdog not
        // at all. m_TableLock only guards m_Monitors — the OS parses a new
        // monitor's EDID (ModesForSerial) while IddCxMonitorArrival is still
        // running.
        std::timed_mutex m_Lock;
        std::mutex m_TableLock;
        struct Entry
        {
            IDDCX_MONITOR Monitor;
            UINT32 Id;
            UINT32 Connector;
            std::vector<Mode> Modes;
        };
        std::vector<Entry> m_Monitors;
        UINT32 m_NextId = 1;
        std::atomic<ULONGLONG> m_LastPing{0};

        Microsoft::WRL::Wrappers::Thread m_hWatchdog;
        Microsoft::WRL::Wrappers::Event m_hStop;
    };
}
