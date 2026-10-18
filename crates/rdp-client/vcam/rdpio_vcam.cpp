// rdpio Camera: the media source behind the host's virtual camera during a
// Hyper stream (Windows 11's MFCreateVirtualCamera).
//
// The Hyper agent creates the virtual camera and feeds it: it decodes the
// laptop's camera (sent over the stream) into a shared NV12 frame,
// `Global\rdpio-camera`. Windows' Frame Server loads this DLL, and when an app
// opens "rdpio Camera" each requested sample gets a copy of the newest frame.
// The source raises `Global\rdpio-camera-wanted` whenever a stream starts or
// stops, so the agent asks the laptop for its camera only while an app uses
// it.
//
// Structure after Microsoft's virtual camera guidance and Simon Mourier's
// VCamSample: an activator (IMFActivate) handing out one media source
// (IMFMediaSourceEx + IMFGetService + IKsControl + IMFSampleAllocatorControl)
// with one stream (IMFMediaStream2 + IKsControl), samples from the pipeline's
// allocator, in system memory.
//
// Built by rdp-client's build.rs into rdpio_vcam.dll and carried inside
// rdpio.exe; the agent writes it under Program Files and registers it.

#include <initguid.h>
#include <windows.h>
#include <unknwn.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mferror.h>
#include <mftransform.h>
#include <mfvirtualcamera.h>
#include <ks.h>
#include <ksmedia.h>
#include <ksproxy.h>
#include <winrt/base.h>
#include <cstdint>
#include <cstring>
#include <mutex>

// {9E8E8B5A-5C3B-4C7E-9B0F-5D2B7A1E6C31}
static const GUID CLSID_RdpioCamera = {0x9e8e8b5a, 0x5c3b, 0x4c7e, {0x9b, 0x0f, 0x5d, 0x2b, 0x7a, 0x1e, 0x6c, 0x31}};
// PINNAME_VIDEO_CAPTURE.
static const GUID PinVideoCapture = {0xfb6c4281, 0x0353, 0x11d1, {0x90, 0x5f, 0x00, 0x00, 0xc0, 0xcc, 0x16, 0xba}};

static const UINT32 WIDTH = 1280, HEIGHT = 720, FPS = 30;
static const UINT32 FRAME_BYTES = WIDTH * HEIGHT * 3 / 2;

// The frame the agent shares (see hyper_camera.rs, which must match).
struct Shared {
    uint32_t magic;           // 'RCAM'
    uint32_t version;         // 1
    uint32_t width, height;
    volatile LONG wanted;     // streams running (this source counts them)
    volatile LONG seq;        // even: stable; odd: the agent is writing
    uint64_t frames;          // frames written so far
    // the NV12 frame follows
};
static const uint32_t SHARED_MAGIC = 0x4D414352;
static const DWORD SHARED_SIZE = sizeof(Shared) + FRAME_BYTES;

