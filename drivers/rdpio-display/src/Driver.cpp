// rdpio Display — see Driver.h. Monitors are added and removed by the Hyper
// agent (Public.h); with none asked for, the adapter shows nothing at all.

#include "Driver.h"

// (Declared in Public.h, defined here.)
EXTERN_C const GUID GUID_DEVINTERFACE_RDPIO_DISPLAY = { 0x6d3b1a7e, 0x2c41, 0x4f0e, { 0x9a, 0x51, 0x3e, 0x8b, 0x7c, 0x21, 0x5d, 0x94 } };

#include <algorithm>
#include <chrono>
#include <cstring>
#include <cwchar>
#include <sddl.h>

using namespace rdpio;
using namespace Microsoft::WRL;

#pragma region helpers

static void Log(const wchar_t* Format, ...)
{
    wchar_t Line[512];
    va_list Args;
    va_start(Args, Format);
    _vsnwprintf_s(Line, _TRUNCATE, Format, Args);
    va_end(Args);
    OutputDebugStringW(L"rdpio-display: ");
    OutputDebugStringW(Line);
    OutputDebugStringW(L"\n");
}

// SYSTEM, Administrators, and us (the driver host's account).
static const wchar_t* kShareSddl = L"D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)";

struct ShareSecurity
{
    SECURITY_ATTRIBUTES Sa = { sizeof(SECURITY_ATTRIBUTES), nullptr, FALSE };
    ShareSecurity()
    {
        if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(kShareSddl, SDDL_REVISION_1, &Sa.lpSecurityDescriptor, nullptr))
            Sa.lpSecurityDescriptor = nullptr;
    }
    ~ShareSecurity()
    {
        if (Sa.lpSecurityDescriptor)
            LocalFree(Sa.lpSecurityDescriptor);
    }
    SECURITY_ATTRIBUTES* Get() { return Sa.lpSecurityDescriptor ? &Sa : nullptr; }
};

// The flight recorder. Set once, before any callback, and never unmapped:
// it lives as long as the driver host.
static RDPIO_TRACE* g_Trace = nullptr;

void rdpio::TraceOpen() noexcept
{
    if (g_Trace)
        return;
    ShareSecurity Security;
    if (!Security.Get())
        return;
    HANDLE Section = CreateFileMappingW(INVALID_HANDLE_VALUE, Security.Get(), PAGE_READWRITE, 0, sizeof(RDPIO_TRACE),
        L"Global\\rdpio-display-trace");
    if (!Section)
    {
        Log(L"no trace section (%u)", GetLastError());
        return;
    }
    auto* View = (RDPIO_TRACE*)MapViewOfFile(Section, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, sizeof(RDPIO_TRACE));
    if (!View)
    {
        Log(L"no trace view (%u)", GetLastError());
        CloseHandle(Section);
        return;
    }
    // (The section handle stays open with the view, for the host's life.)
    // An agent may still hold the last host's section: start it over.
    const UINT32 Boot = (View->Version == RDPIO_TRACE_VERSION) ? View->Boot + 1 : 1;
    memset(View, 0, sizeof(RDPIO_TRACE));
    View->Boot = Boot;
    View->ProcessId = GetCurrentProcessId();
    MemoryBarrier();
    View->Version = RDPIO_TRACE_VERSION;
    g_Trace = View;
    TraceEvent(RDPIO_TRACE_HOST_START, Boot);
}

void rdpio::TraceEvent(UINT32 Code, UINT32 Arg) noexcept
{
    if (!g_Trace)
        return;
    const LONG n = InterlockedIncrement(&g_Trace->Next) - 1;
    RDPIO_TRACE_EVENT& E = g_Trace->Ring[(ULONG)n % RDPIO_TRACE_RING];
    E.TickMs = GetTickCount64();
    E.Arg = Arg;
    E.Code = Code;
}

TraceCall::TraceCall(UINT32 Slot, UINT32 Code, UINT32 Arg) noexcept : m_Slot(Slot), m_Code((LONG)Code)
{
    if (!g_Trace || Slot >= RDPIO_TRACE_SLOTS)
        return;
    RDPIO_TRACE_CALL& C = g_Trace->InCall[Slot];
    m_SavedCode = C.Code;
    m_SavedArg = C.Arg;
    m_SavedStart = C.StartMs;
    C.StartMs = GetTickCount64();
    C.Arg = Arg;
    InterlockedExchange(&C.Code, m_Code);
}

TraceCall::~TraceCall()
{
    if (!g_Trace || m_Slot >= RDPIO_TRACE_SLOTS)
        return;
    RDPIO_TRACE_CALL& C = g_Trace->InCall[m_Slot];
    // (Another thread of the same kind may have taken the slot meanwhile:
    // then it's theirs.)
    if (C.Code != m_Code)
        return;
    C.StartMs = m_SavedStart;
    C.Arg = m_SavedArg;
    InterlockedExchange(&C.Code, m_SavedCode);
}

DWORD rdpio::DeviceBackoffMs(UINT32 Failures) noexcept
{
    static const DWORD Steps[] = { 0, 100, 250, 500, 1000 };
    if (Failures == 0)
        return 0;
    return Steps[std::min<UINT32>(Failures - 1, (UINT32)ARRAYSIZE(Steps) - 1)];
}

bool rdpio::ConsumerGone(UINT32 ConsumerTick, UINT32 NowTick) noexcept
{
    // 8 ticks of 250 ms: the agent writes about every 250 ms while it reads,
    // and looks at least every 100 ms even on a still screen. (A tick a
    // little ahead of ours, written just after we looked, is fresh.)
    return ConsumerTick != 0 && (INT32)(NowTick - ConsumerTick) > 8;
}

// Each callback runs inside this: a C++ exception (an allocation failing)
// must not leave it, which would end the driver host.
template <typename F>
static NTSTATUS Guarded(UINT32 Code, F&& Body) noexcept
{
    try
    {
        return Body();
    }
    catch (...)
    {
        TraceEvent(RDPIO_TRACE_EXCEPTION, Code);
        Log(L"exception in call %u", Code);
        return STATUS_INSUFFICIENT_RESOURCES;
    }
}

static void FillSignalInfo(DISPLAYCONFIG_VIDEO_SIGNAL_INFO& Mode, DWORD Width, DWORD Height, DWORD VSync, bool bMonitorMode)
{
    Mode.totalSize.cx = Mode.activeSize.cx = Width;
    Mode.totalSize.cy = Mode.activeSize.cy = Height;
    Mode.AdditionalSignalInfo.vSyncFreqDivider = bMonitorMode ? 0 : 1;
    Mode.AdditionalSignalInfo.videoStandard = 255;
    Mode.vSyncFreq.Numerator = VSync;
    Mode.vSyncFreq.Denominator = 1;
    Mode.hSyncFreq.Numerator = VSync * Height;
    Mode.hSyncFreq.Denominator = 1;
    Mode.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
    Mode.pixelRate = ((UINT64)VSync) * ((UINT64)Width) * ((UINT64)Height);
}

static IDDCX_MONITOR_MODE MonitorMode(const Mode& M, IDDCX_MONITOR_MODE_ORIGIN Origin)
{
    IDDCX_MONITOR_MODE Out = {};
    Out.Size = sizeof(Out);
    Out.Origin = Origin;
    FillSignalInfo(Out.MonitorVideoSignalInfo, M.Width, M.Height, M.Hz, true);
    return Out;
}

static IDDCX_TARGET_MODE TargetMode(const Mode& M)
{
    IDDCX_TARGET_MODE Out = {};
    Out.Size = sizeof(Out);
    FillSignalInfo(Out.TargetVideoSignalInfo.targetVideoSignalInfo, M.Width, M.Height, M.Hz, false);
    return Out;
}

std::vector<Mode> rdpio::ModesFor(const RDPIO_DISPLAY_MODE& Wanted)
{
    std::vector<Mode> Modes;
    auto Add = [&](DWORD W, DWORD H, DWORD Hz) {
        for (const auto& M : Modes)
        {
            if (M.Width == W && M.Height == H && M.Hz == Hz)
                return;
        }
        Modes.push_back({ W, H, Hz });
    };
    Add(Wanted.Width, Wanted.Height, Wanted.RefreshHz);
    if (Wanted.RefreshHz != 60)
        Add(Wanted.Width, Wanted.Height, 60);
    static const Mode Common[] = {
        { 3840, 2160, 60 }, { 3440, 1440, 60 }, { 2560, 1600, 60 }, { 2560, 1440, 60 }, { 2256, 1504, 60 },
        { 1920, 1200, 60 }, { 1920, 1080, 60 }, { 1680, 1050, 60 }, { 1600, 900, 60 }, { 1440, 900, 60 },
        { 1366, 768, 60 }, { 1280, 800, 60 }, { 1280, 720, 60 },
    };
    for (const auto& M : Common)
    {
        if (M.Width <= Wanted.Width && M.Height <= Wanted.Height)
            Add(M.Width, M.Height, M.Hz);
    }
    return Modes;
}

