//! Direct3D 12 renderer (Windows).
//!
//! A modern low-overhead backend: a single command queue, flip-model swapchain
//! with tearing/VRR support, and compute-shader NV12→RGBA conversion. The D3D12
//! backend intentionally does not yet share a DXVA decode path with the worker
//! thread; CPU-decoded NV12 frames are uploaded and color-converted on the GPU.
#![allow(unused_imports)]

use std::collections::HashMap;

use windows::core::{Interface, Result as WinResult};
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Direct3D12::{
    D3D12CreateDevice, ID3D12CommandAllocator, ID3D12CommandQueue, ID3D12DescriptorHeap,
    ID3D12Device, ID3D12Fence, ID3D12GraphicsCommandList, ID3D12PipelineState, ID3D12Resource,
    ID3D12RootSignature, D3D12_BOX, D3D12_BUFFER_SRV, D3D12_BUFFER_SRV_FLAG_RAW,
    D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_COMMAND_QUEUE_DESC, D3D12_CPU_DESCRIPTOR_HANDLE,
    D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING, D3D12_DESCRIPTOR_HEAP_DESC,
    D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
    D3D12_DESCRIPTOR_RANGE, D3D12_DESCRIPTOR_RANGE_TYPE_SRV, D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
    D3D12_FENCE_FLAG_NONE, D3D12_HEAP_FLAG_NONE, D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_DEFAULT,
    D3D12_HEAP_TYPE_READBACK, D3D12_HEAP_TYPE_UPLOAD, D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    D3D12_RESOURCE_BARRIER, D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
    D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_BUFFER, D3D12_RESOURCE_DIMENSION_TEXTURE2D,
    D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_FLAG_NONE,
    D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST,
    D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_GENERIC_READ,
    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
    D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_TRANSITION_BARRIER,
    D3D12_ROOT_CONSTANTS, D3D12_ROOT_DESCRIPTOR_TABLE, D3D12_ROOT_PARAMETER,
    D3D12_ROOT_PARAMETER_0, D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
    D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE, D3D12_ROOT_SIGNATURE_DESC,
    D3D12_ROOT_SIGNATURE_FLAG_NONE, D3D12_SHADER_BYTECODE, D3D12_SHADER_RESOURCE_VIEW_DESC,
    D3D12_SHADER_RESOURCE_VIEW_DESC_0, D3D12_SRV_DIMENSION_BUFFER, D3D12_SUBRESOURCE_FOOTPRINT,
    D3D12_TEX2D_UAV, D3D12_TEXTURE_COPY_LOCATION, D3D12_TEXTURE_COPY_LOCATION_0,
    D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
    D3D12_TEXTURE_LAYOUT_ROW_MAJOR, D3D12_TEXTURE_LAYOUT_UNKNOWN,
    D3D12_UNORDERED_ACCESS_VIEW_DESC, D3D12_UNORDERED_ACCESS_VIEW_DESC_0,
    D3D12_UAV_DIMENSION_BUFFER, D3D12_UAV_DIMENSION_TEXTURE2D, D3D12_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_UNSPECIFIED, DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIAdapter1, IDXGIFactory2, IDXGISwapChain3,
    DXGI_CREATE_FACTORY_FLAGS, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE, DXGI_PRESENT,
    DXGI_PRESENT_ALLOW_TEARING, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

use crate::Upscaler;

/// HLSL compute shader: NV12 byte-address buffer → RGBA framebuffer.
///
/// Each thread writes one output pixel. The input buffer is the tightly packed
/// NV12 layout (`width*height` Y bytes followed by `width*height/2` interleaved
/// UV bytes). The conversion matches the BT.709 studio-range matrix used in the
/// D3D11 video-processor path.
const NV12_TO_RGBA_HLSL: &str = r#"
ByteAddressBuffer nv12 : register(t0);
RWTexture2D<float4> framebuffer : register(u0);

cbuffer Params : register(b0) {
    uint width;      // full NV12 frame width (the Y/UV stride)
    uint height;     // full frame height
    uint dest_x;     // desktop position of the frame origin
    uint dest_y;
    uint fb_width;
    uint fb_height;
    uint region_x;   // dirty region within the frame (one dispatch per region)
    uint region_y;
    uint region_w;
    uint region_h;
};

static const float3x3 yuv_to_rgb = {
    1.16438356,  0.0,        1.59602678,
    1.16438356, -0.39176229,-0.81296764,
    1.16438356,  2.01723214, 0.0
};

// ByteAddressBuffer loads whole 32-bit words at 4-byte-aligned addresses;
// extract the single byte the address actually names.
uint load_byte(uint addr) {
    return (nv12.Load(addr & ~3u) >> ((addr & 3u) * 8u)) & 0xffu;
}

[numthreads(8, 8, 1)]
void cs_main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= region_w || id.y >= region_h) return;
    uint sx = region_x + id.x;
    uint sy = region_y + id.y;
    if (sx >= width || sy >= height) return;
    uint dx = sx + dest_x;
    uint dy = sy + dest_y;
    if (dx >= fb_width || dy >= fb_height) return;

    float y = load_byte(sy * width + sx) / 255.0;

    uint uv_row = sy / 2;
    uint uv_col = (sx / 2) * 2;
    uint uv_base = width * height + uv_row * width + uv_col;
    float u = load_byte(uv_base) / 255.0 - 0.5;
    float v = load_byte(uv_base + 1) / 255.0 - 0.5;

    float3 yuv = float3(y - 16.0 / 255.0, u, v);
    float3 rgb = mul(yuv, yuv_to_rgb);
    framebuffer[uint2(dx, dy)] = float4(saturate(rgb), 1.0);
}
"#;

/// Compute-shader mirror of the D3D11 scale/sharpen kernels (see the D3D11
/// backend's `SCALE_HLSL` for the algorithm commentary): bilinear, Catmull-Rom
/// bicubic, FSR 1.0 EASU, nearest, and the FSR 1.0 RCAS sharpen. Each thread
/// writes one destination pixel; every tap is clamped to the `srcOff`/`srcSize`
/// slice so per-monitor slices never bleed across a seam. `Bilinear` exists
/// here because D3D12 has no VideoProcessor scale path.
const SCALE_CS_HLSL: &str = r#"
Texture2D<float4> src : register(t0);
RWTexture2D<float4> dst : register(u0);
SamplerState samp : register(s0);
cbuffer Params : register(b0) {
    float2 srcOff;      // slice top-left within src, texels
    float2 srcSize;     // slice size, texels
    float2 outSize;     // destination size, pixels
    float2 invTexSize;  // 1 / full src size
    float2 texSize;     // full src size
    float  sharpness;   // RCAS linear intensity (exp2(-stops))
    float  _pad;
};

float3 loadTexel(float2 p) {
    float2 c = clamp(p, srcOff, srcOff + srcSize - 1.0);
    return src.Load(int3(int2(c), 0)).rgb;
}

float fsrLuma(float3 c) { return 0.5 * c.r + c.g + 0.5 * c.b; }

[numthreads(8, 8, 1)]
void cs_bilinear(uint3 id : SV_DispatchThreadID) {
    if (id.x >= (uint)outSize.x || id.y >= (uint)outSize.y) return;
    float2 uv = (float2(id.xy) + 0.5) / outSize;
    float2 samplePos = srcOff + uv * srcSize;
    float2 lo = srcOff + 0.5;
    float2 hi = srcOff + srcSize - 0.5;
    float2 tuv = clamp(samplePos, lo, hi) * invTexSize;
    dst[id.xy] = float4(src.SampleLevel(samp, tuv, 0.0).rgb, 1.0);
}

[numthreads(8, 8, 1)]
void cs_nearest(uint3 id : SV_DispatchThreadID) {
    if (id.x >= (uint)outSize.x || id.y >= (uint)outSize.y) return;
    float2 uv = (float2(id.xy) + 0.5) / outSize;
    float2 samplePos = srcOff + uv * srcSize;
    dst[id.xy] = float4(loadTexel(floor(samplePos)), 1.0);
}

[numthreads(8, 8, 1)]
void cs_bicubic(uint3 id : SV_DispatchThreadID) {
    if (id.x >= (uint)outSize.x || id.y >= (uint)outSize.y) return;
    float2 uv = (float2(id.xy) + 0.5) / outSize;
    float2 samplePos = srcOff + uv * srcSize;
    float2 texPos1 = floor(samplePos - 0.5) + 0.5;
    float2 f = samplePos - texPos1;
    float2 w0 = f * (-0.5 + f * (1.0 - 0.5 * f));
    float2 w1 = 1.0 + f * f * (-2.5 + 1.5 * f);
    float2 w2 = f * (0.5 + f * (2.0 - 1.5 * f));
    float2 w3 = f * f * (-0.5 + 0.5 * f);
    float2 w12 = w1 + w2;
    float2 offset12 = w2 / w12;
    float2 lo = srcOff + 0.5;
    float2 hi = srcOff + srcSize - 0.5;
    float2 p0  = clamp(texPos1 - 1.0,      lo, hi) * invTexSize;
    float2 p3  = clamp(texPos1 + 2.0,      lo, hi) * invTexSize;
    float2 p12 = clamp(texPos1 + offset12, lo, hi) * invTexSize;
    float4 r = float4(0.0, 0.0, 0.0, 0.0);
    r += src.SampleLevel(samp, float2(p0.x,  p0.y),  0.0) * (w0.x  * w0.y);
    r += src.SampleLevel(samp, float2(p12.x, p0.y),  0.0) * (w12.x * w0.y);
    r += src.SampleLevel(samp, float2(p3.x,  p0.y),  0.0) * (w3.x  * w0.y);
    r += src.SampleLevel(samp, float2(p0.x,  p12.y), 0.0) * (w0.x  * w12.y);
    r += src.SampleLevel(samp, float2(p12.x, p12.y), 0.0) * (w12.x * w12.y);
    r += src.SampleLevel(samp, float2(p3.x,  p12.y), 0.0) * (w3.x  * w12.y);
    r += src.SampleLevel(samp, float2(p0.x,  p3.y),  0.0) * (w0.x  * w3.y);
    r += src.SampleLevel(samp, float2(p12.x, p3.y),  0.0) * (w12.x * w3.y);
    r += src.SampleLevel(samp, float2(p3.x,  p3.y),  0.0) * (w3.x  * w3.y);
    r.a = 1.0;
    dst[id.xy] = r;
}

