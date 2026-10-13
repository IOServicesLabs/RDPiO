// rdpio's bridge to NVIDIA's encoder (NVENC), compiled against the SDK's own
// header (nvEncodeAPI.h, MIT, API 12.0) so every structure is laid out exactly
// as the driver expects. One session encodes D3D11 textures synchronously:
// a picture in, its access unit out. Rust side: src/hyper/nvenc.rs.

#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "nvEncodeAPI.h"

typedef NVENCSTATUS(NVENCAPI *CreateInstanceFn)(NV_ENCODE_API_FUNCTION_LIST *);
typedef NVENCSTATUS(NVENCAPI *MaxVersionFn)(uint32_t *);

typedef struct {
    uint32_t codec; // 0 H.264, 1 HEVC
    uint32_t width, height, fps;
    uint32_t bitrate;   // bits/s
    uint32_t vbv_bytes; // the largest a frame may get
    uint32_t preset;    // 1 (fastest) .. 7 (best)
    uint32_t ltr_frames;
    uint32_t argb; // input textures are BGRA (else NV12)
} RdpNvencConfig;

typedef struct {
    uint32_t force_idr;
    int32_t ltr_mark;      // keep this picture as long-term reference n (-1: no)
    uint32_t ltr_use_mask; // code it from only these long-term references (0: any)
    uint64_t timestamp;
} RdpNvencFrame;

#define MAX_REGISTERED 16

typedef struct {
    HMODULE dll;
    NV_ENCODE_API_FUNCTION_LIST fn;
    void *enc;
    NV_ENC_INITIALIZE_PARAMS init;
    NV_ENC_CONFIG config;
    NV_ENC_OUTPUT_PTR bitstream;
    struct {
        void *texture;
        NV_ENC_REGISTERED_PTR reg;
    } registered[MAX_REGISTERED];
    int nregistered;
    uint32_t codec;
    uint32_t argb;
    uint8_t *out;
    uint32_t out_cap;
    // Asynchronous mode: the driver signals this when a picture is done (the
    // synchronous wait polls on the system timer: up to 15.6 ms late).
    HANDLE done;
    int done_registered;
} RdpNvenc;

static int fail(RdpNvenc *s, char *err, int errlen, const char *what, NVENCSTATUS st) {
    const char *detail = (s && s->enc && s->fn.nvEncGetLastErrorString) ? s->fn.nvEncGetLastErrorString(s->enc) : "";
    if (err && errlen > 0) {
        snprintf(err, (size_t)errlen, "%s failed (NVENC status %d)%s%s", what, (int)st, detail && *detail ? ": " : "", detail ? detail : "");
    }
    return st ? (int)st : -1;
}

// Registered textures are held (a reference each) until unregistered: one
// freed meanwhile could come back at the same address as a new texture, and
// match a registration that's no longer its own.
static void unregister_all(RdpNvenc *s) {
    for (int i = 0; i < s->nregistered; i++) {
        s->fn.nvEncUnregisterResource(s->enc, s->registered[i].reg);
        IUnknown *u = (IUnknown *)s->registered[i].texture;
        u->lpVtbl->Release(u);
    }
    s->nregistered = 0;
}

void rdp_nvenc_close(RdpNvenc *s) {
    if (!s) {
        return;
    }
    if (s->enc) {
        unregister_all(s);
        if (s->done_registered) {
            NV_ENC_EVENT_PARAMS ev = {0};
            ev.version = NV_ENC_EVENT_PARAMS_VER;
            ev.completionEvent = s->done;
            s->fn.nvEncUnregisterAsyncEvent(s->enc, &ev);
        }
        if (s->bitstream) {
            s->fn.nvEncDestroyBitstreamBuffer(s->enc, s->bitstream);
        }
        s->fn.nvEncDestroyEncoder(s->enc);
    }
    if (s->done) {
        CloseHandle(s->done);
    }
    if (s->dll) {
        FreeLibrary(s->dll);
    }
    free(s->out);
    free(s);
}

