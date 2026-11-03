//! The host on battery: a hybrid laptop's integrated GPU, so the discrete one
//! can power off. Measured on a Razer Blade 15 (UHD 630 + RTX 2070 Max-Q)
//! streaming office work: the NVIDIA GPU alone drew 25-32 W of the laptop's
//! 35-42 W with its encoder 1-7% busy — kept awake by the virtual monitor
//! rendered on it, the capture device and the NVENC session. All three live
//! on one GPU (capture opens on the adapter the monitor's output is on; the
//! encoder on the capture's), so moving the host is choosing the virtual
//! monitor's render adapter: [`battery_gpu`] finds the one to choose, and
//! [`encodes`] tells whether its encoder really takes the stream before it is.

use std::time::Instant;

use windows::core::{Interface, Result as WinResult};
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Multithread, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory6, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_CREATE_FACTORY_FLAGS,
    DXGI_GPU_PREFERENCE, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE, DXGI_GPU_PREFERENCE_MINIMUM_POWER,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;

use crate::h264::Codec;

/// A GPU as DXGI lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gpu {
    pub name: String,
    pub luid: i64,
    pub vendor: u32,
    /// Its own video memory (MiB): an integrated GPU has next to none (the
    /// UHD 630 reports 128), a discrete one gigabytes.
    pub dedicated_mib: u64,
    /// A monitor is attached to it (a hybrid laptop's panel: the integrated).
    pub drives_display: bool,
    /// Its place in Windows' most-frugal-first and fastest-first orders.
    pub frugal_rank: u32,
    pub fast_rank: u32,
}

/// Integrated GPUs report at most this much video memory of their own.
const INTEGRATED_MIB: u64 = 512;

/// Microsoft's adapters (the software renderer, a remote session's): never
/// a host's GPU.
const VENDOR_MICROSOFT: u32 = 0x1414;

pub const VENDOR_NVIDIA: u32 = 0x10de;

/// The hardware GPUs, in DXGI's order.
pub fn adapters() -> Vec<Gpu> {
    let mut gpus: Vec<Gpu> = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory2::<IDXGIFactory6>(DXGI_CREATE_FACTORY_FLAGS(0)) }) else { return gpus };
    let rank = |pref: DXGI_GPU_PREFERENCE| -> Vec<i64> {
        (0..16)
            .map_while(|i| unsafe { factory.EnumAdapterByGpuPreference::<IDXGIAdapter1>(i, pref).ok() })
            .filter_map(|a| unsafe { a.GetDesc1().ok() })
            .map(|d| luid_i64(d.AdapterLuid))
            .collect()
    };
    let (frugal, fast) = (rank(DXGI_GPU_PREFERENCE_MINIMUM_POWER), rank(DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE));
    for i in 0..16 {
        let Ok(a) = (unsafe { factory.EnumAdapterByGpuPreference::<IDXGIAdapter1>(i, DXGI_GPU_PREFERENCE_MINIMUM_POWER) }) else { break };
        let Ok(d) = (unsafe { a.GetDesc1() }) else { continue };
        if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 || d.VendorId == VENDOR_MICROSOFT {
            continue;
        }
        let luid = luid_i64(d.AdapterLuid);
        let end = d.Description.iter().position(|&c| c == 0).unwrap_or(d.Description.len());
        let at = |order: &[i64]| order.iter().position(|&l| l == luid).unwrap_or(usize::MAX) as u32;
        gpus.push(Gpu {
            name: String::from_utf16_lossy(&d.Description[..end]),
            luid,
            vendor: d.VendorId,
            dedicated_mib: d.DedicatedVideoMemory as u64 >> 20,
            drives_display: unsafe { a.EnumOutputs(0).is_ok() },
            frugal_rank: at(&frugal),
            fast_rank: at(&fast),
        });
    }
    gpus
}

/// The GPU to run on when on battery: a hybrid machine's integrated GPU —
/// Windows' most frugal, not its fastest (one GPU: nothing to move to), with
/// next to no memory of its own (two discrete GPUs: neither powers off for
/// it). `None` keeps the fastest.
pub fn frugal(gpus: &[Gpu]) -> Option<&Gpu> {
    let fastest = gpus.iter().min_by_key(|g| g.fast_rank)?;
    let frugal = gpus.iter().min_by_key(|g| g.frugal_rank)?;
    (frugal.luid != fastest.luid && frugal.dedicated_mib <= INTEGRATED_MIB).then_some(frugal)
}

/// The fastest GPU's LUID (Windows' high-performance pick: where the host
/// runs when plugged in).
pub fn fastest_luid() -> Option<i64> {
    adapters().into_iter().find(|g| g.fast_rank == 0).map(|g| g.luid)
}

