//! Reed-Solomon erasure coding over GF(2^8), so a frame survives lost packets
//! without waiting a round trip for a resend.
//!
//! Systematic: the `k` data shards go out unchanged, followed by `m` parity
//! shards. Any `k` of the `k + m` shards rebuild the data. Parity row `i` is
//! row `i` of a Cauchy matrix (`1 / (x_i ^ y_j)` with `x_i = k + i`,
//! `y_j = j`); every square submatrix of a Cauchy matrix is invertible, which
//! is what makes any `k` shards enough.

/// Most shards (data + parity) in one coded block.
pub const MAX_SHARDS: usize = 255;

const POLY: u16 = 0x11d;

struct Tables {
    exp: [u8; 512],
    log: [u8; 256],
}

const fn build_tables() -> Tables {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= POLY;
        }
        i += 1;
    }
    while i < 512 {
        exp[i] = exp[i - 255];
        i += 1;
    }
    Tables { exp, log }
}

static T: Tables = build_tables();

fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        T.exp[T.log[a as usize] as usize + T.log[b as usize] as usize]
    }
}

fn inv(a: u8) -> u8 {
    debug_assert!(a != 0);
    T.exp[255 - T.log[a as usize] as usize]
}

/// `out ^= c * src`, byte-wise — the hot loop. Multiplication by `c` is
/// linear over XOR, so `c * s = c * (s & 15) ^ c * (s & 240)`: two 16-entry
/// tables, which AVX2 looks up 32 bytes at a time (PSHUFB), and an ARM CPU
/// (the Android client's Snapdragon) 16 at a time (NEON's TBL).
fn mul_add(out: &mut [u8], src: &[u8], c: u8) {
    match c {
        0 => {}
        1 => out.iter_mut().zip(src).for_each(|(o, s)| *o ^= s),
        _ => {
            let mut lo = [0u8; 16];
            let mut hi = [0u8; 16];
            for x in 0..16u8 {
                lo[x as usize] = mul(c, x);
                hi[x as usize] = mul(c, x << 4);
            }
            #[allow(unused_mut, unused_assignments)]
            let mut done = 0;
            #[cfg(target_arch = "x86_64")]
            if std::arch::is_x86_feature_detected!("avx2") {
                // SAFETY: AVX2 is present (checked above).
                done = unsafe { mul_add_avx2(out, src, &lo, &hi) };
            }
            #[cfg(target_arch = "aarch64")]
            {
                // SAFETY: NEON is part of AArch64.
                done = unsafe { mul_add_neon(out, src, &lo, &hi) };
            }
            for (o, s) in out[done..].iter_mut().zip(&src[done..]) {
                *o ^= lo[(s & 15) as usize] ^ hi[(s >> 4) as usize];
            }
        }
    }
}

/// The AVX2 body of [`mul_add`]; returns how many bytes it did (a multiple
/// of 32 — the caller finishes the tail).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn mul_add_avx2(out: &mut [u8], src: &[u8], lo: &[u8; 16], hi: &[u8; 16]) -> usize {
    use std::arch::x86_64::*;
    let n = out.len().min(src.len()) & !31;
    let lo_t = _mm256_broadcastsi128_si256(_mm_loadu_si128(lo.as_ptr().cast()));
    let hi_t = _mm256_broadcastsi128_si256(_mm_loadu_si128(hi.as_ptr().cast()));
    let mask = _mm256_set1_epi8(0x0F);
    let mut i = 0;
    while i < n {
        let s = _mm256_loadu_si256(src.as_ptr().add(i).cast());
        let l = _mm256_shuffle_epi8(lo_t, _mm256_and_si256(s, mask));
        let h = _mm256_shuffle_epi8(hi_t, _mm256_and_si256(_mm256_srli_epi64(s, 4), mask));
        let o = _mm256_loadu_si256(out.as_ptr().add(i).cast());
        _mm256_storeu_si256(out.as_mut_ptr().add(i).cast(), _mm256_xor_si256(o, _mm256_xor_si256(l, h)));
        i += 32;
    }
    n
}