void easuSet(inout float2 dir, inout float len, float w,
             float lA, float lB, float lC, float lD, float lE) {
    float dc = lD - lC;
    float cb = lC - lB;
    float lenX = max(abs(dc), abs(cb));
    lenX = 1.0 / max(lenX, 1.0 / 32768.0);
    float dirX = lD - lB;
    dir.x += dirX * w;
    lenX = saturate(abs(dirX) * lenX);
    lenX *= lenX;
    len += lenX * w;
    float ec = lE - lC;
    float ca = lC - lA;
    float lenY = max(abs(ec), abs(ca));
    lenY = 1.0 / max(lenY, 1.0 / 32768.0);
    float dirY = lE - lA;
    dir.y += dirY * w;
    lenY = saturate(abs(dirY) * lenY);
    lenY *= lenY;
    len += lenY * w;
}

void easuTap(inout float3 aC, inout float aW, float2 off, float2 dir,
             float2 len, float lob, float clp, float3 c) {
    float2 v;
    v.x = off.x * dir.x + off.y * dir.y;
    v.y = -off.x * dir.y + off.y * dir.x;
    v *= len;
    float d2 = min(v.x * v.x + v.y * v.y, clp);
    float wB = 0.4 * d2 - 1.0;
    float wA = lob * d2 - 1.0;
    wB *= wB;
    wA *= wA;
    wB = 1.5625 * wB - 0.5625;
    float w = wB * wA;
    aC += c * w;
    aW += w;
}

[numthreads(8, 8, 1)]
void cs_easu(uint3 id : SV_DispatchThreadID) {
    if (id.x >= (uint)outSize.x || id.y >= (uint)outSize.y) return;
    float2 uv = (float2(id.xy) + 0.5) / outSize;
    float2 pp = srcOff + uv * srcSize - 0.5;
    float2 fp = floor(pp);
    float2 f = pp - fp;
    float3 cB = loadTexel(fp + float2( 0.0, -1.0));
    float3 cC = loadTexel(fp + float2( 1.0, -1.0));
    float3 cE = loadTexel(fp + float2(-1.0,  0.0));
    float3 cF = loadTexel(fp + float2( 0.0,  0.0));
    float3 cG = loadTexel(fp + float2( 1.0,  0.0));
    float3 cH = loadTexel(fp + float2( 2.0,  0.0));
    float3 cI = loadTexel(fp + float2(-1.0,  1.0));
    float3 cJ = loadTexel(fp + float2( 0.0,  1.0));
    float3 cK = loadTexel(fp + float2( 1.0,  1.0));
    float3 cL = loadTexel(fp + float2( 2.0,  1.0));
    float3 cN = loadTexel(fp + float2( 0.0,  2.0));
    float3 cO = loadTexel(fp + float2( 1.0,  2.0));
    float lB = fsrLuma(cB); float lC = fsrLuma(cC);
    float lE = fsrLuma(cE); float lF = fsrLuma(cF);
    float lG = fsrLuma(cG); float lH = fsrLuma(cH);
    float lI = fsrLuma(cI); float lJ = fsrLuma(cJ);
    float lK = fsrLuma(cK); float lL = fsrLuma(cL);
    float lN = fsrLuma(cN); float lO = fsrLuma(cO);
    float2 dir = float2(0.0, 0.0);
    float len = 0.0;
    easuSet(dir, len, (1.0 - f.x) * (1.0 - f.y), lB, lE, lF, lG, lJ);
    easuSet(dir, len, f.x * (1.0 - f.y),         lC, lF, lG, lH, lK);
    easuSet(dir, len, (1.0 - f.x) * f.y,         lF, lI, lJ, lK, lN);
    easuSet(dir, len, f.x * f.y,                 lG, lJ, lK, lL, lO);
    float dirR = dir.x * dir.x + dir.y * dir.y;
    bool zro = dirR < (1.0 / 32768.0);
    dirR = rsqrt(max(dirR, 1.0 / 32768.0));
    dirR = zro ? 1.0 : dirR;
    dir.x = zro ? 1.0 : dir.x;
    dir *= dirR;
    len = len * 0.5;
    len *= len;
    float stretch = 1.0 / max(max(abs(dir.x), abs(dir.y)), 1.0 / 32768.0);
    float2 len2 = float2(1.0 + (stretch - 1.0) * len, 1.0 - 0.5 * len);
    float lob = 0.5 + ((1.0 / 4.0 - 0.04) - 0.5) * len;
    float clp = 1.0 / max(lob, 1.0 / 32768.0);
    float3 min4 = min(min(cF, cG), min(cJ, cK));
    float3 max4 = max(max(cF, cG), max(cJ, cK));
    float3 aC = float3(0.0, 0.0, 0.0);
    float aW = 0.0;
    easuTap(aC, aW, float2( 0.0, -1.0) - f, dir, len2, lob, clp, cB);
    easuTap(aC, aW, float2( 1.0, -1.0) - f, dir, len2, lob, clp, cC);
    easuTap(aC, aW, float2(-1.0,  0.0) - f, dir, len2, lob, clp, cE);
    easuTap(aC, aW, float2( 0.0,  0.0) - f, dir, len2, lob, clp, cF);
    easuTap(aC, aW, float2( 1.0,  0.0) - f, dir, len2, lob, clp, cG);
    easuTap(aC, aW, float2( 2.0,  0.0) - f, dir, len2, lob, clp, cH);
    easuTap(aC, aW, float2(-1.0,  1.0) - f, dir, len2, lob, clp, cI);
    easuTap(aC, aW, float2( 0.0,  1.0) - f, dir, len2, lob, clp, cJ);
    easuTap(aC, aW, float2( 1.0,  1.0) - f, dir, len2, lob, clp, cK);
    easuTap(aC, aW, float2( 2.0,  1.0) - f, dir, len2, lob, clp, cL);
    easuTap(aC, aW, float2( 0.0,  2.0) - f, dir, len2, lob, clp, cN);
    easuTap(aC, aW, float2( 1.0,  2.0) - f, dir, len2, lob, clp, cO);
    float3 pix = min(max4, max(min4, aC * (1.0 / aW)));
    dst[id.xy] = float4(pix, 1.0);
}

[numthreads(8, 8, 1)]
void cs_rcas(uint3 id : SV_DispatchThreadID) {
    if (id.x >= (uint)outSize.x || id.y >= (uint)outSize.y) return;
    float2 ip = srcOff + float2(id.xy);
    float3 b = loadTexel(ip + float2( 0.0, -1.0));
    float3 d = loadTexel(ip + float2(-1.0,  0.0));
    float3 e = loadTexel(ip);
    float3 f = loadTexel(ip + float2( 1.0,  0.0));
    float3 h = loadTexel(ip + float2( 0.0,  1.0));
    float bL = fsrLuma(b);
    float dL = fsrLuma(d);
    float eL = fsrLuma(e);
    float fL = fsrLuma(f);
    float hL = fsrLuma(h);
    float nz = 0.25 * (bL + dL + fL + hL) - eL;
    float rangeMax = max(max(max(bL, dL), max(eL, fL)), hL);
    float rangeMin = min(min(min(bL, dL), min(eL, fL)), hL);
    nz = saturate(abs(nz) / max(rangeMax - rangeMin, 1.0 / 32768.0));
    nz = -0.5 * nz + 1.0;
    float3 mn4 = min(min(b, d), min(f, h));
    float3 mx4 = max(max(b, d), max(f, h));
    float2 peakC = float2(1.0, -4.0);
    float3 hitMin = mn4 / max(4.0 * mx4, 1.0 / 32768.0);
    float3 hitMax = (peakC.x - mx4) / (4.0 * mn4 + peakC.y - (1.0 / 32768.0));
    float3 lobeRGB = max(-hitMin, hitMax);
    float lobe = max(-0.1875, min(max(max(lobeRGB.r, lobeRGB.g), lobeRGB.b), 0.0)) * sharpness;
    lobe *= nz;
    float rcpL = 1.0 / (4.0 * lobe + 1.0);
    float3 pix = ((b + d + f + h) * lobe + e) * rcpL;
    dst[id.xy] = float4(pix, 1.0);
}
"#;

/// One extra per-monitor present target sharing the same D3D12 device and
/// command queue. When `src_size` differs from the window (`width`×`height`)
/// — render-scale under per-monitor — the slice is upscaled on present.
struct PresentTarget {
    swap_chain: IDXGISwapChain3,
    width: u32,
    height: u32,
    frame_wait: Option<HANDLE>,
    tearing: bool,
    src: (u32, u32),
    src_size: (u32, u32),
}

/// Which compute kernel a scale dispatch runs (the D3D12 mirror of the D3D11
/// pixel-shader kernels; `Bilinear` exists here because D3D12 has no
/// VideoProcessor path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ScaleKernel {
    Bilinear,
    Bicubic,
    Easu,
    Nearest,
    Rcas,
}

