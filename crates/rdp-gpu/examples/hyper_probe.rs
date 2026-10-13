//! Hardware check for the Hyper host pipeline: capture the primary monitor,
//! encode on the GPU, decode the result the way a client would, and report how
//! long each stage takes.
//!
//! `cargo run --release -p rdp-gpu --example hyper_probe [seconds] [WxH]`

#[cfg(windows)]
fn main() {
    use std::time::{Duration, Instant};

    use rdp_gpu::h264::H264GpuDecoder;
    use rdp_gpu::hyper::{list_monitors, DesktopCapture, HwEncoder};

    tracing_subscriber::fmt().with_env_filter("info").init();
    // The system timer at 1 ms, as the host agent runs (`--timer1`).
    if std::env::args().any(|a| a == "--timer1") {
        #[link(name = "winmm")]
        extern "system" {
            fn timeBeginPeriod(ms: u32) -> u32;
        }
        unsafe { timeBeginPeriod(1) };
    }
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let stream = std::env::args().nth(2).and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    });

    let monitors = list_monitors().expect("list monitors");
    for m in &monitors {
        println!("monitor {} {}x{} at {},{} on {}", m.device_name, m.width, m.height, m.left, m.top, m.adapter);
    }
    let primary = &monitors[0];
    let rgb = std::env::args().any(|a| a == "--rgb");
    let mut cap = if rgb && stream.is_none() {
        DesktopCapture::new_rgb(&primary.device_name, 3).expect("capture")
    } else {
        DesktopCapture::new(&primary.device_name, stream, 3).expect("capture")
    };
    let (w, h) = cap.stream;
    println!("capturing {} at {}x{} → stream {}x{}", primary.device_name, cap.desktop.0, cap.desktop.1, w, h);
    let luid = cap.adapter_luid().ok();
    let codec = if std::env::args().any(|a| a == "--hevc") { rdp_gpu::h264::Codec::Hevc } else { rdp_gpu::h264::Codec::H264 };
    let mut enc = HwEncoder::new(&cap.device, luid, w, h, 120, 30_000_000, cap.rgb, codec).expect("hardware encoder");
    println!("encoder: {}", enc.name);

    // A separate device stands in for the client.
    let (dev, ctx) = unsafe {
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
        use windows::Win32::Graphics::Direct3D11::*;
        let mut d = None;
        let mut c = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut d),
            None,
            Some(&mut c),
        )
        .expect("client device");
        (d.unwrap(), c.unwrap())
    };
    let mut dec = H264GpuDecoder::with_codec(w, h, &dev, &ctx, codec).expect("decoder");

    let stats = |label: &str, v: &mut Vec<f64>| {
        if v.is_empty() {
            println!("{label:>22}: (none)");
            return;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
        println!("{label:>22}: n={:<4} p50={:7.2} p90={:7.2} max={:7.2}", v.len(), p(0.5), p(0.9), v[v.len() - 1]);
    };

    let (mut capture_ms, mut encode_ms, mut decode_ms, mut sizes_kb, mut gaps_ms) = (vec![], vec![], vec![], vec![], vec![]);
    let mut key_kb = 0.0;
    let mut decoded = 0;
    let mut last_frame: Option<Instant> = None;
    let mut last_tex = None;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        let t0 = Instant::now();
        let Some(frame) = cap.acquire(100).expect("acquire") else { continue };
        let t1 = Instant::now();
        if let Some(l) = last_frame {
            gaps_ms.push((t1 - l).as_secs_f64() * 1e3);
        }
        last_frame = Some(t1);
        capture_ms.push((t1 - t0).as_secs_f64() * 1e3);
        let out = enc.encode(&frame.nv12).expect("encode").expect("output");
        let t2 = Instant::now();
        encode_ms.push((t2 - t1).as_secs_f64() * 1e3);
        if out.keyframe {
            key_kb = out.data.len() as f64 / 1024.0;
        } else {
            sizes_kb.push(out.data.len() as f64 / 1024.0);
        }
        let t3 = Instant::now();
        decoded += dec.decode(&out.data).expect("decode").len();
        decode_ms.push(t3.elapsed().as_secs_f64() * 1e3);
        last_tex = Some(frame.nv12);
    }
    println!("--- live desktop ({} s; frames only arrive when the screen changes)", secs);
    stats("capture wait ms", &mut capture_ms);
    stats("frame gap ms", &mut gaps_ms);
    stats("encode ms", &mut encode_ms);
    stats("decode ms", &mut decode_ms);
    stats("P-frame KB", &mut sizes_kb);
    println!("{:>22}: {key_kb:.1} KB; frames decoded: {decoded}", "keyframe");

    // Throughput: re-encode one captured frame back to back (a static image,
    // so mostly skip blocks — this measures the encoder's fixed latency).
    if let Some(tex) = last_tex {
        let mut e = vec![];
        let mut k = vec![];
        for i in 0..240 {
            if i % 60 == 0 {
                enc.request_keyframe();
            }
            let t = Instant::now();
            let out = enc.encode(&tex).expect("encode").expect("output");
            let ms = t.elapsed().as_secs_f64() * 1e3;
            if out.keyframe { k.push(ms) } else { e.push(ms) }
        }
        println!("--- back-to-back re-encode of one frame");
        stats("P-frame encode ms", &mut e);
        stats("keyframe encode ms", &mut k);
    }
}

#[cfg(not(windows))]
fn main() {}