/// The NEON body of [`mul_add`]; returns how many bytes it did (a multiple
/// of 16 — the caller finishes the tail).
#[cfg(target_arch = "aarch64")]
unsafe fn mul_add_neon(out: &mut [u8], src: &[u8], lo: &[u8; 16], hi: &[u8; 16]) -> usize {
    use std::arch::aarch64::*;
    let n = out.len().min(src.len()) & !15;
    let lo_t = vld1q_u8(lo.as_ptr());
    let hi_t = vld1q_u8(hi.as_ptr());
    let mask = vdupq_n_u8(0x0F);
    let mut i = 0;
    while i < n {
        let s = vld1q_u8(src.as_ptr().add(i));
        let l = vqtbl1q_u8(lo_t, vandq_u8(s, mask));
        let h = vqtbl1q_u8(hi_t, vshrq_n_u8::<4>(s));
        let o = vld1q_u8(out.as_ptr().add(i));
        vst1q_u8(out.as_mut_ptr().add(i), veorq_u8(o, veorq_u8(l, h)));
        i += 16;
    }
    n
}

/// Coefficient of data shard `j` in parity shard `i` (block of `k` data shards).
fn cauchy(k: usize, i: usize, j: usize) -> u8 {
    inv(((k + i) as u8) ^ (j as u8))
}

/// Parity shards for `data` (all the same length). `m` may be 0.
pub fn encode(data: &[&[u8]], m: usize) -> Vec<Vec<u8>> {
    let k = data.len();
    assert!(k > 0 && k + m <= MAX_SHARDS, "bad shard counts k={k} m={m}");
    let len = data[0].len();
    (0..m)
        .map(|i| {
            let mut p = vec![0u8; len];
            for (j, d) in data.iter().enumerate() {
                mul_add(&mut p, d, cauchy(k, i, j));
            }
            p
        })
        .collect()
}

