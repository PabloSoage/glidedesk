//! Reed-Solomon parity for Moonlight video, as the client's nanors library recovers it
//! (nanors `rs.c` at the commit in THIRD_PARTY.md, MIT): GF(2^8) with the polynomial 0x11d and
//! the Cauchy matrix whose entry for parity shard `j` and data shard `i` is
//! `1 / ((parity_shards + i) ^ j)`.
//!
//! Each parity byte is a sum of products of the data bytes in the same column, so the work is
//! "multiply a whole packet by a constant and add it": done 32 bytes at a time with AVX2 (two
//! table lookups on the nibbles), and with a 256-byte table per constant elsewhere.

/// Products in GF(2^8): `MUL[a][b]`.
static MUL: [[u8; 256]; 256] = mul_table();

const fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        let carry = a & 0x80 != 0;
        a <<= 1;
        if carry {
            a ^= 0x1d; // x^8 = x^4 + x^3 + x^2 + 1
        }
        b >>= 1;
    }
    p
}

const fn mul_table() -> [[u8; 256]; 256] {
    let mut t = [[0u8; 256]; 256];
    let mut a = 0;
    while a < 256 {
        let mut b = 0;
        while b < 256 {
            t[a][b] = gf_mul(a as u8, b as u8);
            b += 1;
        }
        a += 1;
    }
    t
}

fn inv(a: u8) -> u8 {
    // a^254 = a^-1 (a != 0); 0 maps to 0 as in nanors' table.
    let mut r = 1u8;
    let mut base = a;
    let mut e = 254u32;
    while e > 0 {
        if e & 1 != 0 {
            r = MUL[r as usize][base as usize];
        }
        base = MUL[base as usize][base as usize];
        e >>= 1;
    }
    if a == 0 { 0 } else { r }
}

/// The coefficient of data shard `i` in parity shard `j` (nanors `reed_solomon_new_static`).
pub fn coefficient(parity_shards: usize, i: usize, j: usize) -> u8 {
    inv(((parity_shards + i) ^ j) as u8)
}