void rdpio::BuildEdid(BYTE (&E)[128], UINT32 Serial, const RDPIO_DISPLAY_MODE& Wanted)
{
    memset(E, 0, sizeof(E));
    static const BYTE Header[8] = { 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00 };
    memcpy(E, Header, 8);
    // Manufacturer "RDO" (5-bit letters, A = 1), big-endian.
    const UINT16 Mfr = (('R' - '@') << 10) | (('D' - '@') << 5) | ('O' - '@');
    E[8] = (BYTE)(Mfr >> 8);
    E[9] = (BYTE)(Mfr & 0xFF);
    E[10] = 0x01; // product code 0x0001
    E[11] = 0x00;
    E[12] = (BYTE)Serial;
    E[13] = (BYTE)(Serial >> 8);
    E[14] = (BYTE)(Serial >> 16);
    E[15] = (BYTE)(Serial >> 24);
    E[16] = 1;    // week
    E[17] = 36;   // 2026
    E[18] = 1;    // EDID 1.4
    E[19] = 4;
    E[20] = 0xA5; // digital, 8 bits per colour, DisplayPort
    // Physical size (cm): Windows' default scale for the monitor comes from it.
    E[21] = (BYTE)std::min<UINT32>(255, (Wanted.WidthMm + 5) / 10);
    E[22] = (BYTE)std::min<UINT32>(255, (Wanted.HeightMm + 5) / 10);
    E[23] = 120;  // gamma 2.2
    E[24] = 0x06; // sRGB default, preferred timing is the first descriptor
    static const BYTE Chroma[10] = { 0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54 };
    memcpy(E + 25, Chroma, 10);
    // No established timings; standard timings unused.
    for (int i = 38; i < 54; i++)
        E[i] = 0x01;

    // Descriptor 1: the preferred timing. Only a formality — this driver
    // reports its modes itself — but EDID 1.4 wants one. The client's mode if
    // it fits a detailed timing (pixel clock up to 655.35 MHz), else 1080p60.
    DWORD W = Wanted.Width, H = Wanted.Height, Hz = Wanted.RefreshHz;
    const DWORD HBlank = 160, VBlank = std::max<DWORD>(8, H / 20);
    UINT64 Clock = (UINT64)(W + HBlank) * (H + VBlank) * Hz / 10000;
    if (W > 4095 || H > 4095 || Clock > 65535)
    {
        W = 1920, H = 1080, Hz = 60;
        Clock = 14850;
    }
    const DWORD VB = (W == 1920 && H == 1080 && Hz == 60 && Clock == 14850) ? 45 : VBlank;
    const DWORD HB = (VB == 45) ? 280 : HBlank;
    BYTE* D = E + 54;
    D[0] = (BYTE)Clock;
    D[1] = (BYTE)(Clock >> 8);
    D[2] = (BYTE)W;
    D[3] = (BYTE)HB;
    D[4] = (BYTE)(((W >> 8) & 0xF) << 4 | ((HB >> 8) & 0xF));
    D[5] = (BYTE)H;
    D[6] = (BYTE)VB;
    D[7] = (BYTE)(((H >> 8) & 0xF) << 4 | ((VB >> 8) & 0xF));
    D[8] = 48;   // h sync offset
    D[9] = 32;   // h sync width
    D[10] = 0x35; // v sync offset 3, width 5
    D[11] = 0;
    const UINT32 Wmm = std::min<UINT32>(Wanted.WidthMm, 4095), Hmm = std::min<UINT32>(Wanted.HeightMm, 4095);
    D[12] = (BYTE)Wmm;
    D[13] = (BYTE)Hmm;
    D[14] = (BYTE)(((Wmm >> 8) & 0xF) << 4 | ((Hmm >> 8) & 0xF));
    D[17] = 0x1E; // digital separate sync, +h +v

    // Descriptor 2: the name.
    BYTE* N = E + 72;
    N[3] = 0xFC;
    memcpy(N + 5, "rdpio Display", 13);
    // Descriptor 3: range limits (24-255 Hz).
    BYTE* R = E + 90;
    R[3] = 0xFD;
    R[5] = 24;
    R[6] = 255;
    R[7] = 1;
    R[8] = 255;
    R[9] = 255;
    R[10] = 0x01;
    R[11] = 0x0A;
    for (int i = 12; i < 18; i++)
        R[i] = 0x20;
    // Descriptor 4: unused.
    E[108 + 3] = 0x10;

    BYTE Sum = 0;
    for (int i = 0; i < 127; i++)
        Sum += E[i];
    E[127] = (BYTE)(0x100 - Sum);
}

static UINT32 EdidSerial(const BYTE* Edid, UINT DataSize)
{
    if (!Edid || DataSize < 128 || Edid[8] != 0x48 || Edid[9] != 0x8F)
        return 0;
    return (UINT32)Edid[12] | ((UINT32)Edid[13] << 8) | ((UINT32)Edid[14] << 16) | ((UINT32)Edid[15] << 24);
}

#pragma endregion

#pragma region WDF glue

struct DeviceContextWrapper
{
    DeviceContext* pContext;
    void Cleanup()
    {
        // Bounded: a watchdog thread stuck in a call to Windows keeps its
        // context, which is then never freed (the host is going anyway).
        if (pContext && pContext->StopWatchdog(2000))
            delete pContext;
        else if (pContext)
            Log(L"the watchdog thread didn't end; its context is left behind");
        pContext = nullptr;
    }
};

struct MonitorContextWrapper
{
    MonitorContext* pContext;
    void Cleanup()
    {
        delete pContext;
        pContext = nullptr;
    }
};

WDF_DECLARE_CONTEXT_TYPE(DeviceContextWrapper);
WDF_DECLARE_CONTEXT_TYPE(MonitorContextWrapper);

extern "C" DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_DEVICE_ADD RdpioDeviceAdd;
EVT_WDF_DEVICE_D0_ENTRY RdpioDeviceD0Entry;
EVT_IDD_CX_DEVICE_IO_CONTROL RdpioIoDeviceControl;
EVT_IDD_CX_ADAPTER_INIT_FINISHED RdpioAdapterInitFinished;
EVT_IDD_CX_ADAPTER_COMMIT_MODES RdpioAdapterCommitModes;
EVT_IDD_CX_PARSE_MONITOR_DESCRIPTION RdpioParseMonitorDescription;
EVT_IDD_CX_MONITOR_GET_DEFAULT_DESCRIPTION_MODES RdpioMonitorGetDefaultModes;
EVT_IDD_CX_MONITOR_QUERY_TARGET_MODES RdpioMonitorQueryModes;
EVT_IDD_CX_MONITOR_ASSIGN_SWAPCHAIN RdpioMonitorAssignSwapChain;
EVT_IDD_CX_MONITOR_UNASSIGN_SWAPCHAIN RdpioMonitorUnassignSwapChain;

extern "C" BOOL WINAPI DllMain(HINSTANCE, UINT, LPVOID)
{
    return TRUE;
}

static DeviceContext* g_Device = nullptr; // (one adapter; ParseMonitorDescription has no handle)

_Use_decl_annotations_
extern "C" NTSTATUS DriverEntry(PDRIVER_OBJECT pDriverObject, PUNICODE_STRING pRegistryPath)
{
    WDF_DRIVER_CONFIG Config;
    WDF_OBJECT_ATTRIBUTES Attributes;
    WDF_OBJECT_ATTRIBUTES_INIT(&Attributes);
    WDF_DRIVER_CONFIG_INIT(&Config, RdpioDeviceAdd);
    TraceOpen();
    return WdfDriverCreate(pDriverObject, pRegistryPath, &Attributes, &Config, WDF_NO_HANDLE);
}

static NTSTATUS DeviceAdd(PWDFDEVICE_INIT pDeviceInit);

_Use_decl_annotations_
NTSTATUS RdpioDeviceAdd(WDFDRIVER Driver, PWDFDEVICE_INIT pDeviceInit)
{
    UNREFERENCED_PARAMETER(Driver);
    TraceCall Call(RDPIO_TRACE_SLOT_CALLBACK, RDPIO_TRACE_DEVICE_ADD);
    const NTSTATUS Status = Guarded(RDPIO_TRACE_DEVICE_ADD, [&] { return DeviceAdd(pDeviceInit); });
    if (!NT_SUCCESS(Status))
        TraceEvent(RDPIO_TRACE_DEVICE_ADD, (UINT32)Status);
    return Status;
}

