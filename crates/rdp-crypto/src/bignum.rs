//! Minimal unsigned big integer — just enough for RSA modular exponentiation
//! (the Standard RDP Security client-random encryption). Limbs are `u32`,
//! stored little-endian and normalised (no trailing zero limb). Not
//! constant-time; used only for a one-shot public-key operation.

use core::cmp::Ordering;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BigUint {
    /// Little-endian base-2^32 limbs; empty == zero.
    limbs: Vec<u32>,
}

impl BigUint {
    fn normalize(&mut self) {
        while self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
    }

    pub fn zero() -> Self {
        Self { limbs: Vec::new() }
    }

    fn from_u32(v: u32) -> Self {
        let mut x = Self { limbs: vec![v] };
        x.normalize();
        x
    }

    pub fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    /// Build from little-endian bytes.
    pub fn from_bytes_le(bytes: &[u8]) -> Self {
        let mut limbs = Vec::with_capacity(bytes.len() / 4 + 1);
        for chunk in bytes.chunks(4) {
            let mut v = 0u32;
            for (i, &b) in chunk.iter().enumerate() {
                v |= (b as u32) << (8 * i);
            }
            limbs.push(v);
        }
        let mut x = Self { limbs };
        x.normalize();
        x
    }

    /// Build from big-endian bytes.
    pub fn from_bytes_be(bytes: &[u8]) -> Self {
        let le: Vec<u8> = bytes.iter().rev().copied().collect();
        Self::from_bytes_le(&le)
    }

