use std::fs::File;
use std::os::fd::AsRawFd;

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
}

fn tokenize_baseline(text: &[u8]) -> Vec<Vec<u8>> {
    let mut tokens = Vec::new();
    let mut p = 0;
    let len = text.len();
    while p < len {
        if text[p].is_ascii_alphanumeric() {
            let start = p;
            while p < len && text[p].is_ascii_alphanumeric() {
                p += 1;
            }
            let count = (p - start).min(32);
            let mut tok = Vec::with_capacity(count);
            for k in 0..count {
                tok.push(text[start + k] | 0x20);
            }
            tokens.push(tok);
        } else {
            p += 1;
        }
    }
    tokens
}

use std::arch::x86_64::*;

#[target_feature(enable = "avx2")]
unsafe fn tokenize_simd(text: &[u8]) -> Vec<Vec<u8>> {
    let mut tokens = Vec::new();
    let len = text.len();
    let ptr = text.as_ptr();
    let mut p = 0;

    let v_20 = _mm256_set1_epi8(0x20);
    let v_a_minus_1 = _mm256_set1_epi8((b'a' - 1) as i8);
    let v_z_plus_1 = _mm256_set1_epi8((b'z' + 1) as i8);
    let v_0_minus_1 = _mm256_set1_epi8((b'0' - 1) as i8);
    let v_9_plus_1 = _mm256_set1_epi8((b'9' + 1) as i8);

    let mut prev_is_alnum = 0u32;
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

        let mut start_bits = mask & !((mask << 1) | prev_is_alnum);
        prev_is_alnum = (mask >> 31) & 1;

        let mut spanned = false;
        while start_bits != 0 {
            let off = start_bits.trailing_zeros() as usize;
            start_bits &= start_bits - 1;
            let tok_start = p + off;
            let not_mask = (!mask) >> off;
            let tok_len = if not_mask != 0 {
                not_mask.trailing_zeros() as usize
            } else {
                let mut l = 32 - off;
                while p + off + l < len && (*ptr.add(p + off + l)).is_ascii_alphanumeric() {
                    l += 1;
                }
                spanned = true;
                l
            };
            let count = tok_len.min(32);
            let mut tok = Vec::with_capacity(count);
            for k in 0..count {
                tok.push((*ptr.add(tok_start + k)) | 0x20);
            }
            tokens.push(tok);

            if spanned {
                p = tok_start + tok_len;
                prev_is_alnum = 0;
                break;
            }
        }
        if !spanned {
            p += 32;
        }
    }
    while p < len {
        if (*ptr.add(p)).is_ascii_alphanumeric() {
            let tok_start = p;
            while p < len && (*ptr.add(p)).is_ascii_alphanumeric() {
                p += 1;
            }
            let count = (p - tok_start).min(32);
            let mut tok = Vec::with_capacity(count);
            for k in 0..count {
                tok.push((*ptr.add(tok_start + k)) | 0x20);
            }
            tokens.push(tok);
        } else {
            p += 1;
        }
    }
    tokens
}

fn main() {
    let f = File::open("/tmp/sd-full-b/docs.txt").unwrap();
    let flen = f.metadata().unwrap().len() as usize;
    let ptr = unsafe { mmap(std::ptr::null_mut(), flen, 1, 2, f.as_raw_fd(), 0) as *const u8 };
    let slice = unsafe { std::slice::from_raw_parts(ptr, flen) };

    // Find first 1000 lines
    let mut p = 0;
    for doc in 0..50000 {
        // skip id to tab
        while p < flen && slice[p] != b'\t' && slice[p] != b'\n' { p += 1; }
        if p < flen && slice[p] == b'\t' { p += 1; }
        let text_start = p;
        while p < flen && slice[p] != b'\n' { p += 1; }
        let text = &slice[text_start..p];
        if p < flen && slice[p] == b'\n' { p += 1; }

        let b_toks = tokenize_baseline(text);
        let s_toks = unsafe { tokenize_simd(text) };
        if b_toks != s_toks {
            println!("Mismatch at doc {}: baseline has {} tokens, SIMD has {} tokens", doc, b_toks.len(), s_toks.len());
            for (i, (b, s)) in b_toks.iter().zip(s_toks.iter()).enumerate() {
                if b != s {
                    println!("  Token {} diff: baseline={:?}, SIMD={:?}", i, String::from_utf8_lossy(b), String::from_utf8_lossy(s));
                    break;
                }
            }
            if b_toks.len() != s_toks.len() {
                println!("  Length diff: baseline last={:?}, SIMD last={:?}",
                    b_toks.last().map(|x| String::from_utf8_lossy(x)),
                    s_toks.last().map(|x| String::from_utf8_lossy(x)));
            }
            return;
        }
    }
    println!("First 1000 docs match 100% exactly!");
}
