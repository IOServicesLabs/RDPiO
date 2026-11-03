//! Can a hybrid laptop's integrated GPU carry the Hyper host on battery? Lists
//! the GPUs and every hardware encoder with the GPU it runs on, then encodes
//! synthetic office frames (scrolling "text") on the frugal GPU through the
//! host's own encoder ([`HwEncoder`]) and reports how long a frame takes.
//! Nothing is captured: the frames are made on that GPU, so it runs beside a
//! live stream without touching it.
//!
//! `cargo run --release -p rdp-gpu --example igpu_probe [frames]`

#[cfg(windows)]
fn main() {
    use std::time::Instant;

    use rdp_gpu::h264::Codec;
    use rdp_gpu::hyper::{HwEncoder, Tuning};
    use windows::core::Interface;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIFactory4};

    tracing_subscriber::fmt().with_env_filter("info").init();
    // The system timer at 1 ms, as the host agent runs: at Windows' default
    // 15.6 ms an encoder answering through events looks two ticks slow.
    #[link(name = "winmm")]
    extern "system" {
        fn timeBeginPeriod(ms: u32) -> u32;
    }
    unsafe { timeBeginPeriod(1) };
    let frames: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(120);

    for g in rdp_gpu::hyper::battery::adapters() {
        println!("gpu {:<45} luid={:#x} vendor={:#06x} display={} frugal_rank={} fast_rank={}", g.name, g.luid, g.vendor, g.drives_display, g.frugal_rank, g.fast_rank);
    }
    for g in rdp_gpu::hyper::battery::adapters() {
        for codec in [Codec::Hevc, Codec::H264] {
            for (name, vendor) in rdp_gpu::hyper::battery::hardware_encoders(codec, Some(g.luid)) {
                println!("encoder {:<5} on {:#x}: {name} ({vendor})", codec.name(), g.luid);
            }
        }
    }
    let Some(battery) = rdp_gpu::hyper::battery::battery_gpu(&[Codec::Hevc, Codec::H264]) else {
        println!("no integrated GPU with a hardware encoder: nothing to probe");
        return;
    };
    println!("battery GPU: {battery:?}");

    let factory: IDXGIFactory4 = unsafe { windows::Win32::Graphics::Dxgi::CreateDXGIFactory2(Default::default()).expect("factory") };
    let luid = windows::Win32::Foundation::LUID { LowPart: battery.luid as u32, HighPart: (battery.luid >> 32) as i32 };
    let adapter: IDXGIAdapter = unsafe { factory.EnumAdapterByLuid(luid).expect("adapter") };
    let (device, context) = unsafe {
        let (mut d, mut c) = (None, None);
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut d),
            None,
            Some(&mut c),
        )
        .expect("device");
        (d.unwrap(), c.unwrap())
    };
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }
    }
    let _ = context;

    // Office-like NV12 frames: white, with rows of dark "glyphs", scrolled a
    // line further in each (so every frame has new detail to code).
    let make = |(w, h): (u32, u32), shift: u32| -> ID3D11Texture2D {
        let (wu, hu) = (w as usize, h as usize);
        let mut data = vec![128u8; wu * hu * 3 / 2];
        let mut seed = 0x9e37_79b9u32;
        for y in 0..hu {
            let row = (y as u32 + shift * 18) / 18;
            let in_line = (y as u32 + shift * 18) % 18;
            for x in 0..wu {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let glyph = in_line > 3 && in_line < 15 && ((x as u32 / 9).wrapping_mul(2654435761).wrapping_add(row * 40503)) % 7 != 0;
                data[y * wu + x] = if glyph && seed % 3 == 0 { 30 } else { 235 };
            }
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
        unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut t)).expect("texture") };
        t.unwrap()
    };

    let stats = |label: &str, v: &mut Vec<f64>| {
        if v.is_empty() {
            println!("{label:>40}: (none)");
            return;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
        println!("{label:>40}: n={:<4} p50={:6.2} p90={:6.2} max={:6.2} ms", v.len(), p(0.5), p(0.9), v[v.len() - 1]);
    };
    for &(w, h) in &[(2560u32, 1440u32), (1920, 1080)] {
        let textures: Vec<ID3D11Texture2D> = (0..8).map(|i| make((w, h), i)).collect();
        for &codec in &battery.codecs {
            for &(fps, tuning) in &[(30u32, Tuning::GAMING), (60, Tuning::GAMING), (120, Tuning::GAMING), (30, Tuning::office(30)), (60, Tuning::office(60))] {
                let what = format!("{}@{fps}", if tuning == Tuning::GAMING { "game" } else { "office" });
                let label = format!("{} {w}x{h} {what}", codec.name());
                let mut enc = match HwEncoder::new_tuned(&device, Some(battery.luid), w, h, fps, 8_000_000, false, codec, tuning) {
                    Ok(e) => e,
                    Err(e) => {
                        println!("{label:>40}: no encoder: {e}");
                        continue;
                    }
                };
                let (mut p, mut k, mut kb) = (vec![], vec![], vec![]);
                let mut failed = None;
                for i in 0..frames {
                    if i % 60 == 0 && i > 0 {
                        enc.request_keyframe();
                    }
                    let t = Instant::now();
                    match enc.encode(&textures[i % textures.len()]) {
                        Ok(Some(out)) => {
                            let ms = t.elapsed().as_secs_f64() * 1e3;
                            if out.keyframe { k.push(ms) } else { p.push(ms) }
                            kb.push(out.data.len() as f64 / 1024.0);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            failed = Some(e);
                            break;
                        }
                    }
                }
                println!("{label:>40}: {}", enc.name);
                stats("  P-frame encode", &mut p);
                stats("  keyframe encode", &mut k);
                stats("  frame KB", &mut kb);
                if let Some(e) = failed {
                    println!("{:>40}  encode failed: {e}", "");
                }
            }
        }
    }
    // The host hands an encoder that takes it the captured BGRA as is (its
    // own engine converts): does Quick Sync, and how fast?
    for &(w, h) in &[(2560u32, 1440u32), (1920, 1080)] {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let pixels: Vec<u8> = (0..w * h * 4).map(|i| if (i / 4 / 7) % 5 == 0 { 20 } else { 230 }).collect();
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: pixels.as_ptr() as *const _, SysMemPitch: w * 4, SysMemSlicePitch: 0 };
        let mut t = None;
        unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut t)).expect("texture") };
        let t: ID3D11Texture2D = t.unwrap();
        let label = format!("h264 {w}x{h} game@60 BGRA in");
        match HwEncoder::new_tuned(&device, Some(battery.luid), w, h, 60, 8_000_000, true, Codec::H264, Tuning::GAMING) {
            Err(e) => println!("{label:>40}: no encoder: {e}"),
            Ok(mut enc) => {
                let mut p = vec![];
                for i in 0..frames {
                    let t0 = Instant::now();
                    match enc.encode(&t) {
                        Ok(Some(_)) if i > 0 => p.push(t0.elapsed().as_secs_f64() * 1e3),
                        Ok(_) => {}
                        Err(e) => {
                            println!("{label:>40}: encode failed: {e}");
                            break;
                        }
                    }
                }
                println!("{label:>40}: {}", enc.name);
                stats("  frame encode", &mut p);
            }
        }
    }
}

#[cfg(not(windows))]
fn main() {}
