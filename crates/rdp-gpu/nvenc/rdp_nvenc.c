// rdpio's bridge to NVIDIA's encoder (NVENC), compiled against the SDK's own
// header (nvEncodeAPI.h, MIT) so every structure is laid out exactly as the
// driver expects. One session encodes D3D11 textures synchronously: a picture
// in, its access unit out. Rust side: src/hyper/nvenc.rs.
//
// Built twice: against API 12.0 (nvenc/, drivers from 522) and, as
// sdk13/rdp_nvenc13.c with every entry point suffixed `_v13`, against API
// 13.0 (drivers from 570), which adds long-term references for AV1. The Rust
// side opens the newer one where the driver offers it.

#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifdef RDP_NVENC_V13
#define RDP_FN(n) n##_v13
#else
#include "nvEncodeAPI.h"
#define RDP_FN(n) n
#endif

typedef NVENCSTATUS(NVENCAPI *CreateInstanceFn)(NV_ENCODE_API_FUNCTION_LIST *);
typedef NVENCSTATUS(NVENCAPI *MaxVersionFn)(uint32_t *);

typedef struct {
    uint32_t codec; // 0 H.264, 1 HEVC, 2 AV1
    uint32_t width, height, fps;
    uint32_t bitrate;   // bits/s
    uint32_t vbv_bytes; // the largest a frame may get
    uint32_t preset;    // 1 (fastest) .. 7 (best)
    uint32_t ltr_frames;
    uint32_t argb; // input textures are BGRA (else NV12)
    // Gradual (intra) refresh on request, spread over this many pictures
    // (0: none; a lost picture is repaired with a keyframe or a reference).
    uint32_t intra_refresh;
} RdpNvencConfig;

typedef struct {
    uint32_t force_idr;
    int32_t ltr_mark;      // keep this picture as long-term reference n (-1: no)
    uint32_t ltr_use_mask; // code it from only these long-term references (0: any)
    uint64_t timestamp;
    uint32_t intra_refresh; // start a gradual refresh with this picture
} RdpNvencFrame;

#define MAX_REGISTERED 16