// The agent's frame, mapped on first use (in Frame Server; the app processes
// that load this DLL only to activate it never get here).
class Feed {
public:
    void wanted(LONG delta) {
        if (!open()) return;
        InterlockedAdd(&m_view->wanted, delta);
        if (m_wanted) SetEvent(m_wanted);
    }
    // Copy the newest frame into `dst` (`pitch` bytes a row, the UV rows
    // starting `uv_offset` bytes in). Black when there's none.
    void copy(BYTE* dst, LONG pitch, size_t uv_offset) {
        const BYTE* src = nullptr;
        if (open() && m_view->magic == SHARED_MAGIC && m_view->frames > 0 &&
            m_view->width == WIDTH && m_view->height == HEIGHT) {
            src = reinterpret_cast<const BYTE*>(m_view + 1);
        }
        for (int attempt = 0; attempt < 2; attempt++) {
            LONG before = src ? m_view->seq : 0;
            MemoryBarrier();
            for (UINT32 y = 0; y < HEIGHT; y++) {
                if (src) memcpy(dst + y * pitch, src + y * WIDTH, WIDTH);
                else memset(dst + y * pitch, 16, WIDTH);
            }
            for (UINT32 y = 0; y < HEIGHT / 2; y++) {
                BYTE* row = dst + uv_offset + y * pitch;
                if (src) memcpy(row, src + WIDTH * HEIGHT + y * WIDTH, WIDTH);
                else memset(row, 128, WIDTH);
            }
            MemoryBarrier();
            // (A frame half-written while copied: once more.)
            if (!src || ((before & 1) == 0 && m_view->seq == before)) break;
        }
    }
    ~Feed() {
        if (m_view) UnmapViewOfFile(m_view);
        if (m_map) CloseHandle(m_map);
        if (m_wanted) CloseHandle(m_wanted);
    }

private:
    bool open() {
        std::lock_guard<std::mutex> g(m_lock);
        if (m_view) return true;
        if (GetTickCount64() < m_retry_at) return false;
        m_retry_at = GetTickCount64() + 1000;
        m_map = OpenFileMappingW(FILE_MAP_READ | FILE_MAP_WRITE, FALSE, L"Global\\rdpio-camera");
        if (!m_map) return false;
        m_view = static_cast<Shared*>(MapViewOfFile(m_map, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, SHARED_SIZE));
        if (!m_view) {
            CloseHandle(m_map);
            m_map = nullptr;
            return false;
        }
        m_wanted = OpenEventW(EVENT_MODIFY_STATE, FALSE, L"Global\\rdpio-camera-wanted");
        return true;
    }
    std::mutex m_lock;
    HANDLE m_map = nullptr;
    HANDLE m_wanted = nullptr;
    Shared* m_view = nullptr;
    ULONGLONG m_retry_at = 0;
};

// IMFAttributes, delegated to a real attribute store.
#define DELEGATE_ATTRIBUTES(A)                                                                                    \
    STDMETHODIMP GetItem(REFGUID k, PROPVARIANT* v) override { return A->GetItem(k, v); }                         \
    STDMETHODIMP GetItemType(REFGUID k, MF_ATTRIBUTE_TYPE* t) override { return A->GetItemType(k, t); }           \
    STDMETHODIMP CompareItem(REFGUID k, REFPROPVARIANT v, BOOL* r) override { return A->CompareItem(k, v, r); }   \
    STDMETHODIMP Compare(IMFAttributes* t, MF_ATTRIBUTES_MATCH_TYPE m, BOOL* r) override { return A->Compare(t, m, r); } \
    STDMETHODIMP GetUINT32(REFGUID k, UINT32* v) override { return A->GetUINT32(k, v); }                          \
    STDMETHODIMP GetUINT64(REFGUID k, UINT64* v) override { return A->GetUINT64(k, v); }                          \
    STDMETHODIMP GetDouble(REFGUID k, double* v) override { return A->GetDouble(k, v); }                          \
    STDMETHODIMP GetGUID(REFGUID k, GUID* v) override { return A->GetGUID(k, v); }                                \
    STDMETHODIMP GetStringLength(REFGUID k, UINT32* l) override { return A->GetStringLength(k, l); }              \
    STDMETHODIMP GetString(REFGUID k, LPWSTR v, UINT32 n, UINT32* l) override { return A->GetString(k, v, n, l); } \
    STDMETHODIMP GetAllocatedString(REFGUID k, LPWSTR* v, UINT32* l) override { return A->GetAllocatedString(k, v, l); } \
    STDMETHODIMP GetBlobSize(REFGUID k, UINT32* s) override { return A->GetBlobSize(k, s); }                      \
    STDMETHODIMP GetBlob(REFGUID k, UINT8* b, UINT32 n, UINT32* s) override { return A->GetBlob(k, b, n, s); }    \
    STDMETHODIMP GetAllocatedBlob(REFGUID k, UINT8** b, UINT32* s) override { return A->GetAllocatedBlob(k, b, s); } \
    STDMETHODIMP GetUnknown(REFGUID k, REFIID i, LPVOID* p) override { return A->GetUnknown(k, i, p); }           \
    STDMETHODIMP SetItem(REFGUID k, REFPROPVARIANT v) override { return A->SetItem(k, v); }                       \
    STDMETHODIMP DeleteItem(REFGUID k) override { return A->DeleteItem(k); }                                      \
    STDMETHODIMP DeleteAllItems() override { return A->DeleteAllItems(); }                                        \
    STDMETHODIMP SetUINT32(REFGUID k, UINT32 v) override { return A->SetUINT32(k, v); }                           \
    STDMETHODIMP SetUINT64(REFGUID k, UINT64 v) override { return A->SetUINT64(k, v); }                           \
    STDMETHODIMP SetDouble(REFGUID k, double v) override { return A->SetDouble(k, v); }                           \
    STDMETHODIMP SetGUID(REFGUID k, REFGUID v) override { return A->SetGUID(k, v); }                              \
    STDMETHODIMP SetString(REFGUID k, LPCWSTR v) override { return A->SetString(k, v); }                          \
    STDMETHODIMP SetBlob(REFGUID k, const UINT8* b, UINT32 s) override { return A->SetBlob(k, b, s); }            \
    STDMETHODIMP SetUnknown(REFGUID k, IUnknown* u) override { return A->SetUnknown(k, u); }                      \
    STDMETHODIMP LockStore() override { return A->LockStore(); }                                                  \
    STDMETHODIMP UnlockStore() override { return A->UnlockStore(); }                                              \
    STDMETHODIMP GetCount(UINT32* c) override { return A->GetCount(c); }                                          \
    STDMETHODIMP GetItemByIndex(UINT32 i, GUID* k, PROPVARIANT* v) override { return A->GetItemByIndex(i, k, v); } \
    STDMETHODIMP CopyAllItems(IMFAttributes* d) override { return A->CopyAllItems(d); }

