use std::arch::x86_64::*;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::time::Instant;

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
    fn madvise(addr: *mut std::ffi::c_void, length: usize, advice: i32) -> i32;
}

#[target_feature(enable = "avx2")]
unsafe fn tokenize_avx2(slice: &[u8]) -> (usize, u64) {
    let len = slice.len();
    let ptr = slice.as_ptr();
    let mut p = 0;
    let mut total_tokens = 0usize;
    let mut len_sum = 0u64;

    let v_20 = _mm256_set1_epi8(0x20);
    let v_a_minus_1 = _mm256_set1_epi8((b'a' - 1) as i8);
    let v_z_plus_1 = _mm256_set1_epi8((b'z' + 1) as i8);
    let v_0_minus_1 = _mm256_set1_epi8((b'0' - 1) as i8);
    let v_9_plus_1 = _mm256_set1_epi8((b'9' + 1) as i8);

    let mut in_token = false;
    let mut token_start = 0usize;

    while p + 32 <= len {
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

        if mask == 0 {
            if in_token {
                total_tokens += 1;
                len_sum += (p - token_start) as u64;
                in_token = false;
            }
        } else if mask == 0xFFFF_FFFF {
            if !in_token {
                in_token = true;
                token_start = p;
            }
        } else {
            let carry = in_token as u32;
            let mut starts = mask & !((mask << 1) | carry);
            let mut ends = (!mask) & !(mask >> 1); // transitions from 1 to 0
            // Actually simpler: process transitions or iterate
            let mut m = mask;
            let mut curr = p;
            // Count tokens using tzcnt on starts
            while starts != 0 {
                let off = starts.trailing_zeros() as usize;
                starts &= starts - 1;
                total_tokens += 1;
            }
            in_token = (mask & (1 << 31)) != 0;
        }
        p += 32;
    }
    (total_tokens, len_sum)
}

fn main() {
    let f = File::open("/tmp/sd-full-b/docs.txt").unwrap();
    let flen = f.metadata().unwrap().len() as usize;
    let ptr = unsafe { mmap(std::ptr::null_mut(), flen, 1, 1, f.as_raw_fd(), 0) as *const u8 };
    unsafe { madvise(ptr as *mut _, flen, 22); }
    let slice = unsafe { std::slice::from_raw_parts(ptr, flen) };

    println!("Warmup run...");
    let (cnt, _) = unsafe { tokenize_avx2(slice) };
    println!("Tokens found: {}", cnt);

    println!("Timing 5 single-threaded runs on 254 MB docs.txt...");
    for i in 0..5 {
        let t0 = Instant::now();
        let (tokens, _) = unsafe { tokenize_avx2(slice) };
        let d = t0.elapsed();
        println!("Run {}: {:?} ({} tokens, {:.1} MB/s)", i + 1, d, tokens, (flen as f64 / 1e6) / d.as_secs_f64());
    }
}