    /// Serialize to little-endian bytes, zero-padded/truncated to `len`.
    pub fn to_bytes_le(&self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.limbs.len() * 4);
        for &limb in &self.limbs {
            out.extend_from_slice(&limb.to_le_bytes());
        }
        out.resize(len, 0);
        out.truncate(len);
        out
    }

    fn cmp(&self, other: &Self) -> Ordering {
        if self.limbs.len() != other.limbs.len() {
            return self.limbs.len().cmp(&other.limbs.len());
        }
        for i in (0..self.limbs.len()).rev() {
            if self.limbs[i] != other.limbs[i] {
                return self.limbs[i].cmp(&other.limbs[i]);
            }
        }
        Ordering::Equal
    }

    fn bit_len(&self) -> usize {
        match self.limbs.last() {
            None => 0,
            Some(&top) => self.limbs.len() * 32 - top.leading_zeros() as usize,
        }
    }

    fn bit(&self, i: usize) -> bool {
        let limb = i / 32;
        limb < self.limbs.len() && (self.limbs[limb] >> (i % 32)) & 1 == 1
    }

    /// Multiply by two.
    fn shl1(&self) -> Self {
        let mut limbs = Vec::with_capacity(self.limbs.len() + 1);
        let mut carry = 0u32;
        for &l in &self.limbs {
            let v = ((l as u64) << 1) | carry as u64;
            limbs.push(v as u32);
            carry = (v >> 32) as u32;
        }
        if carry != 0 {
            limbs.push(carry);
        }
        let mut x = Self { limbs };
        x.normalize();
        x
    }

    /// `self - other`, assuming `self >= other`.
    fn sub(&self, other: &Self) -> Self {
        let mut limbs = Vec::with_capacity(self.limbs.len());
        let mut borrow = 0i64;
        for i in 0..self.limbs.len() {
            let o = if i < other.limbs.len() {
                other.limbs[i] as i64
            } else {
                0
            };
            let mut d = self.limbs[i] as i64 - o - borrow;
            if d < 0 {
                d += 1 << 32;
                borrow = 1;
            } else {
                borrow = 0;
            }
            limbs.push(d as u32);
        }
        let mut x = Self { limbs };
        x.normalize();
        x
    }

    fn mul(&self, other: &Self) -> Self {
        if self.is_zero() || other.is_zero() {
            return Self::zero();
        }
        let mut limbs = vec![0u32; self.limbs.len() + other.limbs.len()];
        for (i, &a) in self.limbs.iter().enumerate() {
            let mut carry = 0u64;
            for (j, &b) in other.limbs.iter().enumerate() {
                let cur = limbs[i + j] as u64 + a as u64 * b as u64 + carry;
                limbs[i + j] = cur as u32;
                carry = cur >> 32;
            }
            let mut k = i + other.limbs.len();
            while carry > 0 {
                let cur = limbs[k] as u64 + carry;
                limbs[k] = cur as u32;
                carry = cur >> 32;
                k += 1;
            }
        }
        let mut x = Self { limbs };
        x.normalize();
        x
    }

    /// `self mod modulus`, via bit-by-bit long division.
    fn rem(&self, modulus: &Self) -> Self {
        if self.cmp(modulus) == Ordering::Less {
            return self.clone();
        }
        let mut rem = Self::zero();
        for i in (0..self.bit_len()).rev() {
            rem = rem.shl1();
            if self.bit(i) {
                if rem.limbs.is_empty() {
                    rem.limbs.push(1);
                } else {
                    rem.limbs[0] |= 1;
                }
            }
            if rem.cmp(modulus) != Ordering::Less {
                rem = rem.sub(modulus);
            }
        }
        rem
    }

    /// Modular exponentiation: `self^exp mod modulus`.
    pub fn modpow(&self, exp: &Self, modulus: &Self) -> Self {
        if modulus.is_zero() {
            return Self::zero();
        }
        let mut result = Self::from_u32(1).rem(modulus);
        let mut base = self.rem(modulus);
        for i in 0..exp.bit_len() {
            if exp.bit(i) {
                result = result.mul(&base).rem(modulus);
            }
            base = base.mul(&base).rem(modulus);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_rsa_roundtrip() {
        // Textbook RSA: n=3233, e=17, d=413. enc(65)=2790, dec(2790)=65.
        let n = BigUint::from_u32(3233);
        let e = BigUint::from_u32(17);
        let d = BigUint::from_u32(413);
        let m = BigUint::from_u32(65);

        let c = m.modpow(&e, &n);
        assert_eq!(c, BigUint::from_u32(2790));
        assert_eq!(c.modpow(&d, &n), m);
    }

    #[test]
    fn modpow_basic() {
        assert_eq!(
            BigUint::from_u32(2).modpow(&BigUint::from_u32(10), &BigUint::from_u32(1000)),
            BigUint::from_u32(24)
        ); // 1024 mod 1000
        assert_eq!(
            BigUint::from_u32(4).modpow(&BigUint::from_u32(13), &BigUint::from_u32(497)),
            BigUint::from_u32(445)
        );
    }

    #[test]
    fn mul_full_width() {
        let max = BigUint::from_u32(0xFFFF_FFFF);
        // 0xFFFFFFFF^2 = 0xFFFFFFFE00000001.
        assert_eq!(
            max.mul(&max).to_bytes_le(8),
            [0x01, 0x00, 0x00, 0x00, 0xFE, 0xFF, 0xFF, 0xFF]
        );
    }

    #[test]
    fn bytes_roundtrip_le_and_be() {
        let le = [0x78, 0x56, 0x34, 0x12, 0x9a];
        let n = BigUint::from_bytes_le(&le);
        assert_eq!(n.to_bytes_le(5), le);
        assert_eq!(
            BigUint::from_bytes_be(&[0x12, 0x34]).to_bytes_le(2),
            [0x34, 0x12]
        );
    }

    #[test]
    fn multi_limb_modpow_fermat() {
        // Fermat's little theorem: for prime p, a^(p-1) mod p == 1.
        // p = 4294967311 (the smallest prime above 2^32) forces 2-limb math.
        let p = BigUint::from_bytes_le(&4_294_967_311u64.to_le_bytes());
        let p_minus_1 = BigUint::from_bytes_le(&4_294_967_310u64.to_le_bytes());
        assert_eq!(
            BigUint::from_u32(5).modpow(&p_minus_1, &p),
            BigUint::from_u32(1)
        );
        assert_eq!(
            BigUint::from_u32(123_456).modpow(&p_minus_1, &p),
            BigUint::from_u32(1)
        );
    }
}