static NTSTATUS DeviceAdd(PWDFDEVICE_INIT pDeviceInit)
{
    WDF_PNPPOWER_EVENT_CALLBACKS PnpPowerCallbacks;
    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&PnpPowerCallbacks);
    PnpPowerCallbacks.EvtDeviceD0Entry = RdpioDeviceD0Entry;
    WdfDeviceInitSetPnpPowerEventCallbacks(pDeviceInit, &PnpPowerCallbacks);

    IDD_CX_CLIENT_CONFIG IddConfig;
    IDD_CX_CLIENT_CONFIG_INIT(&IddConfig);
    IddConfig.EvtIddCxDeviceIoControl = RdpioIoDeviceControl;
    IddConfig.EvtIddCxAdapterInitFinished = RdpioAdapterInitFinished;
    IddConfig.EvtIddCxParseMonitorDescription = RdpioParseMonitorDescription;
    IddConfig.EvtIddCxMonitorGetDefaultDescriptionModes = RdpioMonitorGetDefaultModes;
    IddConfig.EvtIddCxMonitorQueryTargetModes = RdpioMonitorQueryModes;
    IddConfig.EvtIddCxAdapterCommitModes = RdpioAdapterCommitModes;
    IddConfig.EvtIddCxMonitorAssignSwapChain = RdpioMonitorAssignSwapChain;
    IddConfig.EvtIddCxMonitorUnassignSwapChain = RdpioMonitorUnassignSwapChain;

    NTSTATUS Status = IddCxDeviceInitConfig(pDeviceInit, &IddConfig);
    if (!NT_SUCCESS(Status))
        return Status;

    WDF_OBJECT_ATTRIBUTES Attr;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&Attr, DeviceContextWrapper);
    Attr.EvtCleanupCallback = [](WDFOBJECT Object) {
        auto* p = WdfObjectGet_DeviceContextWrapper(Object);
        if (p)
        {
            if (g_Device == p->pContext)
                g_Device = nullptr;
            p->Cleanup();
        }
    };

    WDFDEVICE Device = nullptr;
    Status = WdfDeviceCreate(&pDeviceInit, &Attr, &Device);
    if (!NT_SUCCESS(Status))
        return Status;

    // How the agent finds us.
    Status = WdfDeviceCreateDeviceInterface(Device, &GUID_DEVINTERFACE_RDPIO_DISPLAY, nullptr);
    if (!NT_SUCCESS(Status))
        return Status;

    Status = IddCxDeviceInitialize(Device);
    if (!NT_SUCCESS(Status))
        return Status;

    auto* pContext = WdfObjectGet_DeviceContextWrapper(Device);
    pContext->pContext = new DeviceContext(Device);
    g_Device = pContext->pContext;
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS RdpioDeviceD0Entry(WDFDEVICE Device, WDF_POWER_DEVICE_STATE PreviousState)
{
    UNREFERENCED_PARAMETER(PreviousState);
    TraceCall Call(RDPIO_TRACE_SLOT_CALLBACK, RDPIO_TRACE_D0_ENTRY);
    return Guarded(RDPIO_TRACE_D0_ENTRY, [&] {
        auto* pContext = WdfObjectGet_DeviceContextWrapper(Device);
        if (pContext && pContext->pContext)
            pContext->pContext->InitAdapter();
        return STATUS_SUCCESS;
    });
}

#pragma endregion

#pragma region Direct3DDevice / SwapChainProcessor

HRESULT Direct3DDevice::Init()
{
    HRESULT hr = CreateDXGIFactory2(0, IID_PPV_ARGS(&DxgiFactory));
    if (FAILED(hr))
        return hr;
    hr = DxgiFactory->EnumAdapterByLuid(AdapterLuid, IID_PPV_ARGS(&Adapter));
    if (FAILED(hr))
        return hr;
    return D3D11CreateDevice(Adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0,
        D3D11_SDK_VERSION, &Device, nullptr, &DeviceContext);
}

// How long an OS callback waits for a swap-chain thread to end. Far inside
// IddCx's 10 s limit on every call into the driver.
static const DWORD kStopWaitMs = 2000;

std::shared_ptr<SwapChainProcessor> SwapChainProcessor::Start(IDDCX_SWAPCHAIN hSwapChain, IDDCX_MONITOR Monitor,
    std::shared_ptr<FrameShare> Share, UINT32 Epoch, LUID RenderAdapter, HANDLE NewFrameEvent)
{
    const UINT32 Id = Share->Id();
    std::shared_ptr<SwapChainProcessor> Me;
    std::shared_ptr<SwapChainProcessor>* ForThread = nullptr;
    try
    {
        Me.reset(new SwapChainProcessor(hSwapChain, Monitor, std::move(Share), Epoch, RenderAdapter, NewFrameEvent));
        ForThread = new std::shared_ptr<SwapChainProcessor>(Me);
    }
    catch (...)
    {
        TraceEvent(RDPIO_TRACE_EXCEPTION, RDPIO_TRACE_ASSIGN_SWAPCHAIN);
    }
    if (ForThread && Me->m_hTerminateEvent.IsValid() && Me->m_hCursorEvent.IsValid())
    {
        // Keeps the handle valid for the thread even if IddCx deletes the
        // swap-chain (with its monitor) while the thread is still in a call.
        WdfObjectReferenceWithTag((WDFOBJECT)hSwapChain, (PVOID)Me.get());
        Me->m_hThread.Attach(CreateThread(nullptr, 0, RunThread, ForThread, 0, nullptr));
        if (Me->m_hThread.IsValid())
            return Me;
        WdfObjectDereferenceWithTag((WDFOBJECT)hSwapChain, (PVOID)Me.get());
    }
    Log(L"monitor %u: couldn't start a swap-chain thread (%u)", Id, GetLastError());
    TraceEvent(RDPIO_TRACE_ASSIGN_SWAPCHAIN, (UINT32)E_OUTOFMEMORY);
    delete ForThread;
    // The OS makes a new swap-chain and tries again.
    WdfObjectDelete((WDFOBJECT)hSwapChain);
    return nullptr;
}

SwapChainProcessor::SwapChainProcessor(IDDCX_SWAPCHAIN hSwapChain, IDDCX_MONITOR Monitor, std::shared_ptr<FrameShare> Share,
    UINT32 Epoch, LUID RenderAdapter, HANDLE NewFrameEvent)
    : m_hSwapChain(hSwapChain), m_Monitor(Monitor), m_Share(std::move(Share)), m_Epoch(Epoch), m_RenderAdapter(RenderAdapter),
      m_hAvailableBufferEvent(NewFrameEvent)
{
    // Manual reset: every wait on the thread sees it.
    m_hTerminateEvent.Attach(CreateEventW(nullptr, TRUE, FALSE, nullptr));
    m_hCursorEvent.Attach(CreateEventW(nullptr, FALSE, FALSE, nullptr));
}

SwapChainProcessor::~SwapChainProcessor()
{
    // Nothing to wait for: the thread holds a reference, so this runs once
    // it has ended (or never started).
}

bool SwapChainProcessor::Stop(DWORD Ms)
{
    LONG Expected = Running;
    m_State.compare_exchange_strong(Expected, StopAsked);
    SetEvent(m_hTerminateEvent.Get());
    if (!m_hThread.IsValid())
        return true;
    {
        TraceCall Call(RDPIO_TRACE_SLOT_SWAPCHAIN, RDPIO_TRACE_STOP_THREAD, m_Share->Id());
        if (WaitForSingleObject(m_hThread.Get(), Ms) == WAIT_OBJECT_0)
            return true;
    }
    Expected = StopAsked;
    if (m_State.compare_exchange_strong(Expected, Detached))
        TraceEvent(RDPIO_TRACE_DETACHED, m_Share->Id());
    // (Else it was just ending, and deletes its swap-chain itself.)
    return false;
}

DWORD CALLBACK SwapChainProcessor::RunThread(LPVOID Argument)
{
    // The thread's own reference: the processor lives until this returns.
    std::unique_ptr<std::shared_ptr<SwapChainProcessor>> Me(static_cast<std::shared_ptr<SwapChainProcessor>*>(Argument));
    (*Me)->Run();
    return 0;
}

void SwapChainProcessor::Run()
{
    const UINT32 Id = m_Share->Id();
    // Frames are what the stream is made of: this thread gets the scheduling
    // a game's present thread would.
    DWORD AvTask = 0;
    HANDLE AvTaskHandle = AvSetMmThreadCharacteristicsW(L"Games", &AvTask);
    if (AvTaskHandle)
        AvSetMmThreadPriority(AvTaskHandle, AVRT_PRIORITY_HIGH);
    Direct3DDevice Device(m_RenderAdapter);
    bool DeviceFailed = false;
    try
    {
        // Made here rather than in the OS's AssignSwapChain call: on a GPU
        // that's resetting or being updated this can take long, and nothing
        // waits on this thread but the bounded Stop.
        HRESULT hr;
        {
            TraceCall Call(RDPIO_TRACE_SLOT_FRAMES, RDPIO_TRACE_D3D_INIT, Id);
            hr = Device.Init();
        }
        if (FAILED(hr))
        {
            Log(L"monitor %u: no D3D device on the render adapter (0x%08x)", Id, hr);
            TraceEvent(RDPIO_TRACE_D3D_INIT, (UINT32)hr);
            DeviceFailed = true;
        }
        else if (!Stopping())
        {
            DeviceFailed = RunCore(Device);
        }
    }
    catch (...)
    {
        TraceEvent(RDPIO_TRACE_EXCEPTION, RDPIO_TRACE_ACQUIRE_BUFFER);
        Log(L"monitor %u: exception on the swap-chain thread", Id);
    }
    // A device that keeps failing (a GPU resetting): wait a little before the
    // OS hands over the next swap-chain, so the two of us don't spin.
    if (DeviceFailed && !Stopping())
    {
        const DWORD Ms = m_Share->NoteDeviceFailure();
        if (Ms)
        {
            TraceEvent(RDPIO_TRACE_BACKOFF, Ms);
            WaitForSingleObject(m_hTerminateEvent.Get(), Ms);
        }
    }
    // Deleting the swap-chain makes the OS hand over a new one if needed. A
    // thread Stop gave up on leaves it to IddCx: by now its monitor may be
    // gone, and the swap-chain with it.
    if (m_State.exchange(Ended) != Detached)
    {
        TraceCall Call(RDPIO_TRACE_SLOT_FRAMES, RDPIO_TRACE_DELETE_SWAPCHAIN, Id);
        WdfObjectDelete((WDFOBJECT)m_hSwapChain);
    }
    WdfObjectDereferenceWithTag((WDFOBJECT)m_hSwapChain, (PVOID)this);
    m_hSwapChain = nullptr;
    if (AvTaskHandle)
        AvRevertMmThreadCharacteristics(AvTaskHandle);
}

