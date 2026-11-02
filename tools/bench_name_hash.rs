use std::fs::File;
use std::os::unix::fs::FileExt;
use std::time::Instant;
use std::arch::x86_64::*;

#[inline(always)]
fn hash_byte_old(h: u64, b: u8) -> u64 {
    (h.rotate_left(5) ^ (b as u64)).wrapping_mul(0x9e3779b97f4a7c15)
}

#[inline(always)]
unsafe fn hash_byte_crc(crc: u64, b: u8) -> u64 {
    _mm_crc32_u8(crc as u32, b) as u64
}

fn main() {
    let path = "/tmp/sd-full-c/records.csv";
    let file = File::open(path).unwrap();
    let len = 20 * 1024 * 1024;
    let mut buf = vec![0u8; len];
    file.read_exact_at(&mut buf, 0).unwrap();

    // Benchmark old hash_byte
    let t0 = Instant::now();
    let mut h1 = 0x517cc1b727220a95u64;
    for &b in &buf[..len] {
        h1 = hash_byte_old(h1, b);
    }
    let d1 = t0.elapsed();
    println!("Old hash_byte on 20MB: {:?} (hash={})", d1, h1);

    // Benchmark crc32_u8
    let t0 = Instant::now();
    let mut h2 = 0u64;
    for &b in &buf[..len] {
        h2 = unsafe { hash_byte_crc(h2, b) };
    }
    let d2 = t0.elapsed();
    println!("CRC32_u8 on 20MB: {:?} (hash={})", d2, h2);

    // Benchmark crc32_u64 on 8-byte chunks
    let t0 = Instant::now();
    let mut h3 = 0u64;
    let mut p = 0;
    while p + 8 <= len {
        let w = unsafe { (buf.as_ptr().add(p) as *const u64).read_unaligned() };
        h3 = unsafe { _mm_crc32_u64(h3, w) };
        p += 8;
    }
    let d3 = t0.elapsed();
    println!("CRC32_u64 on 20MB (8-byte chunks): {:?} (hash={})", d3, h3);
}
