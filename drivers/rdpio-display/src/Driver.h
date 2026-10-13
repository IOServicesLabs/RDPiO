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

    /// What a monitor shares with the Hyper agent (Public.h, RDPIO_FRAMES): a
    /// section, an event, and a ring of GPU textures the agent opens by name.
    /// Frames never leave the GPU. Used by one thread at a time (the
    /// monitor's swap-chain thread).
    class FrameShare
    {
    public:
        explicit FrameShare(UINT32 MonitorId);
        ~FrameShare();

        /// Copy `Surface` into a free texture and tell the agent.
        void Publish(ID3D11Device* Device, ID3D11DeviceContext* Context, ID3D11Texture2D* Surface, UINT64 PresentQpc, LUID Adapter);
        /// Let the textures go (a new swap-chain may bring another device).
        void Reset();
        void SetHardwareCursor(bool On);
        /// `Shape` (with `Data`) only when the shape changed.
        void PublishCursor(bool Visible, const IDDCX_CURSOR_SHAPE_INFO* Shape, const BYTE* Data);

    private:
        bool MakeTextures(ID3D11Device* Device, const D3D11_TEXTURE2D_DESC& Like, LUID Adapter);
        void ReleaseTextures();

        struct Slot
        {
            Microsoft::WRL::ComPtr<ID3D11Texture2D> Texture;
            Microsoft::WRL::ComPtr<IDXGIKeyedMutex> Mutex;
            HANDLE Shared = nullptr; // keeps the name alive
        };

        UINT32 m_Id;
        HANDLE m_Section = nullptr;
        RDPIO_FRAMES* m_View = nullptr;
        HANDLE m_Ready = nullptr;
        Slot m_Slots[RDPIO_FRAMES_SLOTS];
        ID3D11Device* m_TexturesDevice = nullptr; // (compared only)
        D3D11_TEXTURE2D_DESC m_Desc = {};
        UINT32 m_Next = 0;
        // Starts at the time (a monitor id can come again after a restart,
        // with the old textures maybe still open somewhere).
        UINT32 m_Generation = (UINT32)(GetTickCount64() / 1000) << 8;
        bool m_CursorVisible = false;
    };

    /// Takes the frames Windows composes for a monitor off its swap-chain and
    /// shares them with the agent; with the pointer, which the OS leaves out
    /// of the frames once the hardware cursor is set up.
    class SwapChainProcessor
    {
    public:
        SwapChainProcessor(IDDCX_SWAPCHAIN hSwapChain, IDDCX_MONITOR Monitor, std::shared_ptr<Direct3DDevice> Device,
            std::shared_ptr<FrameShare> Share, LUID RenderAdapter, HANDLE NewFrameEvent);
        ~SwapChainProcessor();

    private:
        static DWORD CALLBACK RunThread(LPVOID Argument);
        void Run();
        void RunCore();
        void SetupCursor();
        void QueryCursor();

        IDDCX_SWAPCHAIN m_hSwapChain;
        IDDCX_MONITOR m_Monitor;
        std::shared_ptr<Direct3DDevice> m_Device;
        std::shared_ptr<FrameShare> m_Share;
        LUID m_RenderAdapter;
        HANDLE m_hAvailableBufferEvent;
        Microsoft::WRL::Wrappers::Thread m_hThread;
        Microsoft::WRL::Wrappers::Event m_hTerminateEvent;
        Microsoft::WRL::Wrappers::Event m_hCursorEvent;
        bool m_HardwareCursor = false;
        DWORD m_LastShapeId = 0;
        std::vector<BYTE> m_CursorShape;
    };

    class MonitorContext
    {
    public:
        MonitorContext(IDDCX_MONITOR Monitor, UINT32 Id, std::vector<Mode> Modes)
            : m_Monitor(Monitor), m_Id(Id), m_Modes(std::move(Modes)), m_Share(std::make_shared<FrameShare>(Id)) {}
        ~MonitorContext();

        void AssignSwapChain(IDDCX_SWAPCHAIN SwapChain, LUID RenderAdapter, HANDLE NewFrameEvent);
        void UnassignSwapChain();

        IDDCX_MONITOR Monitor() const { return m_Monitor; }
        UINT32 Id() const { return m_Id; }
        const std::vector<Mode>& Modes() const { return m_Modes; }

    private:
        IDDCX_MONITOR m_Monitor;
        UINT32 m_Id;
        std::vector<Mode> m_Modes;
        std::shared_ptr<FrameShare> m_Share;
        std::unique_ptr<SwapChainProcessor> m_ProcessingThread;
    };

    class DeviceContext
    {
    public:
        explicit DeviceContext(WDFDEVICE WdfDevice);
        ~DeviceContext();

        void InitAdapter();
        void AdapterReady(bool Ready);

        NTSTATUS Add(const RDPIO_DISPLAY_MODE& Wanted, RDPIO_DISPLAY_ADDED* Added);
        void RemoveAll();
        void Ping();
        RDPIO_DISPLAY_INFO Info();

        /// The modes of the monitor with this EDID serial (empty if none).
        std::vector<Mode> ModesForSerial(UINT32 Serial);
        /// The modes of this monitor.
        std::vector<Mode> ModesForMonitor(IDDCX_MONITOR Monitor);

    private:
        static DWORD CALLBACK WatchdogThread(LPVOID Argument);
        void Watchdog();
        void RemoveAllLocked();

        WDFDEVICE m_WdfDevice;
        IDDCX_ADAPTER m_Adapter = nullptr;
        std::atomic<bool> m_Ready{false};

        // m_Lock serializes adding and removing (held across IddCx calls);
        // m_TableLock only guards m_Monitors — the OS parses a new monitor's
        // EDID (ModesForSerial) while IddCxMonitorArrival is still running.
        std::mutex m_Lock;
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