bool SwapChainProcessor::RunCore(Direct3DDevice& Device)
{
    const UINT32 Id = m_Share->Id();
    ComPtr<IDXGIDevice> DxgiDevice;
    HRESULT hr = Device.Device.As(&DxgiDevice);
    if (FAILED(hr))
    {
        TraceEvent(RDPIO_TRACE_SET_DEVICE, (UINT32)hr);
        return true;
    }
    IDARG_IN_SWAPCHAINSETDEVICE SetDevice = {};
    SetDevice.pDevice = DxgiDevice.Get();
    {
        TraceCall Call(RDPIO_TRACE_SLOT_FRAMES, RDPIO_TRACE_SET_DEVICE, Id);
        hr = IddCxSwapChainSetDevice(m_hSwapChain, &SetDevice);
    }
    if (Stopping())
        return false;
    if (FAILED(hr))
    {
        Log(L"monitor %u: the swap-chain didn't take the device (0x%08x)", Id, hr);
        TraceEvent(RDPIO_TRACE_SET_DEVICE, (UINT32)hr);
        return true;
    }
    SetupCursor();

    FrameRing Ring(m_Share, m_Epoch);
    bool First = true;
    bool DeviceWorks = false;
    bool Throttled = false;
    UINT LastFrameNumber = 0;
    // The wait for a frame. Frames, the pointer and a stop each have their
    // own event, so the timeout is only a safety net: on a still screen it
    // grows to 250 ms instead of waking 60 times a second.
    DWORD IdleWait = 16;
    UINT IdleTimeouts = 0;
    for (;;)
    {
        if (Stopping())
            return false;
        if (m_HardwareCursor && WaitForSingleObject(m_hCursorEvent.Get(), 0) == WAIT_OBJECT_0)
            QueryCursor();

        ComPtr<IDXGIResource> AcquiredBuffer;
        IDARG_OUT_RELEASEANDACQUIREBUFFER Buffer = {};
        {
            TraceCall Call(RDPIO_TRACE_SLOT_FRAMES, RDPIO_TRACE_ACQUIRE_BUFFER, Id);
            hr = IddCxSwapChainReleaseAndAcquireBuffer(m_hSwapChain, &Buffer);
        }
        if (SUCCEEDED(hr))
            AcquiredBuffer.Attach(Buffer.MetaData.pSurface);
        if (Stopping())
            return false;
        if (hr == E_PENDING)
        {
            HANDLE WaitHandles[] = { m_hAvailableBufferEvent, m_hTerminateEvent.Get(), m_hCursorEvent.Get() };
            const DWORD WaitResult = WaitForMultipleObjects(ARRAYSIZE(WaitHandles), WaitHandles, FALSE, IdleWait);
            if (WaitResult == WAIT_TIMEOUT)
            {
                if (++IdleTimeouts > 4)
                    IdleWait = std::min<DWORD>(IdleWait * 2, 250);
                continue;
            }
            IdleTimeouts = 0;
            IdleWait = 16;
            if (WaitResult == WAIT_OBJECT_0 + 2)
            {
                if (m_HardwareCursor)
                    QueryCursor();
                continue;
            }
            if (WaitResult == WAIT_OBJECT_0)
                continue;
            if (WaitResult != WAIT_OBJECT_0 + 1)
                TraceEvent(RDPIO_TRACE_ACQUIRE_BUFFER, GetLastError());
            return false; // stopped (or a failed wait)
        }
        if (FAILED(hr))
        {
            // The swap-chain was abandoned (e.g. a mode change).
            TraceEvent(RDPIO_TRACE_ACQUIRE_BUFFER, (UINT32)hr);
            return false;
        }
        IdleTimeouts = 0;
        IdleWait = 16;

        const auto& Meta = Buffer.MetaData;
        // A repeat of the last picture (no dirty rects, same number) needs no
        // copy; a protected one can't be copied.
        const bool Repeat = !First && Meta.PresentationFrameNumber == LastFrameNumber && Meta.DirtyRectCount == 0 &&
            Meta.MoveRegionCount == 0;
        if (!Repeat && !Meta.HwProtectedSurface)
        {
            // A device the GPU removed never recovers: give the swap-chain
            // up, and the next one gets a new device.
            const HRESULT Removed = Device.Device->GetDeviceRemovedReason();
            if (Removed != S_OK)
            {
                Log(L"monitor %u: the D3D device was removed (0x%08x)", Id, Removed);
                TraceEvent(RDPIO_TRACE_DEVICE_REMOVED, (UINT32)Removed);
                return true;
            }
            ComPtr<ID3D11Texture2D> Surface;
            hr = AcquiredBuffer.As(&Surface);
            if (SUCCEEDED(hr))
            {
                if (!DeviceWorks)
                {
                    DeviceWorks = true;
                    m_Share->NoteFrame();
                }
                // Queued on the GPU: the copy is done before the OS reuses the
                // surface, and composition never waits on us.
                TraceCall Call(RDPIO_TRACE_SLOT_FRAMES, RDPIO_TRACE_PUBLISH, Id);
                Ring.Publish(Device.Device.Get(), Device.DeviceContext.Get(), Surface.Get(), Meta.PresentDisplayQPCTime, m_RenderAdapter);
            }
            else
            {
                TraceEvent(RDPIO_TRACE_PUBLISH, (UINT32)hr);
            }
        }
        First = false;
        LastFrameNumber = Meta.PresentationFrameNumber;
        // The copy can stall with the GPU long enough for Stop to give up on
        // this thread; a detached thread makes no further IddCx call, as the
        // swap-chain may be unassigned or deleted by now.
        if (Stopping())
            return false;
        AcquiredBuffer.Reset();
        {
            TraceCall Call(RDPIO_TRACE_SLOT_FRAMES, RDPIO_TRACE_FINISHED_FRAME, Id);
            hr = IddCxSwapChainFinishedProcessingFrame(m_hSwapChain);
        }
        if (Stopping())
            return false;
        if (FAILED(hr))
        {
            TraceEvent(RDPIO_TRACE_FINISHED_FRAME, (UINT32)hr);
            return false;
        }

        // Nobody reads the frames (an agent that says so through
        // ConsumerTick stopped): take them off the swap-chain at about two a
        // second until it's back, each still copied, so the newest picture
        // is there the moment it looks again. Back to full rate within 50 ms.
        if (m_Share->NoReader())
        {
            if (!Throttled)
            {
                Throttled = true;
                TraceEvent(RDPIO_TRACE_NO_READER, 1);
            }
            for (int Slice = 0; Slice < 10 && m_Share->NoReader(); Slice++)
            {
                if (WaitForSingleObject(m_hTerminateEvent.Get(), 50) != WAIT_TIMEOUT)
                    break;
            }
        }
        else if (Throttled)
        {
            Throttled = false;
            TraceEvent(RDPIO_TRACE_NO_READER, 0);
        }
    }
}

void SwapChainProcessor::SetupCursor()
{
    // The pointer is drawn by the client, where the mouse is: keeping it out
    // of the frames means moving it re-encodes nothing.
    IDARG_IN_SETUP_HWCURSOR Cursor = {};
    Cursor.CursorInfo.Size = sizeof(Cursor.CursorInfo);
    Cursor.CursorInfo.AlphaCursorSupport = TRUE;
    Cursor.CursorInfo.ColorXorCursorSupport = IDDCX_XOR_CURSOR_SUPPORT_FULL;
    Cursor.CursorInfo.MaxX = RDPIO_CURSOR_MAX;
    Cursor.CursorInfo.MaxY = RDPIO_CURSOR_MAX;
    Cursor.hNewCursorDataAvailable = m_hCursorEvent.Get();
    NTSTATUS Status;
    {
        TraceCall Call(RDPIO_TRACE_SLOT_FRAMES, RDPIO_TRACE_SETUP_CURSOR, m_Share->Id());
        Status = IddCxMonitorSetupHardwareCursor(m_Monitor, &Cursor);
    }
    if (Stopping())
        return;
    m_HardwareCursor = NT_SUCCESS(Status);
    if (!m_HardwareCursor)
    {
        Log(L"no hardware cursor (0x%08x): the pointer stays in the frames", Status);
        TraceEvent(RDPIO_TRACE_SETUP_CURSOR, (UINT32)Status);
    }
    m_CursorShape.assign(RDPIO_CURSOR_MAX * RDPIO_CURSOR_MAX * 4, 0);
    m_LastShapeId = 0;
    m_Share->SetHardwareCursor(m_Epoch, m_HardwareCursor);
    if (m_HardwareCursor)
        QueryCursor();
}

