//! Offline replay of captured RemoteFX Progressive payloads.
//!
//! A live run with `RDPIO_DUMP_PROG=<dir>` writes each WireToSurface2 progressive
//! payload to `<dir>/prog_NNNNN_<ctx>.bin`. This example replays them through one
//! [`ProgressiveDecoder`] in capture order — bit-identical state evolution to the
//! live session — and writes composited BMP snapshots, so decoder bugs can be
//! isolated from compositing/GPU and reproduced without a server.
//!
//! Usage: cargo run -p rdp-graphics --example prog_replay -- <dump_dir> [out_dir] [every_n] [fps]
//!
//! `fps` controls replay cadence: 0 = flat-out, otherwise maintain that rate.
//! Output: `replay_NNNNN.bmp` snapshots (every `every_n` payloads, default 30,
//! plus the final state), per-payload stats, and a single regression number on
//! stdout.

use rdp_graphics::progressive::ProgressiveDecoder;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(dir) = args.next() else {
        eprintln!("usage: prog_replay <dump_dir> [out_dir] [every_n] [fps]");
        std::process::exit(2);
    };
    let out_dir = args.next().unwrap_or_else(|| dir.clone());
    let every_n: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let fps: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);

    // prog_NNNNN_<ctx>.bin, sorted by NNNNN = capture order.
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read dump dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("prog_") && n.ends_with(".bin"))
        })
        .collect();
    files.sort();
    if files.is_empty() {
        eprintln!("no prog_*.bin files in {dir}");
        std::process::exit(1);
    }
    println!(
        "replaying {} payloads from {dir} (fps={})",
        files.len(),
        if fps == 0 { "max".to_string() } else { fps.to_string() }
    );

    let mut dec = ProgressiveDecoder::new();
    // Canvas sized lazily from the tiles we see (tile coords are surface-absolute).
    let (mut cw, mut ch) = (0usize, 0usize);
    let mut canvas: Vec<u8> = Vec::new();
    let start = Instant::now();
    let mut total_tiles = 0usize;
    let frame_interval = if fps == 0 {
        None
    } else {
        Some(Duration::from_secs_f64(1.0 / fps as f64))
    };
    let mut next_frame = Instant::now();

    for (i, path) in files.iter().enumerate() {
        if let Some(interval) = frame_interval {
            let now = Instant::now();
            if now < next_frame {
                std::thread::sleep(next_frame - now);
            }
            next_frame += interval;
        }
        let data = std::fs::read(path).expect("read payload");
        let ctx: u32 = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.rsplit('_').next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let tiles = dec.decode(ctx, &data);
        total_tiles += tiles.len();
        let s = dec.last_stats();

        // Grow the canvas to cover every tile seen so far.
        for t in &tiles {
            let need_w = (t.x + t.w) as usize;
            let need_h = (t.y + t.h) as usize;
            if need_w > cw || need_h > ch {
                let (nw, nh) = (cw.max(need_w), ch.max(need_h));
                let mut next = vec![0u8; nw * nh * 4];
                for row in 0..ch {
                    let so = row * cw * 4;
                    let dolt = row * nw * 4;
                    next[dolt..dolt + cw * 4].copy_from_slice(&canvas[so..so + cw * 4]);
                }
                canvas = next;
                cw = nw;
                ch = nh;
            }
        }
        for t in &tiles {
            let (tx, ty, tw, th) = (t.x as usize, t.y as usize, t.w as usize, t.h as usize);
            for row in 0..th {
                let dst = ((ty + row) * cw + tx) * 4;
                let src = row * tw * 4;
                canvas[dst..dst + tw * 4].copy_from_slice(&t.rgba[src..src + tw * 4]);
            }
        }

        println!(
            "{i:05} {name} bytes={len} ctx={ctx} simple={} first={} upgrade={} diff={} skipped={} tiles={}",
            s.simple,
            s.first,
            s.upgrade,
            s.diff,
            s.skipped,
            tiles.len(),
            name = path.file_name().unwrap().to_string_lossy(),
            len = data.len(),
        );

        if (i % every_n == every_n - 1 || i + 1 == files.len()) && cw > 0 {
            let out = Path::new(&out_dir).join(format!("replay_{i:05}.bmp"));
            write_bmp(&out, cw, ch, &canvas);
            println!("  wrote {}", out.display());
        }
    }

    let p = dec.perf_totals();
    let elapsed = start.elapsed();
    let secs = elapsed.as_secs_f64();
    println!(
        "perf: tiles={} rlgr={:?} dwt={:?} upgrade={:?} ycbcr={:?}",
        p.tiles, p.rlgr, p.dwt, p.upgrade, p.ycbcr
    );
    println!(
        "throughput: payloads={} tiles={} elapsed={:.3}s payloads_per_sec={:.1} tiles_per_sec={:.1}",
        files.len(),
        total_tiles,
        secs,
        files.len() as f64 / secs.max(1e-9),
        total_tiles as f64 / secs.max(1e-9),
    );
    // Single regression number: sustained tiles/sec (or peak if fps=0).
    let tiles_per_sec = total_tiles as f64 / secs.max(1e-9);
    println!("regression: prog_tiles_per_sec={tiles_per_sec:.1}");
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
    let mut f = std::fs::File::create(path).expect("create bmp");
    f.write_all(&out).expect("write bmp");
}