#define CATCH_RETURN() catch (...) { return winrt::to_hresult(); }

static winrt::com_ptr<IMFAttributes> NewAttributes() {
    winrt::com_ptr<IMFAttributes> a;
    winrt::check_hresult(MFCreateAttributes(a.put(), 8));
    return a;
}

static const NTSTATUS SET_NOT_FOUND = HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND);

struct MediaStream : winrt::implements<MediaStream, IMFMediaStream2, IKsControl, IMFAttributes> {
    DELEGATE_ATTRIBUTES(m_attr)

    MediaStream(IMFMediaSource* source, Feed* feed) : m_source(source), m_feed(feed) {
        m_attr = NewAttributes();
        winrt::check_hresult(m_attr->SetGUID(MF_DEVICESTREAM_STREAM_CATEGORY, PinVideoCapture));
        winrt::check_hresult(m_attr->SetUINT32(MF_DEVICESTREAM_STREAM_ID, 0));
        winrt::check_hresult(m_attr->SetUINT32(MF_DEVICESTREAM_FRAMESERVER_SHARED, 1));
        winrt::check_hresult(m_attr->SetUINT32(MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MFFrameSourceTypes_Color));
        winrt::check_hresult(MFCreateEventQueue(m_queue.put()));

        winrt::com_ptr<IMFMediaType> type;
        winrt::check_hresult(MFCreateMediaType(type.put()));
        winrt::check_hresult(type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video));
        winrt::check_hresult(type->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_NV12));
        winrt::check_hresult(MFSetAttributeSize(type.get(), MF_MT_FRAME_SIZE, WIDTH, HEIGHT));
        winrt::check_hresult(MFSetAttributeRatio(type.get(), MF_MT_FRAME_RATE, FPS, 1));
        winrt::check_hresult(MFSetAttributeRatio(type.get(), MF_MT_PIXEL_ASPECT_RATIO, 1, 1));
        winrt::check_hresult(type->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive));
        winrt::check_hresult(type->SetUINT32(MF_MT_ALL_SAMPLES_INDEPENDENT, TRUE));
        winrt::check_hresult(type->SetUINT32(MF_MT_DEFAULT_STRIDE, WIDTH));
        winrt::check_hresult(type->SetUINT32(MF_MT_SAMPLE_SIZE, FRAME_BYTES));
        winrt::check_hresult(type->SetUINT32(MF_MT_AVG_BITRATE, FRAME_BYTES * 8 * FPS));
        IMFMediaType* types[] = {type.get()};
        winrt::check_hresult(MFCreateStreamDescriptor(0, 1, types, m_descriptor.put()));
        winrt::com_ptr<IMFMediaTypeHandler> handler;
        winrt::check_hresult(m_descriptor->GetMediaTypeHandler(handler.put()));
        winrt::check_hresult(handler->SetCurrentMediaType(type.get()));
    }

    // IMFMediaEventGenerator
    STDMETHODIMP BeginGetEvent(IMFAsyncCallback* cb, IUnknown* state) override {
        auto q = queue();
        return q ? q->BeginGetEvent(cb, state) : MF_E_SHUTDOWN;
    }
    STDMETHODIMP EndGetEvent(IMFAsyncResult* r, IMFMediaEvent** e) override {
        auto q = queue();
        return q ? q->EndGetEvent(r, e) : MF_E_SHUTDOWN;
    }
    STDMETHODIMP GetEvent(DWORD flags, IMFMediaEvent** e) override {
        auto q = queue();
        return q ? q->GetEvent(flags, e) : MF_E_SHUTDOWN;
    }
    STDMETHODIMP QueueEvent(MediaEventType t, REFGUID g, HRESULT s, const PROPVARIANT* v) override {
        auto q = queue();
        return q ? q->QueueEventParamVar(t, g, s, v) : MF_E_SHUTDOWN;
    }

    // IMFMediaStream
    STDMETHODIMP GetMediaSource(IMFMediaSource** s) override {
        if (!s) return E_POINTER;
        std::lock_guard<std::mutex> g(m_lock);
        if (!m_source) return MF_E_SHUTDOWN;
        *s = m_source;
        (*s)->AddRef();
        return S_OK;
    }
    STDMETHODIMP GetStreamDescriptor(IMFStreamDescriptor** d) override {
        if (!d) return E_POINTER;
        std::lock_guard<std::mutex> g(m_lock);
        if (!m_descriptor) return MF_E_SHUTDOWN;
        m_descriptor.copy_to(d);
        return S_OK;
    }
    STDMETHODIMP RequestSample(IUnknown* token) override {
        try {
            std::lock_guard<std::mutex> g(m_lock);
            if (!m_queue || !m_allocator) return MF_E_SHUTDOWN;
            winrt::com_ptr<IMFSample> sample;
            winrt::check_hresult(m_allocator->AllocateSample(sample.put()));
            winrt::com_ptr<IMFMediaBuffer> buffer;
            winrt::check_hresult(sample->GetBufferByIndex(0, buffer.put()));
            if (auto b2 = buffer.try_as<IMF2DBuffer2>()) {
                BYTE *scan0 = nullptr, *start = nullptr;
                LONG pitch = 0;
                DWORD len = 0;
                winrt::check_hresult(b2->Lock2DSize(MF2DBuffer_LockFlags_Write, &scan0, &pitch, &start, &len));
                if (pitch > 0 && len >= DWORD(pitch) * HEIGHT * 3 / 2) {
                    m_feed->copy(scan0, pitch, size_t(pitch) * HEIGHT);
                }
                b2->Unlock2D();
            } else {
                BYTE* data = nullptr;
                DWORD max = 0;
                winrt::check_hresult(buffer->Lock(&data, &max, nullptr));
                if (max >= FRAME_BYTES) m_feed->copy(data, WIDTH, size_t(WIDTH) * HEIGHT);
                buffer->Unlock();
                buffer->SetCurrentLength(FRAME_BYTES);
            }
            winrt::check_hresult(sample->SetSampleTime(MFGetSystemTime()));
            winrt::check_hresult(sample->SetSampleDuration(10'000'000 / FPS));
            if (token) winrt::check_hresult(sample->SetUnknown(MFSampleExtension_Token, token));
            return m_queue->QueueEventParamUnk(MEMediaSample, GUID_NULL, S_OK, sample.get());
        }
        CATCH_RETURN()
    }

    // IMFMediaStream2
    STDMETHODIMP SetStreamState(MF_STREAM_STATE value) override {
        if (value == m_state) return S_OK;
        switch (value) {
        case MF_STREAM_STATE_STOPPED:
            return Stop();
        case MF_STREAM_STATE_PAUSED:
            if (m_state != MF_STREAM_STATE_RUNNING) return MF_E_INVALID_STATE_TRANSITION;
            m_state = value;
            return S_OK;
        case MF_STREAM_STATE_RUNNING:
            return Start(nullptr);
        default:
            return MF_E_INVALID_STATE_TRANSITION;
        }
    }
    STDMETHODIMP GetStreamState(MF_STREAM_STATE* value) override {
        if (!value) return E_POINTER;
        *value = m_state;
        return S_OK;
    }

    // IKsControl
    STDMETHODIMP KsProperty(PKSPROPERTY p, ULONG, LPVOID, ULONG, ULONG* r) override { return (!p || !r) ? E_POINTER : SET_NOT_FOUND; }
    STDMETHODIMP KsMethod(PKSMETHOD m, ULONG, LPVOID, ULONG, ULONG* r) override { return (!m || !r) ? E_POINTER : SET_NOT_FOUND; }
    STDMETHODIMP KsEvent(PKSEVENT, ULONG, LPVOID, ULONG, ULONG* r) override { return !r ? E_POINTER : SET_NOT_FOUND; }

    // For the source.
    HRESULT Start(IMFMediaType* type) {
        try {
            std::lock_guard<std::mutex> g(m_lock);
            if (!m_queue || !m_allocator) return MF_E_SHUTDOWN;
            winrt::com_ptr<IMFMediaType> current;
            if (!type) {
                winrt::com_ptr<IMFMediaTypeHandler> handler;
                winrt::check_hresult(m_descriptor->GetMediaTypeHandler(handler.put()));
                winrt::check_hresult(handler->GetCurrentMediaType(current.put()));
                type = current.get();
            }
            winrt::check_hresult(m_allocator->InitializeSampleAllocator(10, type));
            winrt::check_hresult(m_queue->QueueEventParamVar(MEStreamStarted, GUID_NULL, S_OK, nullptr));
            if (m_state != MF_STREAM_STATE_RUNNING) m_feed->wanted(+1);
            m_state = MF_STREAM_STATE_RUNNING;
            return S_OK;
        }
        CATCH_RETURN()
    }
    HRESULT Stop() {
        std::lock_guard<std::mutex> g(m_lock);
        if (!m_queue) return MF_E_SHUTDOWN;
        if (m_allocator) m_allocator->UninitializeSampleAllocator();
        m_queue->QueueEventParamVar(MEStreamStopped, GUID_NULL, S_OK, nullptr);
        if (m_state == MF_STREAM_STATE_RUNNING) m_feed->wanted(-1);
        m_state = MF_STREAM_STATE_STOPPED;
        return S_OK;
    }
    HRESULT Shutdown() {
        std::lock_guard<std::mutex> g(m_lock);
        if (m_queue) {
            m_queue->Shutdown();
            m_queue = nullptr;
        }
        if (m_allocator) {
            m_allocator->UninitializeSampleAllocator();
            m_allocator = nullptr;
        }
        if (m_state == MF_STREAM_STATE_RUNNING) m_feed->wanted(-1);
        m_state = MF_STREAM_STATE_STOPPED;
        m_descriptor = nullptr;
        m_source = nullptr;
        return S_OK;
    }
    HRESULT SetAllocator(IUnknown* allocator) {
        if (!allocator) return E_POINTER;
        std::lock_guard<std::mutex> g(m_lock);
        m_allocator = nullptr;
        return allocator->QueryInterface(IID_PPV_ARGS(m_allocator.put()));
    }
    winrt::com_ptr<IMFStreamDescriptor> descriptor() {
        std::lock_guard<std::mutex> g(m_lock);
        return m_descriptor;
    }

