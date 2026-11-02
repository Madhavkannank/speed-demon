use std::arch::x86_64::*;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::time::Instant;

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
    fn madvise(addr: *mut std::ffi::c_void, length: usize, advice: i32) -> i32;
}

const BLOOM_BITS: usize = 1 << 20; // 1M bits = 128 KB
const BLOOM_MASK: usize = BLOOM_BITS - 1;
const TABLE_SIZE: usize = 262144; // 256K slots
const TABLE_MASK: usize = TABLE_SIZE - 1;

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct TermSlot {
    w0: u64,
    w1: u64,
    tag: u32,
    id: u32,
}

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
    let q_bytes = std::fs::read("/tmp/sd-full-b/queries.txt").unwrap();
    let mut table = vec![TermSlot::default(); TABLE_SIZE];
    let mut bloom = vec![0u64; BLOOM_BITS / 64];
    let mut vocab_size = 1u32; // 0 = not found

    let mut start = 0;
    while start < q_bytes.len() {
        let mut end = start;
        while end < q_bytes.len() && q_bytes[end] != b'\n' { end += 1; }
        let line = &q_bytes[start..end];
        start = end + 1;
        for w in line.split(|&b| b == b' ' || b == b'"') {
            if !w.is_empty() && w != b"BOOL" && w != b"PHRASE" && w != b"AND" && w != b"OR" && w != b"NOT" {
                let mut buf = [0u8; 32];
                let len = w.len().min(16);
                buf[..len].copy_from_slice(&w[..len]);
                let (h, w0, w1) = hash_token(buf.as_ptr(), len);
                let b_idx = (h as usize) & BLOOM_MASK;
                bloom[b_idx >> 6] |= 1u64 << (b_idx & 63);

                let mut slot = (h as usize) & TABLE_MASK;
                loop {
                    if table[slot].id == 0 {
                        table[slot] = TermSlot { w0, w1, tag: h, id: vocab_size };
                        vocab_size += 1;
                        break;
                    }
                    if table[slot].tag == h && table[slot].w0 == w0 && table[slot].w1 == w1 {
                        break;
                    }
                    slot = (slot + 1) & TABLE_MASK;
                }
            }
        }
    }
    println!("Loaded {} unique query terms into table", vocab_size - 1);

    let f = File::open("/tmp/sd-full-b/docs.txt").unwrap();
    let flen = f.metadata().unwrap().len() as usize;
    let ptr = unsafe { mmap(std::ptr::null_mut(), flen, 1, 1, f.as_raw_fd(), 0) as *const u8 };
    unsafe { madvise(ptr as *mut _, flen, 22); }
    let slice = unsafe { std::slice::from_raw_parts(ptr, flen) };

    println!("Timing AVX2 tokenization + 128KB Bloom + Table lookup on 1 thread...");
    let t0 = Instant::now();
    let mut matched_tokens = 0usize;

    unsafe {
        let v_20 = _mm256_set1_epi8(0x20);
        let v_a_minus_1 = _mm256_set1_epi8((b'a' - 1) as i8);
        let v_z_plus_1 = _mm256_set1_epi8((b'z' + 1) as i8);
        let v_0_minus_1 = _mm256_set1_epi8((b'0' - 1) as i8);
        let v_9_plus_1 = _mm256_set1_epi8((b'9' + 1) as i8);

        let mut p = 0;
        let mut in_token = false;

        while p + 32 <= flen {
            let v = _mm256_loadu_si256(ptr.add(p) as *const __m256i);
            let c_or_20 = _mm256_or_si256(v, v_20);

            let ge_a = _mm256_cmpgt_epi8(c_or_20, v_a_minus_1);
            let le_z = _mm256_cmpgt_epi8(v_z_plus_1, c_or_20);
            let is_alpha = _mm256_and_si256(ge_a, le_z);

            let ge_0 = _mm256_cmpgt_epi8(v, v_0_minus_1);
            let le_9 = _mm256_cmpgt_epi8(v_9_plus_1, v);
            let is_digit = _mm256_and_si256(ge_0, le_9);

            let is_alnum = _mm256_or_si256(is_alpha, is_digit);
            let mask = _mm256_movemask_epi8(is_alnum) as u32;

            if mask != 0 {
                let carry = in_token as u32;
                let mut starts = mask & !((mask << 1) | carry);
                while starts != 0 {
                    let start_bit = starts.trailing_zeros() as usize;
                    starts &= starts - 1;

                    let trailing = (!mask) >> start_bit;
                    let tok_len = if trailing != 0 {
                        trailing.trailing_zeros() as usize
                    } else {
                        // extends past block
                        let mut l = 32 - start_bit;
                        while p + start_bit + l < flen && slice[p + start_bit + l].is_ascii_alphanumeric() {
                            l += 1;
                        }
                        l
                    };

                    let (h, w0, w1) = hash_token(ptr.add(p + start_bit), tok_len);
                    let b_idx = (h as usize) & BLOOM_MASK;
                    if (bloom[b_idx >> 6] & (1u64 << (b_idx & 63))) != 0 {
                        let mut slot = (h as usize) & TABLE_MASK;
                        loop {
                            let entry = &table[slot];
                            if entry.id == 0 { break; }
                            if entry.tag == h && entry.w0 == w0 && entry.w1 == w1 {
                                matched_tokens += 1;
                                break;
                            }
                            slot = (slot + 1) & TABLE_MASK;
                        }
                    }
                }
                in_token = (mask & (1 << 31)) != 0;
            } else {
                in_token = false;
            }
            p += 32;
        }
    }

    let d = t0.elapsed();
    println!("Done in {:?}: {} matched tokens ({:.1} MB/s)", d, matched_tokens, (flen as f64 / 1e6) / d.as_secs_f64());
}