void SwapChainProcessor::QueryCursor()
{
    IDARG_IN_QUERY_HWCURSOR In = {};
    In.LastShapeId = m_LastShapeId;
    In.ShapeBufferSizeInBytes = (UINT)m_CursorShape.size();
    In.pShapeBuffer = m_CursorShape.data();
    IDARG_OUT_QUERY_HWCURSOR Out = {};
    NTSTATUS Status;
    {
        TraceCall Call(RDPIO_TRACE_SLOT_FRAMES, RDPIO_TRACE_QUERY_CURSOR, m_Share->Id());
        Status = IddCxMonitorQueryHardwareCursor(m_Monitor, &In, &Out);
    }
    if (Stopping())
        return;
    if (!NT_SUCCESS(Status))
    {
        // (Once per kind of failure: the pointer moves often.)
        if (Status != m_CursorFailure)
            TraceEvent(RDPIO_TRACE_QUERY_CURSOR, (UINT32)Status);
        m_CursorFailure = Status;
        return;
    }
    m_CursorFailure = 0;
    const bool Updated = Out.IsCursorShapeUpdated && Out.CursorShapeInfo.Width && Out.CursorShapeInfo.Height;
    if (Out.IsCursorShapeUpdated)
        m_LastShapeId = Out.CursorShapeInfo.ShapeId;
    m_Share->PublishCursor(m_Epoch, Out.IsCursorVisible != FALSE, Updated ? &Out.CursorShapeInfo : nullptr, m_CursorShape.data());
}

#pragma endregion

#pragma region MonitorContext

MonitorContext::~MonitorContext()
{
    StopProcessor();
}

void MonitorContext::StopProcessor() noexcept
{
    if (!m_Processor)
        return;
    if (!m_Processor->Stop(kStopWaitMs))
        Log(L"monitor %u: its swap-chain thread is stuck in a call to Windows; it ends by itself", m_Id);
    m_Processor.reset();
}

void MonitorContext::AssignSwapChain(IDDCX_SWAPCHAIN SwapChain, LUID RenderAdapter, HANDLE NewFrameEvent)
{
    StopProcessor();
    const UINT32 Epoch = m_Share->Reset();
    // Only starts the thread: it makes the D3D device itself, and this call
    // returns at once whatever the GPU is doing.
    m_Processor = SwapChainProcessor::Start(SwapChain, m_Monitor, m_Share, Epoch, RenderAdapter, NewFrameEvent);
}

void MonitorContext::UnassignSwapChain()
{
    StopProcessor();
    m_Share->Reset();
}

#pragma endregion

#pragma region FrameShare

FrameShare::FrameShare(UINT32 MonitorId) : m_Id(MonitorId)
{
    ShareSecurity Security;
    if (!Security.Get())
    {
        Log(L"monitor %u: no security descriptor (%u); frames aren't shared", m_Id, GetLastError());
        return;
    }
    wchar_t Name[96];
    swprintf_s(Name, L"Global\\rdpio-display-%u", m_Id);
    m_Section = CreateFileMappingW(INVALID_HANDLE_VALUE, Security.Get(), PAGE_READWRITE, 0, sizeof(RDPIO_FRAMES), Name);
    if (m_Section)
        m_View = (RDPIO_FRAMES*)MapViewOfFile(m_Section, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, sizeof(RDPIO_FRAMES));
    swprintf_s(Name, L"Global\\rdpio-display-%u-ready", m_Id);
    m_Ready = CreateEventW(Security.Get(), FALSE, FALSE, Name);
    if (!m_View || !m_Ready)
    {
        Log(L"monitor %u: couldn't share frames (%u); the agent duplicates the output instead", m_Id, GetLastError());
        if (m_View)
            UnmapViewOfFile(m_View);
        m_View = nullptr;
        return;
    }
    memset(m_View, 0, sizeof(RDPIO_FRAMES));
    m_View->Slot = RDPIO_FRAMES_NO_SLOT;
    MemoryBarrier();
    m_View->Version = RDPIO_FRAMES_VERSION;
}

FrameShare::~FrameShare()
{
    if (m_View)
        UnmapViewOfFile(m_View);
    if (m_Section)
        CloseHandle(m_Section);
    if (m_Ready)
        CloseHandle(m_Ready);
}

bool FrameShare::BeginWrite(UINT32 Epoch)
{
    if (!m_View || Epoch != m_Epoch.load())
        return false;
    // Held only for memory writes, so a short wait is enough. Bounded all
    // the same: a frame thread skips the write, which only drops a frame.
    for (int Try = 0; Try < 20000; Try++)
    {
        LONG Free = 0;
        if (m_Writing.compare_exchange_weak(Free, 1, std::memory_order_acquire))
        {
            if (Epoch == m_Epoch.load())
                return true;
            EndWrite();
            return false;
        }
        if (Try < 1000)
            YieldProcessor();
        else
            SwitchToThread();
    }
    return false;
}

UINT32 FrameShare::Reset()
{
    const UINT32 Epoch = ++m_Epoch;
    if (BeginWrite(Epoch))
    {
        InterlockedIncrement(&m_View->FrameLock);
        m_View->Generation = 0;
        m_View->Slot = RDPIO_FRAMES_NO_SLOT;
        InterlockedIncrement(&m_View->FrameLock);
        EndWrite();
    }
    return Epoch;
}

bool FrameShare::Announce(UINT32 Epoch, UINT32 Generation, const D3D11_TEXTURE2D_DESC& Desc, LUID Adapter)
{
    if (!BeginWrite(Epoch))
        return false;
    InterlockedIncrement(&m_View->FrameLock);
    m_View->Generation = Generation;
    m_View->Width = Desc.Width;
    m_View->Height = Desc.Height;
    m_View->Format = Desc.Format;
    m_View->Adapter = Adapter;
    m_View->Slot = RDPIO_FRAMES_NO_SLOT;
    InterlockedIncrement(&m_View->FrameLock);
    EndWrite();
    return true;
}

UINT32 FrameShare::NewestSlot() const
{
    return m_View ? m_View->Slot : RDPIO_FRAMES_NO_SLOT;
}

bool FrameShare::Publish(UINT32 Epoch, UINT32 Slot, UINT64 PresentQpc)
{
    if (!BeginWrite(Epoch))
        return false;
    InterlockedIncrement(&m_View->FrameLock);
    m_View->Slot = Slot;
    m_View->FrameNumber++;
    m_View->PresentQpc = PresentQpc;
    InterlockedIncrement(&m_View->FrameLock);
    EndWrite();
    SetEvent(m_Ready);
    return true;
}

bool FrameShare::NoReader() const
{
    return m_View && ConsumerGone(m_View->ConsumerTick, (UINT32)(GetTickCount64() / 250));
}

void FrameRing::Release()
{
    for (auto& S : m_Slots)
    {
        if (S.Shared)
            CloseHandle(S.Shared);
        S = Slot{};
    }
    m_TexturesDevice = nullptr;
}

bool FrameRing::MakeTextures(ID3D11Device* Device, const D3D11_TEXTURE2D_DESC& Like, LUID Adapter)
{
    Release();
    const UINT32 Id = m_Share->Id();
    ShareSecurity Security;
    D3D11_TEXTURE2D_DESC Desc = {};
    Desc.Width = Like.Width;
    Desc.Height = Like.Height;
    Desc.MipLevels = 1;
    Desc.ArraySize = 1;
    Desc.Format = Like.Format;
    Desc.SampleDesc.Count = 1;
    Desc.Usage = D3D11_USAGE_DEFAULT;
    // What the agent's video processor and copies want of a source.
    Desc.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
    Desc.MiscFlags = D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX;
    // A new name each time (one counter per monitor, so two rings never
    // share one): the agent may still hold the old textures open.
    const UINT32 Generation = m_Share->NextGeneration();
    for (UINT32 i = 0; i < RDPIO_FRAMES_SLOTS; i++)
    {
        Slot& S = m_Slots[i];
        HRESULT hr = Device->CreateTexture2D(&Desc, nullptr, &S.Texture);
        ComPtr<IDXGIResource1> Resource;
        if (SUCCEEDED(hr))
            hr = S.Texture.As(&Resource);
        wchar_t Name[96];
        swprintf_s(Name, L"Global\\rdpio-display-%u-%u-%u", Id, Generation, i);
        if (SUCCEEDED(hr))
            hr = Resource->CreateSharedHandle(Security.Get(), DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE, Name, &S.Shared);
        if (SUCCEEDED(hr))
            hr = S.Texture.As(&S.Mutex);
        if (FAILED(hr))
        {
            Log(L"monitor %u: shared texture %u failed 0x%08x", Id, i, hr);
            TraceEvent(RDPIO_TRACE_SHARE_TEXTURES, (UINT32)hr);
            Release();
            return false;
        }
    }
    if (!m_Share->Announce(m_Epoch, Generation, Desc, Adapter))
    {
        Release();
        return false;
    }
    m_TexturesDevice = Device;
    m_Desc = Like;
    m_Next = 0;
    Log(L"monitor %u: sharing %ux%u frames (generation %u)", Id, Desc.Width, Desc.Height, Generation);
    return true;
}

