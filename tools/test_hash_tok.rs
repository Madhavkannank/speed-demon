use std::fs;
use std::time::Instant;

#[inline(always)]
fn hash_token(ptr: *const u8, len: usize) -> (u32, u64, u64) {
    if len <= 8 {
        let mask = if len == 8 { u64::MAX } else { (1u64 << (len * 8)) - 1 };
        let raw = unsafe { (ptr as *const u64).read_unaligned() };
        let w0 = (raw | 0x2020202020202020u64) & mask;
        let h = w0.wrapping_mul(0x517cc1b727220a95u64) ^ (len as u64);
        let h32 = (h ^ (h >> 32)) as u32;
        (h32, w0, 0)
    } else {
        let raw0 = unsafe { (ptr as *const u64).read_unaligned() };
        let w0 = raw0 | 0x2020202020202020u64;
        let rem_len = len - 8;
        let mask = if rem_len >= 8 { u64::MAX } else { (1u64 << (rem_len * 8)) - 1 };
        let raw1 = unsafe { (ptr.add(8) as *const u64).read_unaligned() };
        let w1 = (raw1 | 0x2020202020202020u64) & mask;
        let h = (w0.wrapping_mul(0x517cc1b727220a95u64) ^ w1.rotate_left(27)).wrapping_mul(0x9e3779b97f4a7c15u64) ^ (len as u64);
        let h32 = (h ^ (h >> 32)) as u32;
        (h32, w0, w1)
    }
}

fn main() {
    let q_bytes = fs::read("/tmp/sd-full-b/queries.txt").unwrap();
    let mut words = Vec::new();
    let mut start = 0;
    while start < q_bytes.len() {
        let mut end = start;
        while end < q_bytes.len() && q_bytes[end] != b'\n' { end += 1; }
        let line = &q_bytes[start..end];
        start = end + 1;
        for w in line.split(|&b| b == b' ' || b == b'"') {
            if !w.is_empty() && w != b"BOOL" && w != b"PHRASE" && w != b"AND" && w != b"OR" && w != b"NOT" {
                words.push(w.to_vec());
            }
        }
    }
    println!("Total query terms: {}", words.len());
    let t0 = Instant::now();
    let mut hashes = Vec::with_capacity(words.len());
    for w in &words {
        let mut buf = [0u8; 32];
        buf[..w.len()].copy_from_slice(w);
        let (h, w0, w1) = hash_token(buf.as_ptr(), w.len());
        hashes.push((h, w0, w1));
    }
    println!("Hashed {} words in {:?}", words.len(), t0.elapsed());
}
