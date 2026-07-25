//! Offline replay of captured ClearCodec streams.
//!
//! A live run with `RDPIO_DUMP_CC_RAW=<dir>` appends every raw
//! CLEARCODEC_BITMAP_STREAM to `<dir>/cc_<pid>.bin` as length-prefixed records
//! `[x u32][y u32][w u32][h u32][len u32][bytes]` in arrival order. This example
//! replays them through ONE [`ClearDecoder`] in that exact order — bit-identical
//! glyph/vBar cache evolution to the live session — compositing onto a desktop
//! canvas and writing BMP snapshots. That reproduces the live "garbled text"
//! deterministically, with no server, so the decoder bug can be isolated.
//!
//! Usage: cargo run -p rdp-graphics --example cc_replay -- <cc_<pid>.bin> [out_dir] [every_n] [fps]
//!
//! `fps` controls replay cadence: 0 = flat-out (measure peak throughput),
//! otherwise sleep to maintain that frame rate (measure sustained throughput).

use rdp_graphics::clearcodec::{needs_seed, ClearDecoder};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

struct Rec<'a> {
    x: usize,
    y: usize,
    w: u16,
    h: u16,
    stream: &'a [u8],
}

fn parse(data: &[u8]) -> Vec<Rec<'_>> {
    let mut out = Vec::new();
    let mut p = 0usize;
    let u32a = |d: &[u8], o: usize| u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
    while p + 20 <= data.len() {
        let x = u32a(data, p) as usize;
        let y = u32a(data, p + 4) as usize;
        let w = u32a(data, p + 8) as u16;
        let h = u32a(data, p + 12) as u16;
        let len = u32a(data, p + 16) as usize;
        p += 20;
        if p + len > data.len() {
            break;
        }
        out.push(Rec { x, y, w, h, stream: &data[p..p + len] });
        p += len;
    }
    out
}

fn region(fb: &[u8], dw: usize, dh: usize, x: usize, y: usize, w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    for row in 0..h {
        let sy = y + row;
        if sy >= dh || x >= dw {
            continue;
        }
        let cols = (dw - x).min(w);
        let s = (sy * dw + x) * 4;
        let d = row * w * 4;
        out[d..d + cols * 4].copy_from_slice(&fb[s..s + cols * 4]);
    }
    out
}

fn put(fb: &mut [u8], dw: usize, dh: usize, x: usize, y: usize, w: usize, h: usize, rgba: &[u8]) {
    for row in 0..h {
        let dy = y + row;
        if dy >= dh || x >= dw {
            continue;
        }
        let cols = (dw - x).min(w);
        let d = (dy * dw + x) * 4;
        let s = row * w * 4;
        fb[d..d + cols * 4].copy_from_slice(&rgba[s..s + cols * 4]);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: cc_replay <cc_<pid>.bin> [out_dir] [every_n] [fps]");
        std::process::exit(2);
    };
    let out_dir = args.next().unwrap_or_else(|| ".".to_string());
    let every_n: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(400);
    let fps: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);

    let data = std::fs::read(&path).expect("read capture");
    let recs = parse(&data);
    if recs.is_empty() {
        eprintln!("no records parsed from {path}");
        std::process::exit(1);
    }
    let dw = recs.iter().map(|r| r.x + r.w as usize).max().unwrap_or(0);
    let dh = recs.iter().map(|r| r.y + r.h as usize).max().unwrap_or(0);
    println!("replaying {} tiles onto {dw}x{dh} (fps={fps})", recs.len(), fps = if fps == 0 { "max".to_string() } else { fps.to_string() });

    let mut fb = vec![0u8; dw * dh * 4];
    let mut dec = ClearDecoder::new();
    let start = Instant::now();
    let frame_interval = if fps == 0 {
        None
    } else {
        Some(Duration::from_secs_f64(1.0 / fps as f64))
    };
    let mut next_frame = Instant::now();

    for (i, r) in recs.iter().enumerate() {
        if let Some(interval) = frame_interval {
            let now = Instant::now();
            if now < next_frame {
                std::thread::sleep(next_frame - now);
            }
            next_frame += interval;
        }
        let (w, h) = (r.w as usize, r.h as usize);
        let seed = if needs_seed(r.stream, r.w, r.h) {
            Some(region(&fb, dw, dh, r.x, r.y, w, h))
        } else {
            None
        };
        if let Some(rgba) = dec.decode_seeded(r.stream, r.w, r.h, seed.as_deref()) {
            if rgba.len() == w * h * 4 {
                put(&mut fb, dw, dh, r.x, r.y, w, h, &rgba);
            }
        }
        if i % every_n == every_n - 1 || i + 1 == recs.len() {
            let out = Path::new(&out_dir).join(format!("ccrep_{i:05}.bmp"));
            write_bmp(&out, dw, dh, &fb);
            println!("  {i:05} wrote {}", out.display());
        }
    }

    let elapsed = start.elapsed();
    let secs = elapsed.as_secs_f64();
    let tiles_per_sec = recs.len() as f64 / secs.max(1e-9);
    println!(
        "throughput: tiles={} elapsed={:.3}s tiles_per_sec={:.1}",
        recs.len(),
        secs,
        tiles_per_sec,
    );
    // Single regression number: sustained tiles/sec (or peak if fps=0).
    println!("regression: cc_tiles_per_sec={tiles_per_sec:.1}");
}

/// Minimal 24bpp bottom-up BMP writer (input is top-down RGBA).
fn write_bmp(path: &Path, w: usize, h: usize, rgba: &[u8]) {
    let row = (w * 3).div_ceil(4) * 4;
    let pixel_bytes = row * h;
    let mut out = Vec::with_capacity(54 + pixel_bytes);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(54u32 + pixel_bytes as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(pixel_bytes as u32).to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..h).rev() {
        let mut line = Vec::with_capacity(row);
        for x in 0..w {
            let i = (y * w + x) * 4;
            line.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i]]);
        }
        line.resize(row, 0);
        out.extend_from_slice(&line);
    }
    std::fs::File::create(path)
        .expect("create bmp")
        .write_all(&out)
        .expect("write bmp");
}