static const GUID *preset_guid(uint32_t p) {
    switch (p) {
    case 0:
    case 1: return &NV_ENC_PRESET_P1_GUID;
    case 2: return &NV_ENC_PRESET_P2_GUID;
    case 3: return &NV_ENC_PRESET_P3_GUID;
    case 4: return &NV_ENC_PRESET_P4_GUID;
    case 5: return &NV_ENC_PRESET_P5_GUID;
    case 6: return &NV_ENC_PRESET_P6_GUID;
    default: return &NV_ENC_PRESET_P7_GUID;
    }
}

static void set_rate(NV_ENC_CONFIG *c, uint32_t bitrate, uint32_t vbv_bytes) {
    c->rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
    c->rcParams.averageBitRate = bitrate;
    c->rcParams.maxBitRate = bitrate;
    c->rcParams.vbvBufferSize = vbv_bytes * 8;
    c->rcParams.vbvInitialDelay = vbv_bytes * 8;
}

// Open a session on `device` (an ID3D11Device*). 0 on success.
int rdp_nvenc_open(void *device, const RdpNvencConfig *cfg, RdpNvenc **out, char *err, int errlen) {
    *out = NULL;
    RdpNvenc *s = (RdpNvenc *)calloc(1, sizeof(RdpNvenc));
    if (!s) {
        return fail(NULL, err, errlen, "allocation", 0);
    }
    s->codec = cfg->codec;
    s->argb = cfg->argb;
    s->dll = LoadLibraryW(L"nvEncodeAPI64.dll");
    if (!s->dll) {
        rdp_nvenc_close(s);
        return fail(NULL, err, errlen, "loading nvEncodeAPI64.dll (no NVIDIA driver)", 0);
    }
    MaxVersionFn max_version = (MaxVersionFn)GetProcAddress(s->dll, "NvEncodeAPIGetMaxSupportedVersion");
    CreateInstanceFn create = (CreateInstanceFn)GetProcAddress(s->dll, "NvEncodeAPICreateInstance");
    if (!max_version || !create) {
        rdp_nvenc_close(s);
        return fail(NULL, err, errlen, "finding the NVENC entry points", 0);
    }
    uint32_t have = 0;
    max_version(&have);
    uint32_t need = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;
    if (have < need) {
        rdp_nvenc_close(s);
        if (err && errlen > 0) {
            snprintf(err, (size_t)errlen, "the NVIDIA driver offers NVENC API %u.%u; 12.0 needs a newer driver", have >> 4, have & 0xf);
        }
        return -1;
    }
    s->fn.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    NVENCSTATUS st = create(&s->fn);
    if (st) {
        rdp_nvenc_close(s);
        return fail(NULL, err, errlen, "NvEncodeAPICreateInstance", st);
    }
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS open = {0};
    open.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    open.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
    open.device = device;
    open.apiVersion = NVENCAPI_VERSION;
    st = s->fn.nvEncOpenEncodeSessionEx(&open, &s->enc);
    if (st) {
        s->enc = NULL; // (a failed open leaves nothing to destroy)
        rdp_nvenc_close(s);
        return fail(NULL, err, errlen, "opening an NVENC session", st);
    }

    GUID codec = cfg->codec ? NV_ENC_CODEC_HEVC_GUID : NV_ENC_CODEC_H264_GUID;
    const GUID *preset = preset_guid(cfg->preset);
    NV_ENC_PRESET_CONFIG pc = {0};
    pc.version = NV_ENC_PRESET_CONFIG_VER;
    pc.presetCfg.version = NV_ENC_CONFIG_VER;
    // RDPIO_NVENC_TUNING=1 high quality, 2 low latency, 3 ultra-low latency
    // (the default), for measuring.
    NV_ENC_TUNING_INFO tuning = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    if (getenv("RDPIO_NVENC_TUNING")) {
        tuning = (NV_ENC_TUNING_INFO)atoi(getenv("RDPIO_NVENC_TUNING"));
    }
    st = s->fn.nvEncGetEncodePresetConfigEx(s->enc, codec, *preset, tuning, &pc);
    if (st) {
        int r = fail(s, err, errlen, "reading the preset", st);
        rdp_nvenc_close(s);
        return r;
    }
    s->config = pc.presetCfg;
    s->config.version = NV_ENC_CONFIG_VER;
    s->config.gopLength = NVENC_INFINITE_GOPLENGTH;
    s->config.frameIntervalP = 1; // no B-frames
    if (getenv("RDPIO_NVENC_TRACE")) {
        fprintf(stderr, "nvenc preset P%u: multiPass %d lookahead %u aq %u temporalAq %u\n", cfg->preset, (int)s->config.rcParams.multiPass,
                (unsigned)s->config.rcParams.enableLookahead, (unsigned)s->config.rcParams.enableAQ, (unsigned)s->config.rcParams.enableTemporalAQ);
    }
    set_rate(&s->config, cfg->bitrate, cfg->vbv_bytes);
    s->config.rcParams.zeroReorderDelay = 1;
    // One pass, nothing looked ahead: each picture takes the encoder once.
    // (RDPIO_NVENC_MULTIPASS=1|2: quarter- or full-resolution first pass.)
    {
        const char *mp = getenv("RDPIO_NVENC_MULTIPASS");
        s->config.rcParams.multiPass = mp ? (NV_ENC_MULTI_PASS)atoi(mp) : NV_ENC_MULTI_PASS_DISABLED;
    }
    s->config.rcParams.enableLookahead = 0;
    if (getenv("RDPIO_NVENC_AQ")) {
        s->config.rcParams.enableAQ = 1;
        s->config.rcParams.aqStrength = (uint32_t)atoi(getenv("RDPIO_NVENC_AQ"));
    }
    if (cfg->codec) {
        NV_ENC_CONFIG_HEVC *h = &s->config.encodeCodecConfig.hevcConfig;
        s->config.profileGUID = NV_ENC_HEVC_PROFILE_MAIN_GUID;
        h->idrPeriod = NVENC_INFINITE_GOPLENGTH;
        h->repeatSPSPPS = 1;
        if (cfg->ltr_frames) {
            h->enableLTR = 1;
            h->ltrNumFrames = cfg->ltr_frames;
            h->ltrTrustMode = 0;
            // Room for the long-term references beside the short-term one.
            h->maxNumRefFramesInDPB = cfg->ltr_frames + 2;
        }
        NV_ENC_CONFIG_HEVC_VUI_PARAMETERS *v = &h->hevcVUIParameters;
        v->videoSignalTypePresentFlag = 1;
        v->videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        v->videoFullRangeFlag = 0;
        v->colourDescriptionPresentFlag = 1;
        v->colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        v->transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
        v->colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT709;
    } else {
        NV_ENC_CONFIG_H264 *h = &s->config.encodeCodecConfig.h264Config;
        s->config.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
        h->idrPeriod = NVENC_INFINITE_GOPLENGTH;
        h->repeatSPSPPS = 1;
        if (cfg->ltr_frames) {
            h->enableLTR = 1;
            h->ltrNumFrames = cfg->ltr_frames;
            h->ltrTrustMode = 0;
            h->maxNumRefFrames = cfg->ltr_frames + 2;
        }
        NV_ENC_CONFIG_H264_VUI_PARAMETERS *v = &h->h264VUIParameters;
        v->videoSignalTypePresentFlag = 1;
        v->videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        v->videoFullRangeFlag = 0;
        v->colourDescriptionPresentFlag = 1;
        v->colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        v->transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
        v->colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT709;
    }

    s->init.version = NV_ENC_INITIALIZE_PARAMS_VER;
    s->init.encodeGUID = codec;
    s->init.presetGUID = *preset;
    s->init.encodeWidth = cfg->width;
    s->init.encodeHeight = cfg->height;
    s->init.darWidth = cfg->width;
    s->init.darHeight = cfg->height;
    s->init.maxEncodeWidth = cfg->width;
    s->init.maxEncodeHeight = cfg->height;
    s->init.frameRateNum = cfg->fps ? cfg->fps : 60;
    s->init.frameRateDen = 1;
    s->init.enablePTD = 1;
    s->init.enableEncodeAsync = 1;
    s->init.tuningInfo = tuning;
    s->init.encodeConfig = &s->config;
    st = s->fn.nvEncInitializeEncoder(s->enc, &s->init);
    if (st) {
        int r = fail(s, err, errlen, "initializing the encoder", st);
        rdp_nvenc_close(s);
        return r;
    }
    NV_ENC_CREATE_BITSTREAM_BUFFER b = {0};
    b.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
    st = s->fn.nvEncCreateBitstreamBuffer(s->enc, &b);
    if (st) {
        int r = fail(s, err, errlen, "creating the bitstream buffer", st);
        rdp_nvenc_close(s);
        return r;
    }
    s->bitstream = b.bitstreamBuffer;
    s->done = CreateEventW(NULL, FALSE, FALSE, NULL);
    NV_ENC_EVENT_PARAMS ev = {0};
    ev.version = NV_ENC_EVENT_PARAMS_VER;
    ev.completionEvent = s->done;
    st = s->done ? s->fn.nvEncRegisterAsyncEvent(s->enc, &ev) : NV_ENC_ERR_INVALID_EVENT;
    if (st) {
        int r = fail(s, err, errlen, "registering the completion event", st);
        rdp_nvenc_close(s);
        return r;
    }
    s->done_registered = 1;
    *out = s;
    return 0;
}