/// `dst ^= c * src`, byte by byte in GF(2^8).
fn mul_add(c: u8, src: &[u8], dst: &mut [u8]) {
    match c {
        0 => {}
        1 => dst.iter_mut().zip(src).for_each(|(d, s)| *d ^= s),
        _ => {
            #[cfg(target_arch = "x86_64")]
            if std::arch::is_x86_feature_detected!("avx2") {
                // SAFETY: the CPU has AVX2 (checked above).
                unsafe { mul_add_avx2(c, src, dst) };
                return;
            }
            let row = &MUL[c as usize];
            dst.iter_mut()
                .zip(src)
                .for_each(|(d, s)| *d ^= row[*s as usize]);
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn mul_add_avx2(c: u8, src: &[u8], dst: &mut [u8]) {
    use std::arch::x86_64::*;
    let row = &MUL[c as usize];
    // c * x = c * (x & 0x0f) ^ c * (x & 0xf0): two 16-entry tables.
    let mut lo = [0u8; 16];
    let mut hi = [0u8; 16];
    for n in 0..16 {
        lo[n] = row[n];
        hi[n] = row[n << 4];
    }
    let n = src.len().min(dst.len());
    let chunks = n / 32;
    // SAFETY: every load and store below stays within the first `chunks * 32` bytes of `src`
    // and `dst`, which both have at least `n` bytes; unaligned loads and stores are used.
    unsafe {
        let lo_t = _mm256_broadcastsi128_si256(_mm_loadu_si128(lo.as_ptr().cast()));
        let hi_t = _mm256_broadcastsi128_si256(_mm_loadu_si128(hi.as_ptr().cast()));
        let mask = _mm256_set1_epi8(0x0f);
        for k in 0..chunks {
            let s = _mm256_loadu_si256(src.as_ptr().add(k * 32).cast());
            let d = _mm256_loadu_si256(dst.as_ptr().add(k * 32).cast());
            let l = _mm256_shuffle_epi8(lo_t, _mm256_and_si256(s, mask));
            let h = _mm256_shuffle_epi8(hi_t, _mm256_and_si256(_mm256_srli_epi64(s, 4), mask));
            let r = _mm256_xor_si256(d, _mm256_xor_si256(l, h));
            _mm256_storeu_si256(dst.as_mut_ptr().add(k * 32).cast(), r);
        }
    }
    for k in chunks * 32..n {
        dst[k] ^= row[src[k] as usize];
    }
}

/// Fills `parity` (each as long as the data shards) from `data`.
pub fn encode(data: &[&[u8]], parity: &mut [&mut [u8]]) {
    let ps = parity.len();
    for (j, out) in parity.iter_mut().enumerate() {
        out.fill(0);
        for (i, shard) in data.iter().enumerate() {
            mul_add(coefficient(ps, i, j), shard, out);
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn field_matches_nanors_tables() {
        // The first entries of nanors' GF2_8_INV (deps/obl/gf2_8_tables.h, polynomial 285).
        let expected = [
            0u8, 1, 142, 244, 71, 167, 122, 186, 173, 157, 221, 152, 61, 170, 93, 150,
        ];
        for (a, e) in expected.iter().enumerate() {
            assert_eq!(inv(a as u8), *e, "inverse of {a}");
        }
        for a in 1..=255u8 {
            assert_eq!(MUL[a as usize][inv(a) as usize], 1);
        }
    }

    #[test]
    fn simd_and_tables_agree() {
        let src: Vec<u8> = (0..1000).map(|i| (i * 37 + 11) as u8).collect();
        for c in [2u8, 3, 0x53, 0xff] {
            let mut a: Vec<u8> = (0..1000).map(|i| (i * 13) as u8).collect();
            let mut b = a.clone();
            mul_add(c, &src, &mut a);
            let row = &MUL[c as usize];
            b.iter_mut()
                .zip(&src)
                .for_each(|(d, s)| *d ^= row[*s as usize]);
            assert_eq!(a, b);
        }
    }

    /// Solves for lost data shards the way any systematic decoder does: the surviving rows of
    /// [I; P] times the data equal the surviving shards.
    pub fn recover(data: &mut [Vec<u8>], lost: &[usize], parity: &[Vec<u8>]) {
        let (ds, ps) = (data.len(), parity.len());
        let rows: Vec<Vec<u8>> = (0..ps)
            .take(lost.len())
            .map(|j| (0..ds).map(|i| coefficient(ps, i, j)).collect())
            .collect();
        // Move what is known to the right-hand side.
        let mut rhs: Vec<Vec<u8>> = parity[..lost.len()].to_vec();
        for (j, r) in rows.iter().enumerate() {
            for i in (0..ds).filter(|i| !lost.contains(i)) {
                let c = r[i];
                for (o, s) in rhs[j].iter_mut().zip(&data[i]) {
                    *o ^= MUL[c as usize][*s as usize];
                }
            }
        }
        // Gaussian elimination on the square system over the lost columns.
        let mut m: Vec<Vec<u8>> = rows
            .iter()
            .map(|r| lost.iter().map(|&i| r[i]).collect())
            .collect();
        let n = lost.len();
        for col in 0..n {
            let p = (col..n).find(|&r| m[r][col] != 0).unwrap();
            m.swap(col, p);
            rhs.swap(col, p);
            let f = inv(m[col][col]);
            for v in m[col].iter_mut() {
                *v = MUL[f as usize][*v as usize];
            }
            for v in rhs[col].iter_mut() {
                *v = MUL[f as usize][*v as usize];
            }
            for r in 0..n {
                if r != col && m[r][col] != 0 {
                    let g = m[r][col];
                    let (pivot_m, pivot_rhs) = (m[col].clone(), rhs[col].clone());
                    for (v, p) in m[r].iter_mut().zip(&pivot_m) {
                        *v ^= MUL[g as usize][*p as usize];
                    }
                    for (v, p) in rhs[r].iter_mut().zip(&pivot_rhs) {
                        *v ^= MUL[g as usize][*p as usize];
                    }
                }
            }
        }
        for (k, &i) in lost.iter().enumerate() {
            data[i] = rhs[k].clone();
        }
    }

    #[test]
    fn lost_shards_come_back() {
        let (ds, ps, len) = (12, 3, 1408);
        let data: Vec<Vec<u8>> = (0..ds)
            .map(|i| (0..len).map(|k| (i * 31 + k * 7 + k / 5) as u8).collect())
            .collect();
        let mut parity = vec![vec![0u8; len]; ps];
        {
            let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
            let mut outs: Vec<&mut [u8]> = parity.iter_mut().map(Vec::as_mut_slice).collect();
            encode(&refs, &mut outs);
        }
        let mut damaged = data.clone();
        for &i in &[0, 5, 11] {
            damaged[i] = vec![0xaa; len];
        }
        recover(&mut damaged, &[0, 5, 11], &parity);
        assert_eq!(damaged, data);
    }
}