impl ScaleKernel {
    fn entry(self) -> windows::core::PCSTR {
        match self {
            Self::Bilinear => windows::core::s!("cs_bilinear"),
            Self::Bicubic => windows::core::s!("cs_bicubic"),
            Self::Easu => windows::core::s!("cs_easu"),
            Self::Nearest => windows::core::s!("cs_nearest"),
            Self::Rcas => windows::core::s!("cs_rcas"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Bilinear => "bilinear (compute)",
            Self::Bicubic => "Catmull-Rom bicubic (compute)",
            Self::Easu => "FSR 1.0 EASU (compute)",
            Self::Nearest => "nearest-neighbour (compute)",
            Self::Rcas => "FSR 1.0 RCAS sharpen (compute)",
        }
    }
}

/// Direct3D 12 renderer.
pub struct D3D12Renderer {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    // The current in-flight command list, if one has been opened.
    list: Option<ID3D12GraphicsCommandList>,
    swap_chain: IDXGISwapChain3,
    sc_width: u32,
    sc_height: u32,
    tearing: bool,
    low_latency: bool,
    sc_flags: u32,
    frame_wait: Option<HANDLE>,
    fence: ID3D12Fence,
    fence_value: u64,
    fence_event: HANDLE,
    /// Desktop-sized RGBA framebuffer (UNORDERED_ACCESS for compute writes).
    framebuffer: Option<ID3D12Resource>,
    fb_width: u32,
    fb_height: u32,
    /// Scratch texture used for safe framebuffer→framebuffer copies when source
    /// and destination rectangles overlap.
    copy_scratch: Option<ID3D12Resource>,
    /// GPU cache slot → (w, h, texture).
    gfx_cache: HashMap<u16, (u32, u32, ID3D12Resource)>,
    /// Compute conversion pipeline.
    root_signature: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    descriptor_heap: ID3D12DescriptorHeap,
    descriptor_size: usize,
    /// Constant buffer for the compute shader (`width`, `height`, `dest_x`,
    /// `dest_y`, `fb_width`, `fb_height`).
    constant_buffer: ID3D12Resource,
    cb_addr: *mut u8,
    /// Upload heap for NV12 frames and RGBA rectangles.
    upload_buffer: Option<ID3D12Resource>,
    upload_capacity: usize,
    upload_addr: *mut u8,
    /// Current NV12 input size and its default-heap resource (byte-address buffer).
    nv12_input: Option<ID3D12Resource>,
    nv12_capacity: usize,
    upscaler: Upscaler,
    /// RCAS sharpen strength `0.0..=1.0` (`0.0` = off), see [`Self::set_sharpen`].
    sharpen: f32,
    primary_src: (u32, u32),
    /// Primary swapchain's framebuffer slice size in per-monitor mode
    /// (`None` = swapchain-sized, 1:1).
    primary_src_size: Option<(u32, u32)>,
    /// Lazily-built compute upscale pipeline: a root signature (with a static
    /// linear-clamp sampler) plus one PSO per [`ScaleKernel`]. `scale_disabled`
    /// latches scaling off after any failure — crop copies from then on.
    scale_root: Option<ID3D12RootSignature>,
    scale_psos: HashMap<ScaleKernel, ID3D12PipelineState>,
    scale_disabled: bool,
    /// Intermediate UAV textures for the scale/sharpen chain, keyed by size
    /// (`mid_a` holds the upscale output; `mid_b` the sharpened image).
    mid_a: HashMap<(u32, u32), ID3D12Resource>,
    mid_b: HashMap<(u32, u32), ID3D12Resource>,
    /// Rotating descriptor cursor for scale passes — slots 2.. of the heap
    /// (0/1 belong to the NV12 conversion).
    desc_cursor: usize,
    extra_targets: Vec<PresentTarget>,
    gpu_timing_cb: Option<Box<dyn Fn(&str, u64) + Send + Sync>>,
}

/// Round `v` up to the next multiple of `align`.
fn align_up(v: usize, align: usize) -> usize {
    (v + align - 1) & !(align - 1)
}

impl D3D12Renderer {
    /// Create a D3D12 device and low-latency flip-model swapchain for `hwnd`.
    pub fn new(hwnd: HWND, width: u32, height: u32) -> WinResult<Self> {
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let adapter = Self::high_performance_adapter(&factory)
                .ok_or_else(windows::core::Error::from_thread)?;

            let adapter_unk: windows::core::IUnknown = adapter.cast()?;
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(Some(&adapter_unk), D3D_FEATURE_LEVEL_11_0, &mut device)?;
            let device = device.ok_or_else(windows::core::Error::from_thread)?;

            let queue_desc = D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            };
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&queue_desc)?;

            let allocator: ID3D12CommandAllocator =
                device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;

            // Best → good → safe swapchain flags, mirroring the D3D11 path.
            let waitable = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32;
            let tearing_flag = DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32;
            let make = |flags: u32| {
                let desc = DXGI_SWAP_CHAIN_DESC1 {
                    Width: width,
                    Height: height,
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    Stereo: false.into(),
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    BufferCount: 2,
                    Scaling: DXGI_SCALING_STRETCH,
                    SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                    AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
                    Flags: flags,
                };
                factory.CreateSwapChainForHwnd(&queue,
                    hwnd,
                    &desc,
                    None,
                    None,
                )
            };
            let (swap_chain1, sc_flags, tearing) = match make(waitable | tearing_flag) {
                Ok(s) => (s, waitable | tearing_flag, true),
                Err(_) => match make(waitable) {
                    Ok(s) => (s, waitable, false),
                    Err(_) => (make(0)?, 0u32, false),
                },
            };
            let swap_chain: IDXGISwapChain3 = swap_chain1.cast()?;
            let mut frame_wait = None;
            if sc_flags & waitable != 0 {
                let _ = swap_chain.SetMaximumFrameLatency(1);
                let h = swap_chain.GetFrameLatencyWaitableObject();
                if !h.is_invalid() {
                    frame_wait = Some(h);
                }
            }

            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            let fence_event = windows::Win32::System::Threading::CreateEventA(
                None,
                false,
                false,
                None,
            )?;

            let (root_signature, pso) = Self::create_compute_pipeline(&device)?;
            let (descriptor_heap, descriptor_size) = Self::create_descriptor_heap(&device)?;
            let constant_buffer = Self::create_upload_buffer(&device, 256)?;
            let mut cb_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
            constant_buffer.Map(0, None, Some(&mut cb_ptr))?;
            let cb_addr = cb_ptr as *mut u8;

            tracing::info!(
                width,
                height,
                waitable = frame_wait.is_some(),
                tearing,
                "D3D12 device + swapchain created"
            );

