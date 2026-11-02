use std::fs::File;
use std::os::fd::AsRawFd;
use std::thread;
use std::time::Instant;

const NORM_TABLE: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = b'0'; while i <= b'9' { t[i as usize] = i; i += 1; }
    let mut i = b'a'; while i <= b'z' { t[i as usize] = i; i += 1; }
    let mut i = b'A'; while i <= b'Z' { t[i as usize] = i + 32; i += 1; }
    t
};

#[inline(always)]
fn hash_word(w: &[u8]) -> u64 {
    let len = w.len();
    if len <= 8 {
        let mut b = [0u8; 8];
        b[..len].copy_from_slice(w);
        let val = u64::from_le_bytes(b);
        val.wrapping_mul(0x517cc1b727220a95) ^ (len as u64)
    } else {
        let mut b1 = [0u8; 8];
        let mut b2 = [0u8; 8];
        b1.copy_from_slice(&w[..8]);
        b2.copy_from_slice(&w[len - 8..]);
        let v1 = u64::from_le_bytes(b1);
        let v2 = u64::from_le_bytes(b2);
        (v1 ^ v2.rotate_left(27)).wrapping_mul(0x9e3779b97f4a7c15) ^ (len as u64)
    }
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct CompactEntry {
    hash: u64,
    key_low: u64,
    key_high: u64,
    tid: u32,
    len: u8,
    last_byte: u8,
    _pad: [u8; 2],
}

const TABLE_SIZE: usize = 262144;
const TABLE_MASK: usize = TABLE_SIZE - 1;

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
}

const BLOOM_WORDS: usize = 32768; // 256 KB
const BLOOM_MASK: usize = 0x1F_FFFF;