/// The hardware encoders Media Foundation lists for `codec` (fed NV12, as
/// the host does) — on GPU `luid` only, when given — by name and vendor
/// (`VEN_8086`). The GPU must be asked for: an encoder listed without one
/// says nothing of where it runs (no `MFT_ENUM_ADAPTER_LUID` on it), and the
/// list puts the fastest GPU's first — so a device on the integrated GPU
/// handed to the first one gets NVIDIA's, which fails (E_UNEXPECTED).
pub fn hardware_encoders(codec: Codec, luid: Option<i64>) -> Vec<(String, String)> {
    unsafe { encoder_activates(codec, luid) }
        .iter()
        .map(|a| unsafe { (string(a, &MFT_FRIENDLY_NAME_Attribute), string(a, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute)) })
        .collect()
}

/// [`hardware_encoders`]' transforms, best first (not activated).
pub(crate) unsafe fn encoder_activates(codec: Codec, luid: Option<i64>) -> Vec<IMFActivate> {
    if MFStartup(MF_VERSION, MFSTARTUP_LITE).is_err() {
        return Vec::new();
    }
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: codec.subtype() };
    let mut on: Option<IMFAttributes> = None;
    if let Some(luid) = luid {
        if MFCreateAttributes(&mut on, 1).is_err() {
            return Vec::new();
        }
        let _ = on.as_ref().map(|a| a.SetBlob(&MFT_ENUM_ADAPTER_LUID, &luid.to_le_bytes()));
    }
    let mut activates: *mut Option<IMFActivate> = core::ptr::null_mut();
    let mut count = 0u32;
    let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
    let listed = MFTEnum2(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), on.as_ref(), &mut activates, &mut count);
    if listed.is_err() || activates.is_null() {
        return Vec::new();
    }
    let list = (0..count as usize).filter_map(|i| core::ptr::read(activates.add(i))).collect();
    CoTaskMemFree(Some(activates as *const _));
    list
}

unsafe fn string(a: &IMFActivate, key: &windows::core::GUID) -> String {
    let mut s = windows::core::PWSTR::null();
    let mut len = 0u32;
    if a.GetAllocatedString(key, &mut s, &mut len).is_err() {
        return "?".into();
    }
    let out = s.to_string().unwrap_or_default();
    CoTaskMemFree(Some(s.0 as *const _));
    out
}

/// The integrated GPU and what it encodes.
#[derive(Debug, Clone)]
pub struct BatteryGpu {
    pub name: String,
    pub luid: i64,
    /// Of the codecs asked about, those it has a hardware encoder for (in
    /// the order asked), and that encoder's name.
    pub codecs: Vec<Codec>,
    pub encoders: Vec<String>,
}

/// [`frugal`]'s GPU, when it has a hardware encoder for one of `codecs`.
/// Lists the adapters and the encoders (a few ms; no device is made, so the
/// discrete GPU isn't woken).
pub fn battery_gpu(codecs: &[Codec]) -> Option<BatteryGpu> {
    let gpus = adapters();
    let gpu = frugal(&gpus)?;
    let mut found = BatteryGpu { name: gpu.name.clone(), luid: gpu.luid, codecs: Vec::new(), encoders: Vec::new() };
    for &codec in codecs {
        if let Some((name, _)) = hardware_encoders(codec, Some(gpu.luid)).into_iter().next() {
            found.codecs.push(codec);
            found.encoders.push(name);
        }
    }
    (!found.codecs.is_empty()).then_some(found)
}

/// Whether the hardware encoder on GPU `luid` takes a stream of `size` at
/// `fps` in `codec`, as the host would open it: a device on that GPU, the
/// host's own encoder, and a few frames through it. The time a frame took
/// (the slowest of them), or why not. Some encoders open and then refuse
/// the first picture (Media Foundation on this UHD 630 once ended every
/// stream at once that way), so a pass is a frame out, not an open.
pub fn encodes(luid: i64, codec: Codec, (w, h): (u32, u32), fps: u32) -> WinResult<std::time::Duration> {
    let device = device_on(luid)?;
    let frame = text_nv12(&device, (w & !1, h & !1))?;
    let mut enc = super::MfEncoder::new(&device, Some(luid), w & !1, h & !1, fps, 4_000_000, false, codec)?;
    let mut slowest = std::time::Duration::ZERO;
    for _ in 0..3 {
        let t = Instant::now();
        if enc.encode(&frame)?.is_none() {
            // Low-latency mode hands each picture back at once; one that
            // holds them still counts if the next brings one out.
            continue;
        }
        slowest = slowest.max(t.elapsed());
    }
    enc.release();
    if slowest.is_zero() {
        return Err(windows::core::Error::new(windows::Win32::Foundation::E_FAIL, "the encoder put out nothing"));
    }
    Ok(slowest)
}