bool FrameRing::Publish(ID3D11Device* Device, ID3D11DeviceContext* Context, ID3D11Texture2D* Surface, UINT64 PresentQpc, LUID Adapter)
{
    if (!m_Share->Shares())
        return false;
    D3D11_TEXTURE2D_DESC Desc;
    Surface->GetDesc(&Desc);
    if (m_TexturesDevice != Device || Desc.Width != m_Desc.Width || Desc.Height != m_Desc.Height || Desc.Format != m_Desc.Format)
    {
        if (!MakeTextures(Device, Desc, Adapter))
            return false;
    }
    const UINT32 Newest = m_Share->NewestSlot();
    for (UINT32 Try = 0; Try < RDPIO_FRAMES_SLOTS; Try++)
    {
        const UINT32 i = (m_Next + Try) % RDPIO_FRAMES_SLOTS;
        if (i == Newest)
            continue; // the agent may be about to read it
        Slot& S = m_Slots[i];
        // S_OK only: WAIT_TIMEOUT (a success code) means the agent has it.
        HRESULT hr = S.Mutex->AcquireSync(0, 0);
        if (hr != S_OK)
        {
            TraceFailed(RDPIO_TRACE_PUBLISH, hr);
            continue;
        }
        Context->CopyResource(S.Texture.Get(), Surface);
        hr = S.Mutex->ReleaseSync(0);
        if (FAILED(hr))
        {
            TraceEvent(RDPIO_TRACE_PUBLISH, (UINT32)hr);
            return false;
        }
        m_Next = (i + 1) % RDPIO_FRAMES_SLOTS;
        return m_Share->Publish(m_Epoch, i, PresentQpc);
    }
    // Every other slot is held: the agent's behind, and this frame is dropped.
    return false;
}

void FrameShare::SetHardwareCursor(UINT32 Epoch, bool On)
{
    if (!BeginWrite(Epoch))
        return;
    InterlockedIncrement(&m_View->CursorLock);
    m_View->HardwareCursor = On ? 1 : 0;
    InterlockedIncrement(&m_View->CursorLock);
    EndWrite();
    SetEvent(m_Ready);
}

void FrameShare::PublishCursor(UINT32 Epoch, bool Visible, const IDDCX_CURSOR_SHAPE_INFO* Shape, const BYTE* Data)
{
    // Only visibility and shape matter (the client draws the pointer where
    // its mouse is): plain moves wake nobody.
    if (!m_View || (!Shape && Visible == m_CursorVisible))
        return;
    if (!BeginWrite(Epoch))
        return;
    m_CursorVisible = Visible;
    InterlockedIncrement(&m_View->CursorLock);
    m_View->CursorVisible = Visible ? 1 : 0;
    if (Shape)
    {
        const UINT32 Bytes = Shape->Pitch * Shape->Height;
        if (Shape->Width <= RDPIO_CURSOR_MAX && Shape->Height <= RDPIO_CURSOR_MAX && Bytes <= sizeof(m_View->Shape))
        {
            m_View->ShapeId = Shape->ShapeId;
            m_View->ShapeType = Shape->CursorType;
            m_View->ShapeWidth = Shape->Width;
            m_View->ShapeHeight = Shape->Height;
            m_View->ShapePitch = Shape->Pitch;
            m_View->XHot = Shape->XHot;
            m_View->YHot = Shape->YHot;
            memcpy(m_View->Shape, Data, Bytes);
        }
    }
    InterlockedIncrement(&m_View->CursorLock);
    EndWrite();
    SetEvent(m_Ready);
}

#pragma endregion

#pragma region DeviceContext

DeviceContext::DeviceContext(WDFDEVICE WdfDevice) : m_WdfDevice(WdfDevice)
{
    m_hStop.Attach(CreateEventW(nullptr, TRUE, FALSE, nullptr));
    m_hWatchdog.Attach(CreateThread(nullptr, 0, WatchdogThread, this, 0, nullptr));
}

// How long the agent's ADD or REMOVE waits for one already under way before
// it's answered STATUS_DEVICE_BUSY: inside the agent's own limits on those
// calls (2 s for a removal), and far inside IddCx's 10 s.
static const std::chrono::milliseconds kRequestLockWait(1500);

bool DeviceContext::StopWatchdog(DWORD Ms)
{
    SetEvent(m_hStop.Get());
    if (!m_hWatchdog.IsValid())
        return true;
    return WaitForSingleObject(m_hWatchdog.Get(), Ms) == WAIT_OBJECT_0;
}

void DeviceContext::InitAdapter()
{
    IDDCX_ADAPTER_CAPS AdapterCaps = {};
    AdapterCaps.Size = sizeof(AdapterCaps);
    AdapterCaps.MaxMonitorsSupported = RDPIO_DISPLAY_MAX_MONITORS;
    AdapterCaps.EndPointDiagnostics.Size = sizeof(AdapterCaps.EndPointDiagnostics);
    AdapterCaps.EndPointDiagnostics.GammaSupport = IDDCX_FEATURE_IMPLEMENTATION_NONE;
    AdapterCaps.EndPointDiagnostics.TransmissionType = IDDCX_TRANSMISSION_TYPE_OTHER;
    AdapterCaps.EndPointDiagnostics.pEndPointFriendlyName = L"rdpio Display";
    AdapterCaps.EndPointDiagnostics.pEndPointManufacturerName = L"rdpio";
    AdapterCaps.EndPointDiagnostics.pEndPointModelName = L"Hyper virtual display";
    IDDCX_ENDPOINT_VERSION Version = {};
    Version.Size = sizeof(Version);
    Version.MajorVer = 1;
    AdapterCaps.EndPointDiagnostics.pFirmwareVersion = &Version;
    AdapterCaps.EndPointDiagnostics.pHardwareVersion = &Version;

    WDF_OBJECT_ATTRIBUTES Attr;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&Attr, DeviceContextWrapper);
    IDARG_IN_ADAPTER_INIT AdapterInit = {};
    AdapterInit.WdfDevice = m_WdfDevice;
    AdapterInit.pCaps = &AdapterCaps;
    AdapterInit.ObjectAttributes = &Attr;
    IDARG_OUT_ADAPTER_INIT AdapterInitOut;
    NTSTATUS Status = IddCxAdapterInitAsync(&AdapterInit, &AdapterInitOut);
    if (NT_SUCCESS(Status))
    {
        m_Adapter = AdapterInitOut.AdapterObject;
        auto* pContext = WdfObjectGet_DeviceContextWrapper(AdapterInitOut.AdapterObject);
        pContext->pContext = this;
    }
    else
    {
        Log(L"IddCxAdapterInitAsync failed 0x%08x", Status);
    }
}

void DeviceContext::AdapterReady(bool Ready)
{
    m_Ready = Ready;
}