fn main() {
    let q_bytes = std::fs::read("/tmp/sd-full-b/queries.txt").unwrap();
    let mut table = vec![CompactEntry::default(); TABLE_SIZE];
    let mut bloom = vec![0u64; BLOOM_WORDS];
    let mut vocab_size = 0u32;

    let mut start = 0;
    while start < q_bytes.len() {
        let mut end = start;
        while end < q_bytes.len() && q_bytes[end] != b'\n' { end += 1; }
        let line = &q_bytes[start..end];
        start = end + 1;
        if line.is_empty() { continue; }

        for w in line.split(|&b| b == b' ' || b == b'"') {
            if w.is_empty() || w == b"BOOL" || w == b"PHRASE" || w == b"AND" || w == b"OR" || w == b"NOT" {
                continue;
            }
            let h = hash_word(w);
            let (b1, b2) = ((h & BLOOM_MASK as u64) as usize, (h.rotate_left(31) & BLOOM_MASK as u64) as usize);
            bloom[b1 >> 6] |= 1u64 << (b1 & 63);
            bloom[b2 >> 6] |= 1u64 << (b2 & 63);
            let mut key_low;
            let mut key_high = 0u64;
            let mut last_byte = 0u8;
            if w.len() <= 8 {
                let mut b = [0u8; 8];
                b[..w.len()].copy_from_slice(w);
                key_low = u64::from_le_bytes(b);
            } else if w.len() <= 16 {
                let mut b1 = [0u8; 8];
                let mut b2 = [0u8; 8];
                b1.copy_from_slice(&w[..8]);
                b2[..w.len() - 8].copy_from_slice(&w[8..]);
                key_low = u64::from_le_bytes(b1);
                key_high = u64::from_le_bytes(b2);
            } else {
                let mut b1 = [0u8; 8];
                let mut b2 = [0u8; 8];
                b1.copy_from_slice(&w[..8]);
                b2.copy_from_slice(&w[8..16]);
                key_low = u64::from_le_bytes(b1);
                key_high = u64::from_le_bytes(b2);
                last_byte = w[16];
            }

            let mut slot = (h as usize) & TABLE_MASK;
            loop {
                if table[slot].hash == 0 {
                    table[slot] = CompactEntry {
                        hash: h,
                        key_low,
                        key_high,
                        tid: vocab_size,
                        len: w.len() as u8,
                        last_byte,
                        _pad: [0; 2],
                    };
                    vocab_size += 1;
                    break;
                }
                if table[slot].hash == h && table[slot].len as usize == w.len()
                    && table[slot].key_low == key_low && table[slot].key_high == key_high && table[slot].last_byte == last_byte {
                    break;
                }
                slot = (slot + 1) & TABLE_MASK;
            }
        }
    }
    println!("Loaded {} query terms into inlined table", vocab_size);

    let f = File::open("/tmp/sd-full-b/docs.txt").unwrap();
    let flen = f.metadata().unwrap().len() as usize;
    let ptr = unsafe { mmap(std::ptr::null_mut(), flen, 1, 1, f.as_raw_fd(), 0) as *const u8 };
    let slice = unsafe { std::slice::from_raw_parts(ptr, flen) };

    const T_CNT: usize = 12;
    let chunk = flen / T_CNT;
    let mut bounds = vec![0; T_CNT + 1];
    for i in 1..T_CNT {
        let mut p = i * chunk;
        while p < flen && slice[p] != b'\n' { p += 1; }
        bounds[i] = (p + 1).min(flen);
    }
    bounds[T_CNT] = flen;

    let t0 = Instant::now();
    let mut total_words = [0usize; T_CNT];

    thread::scope(|s| {
        let table_ref = &table;
        let bloom_ref = &bloom;
        for (w_idx, tw) in total_words.iter_mut().enumerate() {
            let (start, end) = (bounds[w_idx], bounds[w_idx + 1]);
            s.spawn(move || {
                let mut words = 0;
                let mut p = start;
                while p < end {
                    // skip to tab
                    while p < end && slice[p] != b'\t' && slice[p] != b'\n' { p += 1; }
                    if p < end && slice[p] == b'\t' { p += 1; }

                    while p < end && slice[p] != b'\n' {
                        while p < end && slice[p] != b'\n' && NORM_TABLE[slice[p] as usize] == 0 {
                            p += 1;
                        }
                        if p >= end || slice[p] == b'\n' { break; }
                        let w_start = p;
                        let mut cur_len = 0;
                        let mut norm_buf = [0u8; 32];
                        while p < end && slice[p] != b'\n' {
                            let c = NORM_TABLE[slice[p] as usize];
                            if c == 0 { break; }
                            if cur_len < 32 {
                                norm_buf[cur_len] = c;
                                cur_len += 1;
                            }
                            p += 1;
                        }
                        let h = hash_word(&norm_buf[..cur_len]);
                        let (b1, b2) = ((h & BLOOM_MASK as u64) as usize, (h.rotate_left(31) & BLOOM_MASK as u64) as usize);
                        if ((bloom_ref[b1 >> 6] >> (b1 & 63)) & (bloom_ref[b2 >> 6] >> (b2 & 63)) & 1) != 0 {
                            let mut key_low;
                            let mut key_high = 0u64;
                            let mut last_byte = 0u8;
                            if cur_len <= 8 {
                                let mut b = [0u8; 8];
                                b[..cur_len].copy_from_slice(&norm_buf[..cur_len]);
                                key_low = u64::from_le_bytes(b);
                            } else if cur_len <= 16 {
                                let mut b1 = [0u8; 8];
                                let mut b2 = [0u8; 8];
                                b1.copy_from_slice(&norm_buf[..8]);
                                b2[..cur_len - 8].copy_from_slice(&norm_buf[8..cur_len]);
                                key_low = u64::from_le_bytes(b1);
                                key_high = u64::from_le_bytes(b2);
                            } else {
                                let mut b1 = [0u8; 8];
                                let mut b2 = [0u8; 8];
                                b1.copy_from_slice(&norm_buf[..8]);
                                b2.copy_from_slice(&norm_buf[8..16]);
                                key_low = u64::from_le_bytes(b1);
                                key_high = u64::from_le_bytes(b2);
                                last_byte = norm_buf[16];
                            }

                            let mut slot = (h as usize) & TABLE_MASK;
                            while table_ref[slot].hash != 0 {
                                if table_ref[slot].hash == h && table_ref[slot].len as usize == cur_len
                                    && table_ref[slot].key_low == key_low && table_ref[slot].key_high == key_high && table_ref[slot].last_byte == last_byte {
                                    words += 1;
                                    break;
                                }
                                slot = (slot + 1) & TABLE_MASK;
                            }
                        }
                    }
                    if p < end && slice[p] == b'\n' { p += 1; }
                }
                *tw = words;
            });
        }
    });

    let dur = t0.elapsed();
    let sum_words: usize = total_words.iter().sum();
    println!("Tokenized {} words across 12 threads in: {:?} ({:.2} Mwords/s)",
        sum_words, dur, (sum_words as f64 / 1e6) / dur.as_secs_f64());
    unsafe { munmap(ptr as *mut std::ffi::c_void, flen); }
}