static NV_ENC_REGISTERED_PTR registered(RdpNvenc *s, void *texture, char *err, int errlen, NVENCSTATUS *st) {
    for (int i = 0; i < s->nregistered; i++) {
        if (s->registered[i].texture == texture) {
            return s->registered[i].reg;
        }
    }
    if (s->nregistered == MAX_REGISTERED) {
        unregister_all(s); // a new set of textures (the old ones are gone)
    }
    NV_ENC_REGISTER_RESOURCE r = {0};
    r.version = NV_ENC_REGISTER_RESOURCE_VER;
    r.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
    r.width = s->init.encodeWidth;
    r.height = s->init.encodeHeight;
    r.resourceToRegister = texture;
    r.bufferFormat = s->argb ? NV_ENC_BUFFER_FORMAT_ARGB : NV_ENC_BUFFER_FORMAT_NV12;
    r.bufferUsage = NV_ENC_INPUT_IMAGE;
    *st = s->fn.nvEncRegisterResource(s->enc, &r);
    if (*st) {
        fail(s, err, errlen, "registering a texture", *st);
        return NULL;
    }
    IUnknown *u = (IUnknown *)texture;
    u->lpVtbl->AddRef(u);
    s->registered[s->nregistered].texture = texture;
    s->registered[s->nregistered].reg = r.registeredResource;
    s->nregistered++;
    return r.registeredResource;
}