typedef struct {
    HMODULE dll;
    NV_ENCODE_API_FUNCTION_LIST fn;
    void *enc;
    NV_ENC_INITIALIZE_PARAMS init;
    NV_ENC_CONFIG config;
    // What the session was opened with, the bitrate and preset kept current:
    // a preset change builds the whole configuration again from it.
    RdpNvencConfig cfg;
    NV_ENC_TUNING_INFO tuning;
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

void RDP_FN(rdp_nvenc_close)(RdpNvenc *s) {
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

// The newest NVENC API the installed driver offers ((major << 4) | minor; 0:
// no NVIDIA driver).
uint32_t RDP_FN(rdp_nvenc_driver_api)(void) {
    HMODULE dll = LoadLibraryW(L"nvEncodeAPI64.dll");
    if (!dll) {
        return 0;
    }
    uint32_t have = 0;
    MaxVersionFn max_version = (MaxVersionFn)GetProcAddress(dll, "NvEncodeAPIGetMaxSupportedVersion");
    if (max_version) {
        max_version(&have);
    }
    FreeLibrary(dll);
    return have;
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

static GUID codec_guid(uint32_t codec) {
    return codec == 2 ? NV_ENC_CODEC_AV1_GUID : codec ? NV_ENC_CODEC_HEVC_GUID : NV_ENC_CODEC_H264_GUID;
}

static void set_rate(NV_ENC_CONFIG *c, uint32_t bitrate, uint32_t vbv_bytes) {
    c->rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
    c->rcParams.averageBitRate = bitrate;
    c->rcParams.maxBitRate = bitrate;
    c->rcParams.vbvBufferSize = vbv_bytes * 8;
    c->rcParams.vbvInitialDelay = vbv_bytes * 8;
}

// The whole encoder configuration for `preset`: the preset's own (for the
// session's tuning), then everything rdpio sets on top. Used to open and to
// change the preset in place.
static int configure(RdpNvenc *s, uint32_t preset, NV_ENC_CONFIG *out, char *err, int errlen) {
    const RdpNvencConfig *cfg = &s->cfg;
    NV_ENC_PRESET_CONFIG pc = {0};
    pc.version = NV_ENC_PRESET_CONFIG_VER;
    pc.presetCfg.version = NV_ENC_CONFIG_VER;
    NVENCSTATUS st = s->fn.nvEncGetEncodePresetConfigEx(s->enc, codec_guid(cfg->codec), *preset_guid(preset), s->tuning, &pc);
    if (st) {
        return fail(s, err, errlen, "reading the preset", st);
    }
    NV_ENC_CONFIG c = pc.presetCfg;
    c.version = NV_ENC_CONFIG_VER;
    c.gopLength = NVENC_INFINITE_GOPLENGTH;
    c.frameIntervalP = 1; // no B-frames
    if (getenv("RDPIO_NVENC_TRACE")) {
        fprintf(stderr, "nvenc preset P%u: multiPass %d lookahead %u aq %u temporalAq %u\n", preset, (int)c.rcParams.multiPass,
                (unsigned)c.rcParams.enableLookahead, (unsigned)c.rcParams.enableAQ, (unsigned)c.rcParams.enableTemporalAQ);
    }
    set_rate(&c, cfg->bitrate, cfg->vbv_bytes);
    c.rcParams.zeroReorderDelay = 1;
    // One pass, nothing looked ahead: each picture takes the encoder once.
    // (RDPIO_NVENC_MULTIPASS=1|2: quarter- or full-resolution first pass.)
    {
        const char *mp = getenv("RDPIO_NVENC_MULTIPASS");
        c.rcParams.multiPass = mp ? (NV_ENC_MULTI_PASS)atoi(mp) : NV_ENC_MULTI_PASS_DISABLED;
    }
    c.rcParams.enableLookahead = 0;
    // No picture coded finer than QP 12: below it, a still or nearly still
    // screen's spare bitrate went on detail no one sees. Measured (1080p
    // text, 30 Mbps, a still screen for a second): luma after 1 s 49.9 ->
    // 47.9 dB (H.264), 51.8 -> 49.9 (HEVC) — under a level of error — for
    // 37% / 12% fewer bytes; a desktop with a speck of motion 3.8 -> 0.6 Mbps.
    // RDPIO_NVENC_MINQP=n another floor (0: none).
    const char *mq = getenv("RDPIO_NVENC_MINQP");
    uint32_t min_qp = mq && *mq ? (uint32_t)atoi(mq) : 12;
    if (min_qp > 0) {
        // AV1's quantizer index runs 0-255: 20 is where a still screen of
        // text settles as HEVC's QP 12 does (luma 50.5 dB against 49.9, for
        // 1.6 MB a second against 2.2; 32: 47.5 dB, 48: 44.8).
        uint32_t q = cfg->codec == 2 ? (min_qp * 5 + 1) / 3 : min_qp;
        c.rcParams.enableMinQP = 1;
        c.rcParams.minQP.qpInterP = q;
        c.rcParams.minQP.qpInterB = q;
        c.rcParams.minQP.qpIntra = q;
    }
    if (getenv("RDPIO_NVENC_AQ")) {
        c.rcParams.enableAQ = 1;
        c.rcParams.aqStrength = (uint32_t)atoi(getenv("RDPIO_NVENC_AQ"));
    }
    // Gradual refresh on request only: no periodic wave (the period is past
    // any session's length), each one spread over `intra_refresh` pictures.
    uint32_t ir = cfg->intra_refresh;
    uint32_t ir_period = 0x7fffffff;
    if (cfg->codec == 2) {
        // A plain low-overhead OBU stream (no Annex B, no timing info): each
        // picture one temporal unit, a sequence header on every keyframe.
        NV_ENC_CONFIG_AV1 *a = &c.encodeCodecConfig.av1Config;
        c.profileGUID = NV_ENC_AV1_PROFILE_MAIN_GUID;
        a->level = NV_ENC_LEVEL_AV1_AUTOSELECT;
        a->tier = NV_ENC_TIER_AV1_0;
        a->idrPeriod = NVENC_INFINITE_GOPLENGTH;
        a->repeatSeqHdr = 1;
        a->outputAnnexBFormat = 0;
        a->enableTimingInfo = 0;
        a->enableDecoderModelInfo = 0;
        a->chromaFormatIDC = 1;
#if NVENCAPI_MAJOR_VERSION >= 13
        a->inputBitDepth = NV_ENC_BIT_DEPTH_8;
        a->outputBitDepth = NV_ENC_BIT_DEPTH_8;
        // Long-term references, set per picture as for HEVC (API 13.0 on).
        if (cfg->ltr_frames) {
            a->enableLTR = 1;
            a->ltrNumFrames = cfg->ltr_frames;
        }
#else
        // (API 12.0 has no long-term references for AV1: a loss is repaired
        // with a keyframe.)
        a->inputPixelBitDepthMinus8 = 0;
        a->pixelBitDepthMinus8 = 0;
#endif
        a->colorPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        a->transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
        a->matrixCoefficients = NV_ENC_VUI_MATRIX_COEFFS_BT709;
        a->colorRange = 0;
        if (ir) {
            a->enableIntraRefresh = 1;
            a->intraRefreshPeriod = ir_period;
            a->intraRefreshCnt = ir;
        }
    } else if (cfg->codec) {
        NV_ENC_CONFIG_HEVC *h = &c.encodeCodecConfig.hevcConfig;
        c.profileGUID = NV_ENC_HEVC_PROFILE_MAIN_GUID;
        h->idrPeriod = NVENC_INFINITE_GOPLENGTH;
        h->repeatSPSPPS = 1;
        if (cfg->ltr_frames) {
            h->enableLTR = 1;
            h->ltrNumFrames = cfg->ltr_frames;
            h->ltrTrustMode = 0;
            // Room for the long-term references beside the short-term one.
            h->maxNumRefFramesInDPB = cfg->ltr_frames + 2;
        }
        if (ir) {
            h->enableIntraRefresh = 1;
            h->intraRefreshPeriod = ir_period;
            h->intraRefreshCnt = ir;
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
        NV_ENC_CONFIG_H264 *h = &c.encodeCodecConfig.h264Config;
        c.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
        h->idrPeriod = NVENC_INFINITE_GOPLENGTH;
        h->repeatSPSPPS = 1;
        if (cfg->ltr_frames) {
            h->enableLTR = 1;
            h->ltrNumFrames = cfg->ltr_frames;
            h->ltrTrustMode = 0;
            h->maxNumRefFrames = cfg->ltr_frames + 2;
        }
        if (ir) {
            h->enableIntraRefresh = 1;
            h->intraRefreshPeriod = ir_period;
            h->intraRefreshCnt = ir;
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
    *out = c;
    return 0;
}

// Open a session on `device` (an ID3D11Device*). 0 on success.
int RDP_FN(rdp_nvenc_open)(void *device, const RdpNvencConfig *cfg, RdpNvenc **out, char *err, int errlen) {
    *out = NULL;
    RdpNvenc *s = (RdpNvenc *)calloc(1, sizeof(RdpNvenc));
    if (!s) {
        return fail(NULL, err, errlen, "allocation", 0);
    }
    s->codec = cfg->codec;
    s->argb = cfg->argb;
    s->cfg = *cfg;
    s->dll = LoadLibraryW(L"nvEncodeAPI64.dll");
    if (!s->dll) {
        RDP_FN(rdp_nvenc_close)(s);
        return fail(NULL, err, errlen, "loading nvEncodeAPI64.dll (no NVIDIA driver)", 0);
    }
    MaxVersionFn max_version = (MaxVersionFn)GetProcAddress(s->dll, "NvEncodeAPIGetMaxSupportedVersion");
    CreateInstanceFn create = (CreateInstanceFn)GetProcAddress(s->dll, "NvEncodeAPICreateInstance");
    if (!max_version || !create) {
        RDP_FN(rdp_nvenc_close)(s);
        return fail(NULL, err, errlen, "finding the NVENC entry points", 0);
    }
    uint32_t have = 0;
    max_version(&have);
    uint32_t need = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;
    if (have < need) {
        RDP_FN(rdp_nvenc_close)(s);
        if (err && errlen > 0) {
            snprintf(err, (size_t)errlen, "the NVIDIA driver offers NVENC API %u.%u; %u.%u needs a newer driver", have >> 4, have & 0xf,
                     (unsigned)NVENCAPI_MAJOR_VERSION, (unsigned)NVENCAPI_MINOR_VERSION);
        }
        return -1;
    }
    s->fn.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    NVENCSTATUS st = create(&s->fn);
    if (st) {
        RDP_FN(rdp_nvenc_close)(s);
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
        RDP_FN(rdp_nvenc_close)(s);
        return fail(NULL, err, errlen, "opening an NVENC session", st);
    }

    // RDPIO_NVENC_TUNING=1 high quality, 2 low latency, 3 ultra-low latency
    // (the default), for measuring.
    s->tuning = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    if (getenv("RDPIO_NVENC_TUNING")) {
        s->tuning = (NV_ENC_TUNING_INFO)atoi(getenv("RDPIO_NVENC_TUNING"));
    }
    int r = configure(s, cfg->preset, &s->config, err, errlen);
    if (r) {
        RDP_FN(rdp_nvenc_close)(s);
        return r;
    }

    s->init.version = NV_ENC_INITIALIZE_PARAMS_VER;
    s->init.encodeGUID = codec_guid(cfg->codec);
    s->init.presetGUID = *preset_guid(cfg->preset);
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
    s->init.tuningInfo = s->tuning;
    s->init.encodeConfig = &s->config;
    st = s->fn.nvEncInitializeEncoder(s->enc, &s->init);
    if (st) {
        r = fail(s, err, errlen, "initializing the encoder", st);
        RDP_FN(rdp_nvenc_close)(s);
        return r;
    }
    NV_ENC_CREATE_BITSTREAM_BUFFER b = {0};
    b.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
    st = s->fn.nvEncCreateBitstreamBuffer(s->enc, &b);
    if (st) {
        r = fail(s, err, errlen, "creating the bitstream buffer", st);
        RDP_FN(rdp_nvenc_close)(s);
        return r;
    }
    s->bitstream = b.bitstreamBuffer;
    s->done = CreateEventW(NULL, FALSE, FALSE, NULL);
    NV_ENC_EVENT_PARAMS ev = {0};
    ev.version = NV_ENC_EVENT_PARAMS_VER;
    ev.completionEvent = s->done;
    st = s->done ? s->fn.nvEncRegisterAsyncEvent(s->enc, &ev) : NV_ENC_ERR_INVALID_EVENT;
    if (st) {
        r = fail(s, err, errlen, "registering the completion event", st);
        RDP_FN(rdp_nvenc_close)(s);
        return r;
    }
    s->done_registered = 1;
    *out = s;
    return 0;
}

// One of the encoder's capabilities (NV_ENC_CAPS) for the session's codec
// (-1: the driver wouldn't say).
int RDP_FN(rdp_nvenc_cap)(RdpNvenc *s, int32_t cap) {
    NV_ENC_CAPS_PARAM p = {0};
    p.version = NV_ENC_CAPS_PARAM_VER;
    p.capsToQuery = (NV_ENC_CAPS)cap;
    int v = -1;
    if (s->fn.nvEncGetEncodeCaps(s->enc, codec_guid(s->codec), &p, &v)) {
        return -1;
    }
    return v;
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

static double now_ms(void) {
    LARGE_INTEGER c, f;
    QueryPerformanceCounter(&c);
    QueryPerformanceFrequency(&f);
    return (double)c.QuadPart * 1000.0 / (double)f.QuadPart;
}

// Encode `texture` (an ID3D11Texture2D* of the session's size). The access
// unit is left in *data / *len (valid until the next call). 0 on success.
int RDP_FN(rdp_nvenc_encode)(RdpNvenc *s, void *texture, const RdpNvencFrame *f, const uint8_t **data, uint32_t *len, uint32_t *keyframe, uint32_t *ltr, char *err,
                             int errlen) {
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
    // A gradual refresh: only where the session was opened for it, never on
    // a keyframe (which refreshes everything at once).
    uint32_t wave = !f->force_idr && f->intra_refresh && s->cfg.intra_refresh ? f->intra_refresh : 0;
    if (s->codec == 2) {
        NV_ENC_PIC_PARAMS_AV1 *a = &p.codecPicParams.av1PicParams;
        a->forceIntraRefreshWithFrameCnt = wave;
#if NVENCAPI_MAJOR_VERSION >= 13
        if (s->cfg.ltr_frames) {
            if (f->ltr_mark >= 0) {
                a->ltrMarkFrame = 1;
                a->ltrMarkFrameIdx = (uint32_t)f->ltr_mark;
            }
            if (f->ltr_use_mask) {
                a->ltrUseFrames = 1;
                a->ltrUseFrameBitmap = f->ltr_use_mask;
            }
        }
#endif
    } else if (s->codec) {
        NV_ENC_PIC_PARAMS_HEVC *h = &p.codecPicParams.hevcPicParams;
        h->forceIntraRefreshWithFrameCnt = wave;
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
        h->forceIntraRefreshWithFrameCnt = wave;
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
    // (An AV1 key frame may come back as an I picture.)
    *keyframe = lock.pictureType == NV_ENC_PIC_TYPE_IDR || (s->codec == 2 && lock.pictureType == NV_ENC_PIC_TYPE_I);
    *ltr = lock.ltrFrame;
    s->fn.nvEncUnlockBitstream(s->enc, s->bitstream);
    s->fn.nvEncUnmapInputResource(s->enc, map.mappedResource);
    t4 = now_ms();
    if (trace) {
        fprintf(stderr, "nvenc: map %.2f encode %.2f wait %.2f lock+copy+unmap %.2f ms (%u bytes)\n", t1 - t0, t2 - t1, t3 - t2, t4 - t3, *len);
    }
    return 0;
}

static int reconfigure(RdpNvenc *s, NV_ENC_CONFIG *c, const GUID *preset, char *err, int errlen, const char *what) {
    NV_ENC_RECONFIGURE_PARAMS r = {0};
    r.version = NV_ENC_RECONFIGURE_PARAMS_VER;
    r.reInitEncodeParams = s->init;
    r.reInitEncodeParams.presetGUID = *preset;
    r.reInitEncodeParams.encodeConfig = c;
    r.resetEncoder = 0;
    r.forceIDR = 0;
    NVENCSTATUS st = s->fn.nvEncReconfigureEncoder(s->enc, &r);
    if (st) {
        return fail(s, err, errlen, what, st);
    }
    s->init.presetGUID = *preset;
    s->config = *c;
    s->init.encodeConfig = &s->config;
    return 0;
}

// A new bitrate (and largest frame), in place: no new session, no keyframe.
int RDP_FN(rdp_nvenc_set_bitrate)(RdpNvenc *s, uint32_t bitrate, uint32_t vbv_bytes, char *err, int errlen) {
    NV_ENC_CONFIG c = s->config;
    set_rate(&c, bitrate, vbv_bytes);
    int r = reconfigure(s, &c, &s->init.presetGUID, err, errlen, "changing the bitrate");
    if (!r) {
        s->cfg.bitrate = bitrate;
        s->cfg.vbv_bytes = vbv_bytes;
    }
    return r;
}

// Another preset (P1-P7), in place: the preset's whole configuration with
// rdpio's settings and the current bitrate on top, no new session, no
// keyframe (the references carry on).
int RDP_FN(rdp_nvenc_set_preset)(RdpNvenc *s, uint32_t preset, char *err, int errlen) {
    NV_ENC_CONFIG c;
    int r = configure(s, preset, &c, err, errlen);
    if (r) {
        return r;
    }
    r = reconfigure(s, &c, preset_guid(preset), err, errlen, "changing the preset");
    if (!r) {
        s->cfg.preset = preset;
    }
    return r;
}