NTSTATUS DeviceContext::Add(const RDPIO_DISPLAY_MODE& Wanted, RDPIO_DISPLAY_ADDED* Added)
{
    if (!m_Ready || !m_Adapter)
        return STATUS_DEVICE_NOT_READY;
    if (Wanted.Width < 640 || Wanted.Width > 7680 || Wanted.Height < 480 || Wanted.Height > 4320 || Wanted.RefreshHz < 24 ||
        Wanted.RefreshHz > 500)
        return STATUS_INVALID_PARAMETER;

    std::unique_lock<std::timed_mutex> Guard(m_Lock, std::defer_lock);
    if (!Guard.try_lock_for(kRequestLockWait))
        return STATUS_DEVICE_BUSY;
    m_LastPing = GetTickCount64();
    if (Wanted.Flags & RDPIO_DISPLAY_FLAG_REPLACE)
        RemoveAllLocked(RDPIO_TRACE_SLOT_CALLBACK);
    std::unique_lock<std::mutex> Table(m_TableLock);
    if (m_Monitors.size() >= RDPIO_DISPLAY_MAX_MONITORS)
        return STATUS_TOO_MANY_NODES;

    // The GPU to compose this monitor's desktop on: the one that encodes.
    if (Wanted.RenderAdapter.LowPart || Wanted.RenderAdapter.HighPart)
    {
        IDARG_IN_ADAPTERSETRENDERADAPTER Render = {};
        Render.PreferredRenderAdapter = Wanted.RenderAdapter;
        IddCxAdapterSetRenderAdapter(m_Adapter, &Render);
    }

    UINT32 Connector = 0;
    for (; Connector < RDPIO_DISPLAY_MAX_MONITORS; Connector++)
    {
        bool Used = false;
        for (const auto& E : m_Monitors)
            Used |= E.Connector == Connector;
        if (!Used)
            break;
    }
    const UINT32 Id = m_NextId++;
    auto Modes = ModesFor(Wanted);
    BYTE Edid[128];
    BuildEdid(Edid, Id, Wanted);
    // Recorded before the monitor exists: the OS parses its EDID during
    // IddCxMonitorArrival and asks for these modes.
    m_Monitors.push_back({ nullptr, Id, Connector, Modes });
    Table.unlock();
    auto Forget = [&] {
        std::lock_guard<std::mutex> T(m_TableLock);
        m_Monitors.erase(std::remove_if(m_Monitors.begin(), m_Monitors.end(), [&](const Entry& E) { return E.Id == Id; }), m_Monitors.end());
    };

    WDF_OBJECT_ATTRIBUTES Attr;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&Attr, MonitorContextWrapper);
    // The monitor's context goes with the monitor (its swap-chain thread is
    // stopped, bounded, and its shared section closed).
    Attr.EvtCleanupCallback = [](WDFOBJECT Object) {
        auto* p = WdfObjectGet_MonitorContextWrapper(Object);
        if (p)
            p->Cleanup();
    };
    IDDCX_MONITOR_INFO MonitorInfo = {};
    MonitorInfo.Size = sizeof(MonitorInfo);
    MonitorInfo.MonitorType = DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL;
    MonitorInfo.ConnectorIndex = Connector;
    MonitorInfo.MonitorDescription.Size = sizeof(MonitorInfo.MonitorDescription);
    MonitorInfo.MonitorDescription.Type = IDDCX_MONITOR_DESCRIPTION_TYPE_EDID;
    MonitorInfo.MonitorDescription.DataSize = sizeof(Edid);
    MonitorInfo.MonitorDescription.pData = Edid;
    // A stable container per connector: Windows keeps the monitor's settings.
    MonitorInfo.MonitorContainerId = { 0x6d3b1a7e, 0x2c41, 0x4f0e, { 0x9a, 0x51, 0x3e, 0x8b, 0x7c, 0x21, 0x5d, (BYTE)(0xA0 + Connector) } };

    IDARG_IN_MONITORCREATE MonitorCreate = {};
    MonitorCreate.ObjectAttributes = &Attr;
    MonitorCreate.pMonitorInfo = &MonitorInfo;
    IDARG_OUT_MONITORCREATE MonitorCreateOut = {};
    NTSTATUS Status;
    {
        TraceCall Call(RDPIO_TRACE_SLOT_CALLBACK, RDPIO_TRACE_MONITOR_CREATE, Id);
        Status = IddCxMonitorCreate(m_Adapter, &MonitorCreate, &MonitorCreateOut);
    }
    if (!NT_SUCCESS(Status))
    {
        Forget();
        Log(L"IddCxMonitorCreate failed 0x%08x", Status);
        TraceEvent(RDPIO_TRACE_MONITOR_CREATE, (UINT32)Status);
        return Status;
    }
    {
        std::lock_guard<std::mutex> T(m_TableLock);
        for (auto& E : m_Monitors)
        {
            if (E.Id == Id)
                E.Monitor = MonitorCreateOut.MonitorObject;
        }
    }
    auto* Wrapper = WdfObjectGet_MonitorContextWrapper(MonitorCreateOut.MonitorObject);
    try
    {
        Wrapper->pContext = new MonitorContext(MonitorCreateOut.MonitorObject, Id, Modes);
    }
    catch (...)
    {
        Log(L"monitor %u: out of memory", Id);
        TraceEvent(RDPIO_TRACE_EXCEPTION, RDPIO_TRACE_MONITOR_CREATE);
        WdfObjectDelete(MonitorCreateOut.MonitorObject);
        Forget();
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    IDARG_OUT_MONITORARRIVAL ArrivalOut = {};
    {
        TraceCall Call(RDPIO_TRACE_SLOT_CALLBACK, RDPIO_TRACE_MONITOR_ARRIVAL, Id);
        Status = IddCxMonitorArrival(MonitorCreateOut.MonitorObject, &ArrivalOut);
    }
    if (!NT_SUCCESS(Status))
    {
        Log(L"IddCxMonitorArrival failed 0x%08x", Status);
        TraceEvent(RDPIO_TRACE_MONITOR_ARRIVAL, (UINT32)Status);
        // (Its context goes with it, through the cleanup callback.)
        WdfObjectDelete(MonitorCreateOut.MonitorObject);
        Forget();
        return Status;
    }
    Log(L"monitor %u: %ux%u @ %u Hz on connector %u", Id, Wanted.Width, Wanted.Height, Wanted.RefreshHz, Connector);
    if (Added)
    {
        Added->MonitorId = Id;
        Added->ConnectorIndex = Connector;
    }
    return STATUS_SUCCESS;
}

void DeviceContext::RemoveAllLocked(UINT32 TraceSlot)
{
    std::vector<Entry> Leaving;
    {
        std::lock_guard<std::mutex> T(m_TableLock);
        Leaving.swap(m_Monitors);
    }
    for (auto& E : Leaving)
    {
        if (E.Monitor)
        {
            NTSTATUS Status;
            {
                TraceCall Call(TraceSlot, RDPIO_TRACE_MONITOR_DEPARTURE, E.Id);
                Status = IddCxMonitorDeparture(E.Monitor);
            }
            Log(L"monitor %u departs (0x%08x)", E.Id, Status);
            TraceEvent(RDPIO_TRACE_MONITOR_DEPARTURE, (UINT32)Status);
        }
    }
}

NTSTATUS DeviceContext::RemoveAll()
{
    std::unique_lock<std::timed_mutex> Guard(m_Lock, std::defer_lock);
    if (!Guard.try_lock_for(kRequestLockWait))
        return STATUS_DEVICE_BUSY;
    RemoveAllLocked(RDPIO_TRACE_SLOT_CALLBACK);
    return STATUS_SUCCESS;
}

void DeviceContext::Ping()
{
    m_LastPing = GetTickCount64();
}

RDPIO_DISPLAY_INFO DeviceContext::Info()
{
    std::lock_guard<std::mutex> Guard(m_TableLock);
    m_LastPing = GetTickCount64();
    RDPIO_DISPLAY_INFO I = {};
    I.InterfaceVersion = RDPIO_DISPLAY_INTERFACE_VERSION;
    I.MaxMonitors = RDPIO_DISPLAY_MAX_MONITORS;
    I.Monitors = (UINT32)m_Monitors.size();
    I.AdapterReady = m_Ready ? 1 : 0;
    return I;
}

std::vector<Mode> DeviceContext::ModesForSerial(UINT32 Serial)
{
    std::lock_guard<std::mutex> Guard(m_TableLock);
    for (const auto& E : m_Monitors)
    {
        if (E.Id == Serial)
            return E.Modes;
    }
    return {};
}

std::vector<Mode> DeviceContext::ModesForMonitor(IDDCX_MONITOR Monitor)
{
    auto* Wrapper = WdfObjectGet_MonitorContextWrapper(Monitor);
    if (Wrapper && Wrapper->pContext)
        return Wrapper->pContext->Modes();
    return {};
}

DWORD CALLBACK DeviceContext::WatchdogThread(LPVOID Argument)
{
    try
    {
        reinterpret_cast<DeviceContext*>(Argument)->Watchdog();
    }
    catch (...)
    {
        TraceEvent(RDPIO_TRACE_EXCEPTION, RDPIO_TRACE_WATCHDOG_REMOVE);
        Log(L"exception on the watchdog thread; it stops");
    }
    return 0;
}

void DeviceContext::Watchdog()
{
    while (WaitForSingleObject(m_hStop.Get(), 1000) == WAIT_TIMEOUT)
    {
        // An addition or removal under way: look again next second.
        std::unique_lock<std::timed_mutex> Guard(m_Lock, std::try_to_lock);
        if (!Guard.owns_lock())
            continue;
        bool Any;
        {
            std::lock_guard<std::mutex> T(m_TableLock);
            Any = !m_Monitors.empty();
        }
        if (Any && GetTickCount64() - m_LastPing > RDPIO_DISPLAY_WATCHDOG_MS)
        {
            Log(L"the agent stopped pinging: removing its monitors");
            TraceEvent(RDPIO_TRACE_WATCHDOG_REMOVE, 0);
            RemoveAllLocked(RDPIO_TRACE_SLOT_WATCHDOG);
        }
    }
}

#pragma endregion

#pragma region DDI callbacks