            Ok(Self {
                device,
                queue,
                allocator,
                list: None,
                swap_chain,
                sc_width: width,
                sc_height: height,
                tearing,
                low_latency: false,
                sc_flags,
                frame_wait,
                fence,
                fence_value: 0,
                fence_event,
                framebuffer: None,
                fb_width: 0,
                fb_height: 0,
                copy_scratch: None,
                gfx_cache: HashMap::new(),
                root_signature,
                pso,
                descriptor_heap,
                descriptor_size,
                constant_buffer,
                cb_addr,
                upload_buffer: None,
                upload_capacity: 0,
                upload_addr: std::ptr::null_mut(),
                nv12_input: None,
                nv12_capacity: 0,
                upscaler: Upscaler::default(),
                sharpen: 0.0,
                primary_src: (0, 0),
                primary_src_size: None,
                scale_root: None,
                scale_psos: HashMap::new(),
                scale_disabled: false,
                mid_a: HashMap::new(),
                mid_b: HashMap::new(),
                desc_cursor: 2,
                extra_targets: Vec::new(),
                gpu_timing_cb: None,
            })
        }
    }

    fn high_performance_adapter(factory: &IDXGIFactory2) -> Option<IDXGIAdapter1> {
        unsafe {
            let factory6 = factory.cast::<windows::Win32::Graphics::Dxgi::IDXGIFactory6>().ok()?;
            let adapter: IDXGIAdapter1 = factory6
                .EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
                .ok()?;
            if let Ok(desc) = adapter.GetDesc1() {
                let end = desc
                    .Description
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(desc.Description.len());
                let name = String::from_utf16_lossy(&desc.Description[..end]);
                tracing::info!(adapter = %name, "selecting high-performance GPU adapter");
            }
            Some(adapter)
        }
    }

    fn create_compute_pipeline(device: &ID3D12Device) -> WinResult<(ID3D12RootSignature, ID3D12PipelineState)> {
        unsafe {
            // Root signature: constants (b0), SRV descriptor table (t0), UAV
            // descriptor table (u0).
            let ranges = [
                windows::Win32::Graphics::Direct3D12::D3D12_DESCRIPTOR_RANGE {
                    RangeType: windows::Win32::Graphics::Direct3D12::D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                    NumDescriptors: 1,
                    BaseShaderRegister: 0,
                    RegisterSpace: 0,
                    OffsetInDescriptorsFromTableStart: 0,
                },
                windows::Win32::Graphics::Direct3D12::D3D12_DESCRIPTOR_RANGE {
                    RangeType: windows::Win32::Graphics::Direct3D12::D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
                    NumDescriptors: 1,
                    BaseShaderRegister: 0,
                    RegisterSpace: 0,
                    OffsetInDescriptorsFromTableStart: 1,
                },
            ];
            let params = [
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                    Anonymous: windows::Win32::Graphics::Direct3D12::D3D12_ROOT_PARAMETER_0 {
                        Constants: windows::Win32::Graphics::Direct3D12::D3D12_ROOT_CONSTANTS {
                            ShaderRegister: 0,
                            RegisterSpace: 0,
                            Num32BitValues: 10,
                        },
                    },
                    ShaderVisibility: windows::Win32::Graphics::Direct3D12::D3D12_SHADER_VISIBILITY_ALL,
                },
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                    Anonymous: windows::Win32::Graphics::Direct3D12::D3D12_ROOT_PARAMETER_0 {
                        DescriptorTable: windows::Win32::Graphics::Direct3D12::D3D12_ROOT_DESCRIPTOR_TABLE {
                            NumDescriptorRanges: ranges.len() as u32,
                            pDescriptorRanges: ranges.as_ptr(),
                        },
                    },
                    ShaderVisibility: windows::Win32::Graphics::Direct3D12::D3D12_SHADER_VISIBILITY_ALL,
                },
            ];
            let desc = D3D12_ROOT_SIGNATURE_DESC {
                NumParameters: params.len() as u32,
                pParameters: params.as_ptr(),
                NumStaticSamplers: 0,
                pStaticSamplers: std::ptr::null(),
                Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
            };
            let mut signature: Option<ID3DBlob> = None;
            let mut error: Option<ID3DBlob> = None;
            windows::Win32::Graphics::Direct3D12::D3D12SerializeRootSignature(
                &desc,
                windows::Win32::Graphics::Direct3D12::D3D_ROOT_SIGNATURE_VERSION_1_0,
                &mut signature,
                Some(&mut error),
            )?;
            if let Some(err) = error {
                let slice = core::slice::from_raw_parts(
                    err.GetBufferPointer() as *const u8,
                    err.GetBufferSize(),
                );
                tracing::warn!(error = %String::from_utf8_lossy(slice), "D3D12 root signature error");
            }
            let signature = signature.ok_or_else(windows::core::Error::from_thread)?;
            let signature_bytes = core::slice::from_raw_parts(
                signature.GetBufferPointer() as *const u8,
                signature.GetBufferSize(),
            );
            let root_signature: ID3D12RootSignature =
                device.CreateRootSignature(0, signature_bytes)?;

            let cs_blob = compile_compute_shader()?;
            let cs_bytes = core::slice::from_raw_parts(
                cs_blob.GetBufferPointer() as *const u8,
                cs_blob.GetBufferSize(),
            );
            let pso_desc = windows::Win32::Graphics::Direct3D12::D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: core::mem::ManuallyDrop::new(Some(root_signature.clone())),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: cs_bytes.as_ptr() as *const _,
                    BytecodeLength: cs_bytes.len(),
                },
                ..Default::default()
            };
            let pso: ID3D12PipelineState = device.CreateComputePipelineState(&pso_desc)?;
            Ok((root_signature, pso))
        }
    }

    fn create_descriptor_heap(device: &ID3D12Device) -> WinResult<(ID3D12DescriptorHeap, usize)> {
        unsafe {
            let desc = D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                // 0/1: NV12 conversion. 2..: rotating SRV/UAV pairs for the
                // scale/sharpen passes (up to two per present surface per frame).
                NumDescriptors: 64,
                Flags: windows::Win32::Graphics::Direct3D12::D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            };
            let heap: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&desc)?;
            let size = device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            Ok((heap, size))
        }
    }

    fn create_upload_buffer(device: &ID3D12Device, size: usize) -> WinResult<ID3D12Resource> {
        unsafe {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Alignment: 0,
                Width: size as u64,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_UNKNOWN,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: windows::Win32::Graphics::Direct3D12::D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: D3D12_RESOURCE_FLAG_NONE,
            };
            let props = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_UPLOAD,
                ..Default::default()
            };
            let mut resource: Option<ID3D12Resource> = None;
            device.CreateCommittedResource(
                &props,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                None,
                &mut resource,
            )?;
            resource.ok_or_else(windows::core::Error::from_thread)
        }
    }

    /// Open a new command list for this frame, resetting the allocator first if
    /// this is the first list after a submit. The allocator may only be reset
    /// once its previous list has finished on the GPU, so wait out the fence
    /// signalled by [`Self::flush_list`]/[`Self::submit_and_wait`] first.
    fn begin_list(&mut self) -> WinResult<()> {
        unsafe {
            if self.list.is_none() {
                self.wait_for_fence();
                self.allocator.Reset()?;
                let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                    0,
                    D3D12_COMMAND_LIST_TYPE_DIRECT,
                    &self.allocator,
                    None,
                )?;
                self.list = Some(list);
            }
            Ok(())
        }
    }

    /// Block (bounded) until every signalled fence value has completed.
    fn wait_for_fence(&self) {
        unsafe {
            if self.fence.GetCompletedValue() < self.fence_value
                && self
                    .fence
                    .SetEventOnCompletion(self.fence_value, self.fence_event)
                    .is_ok()
            {
                let _ = windows::Win32::System::Threading::WaitForSingleObject(
                    self.fence_event,
                    5000,
                );
            }
        }
    }

    /// Close and execute the current command list without blocking the CPU,
    /// signalling the fence so the next [`Self::begin_list`] can safely reset
    /// the allocator once the GPU is done.
    fn flush_list(&mut self) -> WinResult<()> {
        unsafe {
            let list = match self.list.take() {
                Some(l) => l,
                None => return Ok(()),
            };
            list.Close()?;
            let lists = [Some(
                list.cast::<windows::Win32::Graphics::Direct3D12::ID3D12CommandList>()?,
            )];
            self.queue.ExecuteCommandLists(&lists);
            self.fence_value += 1;
            self.queue.Signal(&self.fence, self.fence_value)?;
            Ok(())
        }
    }

    /// Close the current command list, execute it, and wait for completion.
    fn submit_and_wait(&mut self) -> WinResult<()> {
        unsafe {
            let list = match self.list.take() {
                Some(l) => l,
                None => return Ok(()),
            };
            list.Close()?;
            let lists = [Some(list.cast::<windows::Win32::Graphics::Direct3D12::ID3D12CommandList>()?)];
            self.queue.ExecuteCommandLists(&lists);
            self.fence_value += 1;
            self.queue.Signal(&self.fence, self.fence_value)?;
            if self.fence.GetCompletedValue() < self.fence_value {
                self.fence.SetEventOnCompletion(self.fence_value, self.fence_event)?;
                let _ = windows::Win32::System::Threading::WaitForSingleObject(
                    self.fence_event,
                    5000,
                );
            }
            Ok(())
        }
    }

    /// Ensure `self.upload_buffer` can hold at least `size` bytes.
    fn ensure_upload(&mut self, size: usize) -> WinResult<()> {
        if self.upload_capacity >= size {
            return Ok(());
        }
        unsafe {
            if let Some(old) = self.upload_buffer.take() {
                if !self.upload_addr.is_null() {
                    old.Unmap(0, None);
                }
            }
        }
        let new_size = size.max(1024 * 1024);
        let buf = Self::create_upload_buffer(&self.device, new_size)?;
        unsafe {
            let mut upload_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
            buf.Map(0, None, Some(&mut upload_ptr))?;
            self.upload_addr = upload_ptr as *mut u8;
        }
        self.upload_buffer = Some(buf);
        self.upload_capacity = new_size;
        Ok(())
    }

    fn wait_for_frame(&self) {
        if let Some(h) = self.frame_wait {
            unsafe {
                let _ = windows::Win32::System::Threading::WaitForSingleObjectEx(h, 100, false);
            }
        }
    }

    pub fn set_low_latency(&mut self, on: bool) {
        if on && !self.tearing {
            tracing::warn!("D3D12: low-latency tearing present requested but unsupported");
        }
        self.low_latency = on && self.tearing;
        tracing::info!(low_latency = self.low_latency, "D3D12 present mode set");
    }

    pub fn set_upscaler(&mut self, mode: Upscaler) {
        // The driver AI super-resolution rides the D3D11 VideoProcessor, which
        // this backend doesn't use — substitute the strongest shader kernel.
        self.upscaler = if mode == Upscaler::Vsr {
            tracing::info!("D3D12: AI video SR needs the D3D11 backend; using FSR (EASU) instead");
            Upscaler::Fsr
        } else {
            mode
        };
        tracing::info!(mode = ?self.upscaler, "D3D12 upscaler selected");
    }

    /// Set the RCAS adaptive-sharpen strength (`0.0` = off, `1.0` = maximum).
    pub fn set_sharpen(&mut self, strength: f32) {
        self.sharpen = strength.clamp(0.0, 1.0);
        if self.sharpen > 0.0 {
            tracing::info!(strength = self.sharpen, "D3D12 RCAS sharpen enabled");
        }
    }

    /// Set the framebuffer slice the primary swapchain presents from (offset +
    /// size; `src_w`/`src_h` of 0 = swapchain-sized, 1:1).
    pub fn set_primary_src(&mut self, x: u32, y: u32, src_w: u32, src_h: u32) {
        self.primary_src = (x, y);
        self.primary_src_size = (src_w != 0 && src_h != 0).then_some((src_w, src_h));
    }

    /// Resize the swapchain backbuffers.
    pub fn resize(&mut self, width: u32, height: u32) -> WinResult<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        unsafe {
            self.submit_and_wait()?;
            self.swap_chain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(self.sc_flags as i32),
            )?;
            self.sc_width = width;
            self.sc_height = height;
            // Old destination-sized intermediates are stale now.
            self.mid_a.clear();
            self.mid_b.clear();
            Ok(())
        }
    }

    /// Allocate (or reallocate) the desktop framebuffer.
    pub fn ensure_framebuffer(&mut self, width: u32, height: u32) -> WinResult<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        if self.framebuffer.is_some() && self.fb_width == width && self.fb_height == height {
            return Ok(());
        }
        self.submit_and_wait()?;
        unsafe {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Alignment: 0,
                Width: width as u64,
                Height: height,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: windows::Win32::Graphics::Direct3D12::D3D12_TEXTURE_LAYOUT_UNKNOWN,
                Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            };
            let props = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let mut tex: Option<ID3D12Resource> = None;
            self.device.CreateCommittedResource(
                &props,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut tex,
            )?;
            self.framebuffer = tex;
        }
        self.fb_width = width;
        self.fb_height = height;
        self.copy_scratch = None;
        self.gfx_cache.clear();
        tracing::debug!(width, height, "D3D12 framebuffer (re)allocated");
        Ok(())
    }

    fn transition(
        list: &ID3D12GraphicsCommandList,
        resource: &ID3D12Resource,
        before: windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES,
        after: windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES,
    ) {
        unsafe {
            let barrier = D3D12_RESOURCE_BARRIER {
                Type: windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                Anonymous: windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER_0 {
                    Transition: core::mem::ManuallyDrop::new(
                        windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_TRANSITION_BARRIER {
                            pResource: core::mem::ManuallyDrop::new(Some(resource.clone())),
                            Subresource: windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                            StateBefore: before,
                            StateAfter: after,
                        },
                    ),
                },
                ..Default::default()
            };
            list.ResourceBarrier(&[barrier]);
        }
    }

    /// Upload an RGBA rectangle into the framebuffer.
    pub fn update_rect(
        &mut self,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        rgba: &[u8],
    ) {
        if self.framebuffer.is_none() {
            let _ = self.ensure_framebuffer((x as u32 + w as u32).max(1), (y as u32 + h as u32).max(1));
        }
        let (x, y, w, h) = (x as u32, y as u32, w as u32, h as u32);
        if w == 0 || h == 0 || x >= self.fb_width || y >= self.fb_height {
            return;
        }
        let row_pitch = w * 4;
        let need = (row_pitch * h) as usize;
        if rgba.len() < need {
            tracing::warn!(have = rgba.len(), need, "D3D12 short bitmap buffer; dropping rect");
            return;
        }
        let cw = w.min(self.fb_width - x);
        let ch = h.min(self.fb_height - y);
        if cw == 0 || ch == 0 {
            return;
        }
        if let Err(e) = self.ensure_upload(need) {
            tracing::warn!(error = %e, "D3D12 upload buffer allocation failed");
            return;
        }
        let Some(fb) = self.framebuffer.clone() else { return };
        let Some(upload) = self.upload_buffer.clone() else { return };
        unsafe {
            for row in 0..ch {
                let src = (row * w * 4) as usize;
                let dst = (row * cw * 4) as usize;
                std::ptr::copy_nonoverlapping(
                    rgba.as_ptr().add(src),
                    self.upload_addr.add(dst),
                    (cw * 4) as usize,
                );
            }
            let _ = self.begin_list();
            let list = self.list.as_ref().unwrap();
            Self::transition(
                list,
                &fb,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST,
            );
            let src_location = D3D12_TEXTURE_COPY_LOCATION {
                pResource: core::mem::ManuallyDrop::new(Some(upload)),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: windows::Win32::Graphics::Direct3D12::D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                            Width: cw,
                            Height: ch,
                            Depth: 1,
                            RowPitch: cw * 4,
                        },
                    },
                },
            };
            let dst_location = D3D12_TEXTURE_COPY_LOCATION {
                pResource: core::mem::ManuallyDrop::new(Some(fb.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            list.CopyTextureRegion(
                &dst_location,
                x,
                y,
                0,
                &src_location,
                Some(&D3D12_BOX {
                    left: 0,
                    top: 0,
                    front: 0,
                    right: cw,
                    bottom: ch,
                    back: 1,
                },
                ),
            );
            Self::transition(
                list,
                &fb,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_COMMON,
            );
        }
    }

    /// Convert an NV12 frame to RGBA on the GPU and write it into the framebuffer.
    pub fn blit_nv12(
        &mut self,
        dest_x: u32,
        dest_y: u32,
        w: u32,
        h: u32,
        nv12: &[u8],
        regions: &[(u32, u32, u32, u32)],
    ) -> bool {
        if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
            return false;
        }
        let need = (w * h + w * (h / 2)) as usize;
        if nv12.len() < need {
            return false;
        }
        if self.framebuffer.is_none() {
            let _ = self.ensure_framebuffer((dest_x + w).max(1), (dest_y + h).max(1));
        }
        if self.fb_width == 0 || self.fb_height == 0 {
            return false;
        }
        if dest_x >= self.fb_width || dest_y >= self.fb_height {
            return false;
        }
        let cw = w.min(self.fb_width - dest_x);
        let ch = h.min(self.fb_height - dest_y);
        if cw == 0 || ch == 0 {
            return false;
        }
        if let Err(e) = self.ensure_nv12_buffer(need) {
            tracing::warn!(error = %e, "D3D12 NV12 buffer allocation failed");
            return false;
        }
        let Some(fb) = self.framebuffer.clone() else { return false };
        let Some(nv12_buf) = self.nv12_input.clone() else { return false };
        let Some(upload) = self.upload_buffer.clone() else { return false };
        let cb_addr = self.cb_addr;
        let descriptor_heap = self.descriptor_heap.clone();
        let descriptor_size = self.descriptor_size;
        let root_signature = self.root_signature.clone();
        let pso = self.pso.clone();
        let device = self.device.clone();
        let fb_width = self.fb_width;
        unsafe {
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), self.upload_addr, need);
            let _ = self.begin_list();
            let list = self.list.as_ref().unwrap();

            Self::transition(
                list,
                &nv12_buf,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST,
            );
            let src_location = D3D12_TEXTURE_COPY_LOCATION {
                pResource: core::mem::ManuallyDrop::new(Some(upload)),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: windows::Win32::Graphics::Direct3D12::D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT_UNKNOWN,
                            Width: need as u32,
                            Height: 1,
                            Depth: 1,
                            RowPitch: need as u32,
                        },
                    },
                },
            };
            let dst_location = D3D12_TEXTURE_COPY_LOCATION {
                pResource: core::mem::ManuallyDrop::new(Some(nv12_buf.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            list.CopyTextureRegion(
                &dst_location,
                0,
                0,
                0,
                &src_location,
                None,
            );
            Self::transition(
                list,
                &nv12_buf,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );

            Self::transition(
                list,
                &fb,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );

            list.SetComputeRootSignature(&root_signature);
            list.SetPipelineState(&pso);
            list.SetDescriptorHeaps(&[Some(descriptor_heap.clone())]);
            let _ = cb_addr; // constants are recorded per region below
            let heap_start = descriptor_heap.GetCPUDescriptorHandleForHeapStart();
            let srv_handle = heap_start;
            let uav_handle = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: heap_start.ptr + descriptor_size,
            };
            device.CreateShaderResourceView(
                &nv12_buf,
                Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                    Shader4ComponentMapping: windows::Win32::Graphics::Direct3D12::D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Buffer: windows::Win32::Graphics::Direct3D12::D3D12_BUFFER_SRV {
                            FirstElement: 0,
                            NumElements: (need / 4).max(1) as u32,
                            Flags: windows::Win32::Graphics::Direct3D12::D3D12_BUFFER_SRV_FLAG_RAW,
                            ..Default::default()
                        },
                    },
                },
                ),
                srv_handle,
            );
            device.CreateUnorderedAccessView(
                &fb,
                None,
                Some(
                &D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Texture2D: D3D12_TEX2D_UAV {
                            MipSlice: 0,
                            PlaneSlice: 0,
                        },
                    },
                },
                ),
                uav_handle,
            );
            let gpu_start = descriptor_heap.GetGPUDescriptorHandleForHeapStart();
            let gpu_table = windows::Win32::Graphics::Direct3D12::D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: gpu_start.ptr,
            };
            list.SetComputeRootDescriptorTable(1, gpu_table);

            // One dispatch per dirty region (whole frame when none given):
            // painting only the region rects keeps out-of-region reference
            // content from stomping fresher pixels other codecs painted.
            let whole = [(0u32, 0u32, cw, ch)];
            let regions = if regions.is_empty() { &whole[..] } else { regions };
            for &(rx, ry, rw, rh) in regions {
                if rx >= cw || ry >= ch {
                    continue;
                }
                let rw = rw.min(cw - rx);
                let rh = rh.min(ch - ry);
                if rw == 0 || rh == 0 {
                    continue;
                }
                let params = [
                    w,
                    h,
                    dest_x,
                    dest_y,
                    fb_width,
                    self.fb_height,
                    rx,
                    ry,
                    rw,
                    rh,
                ];
                list.SetComputeRoot32BitConstants(
                    0,
                    params.len() as u32,
                    params.as_ptr() as *const core::ffi::c_void,
                    0,
                );
                list.Dispatch(rw.div_ceil(8), rh.div_ceil(8), 1);
            }

            Self::transition(
                list,
                &fb,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COMMON,
            );
        }
        true
    }

    fn ensure_nv12_buffer(&mut self, size: usize) -> WinResult<()> {
        if self.nv12_capacity >= size {
            return Ok(());
        }
        unsafe {
            let aligned = align_up(size, 1024);
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Alignment: 0,
                Width: aligned as u64,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_UNKNOWN,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: windows::Win32::Graphics::Direct3D12::D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            };
            let props = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let mut buf: Option<ID3D12Resource> = None;
            self.device.CreateCommittedResource(
                &props,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut buf,
            )?;
            self.nv12_input = buf;
            self.nv12_capacity = aligned;
            Ok(())
        }
    }

    /// GPU NV12 texture blit is not implemented for D3D12 in this version.
    pub fn blit_texture(
        &mut self,
        _dest_x: u32,
        _dest_y: u32,
        _w: u32,
        _h: u32,
        _tex: &ID3D11Texture2D,
    ) -> bool {
        false
    }

    pub fn disable_gpu_yuv(&mut self) {
        // The D3D12 backend always uses GPU compute for YUV conversion; this is a
        // no-op, but callers can still force CPU decode upstream.
        tracing::debug!("D3D12: disable_gpu_yuv is a no-op");
    }

    pub fn gpu_yuv_available(&self) -> bool {
        true
    }

    /// Copy a rectangle on the GPU, using a scratch texture for overlapping copies.
    pub fn copy_rect(
        &mut self,
        sx: u16,
        sy: u16,
        w: u16,
        h: u16,
        dx: u16,
        dy: u16,
    ) {
        let (sx, sy, dx, dy, w, h) = (
            sx as u32, sy as u32, dx as u32, dy as u32, w as u32, h as u32,
        );
        if w == 0 || h == 0 {
            return;
        }
        if sx >= self.fb_width || sy >= self.fb_height || dx >= self.fb_width || dy >= self.fb_height
        {
            return;
        }
        let cw = w.min(self.fb_width - sx).min(self.fb_width - dx);
        let ch = h.min(self.fb_height - sy).min(self.fb_height - dy);
        if cw == 0 || ch == 0 {
            return;
        }
        let overlapping = sx < dx + cw && sx + cw > dx && sy < dy + ch && sy + ch > dy;
        if overlapping && self.copy_scratch.is_none() {
            self.copy_scratch = Self::create_default_texture(
                &self.device,
                self.fb_width,
                self.fb_height,
                DXGI_FORMAT_R8G8B8A8_UNORM,
            ).ok();
        }
        let Some(fb) = self.framebuffer.clone() else { return };
        let scratch = self.copy_scratch.clone();
        let _ = self.begin_list();
        let list = self.list.as_ref().unwrap();
        if overlapping {
            if let Some(scratch) = scratch.as_ref() {
                Self::copy_region(list, &fb, sx, sy, scratch, 0, 0, cw, ch);
                Self::copy_region(list, scratch, 0, 0, &fb, dx, dy, cw, ch);
            }
        } else {
            Self::copy_region(list, &fb, sx, sy, &fb, dx, dy, cw, ch);
        }
    }

    fn copy_region(
        list: &ID3D12GraphicsCommandList,
        src: &ID3D12Resource,
        sx: u32,
        sy: u32,
        dst: &ID3D12Resource,
        dx: u32,
        dy: u32,
        w: u32,
        h: u32,
    ) {
        unsafe {
            Self::transition(list, src, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE);
            Self::transition(list, dst, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST);
            let src_loc = D3D12_TEXTURE_COPY_LOCATION {
                pResource: core::mem::ManuallyDrop::new(Some(src.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            let dst_loc = D3D12_TEXTURE_COPY_LOCATION {
                pResource: core::mem::ManuallyDrop::new(Some(dst.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            let src_box = D3D12_BOX {
                left: sx,
                top: sy,
                front: 0,
                right: sx + w,
                bottom: sy + h,
                back: 1,
            };
            list.CopyTextureRegion(
                &dst_loc,
                dx,
                dy,
                0,
                &src_loc,
                Some(&src_box),
            );
            Self::transition(list, src, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON);
            Self::transition(list, dst, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON);
        }
    }

    fn create_default_texture(
        device: &ID3D12Device,
        width: u32,
        height: u32,
        format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    ) -> WinResult<ID3D12Resource> {
        unsafe {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Alignment: 0,
                Width: width as u64,
                Height: height,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: windows::Win32::Graphics::Direct3D12::D3D12_TEXTURE_LAYOUT_UNKNOWN,
                Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            };
            let props = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let mut tex: Option<ID3D12Resource> = None;
            device.CreateCommittedResource(
                &props,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut tex,
            )?;
            tex.ok_or_else(windows::core::Error::from_thread)
        }
    }

    pub fn cache_rect(
        &mut self,
        slot: u16,
        sx: u16,
        sy: u16,
        w: u16,
        h: u16,
    ) {
        let (sx, sy, w, h) = (sx as u32, sy as u32, w as u32, h as u32);
        if w == 0 || h == 0 || sx >= self.fb_width || sy >= self.fb_height {
            return;
        }
        let cw = w.min(self.fb_width - sx);
        let ch = h.min(self.fb_height - sy);
        if cw == 0 || ch == 0 {
            return;
        }
        let Ok(tex) = Self::create_default_texture(
            &self.device,
            cw,
            ch,
            DXGI_FORMAT_R8G8B8A8_UNORM,
        ) else {
            return;
        };
        let Some(fb) = self.framebuffer.clone() else { return };
        let _ = self.begin_list();
        let list = self.list.as_ref().unwrap();
        Self::copy_region(list, &fb, sx, sy, &tex, 0, 0, cw, ch);
        self.gfx_cache.insert(slot, (cw, ch, tex));
    }

    pub fn cache_blit(
        &mut self,
        slot: u16,
        dx: u16,
        dy: u16,
    ) {
        let Some((cw, ch, tex)) = self.gfx_cache.get(&slot).map(|(w, h, t)| (*w, *h, t.clone())) else {
            return;
        };
        let (dx, dy) = (dx as u32, dy as u32);
        if dx >= self.fb_width || dy >= self.fb_height {
            return;
        }
        let cw = cw.min(self.fb_width - dx);
        let ch = ch.min(self.fb_height - dy);
        if cw == 0 || ch == 0 {
            return;
        }
        let Some(fb) = self.framebuffer.clone() else { return };
        let _ = self.begin_list();
        let list = self.list.as_ref().unwrap();
        Self::copy_region(list, &tex, 0, 0, &fb, dx, dy, cw, ch);
    }

    /// Clear the backbuffer to a color and present.
    ///
    /// The D3D12 backend fills the framebuffer with `rgba` through an upload copy
    /// and then presents.
    pub fn present_clear(&mut self, rgba: [f32; 4]) -> WinResult<()> {
        self.wait_for_frame();
        self.submit_and_wait()?;
        let _ = self.ensure_framebuffer(self.sc_width, self.sc_height);
        let Some(fb) = self.framebuffer.clone() else {
            // No framebuffer: present a blank backbuffer.
            return self.present_internal();
        };
        let color: [u8; 4] = [
            (rgba[0] * 255.0) as u8,
            (rgba[1] * 255.0) as u8,
            (rgba[2] * 255.0) as u8,
            (rgba[3] * 255.0) as u8,
        ];
        let size = (self.sc_width * self.sc_height * 4) as usize;
        if let Err(e) = self.ensure_upload(size) {
            tracing::warn!(error = %e, "D3D12 clear upload buffer failed");
            return self.present_internal();
        }
        let Some(upload) = self.upload_buffer.clone() else {
            return self.present_internal();
        };
        unsafe {
            for chunk in std::slice::from_raw_parts_mut(self.upload_addr, size).chunks_exact_mut(4) {
                chunk.copy_from_slice(&color);
            }
            let _ = self.begin_list();
            let list = self.list.as_ref().unwrap();
            Self::transition(list, &fb, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST);
            let src_loc = D3D12_TEXTURE_COPY_LOCATION {
                pResource: core::mem::ManuallyDrop::new(Some(upload)),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: windows::Win32::Graphics::Direct3D12::D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                            Width: self.sc_width,
                            Height: self.sc_height,
                            Depth: 1,
                            RowPitch: self.sc_width * 4,
                        },
                    },
                },
            };
            let dst_loc = D3D12_TEXTURE_COPY_LOCATION {
                pResource: core::mem::ManuallyDrop::new(Some(fb.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            list.CopyTextureRegion(&dst_loc,
                0,
                0,
                0,
                &src_loc,
                None,
            );
            Self::transition(list, &fb, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON);
        }
        self.present_frame()
    }

    fn copy_whole_texture(
        list: &ID3D12GraphicsCommandList,
        src: &ID3D12Resource,
        dst: &ID3D12Resource,
    ) {
        unsafe {
            let src_loc = D3D12_TEXTURE_COPY_LOCATION {
                pResource: core::mem::ManuallyDrop::new(Some(src.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            let dst_loc = D3D12_TEXTURE_COPY_LOCATION {
                pResource: core::mem::ManuallyDrop::new(Some(dst.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            list.CopyTextureRegion(&dst_loc, 0, 0, 0, &src_loc, None);
        }
    }

    fn present_internal(&mut self) -> WinResult<()> {
        // Execute pending work through flush_list so the fence is signalled and
        // the next begin_list can safely reset the allocator.
        self.flush_list()?;
        unsafe {
            let (sync, flags) = if self.low_latency && self.tearing {
                (0, DXGI_PRESENT_ALLOW_TEARING)
            } else {
                (1, DXGI_PRESENT(0))
            };
            self.swap_chain.Present(sync, flags).ok()
        }
    }

    /// Present the framebuffer, copying/scaling to the swapchain backbuffer(s).
    /// A slice smaller than its destination (render-scale) — or an active
    /// `--sharpen` — routes through the compute scale/sharpen chain; otherwise
    /// (and on any scale failure) it is a plain crop/1:1 copy.
    pub fn present_frame(&mut self) -> WinResult<()> {
        self.wait_for_frame();
        self.submit_and_wait()?;
        if self.framebuffer.is_none() {
            return self.present_clear([0.06, 0.09, 0.16, 1.0]);
        }
        let fb = self.framebuffer.as_ref().unwrap().clone();
        unsafe {
            if !self.extra_targets.is_empty() {
                // Record every surface's paint into one list, execute it, then
                // present each swapchain — Present rides the shared queue, so
                // it is ordered after the executed list without a CPU wait.
                let primary_dst = (self.sc_width, self.sc_height);
                let primary_src_size = self.primary_src_size.unwrap_or(primary_dst);
                let surfaces: Vec<(
                    IDXGISwapChain3,
                    (u32, u32),
                    (u32, u32),
                    (u32, u32),
                    Option<HANDLE>,
                    bool,
                )> = std::iter::once((
                    self.swap_chain.clone(),
                    self.primary_src,
                    primary_src_size,
                    primary_dst,
                    self.frame_wait,
                    self.tearing,
                ))
                .chain(self.extra_targets.iter().map(|t| {
                    (
                        t.swap_chain.clone(),
                        t.src,
                        t.src_size,
                        (t.width, t.height),
                        t.frame_wait,
                        t.tearing,
                    )
                }))
                .collect();
                for (chain, src, src_size, dst_size, _, _) in &surfaces {
                    let bb_index = chain.GetCurrentBackBufferIndex();
                    let back_buffer: ID3D12Resource = chain.GetBuffer(bb_index)?;
                    self.record_surface_paint(&fb, &back_buffer, *src, *src_size, *dst_size)?;
                }
                self.flush_list()?;
                for (chain, _, _, _, frame_wait, tearing) in &surfaces {
                    if let Some(h) = frame_wait {
                        let _ = windows::Win32::System::Threading::WaitForSingleObjectEx(
                            *h, 100, false,
                        );
                    }
                    let (sync, flags) = if self.low_latency && *tearing {
                        (0, DXGI_PRESENT_ALLOW_TEARING)
                    } else {
                        (1, DXGI_PRESENT(0))
                    };
                    let _ = chain.Present(sync, flags);
                }
                return Ok(());
            }
            // Single window.
            let bb_index = self.swap_chain.GetCurrentBackBufferIndex();
            let back_buffer: ID3D12Resource = self.swap_chain.GetBuffer(bb_index)?;
            let (fb_w, fb_h) = (self.fb_width, self.fb_height);
            let (sc_w, sc_h) = (self.sc_width, self.sc_height);
            self.record_surface_paint(&fb, &back_buffer, (0, 0), (fb_w, fb_h), (sc_w, sc_h))?;
            self.present_internal()
        }
    }

    /// Record one present surface's paint into the current command list: the
    /// compute scale (+ optional RCAS sharpen) chain when the slice size differs
    /// from the destination (or sharpening is on), else — and on any scale
    /// failure — a crop/1:1 copy.
    unsafe fn record_surface_paint(
        &mut self,
        fb: &ID3D12Resource,
        back_buffer: &ID3D12Resource,
        src: (u32, u32),
        src_size: (u32, u32),
        dst_size: (u32, u32),
    ) -> WinResult<()> {
        let fb_size = (self.fb_width, self.fb_height);
        let src_rect = (src.0, src.1, src_size.0, src_size.1);
        let in_bounds =
            src.0 + src_size.0 <= fb_size.0 && src.1 + src_size.1 <= fb_size.1;
        let needs_scale = src_size != dst_size;
        let sharpen_on = self.sharpen > 0.0;
        if (needs_scale || sharpen_on)
            && in_bounds
            && src_size.0 > 0
            && src_size.1 > 0
            && dst_size.0 > 0
            && dst_size.1 > 0
            && !self.scale_disabled
            && self.ensure_scale_pipeline()
        {
            match self.record_scale_chain(fb, fb_size, src_rect, back_buffer, dst_size, needs_scale)
            {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!(error = %e, "D3D12 scale chain failed; cropping instead");
                    self.scale_disabled = true;
                }
            }
        }
        // Crop/1:1 copy fallback, clipped to the framebuffer.
        self.begin_list()?;
        let list = self.list.clone().unwrap();
        let cw = src_size.0.min(dst_size.0).min(fb_size.0.saturating_sub(src.0));
        let ch = src_size.1.min(dst_size.1).min(fb_size.1.saturating_sub(src.1));
        if cw > 0 && ch > 0 {
            Self::transition(
                &list,
                back_buffer,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST,
            );
            Self::copy_region(&list, fb, src.0, src.1, back_buffer, 0, 0, cw, ch);
            Self::transition(
                &list,
                back_buffer,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_COMMON,
            );
        }
        Ok(())
    }

    /// Record the scale (+ optional sharpen) dispatches for one surface and the
    /// copy of the finished image into its backbuffer.
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_scale_chain(
        &mut self,
        fb: &ID3D12Resource,
        fb_size: (u32, u32),
        src_rect: (u32, u32, u32, u32),
        back_buffer: &ID3D12Resource,
        dst_size: (u32, u32),
        needs_scale: bool,
    ) -> WinResult<()> {
        let sharpen_on = self.sharpen > 0.0;
        let kernel = match self.upscaler {
            Upscaler::Bicubic => ScaleKernel::Bicubic,
            Upscaler::Fsr | Upscaler::Vsr => ScaleKernel::Easu,
            Upscaler::Nearest => ScaleKernel::Nearest,
            Upscaler::Bilinear => ScaleKernel::Bilinear,
        };
        let stops = 2.0 * (1.0 - self.sharpen.clamp(0.0, 1.0));
        let sharpness = (-stops).exp2();
        let full_rect = (0, 0, dst_size.0, dst_size.1);
        let final_tex: ID3D12Resource = if !needs_scale {
            // 1:1 — only the sharpen pass, straight from the framebuffer slice.
            let out = self.ensure_mid(false, dst_size)?;
            self.record_scale_dispatch(
                ScaleKernel::Rcas,
                fb,
                fb_size,
                src_rect,
                &out,
                dst_size,
                sharpness,
            )?;
            out
        } else if sharpen_on {
            let a = self.ensure_mid(true, dst_size)?;
            self.record_scale_dispatch(kernel, fb, fb_size, src_rect, &a, dst_size, 0.0)?;
            let b = self.ensure_mid(false, dst_size)?;
            self.record_scale_dispatch(
                ScaleKernel::Rcas,
                &a,
                dst_size,
                full_rect,
                &b,
                dst_size,
                sharpness,
            )?;
            b
        } else {
            let a = self.ensure_mid(true, dst_size)?;
            self.record_scale_dispatch(kernel, fb, fb_size, src_rect, &a, dst_size, 0.0)?;
            a
        };
        // Copy the finished image to the backbuffer (swapchain buffers can't be
        // UAVs, so the chain always ends in a copy).
        self.begin_list()?;
        let list = self.list.clone().unwrap();
        Self::transition(
            &list,
            &final_tex,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        Self::transition(
            &list,
            back_buffer,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        Self::copy_whole_texture(&list, &final_tex, back_buffer);
        Self::transition(
            &list,
            back_buffer,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_COMMON,
        );
        Self::transition(
            &list,
            &final_tex,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_COMMON,
        );
        Ok(())
    }

    /// Record one compute scale/sharpen dispatch: `src_rect` of `src_res` →
    /// the whole of `dst_res` (`dst_size`).
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_scale_dispatch(
        &mut self,
        kernel: ScaleKernel,
        src_res: &ID3D12Resource,
        src_tex_size: (u32, u32),
        src_rect: (u32, u32, u32, u32),
        dst_res: &ID3D12Resource,
        dst_size: (u32, u32),
        sharpness: f32,
    ) -> WinResult<()> {
        let pso = self.scale_pso(kernel)?;
        let root = self
            .scale_root
            .clone()
            .ok_or_else(windows::core::Error::from_thread)?;
        self.begin_list()?;
        let list = self.list.clone().unwrap();
        let (srv_cpu, uav_cpu, gpu_table) = self.alloc_scale_descriptors();
        self.device.CreateShaderResourceView(src_res, None, srv_cpu);
        self.device.CreateUnorderedAccessView(
            dst_res,
            None,
            Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_UAV {
                        MipSlice: 0,
                        PlaneSlice: 0,
                    },
                },
            }),
            uav_cpu,
        );
        Self::transition(
            &list,
            src_res,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        );
        Self::transition(
            &list,
            dst_res,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        );
        // Must mirror the HLSL `Params` cbuffer layout.
        let params: [f32; 12] = [
            src_rect.0 as f32,
            src_rect.1 as f32,
            src_rect.2 as f32,
            src_rect.3 as f32,
            dst_size.0 as f32,
            dst_size.1 as f32,
            1.0 / src_tex_size.0.max(1) as f32,
            1.0 / src_tex_size.1.max(1) as f32,
            src_tex_size.0 as f32,
            src_tex_size.1 as f32,
            sharpness,
            0.0,
        ];
        list.SetComputeRootSignature(&root);
        list.SetPipelineState(&pso);
        list.SetDescriptorHeaps(&[Some(self.descriptor_heap.clone())]);
        list.SetComputeRoot32BitConstants(
            0,
            params.len() as u32,
            params.as_ptr() as *const core::ffi::c_void,
            0,
        );
        list.SetComputeRootDescriptorTable(1, gpu_table);
        list.Dispatch(dst_size.0.div_ceil(8), dst_size.1.div_ceil(8), 1);
        Self::transition(
            &list,
            dst_res,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COMMON,
        );
        Self::transition(
            &list,
            src_res,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_COMMON,
        );
        Ok(())
    }

    /// Build the scale root signature on first use. Returns `false` (latching
    /// `scale_disabled`) if it can't be built.
    fn ensure_scale_pipeline(&mut self) -> bool {
        if self.scale_root.is_some() {
            return true;
        }
        match Self::create_scale_root_signature(&self.device) {
            Ok(rs) => {
                self.scale_root = Some(rs);
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "D3D12 scale root signature failed; scaling disabled");
                self.scale_disabled = true;
                false
            }
        }
    }

    /// Root signature for the scale kernels: 12 root constants (b0), a static
    /// linear-clamp sampler (s0), and an SRV+UAV descriptor table (t0, u0).
    fn create_scale_root_signature(device: &ID3D12Device) -> WinResult<ID3D12RootSignature> {
        use windows::Win32::Graphics::Direct3D12::{
            D3D12SerializeRootSignature, D3D12_COMPARISON_FUNC_NEVER,
            D3D12_FILTER_MIN_MAG_MIP_LINEAR, D3D12_SHADER_VISIBILITY_ALL,
            D3D12_STATIC_BORDER_COLOR_TRANSPARENT_BLACK, D3D12_STATIC_SAMPLER_DESC,
            D3D12_TEXTURE_ADDRESS_MODE_CLAMP, D3D_ROOT_SIGNATURE_VERSION_1_0,
        };
        unsafe {
            let ranges = [
                D3D12_DESCRIPTOR_RANGE {
                    RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                    NumDescriptors: 1,
                    BaseShaderRegister: 0,
                    RegisterSpace: 0,
                    OffsetInDescriptorsFromTableStart: 0,
                },
                D3D12_DESCRIPTOR_RANGE {
                    RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
                    NumDescriptors: 1,
                    BaseShaderRegister: 0,
                    RegisterSpace: 0,
                    OffsetInDescriptorsFromTableStart: 1,
                },
            ];
            let params = [
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                    Anonymous: D3D12_ROOT_PARAMETER_0 {
                        Constants: D3D12_ROOT_CONSTANTS {
                            ShaderRegister: 0,
                            RegisterSpace: 0,
                            Num32BitValues: 12,
                        },
                    },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                },
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                    Anonymous: D3D12_ROOT_PARAMETER_0 {
                        DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                            NumDescriptorRanges: ranges.len() as u32,
                            pDescriptorRanges: ranges.as_ptr(),
                        },
                    },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                },
            ];
            let sampler = D3D12_STATIC_SAMPLER_DESC {
                Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                MipLODBias: 0.0,
                MaxAnisotropy: 1,
                ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
                BorderColor: D3D12_STATIC_BORDER_COLOR_TRANSPARENT_BLACK,
                MinLOD: 0.0,
                MaxLOD: f32::MAX,
                ShaderRegister: 0,
                RegisterSpace: 0,
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            };
            let desc = D3D12_ROOT_SIGNATURE_DESC {
                NumParameters: params.len() as u32,
                pParameters: params.as_ptr(),
                NumStaticSamplers: 1,
                pStaticSamplers: &sampler,
                Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
            };
            let mut signature: Option<ID3DBlob> = None;
            let mut error: Option<ID3DBlob> = None;
            D3D12SerializeRootSignature(
                &desc,
                D3D_ROOT_SIGNATURE_VERSION_1_0,
                &mut signature,
                Some(&mut error),
            )?;
            if let Some(err) = error {
                let slice = core::slice::from_raw_parts(
                    err.GetBufferPointer() as *const u8,
                    err.GetBufferSize(),
                );
                tracing::warn!(error = %String::from_utf8_lossy(slice), "D3D12 scale root signature error");
            }
            let signature = signature.ok_or_else(windows::core::Error::from_thread)?;
            let bytes = core::slice::from_raw_parts(
                signature.GetBufferPointer() as *const u8,
                signature.GetBufferSize(),
            );
            device.CreateRootSignature(0, bytes)
        }
    }

    /// The PSO for `kernel`, compiling it on first use.
    fn scale_pso(&mut self, kernel: ScaleKernel) -> WinResult<ID3D12PipelineState> {
        if let Some(p) = self.scale_psos.get(&kernel) {
            return Ok(p.clone());
        }
        let root = self
            .scale_root
            .clone()
            .ok_or_else(windows::core::Error::from_thread)?;
        unsafe {
            let blob = compile_scale_kernel(kernel)?;
            let bytes = core::slice::from_raw_parts(
                blob.GetBufferPointer() as *const u8,
                blob.GetBufferSize(),
            );
            let pso_desc =
                windows::Win32::Graphics::Direct3D12::D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: core::mem::ManuallyDrop::new(Some(root)),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: bytes.as_ptr() as *const _,
                        BytecodeLength: bytes.len(),
                    },
                    ..Default::default()
                };
            let pso: ID3D12PipelineState = self.device.CreateComputePipelineState(&pso_desc)?;
            self.scale_psos.insert(kernel, pso.clone());
            tracing::info!(kernel = kernel.label(), "D3D12 scale kernel compiled");
            Ok(pso)
        }
    }

    /// The cached intermediate UAV texture for this size (`first` = the upscale
    /// output `mid_a`; otherwise the sharpen output `mid_b`).
    fn ensure_mid(&mut self, first: bool, size: (u32, u32)) -> WinResult<ID3D12Resource> {
        let map = if first { &mut self.mid_a } else { &mut self.mid_b };
        if let Some(t) = map.get(&size) {
            return Ok(t.clone());
        }
        let tex = Self::create_uav_texture(&self.device, size.0, size.1)?;
        map.insert(size, tex.clone());
        Ok(tex)
    }

    /// A default-heap RGBA texture with UAV access (created in COMMON).
    fn create_uav_texture(device: &ID3D12Device, w: u32, h: u32) -> WinResult<ID3D12Resource> {
        unsafe {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Alignment: 0,
                Width: w as u64,
                Height: h,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            };
            let props = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let mut tex: Option<ID3D12Resource> = None;
            device.CreateCommittedResource(
                &props,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut tex,
            )?;
            tex.ok_or_else(windows::core::Error::from_thread)
        }
    }

    /// The next rotating SRV/UAV descriptor pair for a scale pass (slots 2..64;
    /// 0/1 belong to the NV12 conversion). Safe to rotate because
    /// [`Self::begin_list`] waits out the previous list before reuse.
    fn alloc_scale_descriptors(
        &mut self,
    ) -> (
        D3D12_CPU_DESCRIPTOR_HANDLE,
        D3D12_CPU_DESCRIPTOR_HANDLE,
        windows::Win32::Graphics::Direct3D12::D3D12_GPU_DESCRIPTOR_HANDLE,
    ) {
        const HEAP_SLOTS: usize = 64;
        if self.desc_cursor + 2 > HEAP_SLOTS {
            self.desc_cursor = 2;
        }
        let base = self.desc_cursor;
        self.desc_cursor += 2;
        unsafe {
            let cpu_start = self.descriptor_heap.GetCPUDescriptorHandleForHeapStart();
            let gpu_start = self.descriptor_heap.GetGPUDescriptorHandleForHeapStart();
            let srv_cpu = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: cpu_start.ptr + base * self.descriptor_size,
            };
            let uav_cpu = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: cpu_start.ptr + (base + 1) * self.descriptor_size,
            };
            let gpu = windows::Win32::Graphics::Direct3D12::D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: gpu_start.ptr + (base as u64) * (self.descriptor_size as u64),
            };
            (srv_cpu, uav_cpu, gpu)
        }
    }

    /// Read the framebuffer back to CPU as tightly-packed RGBA.
    pub fn readback_framebuffer(&mut self) -> Option<(u32, u32, Vec<u8>)> {
        let fb = self.framebuffer.clone()?;
        let (w, h) = (self.fb_width, self.fb_height);
        if w == 0 || h == 0 {
            return None;
        }
        let row_pitch = (w * 4) as usize;
        let readback = Self::create_readback_texture(&self.device, w, h, row_pitch,
        ).ok()?;
        let _ = self.begin_list();
        let list = self.list.as_ref().unwrap();
        Self::transition(list, &fb, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE);
        Self::copy_whole_texture(list, &fb, &readback);
        Self::transition(list, &fb, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON);
        self.submit_and_wait().ok()?;
        unsafe {
            let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
            readback.Map(0, None, Some(&mut ptr)).ok()?;
            let src = std::slice::from_raw_parts(ptr as *const u8, row_pitch * h as usize);
            let mut out = vec![0u8; row_pitch * h as usize];
            out.copy_from_slice(src);
            let _ = readback.Unmap(0, None);
            Some((w, h, out))
        }
    }

    fn create_readback_texture(
        device: &ID3D12Device,
        _width: u32,
        height: u32,
        row_pitch: usize,
    ) -> WinResult<ID3D12Resource> {
        unsafe {
            let total = row_pitch * height as usize;
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Alignment: 0,
                Width: total as u64,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_UNKNOWN,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: windows::Win32::Graphics::Direct3D12::D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: D3D12_RESOURCE_FLAG_NONE,
            };
            let props = D3D12_HEAP_PROPERTIES {
                Type: windows::Win32::Graphics::Direct3D12::D3D12_HEAP_TYPE_READBACK,
                ..Default::default()
            };
            let mut buf: Option<ID3D12Resource> = None;
            device.CreateCommittedResource(
                &props,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buf,
            )?;
            buf.ok_or_else(windows::core::Error::from_thread)
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_present_target(
        &mut self,
        hwnd_raw: isize,
        width: u32,
        height: u32,
        src_x: u32,
        src_y: u32,
        src_w: u32,
        src_h: u32,
    ) -> WinResult<()> {
        unsafe {
            let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
            let factory: IDXGIFactory2 = self
                .swap_chain
                .GetParent::<IDXGIFactory2>()?;
            let waitable = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32;
            let tearing_flag = DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32;
            let make = |flags: u32| {
                let desc = DXGI_SWAP_CHAIN_DESC1 {
                    Width: width,
                    Height: height,
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    Stereo: false.into(),
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    BufferCount: 2,
                    Scaling: DXGI_SCALING_STRETCH,
                    SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                    AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
                    Flags: flags,
                };
                factory.CreateSwapChainForHwnd(&self.queue,
                    hwnd,
                    &desc,
                    None,
                    None,
                )
            };
            let (sc1, flags) = match make(waitable | tearing_flag) {
                Ok(s) => (s, waitable | tearing_flag),
                Err(_) => match make(waitable) {
                    Ok(s) => (s, waitable),
                    Err(_) => (make(0)?, 0u32),
                },
            };
            let sc: IDXGISwapChain3 = sc1.cast()?;
            let mut frame_wait = None;
            if flags & waitable != 0 {
                let _ = sc.SetMaximumFrameLatency(1);
                let h = sc.GetFrameLatencyWaitableObject();
                if !h.is_invalid() {
                    frame_wait = Some(h);
                }
            }
            let src_size = if src_w != 0 && src_h != 0 {
                (src_w, src_h)
            } else {
                (width, height)
            };
            tracing::info!(
                width,
                height,
                src_x,
                src_y,
                src_w = src_size.0,
                src_h = src_size.1,
                "D3D12 per-monitor present target added"
            );
            self.extra_targets.push(PresentTarget {
                swap_chain: sc,
                width,
                height,
                frame_wait,
                tearing: flags & tearing_flag != 0,
                src: (src_x, src_y),
                src_size,
            });
            Ok(())
        }
    }

    pub fn set_gpu_timing_callback(
        &mut self,
        _cb: Option<Box<dyn Fn(&str, u64) + Send + Sync>>,
    ) {
        // D3D12 timestamp queries are not wired yet; ignore silently.
        self.gpu_timing_cb = _cb;
    }
}