/// Rebuild missing data shards in place. `shards` holds the `k` data shards
/// then the `m` parity shards, `None` where lost. Returns `false` if fewer
/// than `k` shards arrived (unrecoverable). Parity shards are not rebuilt.
///
/// Only the lost shards' equations are solved: with `e` data shards lost and
/// `e` parity shards `p` used, `p - (the arrived data's share of p)` is the
/// lost shards' share, an `e x e` Cauchy system (invertible, like every
/// square piece of a Cauchy matrix) — not the whole `k x k` one.
pub fn reconstruct(shards: &mut [Option<Vec<u8>>], k: usize) -> bool {
    let m = shards.len() - k;
    let missing: Vec<usize> = (0..k).filter(|&j| shards[j].is_none()).collect();
    if missing.is_empty() {
        return true;
    }
    let parity: Vec<usize> = (0..m).filter(|&i| shards[k + i].is_some()).take(missing.len()).collect();
    if parity.len() < missing.len() {
        return false;
    }
    let e = missing.len();
    let len = shards[k + parity[0]].as_ref().map(Vec::len).unwrap_or(0);
    // Each used parity shard less what the arrived data shards put into it.
    let mut residual: Vec<Vec<u8>> = parity
        .iter()
        .map(|&i| {
            let mut r = shards[k + i].clone().unwrap_or_default();
            for j in (0..k).filter(|j| shards[*j].is_some()) {
                if let Some(d) = shards[j].as_deref() {
                    mul_add(&mut r, d, cauchy(k, i, j));
                }
            }
            r
        })
        .collect();
    // Invert the e x e system (Gauss-Jordan): rows the parity used, columns
    // the lost shards.
    let mut a: Vec<Vec<u8>> = parity.iter().map(|&i| missing.iter().map(|&j| cauchy(k, i, j)).collect()).collect();
    let mut b: Vec<Vec<u8>> = (0..e).map(|r| (0..e).map(|c| (r == c) as u8).collect()).collect();
    for col in 0..e {
        let Some(pivot) = (col..e).find(|&r| a[r][col] != 0) else {
            return false;
        };
        a.swap(col, pivot);
        b.swap(col, pivot);
        let f = inv(a[col][col]);
        for c in 0..e {
            a[col][c] = mul(a[col][c], f);
            b[col][c] = mul(b[col][c], f);
        }
        for r in 0..e {
            if r != col && a[r][col] != 0 {
                let f = a[r][col];
                for c in 0..e {
                    let (ac, bc) = (a[col][c], b[col][c]);
                    a[r][c] ^= mul(f, ac);
                    b[r][c] ^= mul(f, bc);
                }
            }
        }
    }
    // Lost shard missing[x] = sum over r of a^-1[x][r] * residual[r]; with
    // one lost (the usual case) that's the residual scaled, in place.
    if e == 1 {
        let mut out = vec![0u8; len];
        mul_add(&mut out, &residual[0], b[0][0]);
        shards[missing[0]] = Some(out);
        return true;
    }
    let mut rebuilt = Vec::with_capacity(e);
    for x in 0..e {
        let mut out = vec![0u8; len];
        for (r, res) in residual.iter_mut().enumerate() {
            mul_add(&mut out, res, b[x][r]);
        }
        rebuilt.push(out);
    }
    for (j, out) in missing.into_iter().zip(rebuilt) {
        shards[j] = Some(out);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shards(k: usize, len: usize, seed: u8) -> Vec<Vec<u8>> {
        (0..k)
            .map(|i| (0..len).map(|b| (b as u8).wrapping_mul(31).wrapping_add(i as u8 ^ seed)).collect())
            .collect()
    }

    #[test]
    fn the_fast_kernel_matches_plain_multiplication() {
        for c in [0u8, 1, 2, 3, 0x53, 0x8e, 0xff] {
            for len in [0usize, 1, 31, 32, 33, 64, 1146] {
                let src: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
                let mut out: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
                let want: Vec<u8> = out.iter().zip(&src).map(|(o, s)| o ^ mul(c, *s)).collect();
                mul_add(&mut out, &src, c);
                assert_eq!(out, want, "c={c} len={len}");
            }
        }
    }

    #[test]
    fn field_inverse_is_inverse() {
        for a in 1..=255u8 {
            assert_eq!(mul(a, inv(a)), 1, "a={a}");
        }
    }

    #[test]
    fn any_k_of_k_plus_m_rebuild_the_data() {
        let (k, m, len) = (10, 4, 97);
        let data = shards(k, len, 7);
        let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
        let parity = encode(&refs, m);
        // Lose every combination of up to m shards from a sliding window.
        for start in 0..k + m {
            for lost in 1..=m {
                let mut all: Vec<Option<Vec<u8>>> =
                    data.iter().cloned().chain(parity.iter().cloned()).map(Some).collect();
                for x in 0..lost {
                    all[(start + x * 3) % (k + m)] = None;
                }
                assert!(reconstruct(&mut all, k), "start={start} lost={lost}");
                for j in 0..k {
                    assert_eq!(all[j].as_deref(), Some(data[j].as_slice()), "shard {j}");
                }
            }
        }
    }

    #[test]
    fn more_losses_than_parity_fail_cleanly() {
        let data = shards(4, 16, 1);
        let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
        let parity = encode(&refs, 2);
        let mut all: Vec<Option<Vec<u8>>> = data.into_iter().chain(parity).map(Some).collect();
        all[0] = None;
        all[1] = None;
        all[5] = None;
        assert!(!reconstruct(&mut all, 4));
    }

    #[test]
    fn large_blocks_work() {
        let (k, m) = (200, 55);
        let data = shards(k, 64, 3);
        let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
        let parity = encode(&refs, m);
        let mut all: Vec<Option<Vec<u8>>> = data.iter().cloned().chain(parity).map(Some).collect();
        for j in (0..k).step_by(4).take(m) {
            all[j] = None;
        }
        assert!(reconstruct(&mut all, k));
        assert_eq!(all[8].as_deref(), Some(data[8].as_slice()));
    }
}
