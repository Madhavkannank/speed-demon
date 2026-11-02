use std::time::Instant;
use std::arch::x86_64::*;

#[inline(always)]
fn hash_word_old(w: &[u8]) -> u64 {
    let mut h = 0x517cc1b727220a95u64;
    for &x in w { h = (h.rotate_left(5) ^ (x as u64)).wrapping_mul(0x9e3779b97f4a7c15); }
    h
}

#[inline(always)]
unsafe fn hash_word_crc(w: &[u8]) -> u64 {
    let mut crc = 0u64;
    let mut p = w.as_ptr();
    let mut len = w.len();
    while len >= 8 {
        let val = (p as *const u64).read_unaligned();
        crc = _mm_crc32_u64(crc, val);
        p = p.add(8);
        len -= 8;
    }
    if len > 0 {
        let mut rem = [0u8; 8];
        std::ptr::copy_nonoverlapping(p, rem.as_mut_ptr(), len);
        let val = u64::from_le_bytes(rem);
        crc = _mm_crc32_u64(crc, val);
    }
    crc
}

fn main() {
    let words: Vec<Vec<u8>> = (0..100_000).map(|i| format!("word_{}", i).into_bytes()).collect();

    let t0 = Instant::now();
    let mut s1 = 0u64;
    for _ in 0..10 {
        for w in &words {
            s1 = s1.wrapping_add(hash_word_old(w));
        }
    }
    let d1 = t0.elapsed();
    println!("Old hash on 1M words: {:?} (sum={})", d1, s1);

    let t0 = Instant::now();
    let mut s2 = 0u64;
    for _ in 0..10 {
        for w in &words {
            s2 = s2.wrapping_add(unsafe { hash_word_crc(w) });
        }
    }
    let d2 = t0.elapsed();
    println!("CRC32 hash on 1M words: {:?} (sum={})", d2, s2);
}