/// A D3D11 device on the GPU `luid`, made as the capture makes its own.
pub fn device_on(luid: i64) -> WinResult<ID3D11Device> {
    unsafe {
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let adapter: IDXGIAdapter = factory.EnumAdapterByLuid(LUID { LowPart: luid as u32, HighPart: (luid >> 32) as i32 })?;
        let mut device = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
        let device = device.expect("device");
        // (The encoder uses it from its own threads, as with the capture's.)
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        Ok(device)
    }
}

/// An NV12 frame of dark "glyphs" on white, like the capture's ring
/// textures: detail to code (Intel's HEVC encoder takes a flat grey frame
/// and fails on the first one with text).
fn text_nv12(device: &ID3D11Device, (w, h): (u32, u32)) -> WinResult<ID3D11Texture2D> {
    let mut data = vec![128u8; (w * h * 3 / 2) as usize];
    for (i, y) in data[..(w * h) as usize].iter_mut().enumerate() {
        let (x, row) = (i as u32 % w, i as u32 / w);
        let glyph = row % 18 > 3 && row % 18 < 15 && (x / 9).wrapping_mul(2654435761).wrapping_add(row / 18 * 40503) % 7 != 0;
        *y = if glyph && (x * 7 + row * 13) % 3 == 0 { 30 } else { 235 };
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr() as *const _, SysMemPitch: w, SysMemSlicePitch: 0 };
    let mut t = None;
    unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut t))? };
    Ok(t.expect("texture"))
}

/// Whether `device` could be on an NVIDIA GPU (false only when its adapter
/// says another vendor).
pub fn may_be_nvidia(device: &ID3D11Device) -> bool {
    let vendor = unsafe { device.cast::<IDXGIDevice>().and_then(|d| d.GetAdapter()).and_then(|a| a.GetDesc()).map(|d| d.VendorId) };
    vendor.map_or(true, |v| v == VENDOR_NVIDIA)
}

/// Unplugged: `GetSystemPowerStatus` says the AC line is off (a desktop
/// says 255, unknown: never on battery).
pub fn on_battery() -> bool {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    // (`RDPIO_POWER=battery` or `ac`: as if — for A/B runs of what
    // follows the power source, on whatever cable the test machine has.)
    match std::env::var("RDPIO_POWER").as_deref() {
        Ok("battery") => return true,
        Ok("ac") => return false,
        _ => {}
    }
    let mut s = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut s).is_ok() && s.ACLineStatus == 0 }
}

fn luid_i64(l: LUID) -> i64 {
    ((l.HighPart as i64) << 32) | l.LowPart as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(name: &str, luid: i64, mib: u64, frugal_rank: u32, fast_rank: u32) -> Gpu {
        Gpu { name: name.into(), luid, vendor: 0, dedicated_mib: mib, drives_display: false, frugal_rank, fast_rank }
    }

    #[test]
    fn a_hybrid_laptop_runs_on_its_integrated_gpu() {
        // The Blade 15: Windows lists the UHD 630 first when frugal, the RTX first when fast.
        let gpus = [gpu("Intel(R) UHD Graphics 630", 0x446154f7, 128, 0, 1), gpu("NVIDIA GeForce RTX 2070 with Max-Q Design", 0xf4f8, 8000, 1, 0)];
        assert_eq!(frugal(&gpus).map(|g| g.luid), Some(0x446154f7));
    }

    #[test]
    fn one_gpu_or_two_discrete_ones_stay_put() {
        assert_eq!(frugal(&[gpu("RTX 4080 SUPER", 1, 16000, 0, 0)]), None);
        // An iGPU that isn't the most frugal (a desktop's, disabled for power): nothing.
        assert_eq!(frugal(&[gpu("RX 6600", 1, 8000, 0, 1), gpu("RTX 3080", 2, 10000, 1, 0)]), None);
        assert_eq!(frugal(&[]), None);
    }

    /// On a machine with an integrated GPU, it's found with an H.264
    /// encoder, and that encoder codes a 1080p frame. (Elsewhere: nothing to
    /// check.)
    #[test]
    fn the_integrated_gpu_found_here_encodes() {
        let Some(g) = battery_gpu(&[Codec::H264]) else { return };
        assert_eq!(g.codecs, [Codec::H264]);
        let took = encodes(g.luid, Codec::H264, (1920, 1080), 60);
        assert!(took.is_ok(), "{} on {}: {took:?}", g.encoders[0], g.name);
    }

    /// What the trial is for: Intel's HEVC encoder opens with the host's
    /// settings, then fails the first picture with detail in it. Shown here
    /// (on a machine whose integrated GPU has one), not relied on.
    #[test]
    #[ignore = "hardware: prints what the integrated GPU's HEVC encoder does"]
    fn the_integrated_hevc_encoder_on_text() {
        let Some(g) = battery_gpu(&[Codec::Hevc]) else { return };
        println!("{}: {:?}", g.encoders[0], encodes(g.luid, Codec::Hevc, (1920, 1080), 60));
    }
}