private:
    winrt::com_ptr<IMFMediaEventQueue> queue() {
        std::lock_guard<std::mutex> g(m_lock);
        return m_queue;
    }
    std::mutex m_lock;
    winrt::com_ptr<IMFAttributes> m_attr;
    winrt::com_ptr<IMFMediaEventQueue> m_queue;
    winrt::com_ptr<IMFStreamDescriptor> m_descriptor;
    winrt::com_ptr<IMFVideoSampleAllocatorEx> m_allocator;
    IMFMediaSource* m_source; // (the source owns this stream)
    Feed* m_feed;
    MF_STREAM_STATE m_state = MF_STREAM_STATE_STOPPED;
};

struct MediaSource
    : winrt::implements<MediaSource, IMFMediaSourceEx, IMFGetService, IKsControl, IMFSampleAllocatorControl, IMFAttributes> {
    DELEGATE_ATTRIBUTES(m_attr)

    void Initialize(IMFAttributes* from) {
        m_attr = NewAttributes();
        if (from) winrt::check_hresult(from->CopyAllItems(m_attr.get()));
        // Profiles: the legacy one (≤ 30 fps), as cameras declare.
        winrt::com_ptr<IMFSensorProfileCollection> profiles;
        winrt::com_ptr<IMFSensorProfile> legacy;
        if (SUCCEEDED(MFCreateSensorProfileCollection(profiles.put())) &&
            SUCCEEDED(MFCreateSensorProfile(KSCAMERAPROFILE_Legacy, 0, nullptr, legacy.put())) &&
            SUCCEEDED(legacy->AddProfileFilter(0, L"((RES==;FRT<=30,1;SUT==))")) &&
            SUCCEEDED(profiles->AddProfile(legacy.get()))) {
            m_attr->SetUnknown(MF_DEVICEMFT_SENSORPROFILE_COLLECTION, profiles.get());
        }
        m_stream = winrt::make_self<MediaStream>(this, &m_feed);
        IMFStreamDescriptor* sd[] = {m_stream->descriptor().get()};
        winrt::check_hresult(MFCreatePresentationDescriptor(1, sd, m_descriptor.put()));
        winrt::check_hresult(MFCreateEventQueue(m_queue.put()));
    }

    // IMFMediaEventGenerator
    STDMETHODIMP BeginGetEvent(IMFAsyncCallback* cb, IUnknown* state) override {
        auto q = queue();
        return q ? q->BeginGetEvent(cb, state) : MF_E_SHUTDOWN;
    }
    STDMETHODIMP EndGetEvent(IMFAsyncResult* r, IMFMediaEvent** e) override {
        auto q = queue();
        return q ? q->EndGetEvent(r, e) : MF_E_SHUTDOWN;
    }
    STDMETHODIMP GetEvent(DWORD flags, IMFMediaEvent** e) override {
        auto q = queue();
        return q ? q->GetEvent(flags, e) : MF_E_SHUTDOWN;
    }
    STDMETHODIMP QueueEvent(MediaEventType t, REFGUID g, HRESULT s, const PROPVARIANT* v) override {
        auto q = queue();
        return q ? q->QueueEventParamVar(t, g, s, v) : MF_E_SHUTDOWN;
    }

    // IMFMediaSource
    STDMETHODIMP CreatePresentationDescriptor(IMFPresentationDescriptor** pd) override {
        if (!pd) return E_POINTER;
        std::lock_guard<std::mutex> g(m_lock);
        if (!m_descriptor) return MF_E_SHUTDOWN;
        return m_descriptor->Clone(pd);
    }
    STDMETHODIMP GetCharacteristics(DWORD* c) override {
        if (!c) return E_POINTER;
        *c = MFMEDIASOURCE_IS_LIVE;
        return S_OK;
    }
    STDMETHODIMP Pause() override { return MF_E_INVALID_STATE_TRANSITION; }
    STDMETHODIMP Shutdown() override {
        winrt::com_ptr<IMFMediaEventQueue> q;
        winrt::com_ptr<MediaStream> s;
        {
            std::lock_guard<std::mutex> g(m_lock);
            if (!m_queue) return MF_E_SHUTDOWN;
            q = std::move(m_queue);
            s = std::move(m_stream);
            m_descriptor = nullptr;
        }
        q->Shutdown();
        if (s) s->Shutdown();
        return S_OK;
    }
    STDMETHODIMP Start(IMFPresentationDescriptor* pd, const GUID* format, const PROPVARIANT* position) override {
        try {
            if (!pd || !position) return E_POINTER;
            if (format && *format != GUID_NULL) return E_INVALIDARG;
            winrt::com_ptr<IMFMediaEventQueue> q;
            winrt::com_ptr<MediaStream> stream;
            {
                std::lock_guard<std::mutex> g(m_lock);
                if (!m_queue || !m_stream) return MF_E_SHUTDOWN;
                q = m_queue;
                stream = m_stream;
            }
            DWORD count = 0;
            winrt::check_hresult(pd->GetStreamDescriptorCount(&count));
            if (count != 1) return E_INVALIDARG;
            BOOL selected = FALSE;
            winrt::com_ptr<IMFStreamDescriptor> sd;
            winrt::check_hresult(pd->GetStreamDescriptorByIndex(0, &selected, sd.put()));
            MF_STREAM_STATE state = MF_STREAM_STATE_STOPPED;
            stream->GetStreamState(&state);
            if (selected) {
                winrt::com_ptr<IMFMediaTypeHandler> handler;
                winrt::com_ptr<IMFMediaType> type;
                winrt::check_hresult(sd->GetMediaTypeHandler(handler.put()));
                winrt::check_hresult(handler->GetCurrentMediaType(type.put()));
                winrt::check_hresult(q->QueueEventParamUnk(state == MF_STREAM_STATE_RUNNING ? MEUpdatedStream : MENewStream,
                                                           GUID_NULL, S_OK, stream.as<IUnknown>().get()));
                winrt::check_hresult(stream->Start(type.get()));
            } else if (state == MF_STREAM_STATE_RUNNING) {
                stream->Stop();
            }
            PROPVARIANT time;
            PropVariantInit(&time);
            time.vt = VT_I8;
            time.hVal.QuadPart = MFGetSystemTime();
            return q->QueueEventParamVar(MESourceStarted, GUID_NULL, S_OK, &time);
        }
        CATCH_RETURN()
    }
    STDMETHODIMP Stop() override {
        winrt::com_ptr<IMFMediaEventQueue> q;
        winrt::com_ptr<MediaStream> stream;
        {
            std::lock_guard<std::mutex> g(m_lock);
            if (!m_queue) return MF_E_SHUTDOWN;
            q = m_queue;
            stream = m_stream;
        }
        if (stream) stream->Stop();
        PROPVARIANT time;
        PropVariantInit(&time);
        time.vt = VT_I8;
        time.hVal.QuadPart = MFGetSystemTime();
        return q->QueueEventParamVar(MESourceStopped, GUID_NULL, S_OK, &time);
    }

    // IMFMediaSourceEx
    STDMETHODIMP GetSourceAttributes(IMFAttributes** a) override {
        if (!a) return E_POINTER;
        return QueryInterface(IID_PPV_ARGS(a));
    }
    STDMETHODIMP GetStreamAttributes(DWORD id, IMFAttributes** a) override {
        if (!a) return E_POINTER;
        *a = nullptr;
        std::lock_guard<std::mutex> g(m_lock);
        if (id != 0 || !m_stream) return MF_E_INVALIDSTREAMNUMBER;
        return m_stream->QueryInterface(IID_PPV_ARGS(a));
    }
    // (Samples stay in system memory: the frame comes from the CPU.)
    STDMETHODIMP SetD3DManager(IUnknown*) override { return S_OK; }

    // IMFGetService
    STDMETHODIMP GetService(REFGUID, REFIID, LPVOID*) override { return MF_E_UNSUPPORTED_SERVICE; }

    // IKsControl
    STDMETHODIMP KsProperty(PKSPROPERTY p, ULONG, LPVOID, ULONG, ULONG* r) override { return (!p || !r) ? E_POINTER : SET_NOT_FOUND; }
    STDMETHODIMP KsMethod(PKSMETHOD m, ULONG, LPVOID, ULONG, ULONG* r) override { return (!m || !r) ? E_POINTER : SET_NOT_FOUND; }
    STDMETHODIMP KsEvent(PKSEVENT, ULONG, LPVOID, ULONG, ULONG* r) override { return !r ? E_POINTER : SET_NOT_FOUND; }

    // IMFSampleAllocatorControl
    STDMETHODIMP SetDefaultAllocator(DWORD id, IUnknown* allocator) override {
        if (!allocator) return E_POINTER;
        winrt::com_ptr<MediaStream> s;
        {
            std::lock_guard<std::mutex> g(m_lock);
            s = m_stream;
        }
        if (id != 0 || !s) return E_FAIL;
        return s->SetAllocator(allocator);
    }
    STDMETHODIMP GetAllocatorUsage(DWORD id, DWORD* input, MFSampleAllocatorUsage* usage) override {
        if (!input || !usage) return E_POINTER;
        if (id != 0) return E_FAIL;
        *input = id;
        *usage = MFSampleAllocatorUsage_UsesProvidedAllocator;
        return S_OK;
    }

private:
    winrt::com_ptr<IMFMediaEventQueue> queue() {
        std::lock_guard<std::mutex> g(m_lock);
        return m_queue;
    }
    std::mutex m_lock;
    winrt::com_ptr<IMFAttributes> m_attr;
    winrt::com_ptr<IMFMediaEventQueue> m_queue;
    winrt::com_ptr<IMFPresentationDescriptor> m_descriptor;
    winrt::com_ptr<MediaStream> m_stream;
    Feed m_feed;
};

