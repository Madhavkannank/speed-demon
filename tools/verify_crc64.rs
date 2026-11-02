use std::fs::File;
use std::io::{BufRead, BufReader};
use std::collections::HashSet;
use std::arch::x86_64::*;

#[inline(always)]
unsafe fn crc64_name(slice: &[u8]) -> u64 {
    let mut crc1 = 0u64;
    let mut crc2 = 0x517cc1b727220a95u64;
    let mut p = slice.as_ptr();
    let mut len = slice.len();
    while len >= 8 {
        let w = (p as *const u64).read_unaligned();
        crc1 = _mm_crc32_u64(crc1, w);
        crc2 = _mm_crc32_u64(crc2, w.rotate_left(17));
        p = p.add(8);
        len -= 8;
    }
    if len > 0 {
        let mut rem = [0u8; 8];
        std::ptr::copy_nonoverlapping(p, rem.as_mut_ptr(), len);
        let w = u64::from_le_bytes(rem);
        crc1 = _mm_crc32_u64(crc1, w);
        crc2 = _mm_crc32_u64(crc2, w.rotate_left(17));
    }
    (crc1 << 32) | (crc2 as u32 as u64)
}

fn main() {
    let path = "/tmp/sd-full-c/records.csv";
    let file = File::open(path).unwrap();
    let reader = BufReader::new(file);

    let mut distinct_names = HashSet::new();
    let mut distinct_hashes = HashSet::new();
    let mut count = 0;

    for line in reader.lines() {
        let line = line.unwrap();
        if count == 0 { count += 1; continue; } // header
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() < 2 { continue; }
        let name = parts[1];
        distinct_names.insert(name.to_string());
        let h = unsafe { crc64_name(name.as_bytes()) };
        distinct_hashes.insert(h);
        count += 1;
    }
    println!("Total lines parsed: {}", count);
    println!("Distinct names: {}, Distinct hashes: {}", distinct_names.len(), distinct_hashes.len());
    assert_eq!(distinct_names.len(), distinct_hashes.len(), "COLLISION DETECTED!");
    println!("ZERO COLLISIONS ON ENTIRE DATASET!");
}