unsafe fn compile_compute_shader() -> WinResult<ID3DBlob> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let res = D3DCompile(
        NV12_TO_RGBA_HLSL.as_ptr() as *const core::ffi::c_void,
        NV12_TO_RGBA_HLSL.len(),
        windows::core::s!("nv12_to_rgba.hlsl"),
        None,
        None,
        windows::core::s!("cs_main"),
        windows::core::s!("cs_5_1"),
        0,
        0,
        &mut code,
        Some(&mut errors),
    );
    if let Err(e) = res {
        if let Some(err) = &errors {
            let msg = core::slice::from_raw_parts(
                err.GetBufferPointer() as *const u8,
                err.GetBufferSize(),
            );
            tracing::warn!(error = %String::from_utf8_lossy(msg), "D3D12 compute shader compile failed");
        }
        return Err(e);
    }
    code.ok_or_else(windows::core::Error::from_thread)
}

/// Compile one [`SCALE_CS_HLSL`] kernel at runtime via d3dcompiler.
unsafe fn compile_scale_kernel(kernel: ScaleKernel) -> WinResult<ID3DBlob> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let res = D3DCompile(
        SCALE_CS_HLSL.as_ptr() as *const core::ffi::c_void,
        SCALE_CS_HLSL.len(),
        windows::core::s!("scale_cs.hlsl"),
        None,
        None,
        kernel.entry(),
        windows::core::s!("cs_5_1"),
        0,
        0,
        &mut code,
        Some(&mut errors),
    );
    if let Err(e) = res {
        if let Some(err) = &errors {
            let msg = core::slice::from_raw_parts(
                err.GetBufferPointer() as *const u8,
                err.GetBufferSize(),
            );
            tracing::warn!(error = %String::from_utf8_lossy(msg), "D3D12 scale shader compile failed");
        }
        return Err(e);
    }
    code.ok_or_else(windows::core::Error::from_thread)
}

impl Drop for D3D12Renderer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.submit_and_wait();
            if !self.cb_addr.is_null() {
                let _ = self.constant_buffer.Unmap(0, None);
            }
            if !self.upload_addr.is_null() {
                if let Some(buf) = self.upload_buffer.as_ref() {
                    let _ = buf.Unmap(0, None);
                }
            }
            let _ = windows::Win32::Foundation::CloseHandle(self.fence_event);
        }
    }
}

#[cfg(test)]
mod shader_tests {
    use super::*;

    /// Every compute scale kernel must compile via d3dcompiler (no GPU needed).
    #[test]
    fn scale_cs_hlsl_compiles_for_every_kernel() {
        for kernel in [
            ScaleKernel::Bilinear,
            ScaleKernel::Bicubic,
            ScaleKernel::Easu,
            ScaleKernel::Nearest,
            ScaleKernel::Rcas,
        ] {
            unsafe {
                compile_scale_kernel(kernel)
                    .unwrap_or_else(|e| panic!("{}: {e}", kernel.label()));
            }
        }
    }
}