_Use_decl_annotations_
VOID RdpioIoDeviceControl(WDFDEVICE Device, WDFREQUEST Request, size_t OutputBufferLength, size_t InputBufferLength, ULONG IoControlCode)
{
    UNREFERENCED_PARAMETER(OutputBufferLength);
    UNREFERENCED_PARAMETER(InputBufferLength);
    UINT32 Code = 0;
    switch (IoControlCode)
    {
    case IOCTL_RDPIO_DISPLAY_INFO:
        Code = RDPIO_TRACE_IOCTL_INFO;
        break;
    case IOCTL_RDPIO_DISPLAY_ADD:
        Code = RDPIO_TRACE_IOCTL_ADD;
        break;
    case IOCTL_RDPIO_DISPLAY_REMOVE:
        Code = RDPIO_TRACE_IOCTL_REMOVE;
        break;
    case IOCTL_RDPIO_DISPLAY_PING:
        Code = RDPIO_TRACE_IOCTL_PING;
        break;
    }
    TraceCall Call(RDPIO_TRACE_SLOT_CALLBACK, Code);
    ULONG_PTR Info = 0;
    // Completed exactly once, below, whatever happens inside.
    const NTSTATUS Status = Guarded(Code, [&]() -> NTSTATUS {
        auto* Wrapper = WdfObjectGet_DeviceContextWrapper(Device);
        DeviceContext* Ctx = Wrapper ? Wrapper->pContext : nullptr;
        if (!Ctx)
            return STATUS_DEVICE_NOT_READY;
        NTSTATUS S = STATUS_INVALID_DEVICE_REQUEST;
        switch (IoControlCode)
        {
        case IOCTL_RDPIO_DISPLAY_INFO: {
            RDPIO_DISPLAY_INFO* Out = nullptr;
            S = WdfRequestRetrieveOutputBuffer(Request, sizeof(*Out), (PVOID*)&Out, nullptr);
            if (NT_SUCCESS(S))
            {
                *Out = Ctx->Info();
                Info = sizeof(*Out);
            }
            break;
        }
        case IOCTL_RDPIO_DISPLAY_ADD: {
            RDPIO_DISPLAY_MODE* In = nullptr;
            S = WdfRequestRetrieveInputBuffer(Request, sizeof(*In), (PVOID*)&In, nullptr);
            if (!NT_SUCCESS(S))
                break;
            const RDPIO_DISPLAY_MODE Wanted = *In; // (the same buffer is the output)
            RDPIO_DISPLAY_ADDED Added = {};
            S = Ctx->Add(Wanted, &Added);
            RDPIO_DISPLAY_ADDED* Out = nullptr;
            if (NT_SUCCESS(S) && NT_SUCCESS(WdfRequestRetrieveOutputBuffer(Request, sizeof(*Out), (PVOID*)&Out, nullptr)))
            {
                *Out = Added;
                Info = sizeof(*Out);
            }
            break;
        }
        case IOCTL_RDPIO_DISPLAY_REMOVE:
            S = Ctx->RemoveAll();
            break;
        case IOCTL_RDPIO_DISPLAY_PING:
            Ctx->Ping();
            S = STATUS_SUCCESS;
            break;
        }
        return S;
    });
    if (Status == STATUS_DEVICE_BUSY)
    {
        Log(L"request %u: another addition or removal is still under way", Code);
        TraceEvent(RDPIO_TRACE_BUSY, Code);
    }
    else if (!NT_SUCCESS(Status) && Code != 0)
    {
        TraceEvent(Code, (UINT32)Status);
    }
    WdfRequestCompleteWithInformation(Request, Status, NT_SUCCESS(Status) ? Info : 0);
}

_Use_decl_annotations_
NTSTATUS RdpioAdapterInitFinished(IDDCX_ADAPTER AdapterObject, const IDARG_IN_ADAPTER_INIT_FINISHED* pInArgs)
{
    TraceCall Call(RDPIO_TRACE_SLOT_CALLBACK, RDPIO_TRACE_ADAPTER_INIT);
    TraceFailed(RDPIO_TRACE_ADAPTER_INIT, pInArgs->AdapterInitStatus);
    auto* Wrapper = WdfObjectGet_DeviceContextWrapper(AdapterObject);
    if (Wrapper && Wrapper->pContext)
        Wrapper->pContext->AdapterReady(NT_SUCCESS(pInArgs->AdapterInitStatus));
    // No monitors until the agent asks for one.
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS RdpioAdapterCommitModes(IDDCX_ADAPTER AdapterObject, const IDARG_IN_COMMITMODES* pInArgs)
{
    UNREFERENCED_PARAMETER(AdapterObject);
    UNREFERENCED_PARAMETER(pInArgs);
    TraceCall Call(RDPIO_TRACE_SLOT_CALLBACK, RDPIO_TRACE_COMMIT_MODES);
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS RdpioParseMonitorDescription(const IDARG_IN_PARSEMONITORDESCRIPTION* pInArgs, IDARG_OUT_PARSEMONITORDESCRIPTION* pOutArgs)
{
    TraceCall Call(RDPIO_TRACE_SLOT_CALLBACK, RDPIO_TRACE_PARSE_DESCRIPTION);
    return Guarded(RDPIO_TRACE_PARSE_DESCRIPTION, [&]() -> NTSTATUS {
        const UINT32 Serial = EdidSerial((const BYTE*)pInArgs->MonitorDescription.pData, pInArgs->MonitorDescription.DataSize);
        std::vector<Mode> Modes = (Serial && g_Device) ? g_Device->ModesForSerial(Serial) : std::vector<Mode>{};
        if (Modes.empty())
            return STATUS_INVALID_PARAMETER;
        pOutArgs->MonitorModeBufferOutputCount = (UINT)Modes.size();
        if (pInArgs->MonitorModeBufferInputCount < Modes.size())
            return (pInArgs->MonitorModeBufferInputCount > 0) ? STATUS_BUFFER_TOO_SMALL : STATUS_SUCCESS;
        for (size_t i = 0; i < Modes.size(); i++)
            pInArgs->pMonitorModes[i] = MonitorMode(Modes[i], IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR);
        pOutArgs->PreferredMonitorModeIdx = 0;
        return STATUS_SUCCESS;
    });
}

_Use_decl_annotations_
NTSTATUS RdpioMonitorGetDefaultModes(IDDCX_MONITOR MonitorObject, const IDARG_IN_GETDEFAULTDESCRIPTIONMODES* pInArgs,
    IDARG_OUT_GETDEFAULTDESCRIPTIONMODES* pOutArgs)
{
    TraceCall Call(RDPIO_TRACE_SLOT_CALLBACK, RDPIO_TRACE_DEFAULT_MODES);
    return Guarded(RDPIO_TRACE_DEFAULT_MODES, [&]() -> NTSTATUS {
        // (Only for a monitor without an EDID; ours always have one.)
        std::vector<Mode> Modes = g_Device ? g_Device->ModesForMonitor(MonitorObject) : std::vector<Mode>{};
        if (Modes.empty())
            Modes.push_back({ 1920, 1080, 60 });
        pOutArgs->DefaultMonitorModeBufferOutputCount = (UINT)Modes.size();
        if (pInArgs->DefaultMonitorModeBufferInputCount == 0)
            return STATUS_SUCCESS;
        if (pInArgs->DefaultMonitorModeBufferInputCount < Modes.size())
            return STATUS_BUFFER_TOO_SMALL;
        for (size_t i = 0; i < Modes.size(); i++)
            pInArgs->pDefaultMonitorModes[i] = MonitorMode(Modes[i], IDDCX_MONITOR_MODE_ORIGIN_DRIVER);
        pOutArgs->PreferredMonitorModeIdx = 0;
        return STATUS_SUCCESS;
    });
}

_Use_decl_annotations_
NTSTATUS RdpioMonitorQueryModes(IDDCX_MONITOR MonitorObject, const IDARG_IN_QUERYTARGETMODES* pInArgs, IDARG_OUT_QUERYTARGETMODES* pOutArgs)
{
    TraceCall Call(RDPIO_TRACE_SLOT_CALLBACK, RDPIO_TRACE_TARGET_MODES);
    return Guarded(RDPIO_TRACE_TARGET_MODES, [&]() -> NTSTATUS {
        // What we can compose and hand on: exactly the monitor's modes.
        std::vector<Mode> Modes = g_Device ? g_Device->ModesForMonitor(MonitorObject) : std::vector<Mode>{};
        pOutArgs->TargetModeBufferOutputCount = (UINT)Modes.size();
        if (pInArgs->TargetModeBufferInputCount >= Modes.size())
        {
            for (size_t i = 0; i < Modes.size(); i++)
                pInArgs->pTargetModes[i] = TargetMode(Modes[i]);
        }
        return STATUS_SUCCESS;
    });
}

_Use_decl_annotations_
NTSTATUS RdpioMonitorAssignSwapChain(IDDCX_MONITOR MonitorObject, const IDARG_IN_SETSWAPCHAIN* pInArgs)
{
    auto* Wrapper = WdfObjectGet_MonitorContextWrapper(MonitorObject);
    MonitorContext* Ctx = Wrapper ? Wrapper->pContext : nullptr;
    TraceCall Call(RDPIO_TRACE_SLOT_SWAPCHAIN, RDPIO_TRACE_ASSIGN_SWAPCHAIN, Ctx ? Ctx->Id() : 0);
    if (!Ctx)
    {
        // A monitor already on its way out: nothing will use this one.
        WdfObjectDelete((WDFOBJECT)pInArgs->hSwapChain);
        return STATUS_SUCCESS;
    }
    TraceEvent(RDPIO_TRACE_ASSIGN_SWAPCHAIN, Ctx->Id());
    return Guarded(RDPIO_TRACE_ASSIGN_SWAPCHAIN, [&]() -> NTSTATUS {
        Ctx->AssignSwapChain(pInArgs->hSwapChain, pInArgs->RenderAdapterLuid, pInArgs->hNextSurfaceAvailable);
        return STATUS_SUCCESS;
    });
}

_Use_decl_annotations_
NTSTATUS RdpioMonitorUnassignSwapChain(IDDCX_MONITOR MonitorObject)
{
    auto* Wrapper = WdfObjectGet_MonitorContextWrapper(MonitorObject);
    MonitorContext* Ctx = Wrapper ? Wrapper->pContext : nullptr;
    TraceCall Call(RDPIO_TRACE_SLOT_SWAPCHAIN, RDPIO_TRACE_UNASSIGN_SWAPCHAIN, Ctx ? Ctx->Id() : 0);
    if (!Ctx)
        return STATUS_SUCCESS;
    TraceEvent(RDPIO_TRACE_UNASSIGN_SWAPCHAIN, Ctx->Id());
    return Guarded(RDPIO_TRACE_UNASSIGN_SWAPCHAIN, [&]() -> NTSTATUS {
        Ctx->UnassignSwapChain();
        return STATUS_SUCCESS;
    });
}

#pragma endregion