// Encode `texture` (an ID3D11Texture2D* of the session's size). The access
// unit is left in *data / *len (valid until the next call). 0 on success.
static double now_ms(void) {
    LARGE_INTEGER c, f;
    QueryPerformanceCounter(&c);
    QueryPerformanceFrequency(&f);
    return (double)c.QuadPart * 1000.0 / (double)f.QuadPart;
}

int rdp_nvenc_encode(RdpNvenc *s, void *texture, const RdpNvencFrame *f, const uint8_t **data, uint32_t *len, uint32_t *keyframe, uint32_t *ltr, char *err, int errlen) {
    NVENCSTATUS st = 0;
    static int trace = -1;
    if (trace < 0) {
        trace = getenv("RDPIO_NVENC_TRACE") != NULL;
    }
    double t0 = now_ms(), t1 = 0, t2 = 0, t3 = 0, t4 = 0;
    NV_ENC_REGISTERED_PTR reg = registered(s, texture, err, errlen, &st);
    if (!reg) {
        return st ? (int)st : -1;
    }
    NV_ENC_MAP_INPUT_RESOURCE map = {0};
    map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
    map.registeredResource = reg;
    st = s->fn.nvEncMapInputResource(s->enc, &map);
    if (st) {
        return fail(s, err, errlen, "mapping the texture", st);
    }
    t1 = now_ms();
    NV_ENC_PIC_PARAMS p = {0};
    p.version = NV_ENC_PIC_PARAMS_VER;
    p.inputWidth = s->init.encodeWidth;
    p.inputHeight = s->init.encodeHeight;
    p.inputBuffer = map.mappedResource;
    p.bufferFmt = map.mappedBufferFmt;
    p.outputBitstream = s->bitstream;
    p.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    p.inputTimeStamp = f->timestamp;
    p.completionEvent = s->done;
    if (f->force_idr) {
        p.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
    }
    if (s->codec) {
        NV_ENC_PIC_PARAMS_HEVC *h = &p.codecPicParams.hevcPicParams;
        if (f->ltr_mark >= 0) {
            h->ltrMarkFrame = 1;
            h->ltrMarkFrameIdx = (uint32_t)f->ltr_mark;
        }
        if (f->ltr_use_mask) {
            h->ltrUseFrames = 1;
            h->ltrUseFrameBitmap = f->ltr_use_mask;
        }
    } else {
        NV_ENC_PIC_PARAMS_H264 *h = &p.codecPicParams.h264PicParams;
        if (f->ltr_mark >= 0) {
            h->ltrMarkFrame = 1;
            h->ltrMarkFrameIdx = (uint32_t)f->ltr_mark;
        }
        if (f->ltr_use_mask) {
            h->ltrUseFrames = 1;
            h->ltrUseFrameBitmap = f->ltr_use_mask;
        }
    }
    st = s->fn.nvEncEncodePicture(s->enc, &p);
    if (st) {
        s->fn.nvEncUnmapInputResource(s->enc, map.mappedResource);
        return fail(s, err, errlen, "encoding", st);
    }
    t2 = now_ms();
    // A second at most: a lost device or a driver reset is an error, not a
    // frozen stream.
    if (WaitForSingleObject(s->done, 1000) != WAIT_OBJECT_0) {
        s->fn.nvEncUnmapInputResource(s->enc, map.mappedResource);
        return fail(s, err, errlen, "waiting for the picture (1 s)", 0);
    }
    t3 = now_ms();
    NV_ENC_LOCK_BITSTREAM lock = {0};
    lock.version = NV_ENC_LOCK_BITSTREAM_VER;
    lock.outputBitstream = s->bitstream;
    st = s->fn.nvEncLockBitstream(s->enc, &lock);
    if (st) {
        s->fn.nvEncUnmapInputResource(s->enc, map.mappedResource);
        return fail(s, err, errlen, "reading the bitstream", st);
    }
    if (lock.bitstreamSizeInBytes > s->out_cap) {
        uint32_t cap = lock.bitstreamSizeInBytes + lock.bitstreamSizeInBytes / 2;
        uint8_t *grown = (uint8_t *)realloc(s->out, cap);
        if (!grown) {
            s->fn.nvEncUnlockBitstream(s->enc, s->bitstream);
            s->fn.nvEncUnmapInputResource(s->enc, map.mappedResource);
            return fail(s, err, errlen, "allocation", 0);
        }
        s->out = grown;
        s->out_cap = cap;
    }
    memcpy(s->out, lock.bitstreamBufferPtr, lock.bitstreamSizeInBytes);
    *data = s->out;
    *len = lock.bitstreamSizeInBytes;
    *keyframe = lock.pictureType == NV_ENC_PIC_TYPE_IDR;
    *ltr = lock.ltrFrame;
    s->fn.nvEncUnlockBitstream(s->enc, s->bitstream);
    s->fn.nvEncUnmapInputResource(s->enc, map.mappedResource);
    t4 = now_ms();
    if (trace) {
        fprintf(stderr, "nvenc: map %.2f encode %.2f wait %.2f lock+copy+unmap %.2f ms (%u bytes)\n", t1 - t0, t2 - t1, t3 - t2, t4 - t3, *len);
    }
    return 0;
}

// A new bitrate (and largest frame), in place: no new session, no keyframe.
int rdp_nvenc_set_bitrate(RdpNvenc *s, uint32_t bitrate, uint32_t vbv_bytes, char *err, int errlen) {
    NV_ENC_CONFIG before = s->config;
    set_rate(&s->config, bitrate, vbv_bytes);
    NV_ENC_RECONFIGURE_PARAMS r = {0};
    r.version = NV_ENC_RECONFIGURE_PARAMS_VER;
    r.reInitEncodeParams = s->init;
    r.reInitEncodeParams.encodeConfig = &s->config;
    r.resetEncoder = 0;
    r.forceIDR = 0;
    NVENCSTATUS st = s->fn.nvEncReconfigureEncoder(s->enc, &r);
    if (st) {
        s->config = before;
        return fail(s, err, errlen, "changing the bitrate", st);
    }
    return 0;
}