struct Activator : winrt::implements<Activator, IMFActivate> {
    DELEGATE_ATTRIBUTES(m_attr)

    Activator() {
        m_attr = NewAttributes();
        winrt::check_hresult(m_attr->SetUINT32(MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES, 1));
        winrt::check_hresult(m_attr->SetGUID(MFT_TRANSFORM_CLSID_Attribute, CLSID_RdpioCamera));
        m_source = winrt::make_self<MediaSource>();
        m_source->Initialize(m_attr.get());
    }

    STDMETHODIMP ActivateObject(REFIID riid, void** ppv) override {
        if (!ppv) return E_POINTER;
        *ppv = nullptr;
        std::lock_guard<std::mutex> g(m_lock);
        if (!m_source) return MF_E_SHUTDOWN;
        return m_source->QueryInterface(riid, ppv);
    }
    STDMETHODIMP ShutdownObject() override { return S_OK; }
    STDMETHODIMP DetachObject() override {
        std::lock_guard<std::mutex> g(m_lock);
        m_source = nullptr;
        return S_OK;
    }

private:
    std::mutex m_lock;
    winrt::com_ptr<IMFAttributes> m_attr;
    winrt::com_ptr<MediaSource> m_source;
};

struct ClassFactory : winrt::implements<ClassFactory, IClassFactory> {
    STDMETHODIMP CreateInstance(IUnknown* outer, REFIID riid, void** ppv) override {
        if (!ppv) return E_POINTER;
        *ppv = nullptr;
        if (outer) return CLASS_E_NOAGGREGATION;
        try {
            return winrt::make<Activator>()->QueryInterface(riid, ppv);
        }
        CATCH_RETURN()
    }
    STDMETHODIMP LockServer(BOOL lock) override {
        if (lock) ++winrt::get_module_lock();
        else --winrt::get_module_lock();
        return S_OK;
    }
};

extern "C" HRESULT __stdcall DllGetClassObject(REFCLSID clsid, REFIID riid, void** ppv) {
    if (!ppv) return E_POINTER;
    *ppv = nullptr;
    if (clsid != CLSID_RdpioCamera) return CLASS_E_CLASSNOTAVAILABLE;
    try {
        return winrt::make<ClassFactory>()->QueryInterface(riid, ppv);
    }
    CATCH_RETURN()
}

extern "C" HRESULT __stdcall DllCanUnloadNow() {
    return winrt::get_module_lock() ? S_FALSE : S_OK;
}
