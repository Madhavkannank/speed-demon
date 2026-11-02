use std::arch::x86_64::*;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::time::Instant;

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
    fn madvise(addr: *mut std::ffi::c_void, length: usize, advice: i32) -> i32;
}

const BLOOM_BITS: usize = 1 << 20;
const BLOOM_MASK: usize = BLOOM_BITS - 1;
const TABLE_SIZE: usize = 262144;
const TABLE_MASK: usize = TABLE_SIZE - 1;

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct TableSlot {
    w0: u64,
    w1: u64,
    tag: u32,
    id: u32,
    len: u8,
    last_byte: u8,
    _pad: [u8; 6],
}

#[inline(always)]
fn hash_token(ptr: *const u8, len: usize) -> (u32, u64, u64, u8) {
    if len <= 8 {
        let mask = if len == 8 { u64::MAX } else { (1u64 << (len * 8)) - 1 };
        let raw = unsafe { (ptr as *const u64).read_unaligned() };
        let w0 = (raw | 0x2020202020202020u64) & mask;
        let h = w0.wrapping_mul(0x517cc1b727220a95u64) ^ (len as u64);
        let h32 = (h ^ (h >> 32)) as u32;
        (h32, w0, 0, 0)
    } else {
        let raw0 = unsafe { (ptr as *const u64).read_unaligned() };
        let w0 = raw0 | 0x2020202020202020u64;
        let rem_len = len - 8;
        let mask = if rem_len >= 8 { u64::MAX } else { (1u64 << (rem_len * 8)) - 1 };
        let raw1 = unsafe { (ptr.add(8) as *const u64).read_unaligned() };
        let w1 = (raw1 | 0x2020202020202020u64) & mask;
        let last_b = if len > 16 { unsafe { *ptr.add(16) | 0x20 } } else { 0 };
        let h = (w0.wrapping_mul(0x517cc1b727220a95u64) ^ w1.rotate_left(27) ^ (last_b as u64).rotate_left(43))
            .wrapping_mul(0x9e3779b97f4a7c15u64) ^ (len as u64);
        let h32 = (h ^ (h >> 32)) as u32;
        (h32, w0, w1, last_b)
    }
}

fn main() {
    let f = File::open("/tmp/sd-full-b/docs.txt").unwrap();
    let flen = f.metadata().unwrap().len() as usize;
    let ptr = unsafe { mmap(std::ptr::null_mut(), flen, 1, 2, f.as_raw_fd(), 0) as *const u8 };
    unsafe { madvise(ptr as *mut _, flen, 22); }
    let slice = unsafe { std::slice::from_raw_parts(ptr, flen) };

    let bloom = vec![0u64; BLOOM_BITS / 64];
    let slots = vec![TableSlot::default(); TABLE_SIZE];

    let num_threads = 12;
    let chunk = flen / num_threads;
    let mut bounds = Vec::new();
    bounds.push(0);
    for i in 1..num_threads {
        let mut p = i * chunk;
        while p < flen && slice[p] != b'\n' { p += 1; }
        if p < flen { p += 1; }
        bounds.push(p);
    }
    bounds.push(flen);

    for test_mode in 1..=3 {
        let t0 = Instant::now();
        std::thread::scope(|s| {
            for w in 0..num_threads {
                let (st, en) = (bounds[w], bounds[w + 1]);
                let s_slice = &slice[st..en];
                let b_ref = &bloom;
                let s_ref = &slots;
                s.spawn(move || unsafe {
                    let mut p = 0;
                    let len = s_slice.len();
                    let p_ptr = s_slice.as_ptr();

                    let v_20 = _mm256_set1_epi8(0x20);
                    let v_a_minus_1 = _mm256_set1_epi8((b'a' - 1) as i8);
                    let v_z_plus_1 = _mm256_set1_epi8((b'z' + 1) as i8);
                    let v_0_minus_1 = _mm256_set1_epi8((b'0' - 1) as i8);
                    let v_9_plus_1 = _mm256_set1_epi8((b'9' + 1) as i8);
                    let v_nl = _mm256_set1_epi8(b'\n' as i8);

                    let mut total_tid = 0u64;

                    while p < len {
                        while p < len && *p_ptr.add(p) != b'\t' && *p_ptr.add(p) != b'\n' { p += 1; }
                        if p < len && *p_ptr.add(p) == b'\t' { p += 1; }

                        let mut prev_is_alnum = 0u32;
                        let mut doc_ended = false;

                        while p + 32 <= len {
                            let v = _mm256_loadu_si256(p_ptr.add(p) as *const __m256i);
                            let is_nl = _mm256_cmpeq_epi8(v, v_nl);
                            let nl_mask = _mm256_movemask_epi8(is_nl) as u32;

                            let c_or_20 = _mm256_or_si256(v, v_20);
                            let ge_a = _mm256_cmpgt_epi8(c_or_20, v_a_minus_1);
                            let le_z = _mm256_cmpgt_epi8(v_z_plus_1, c_or_20);
                            let is_alpha = _mm256_and_si256(ge_a, le_z);

                            let ge_0 = _mm256_cmpgt_epi8(v, v_0_minus_1);
                            let le_9 = _mm256_cmpgt_epi8(v_9_plus_1, v);
                            let is_digit = _mm256_and_si256(ge_0, le_9);

                            let is_alnum = _mm256_or_si256(is_alpha, is_digit);
                            let mut mask = _mm256_movemask_epi8(is_alnum) as u32;

                            let has_nl = nl_mask != 0;
                            if has_nl {
                                let nl_off = nl_mask.trailing_zeros();
                                mask &= (1u32 << nl_off).wrapping_sub(1);
                            }

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
                                    while p + off + l < len && (*p_ptr.add(p + off + l)).is_ascii_alphanumeric() {
                                        l += 1;
                                    }
                                    spanned = true;
                                    l
                                };

                                if test_mode >= 2 {
                                    let (h, w0, w1, last_b) = hash_token(p_ptr.add(tok_start), tok_len);
                                    let b_idx = (h as usize) & BLOOM_MASK;
                                    if test_mode >= 3 && ((b_ref[b_idx >> 6] >> (b_idx & 63)) & 1) != 0 {
                                        let mut idx = (h as usize) & TABLE_MASK;
                                        loop {
                                            let slot = s_ref.get_unchecked(idx);
                                            if slot.id == 0 { break; }
                                            if slot.tag == h && slot.len as usize == tok_len && slot.w0 == w0 && slot.w1 == w1 && slot.last_byte == last_b {
                                                total_tid += slot.id as u64;
                                                break;
                                            }
                                            idx = (idx + 1) & TABLE_MASK;
                                        }
                                    }
                                }

                                if spanned {
                                    p = tok_start + tok_len;
                                    prev_is_alnum = 0;
                                    break;
                                }
                            }

                            if has_nl {
                                let nl_off = nl_mask.trailing_zeros() as usize;
                                p += nl_off + 1;
                                doc_ended = true;
                                break;
                            } else if !spanned {
                                p += 32;
                            }
                        }

                        if !doc_ended {
                            while p < len && *p_ptr.add(p) != b'\n' {
                                if (*p_ptr.add(p)).is_ascii_alphanumeric() {
                                    let tok_start = p;
                                    while p < len && (*p_ptr.add(p)).is_ascii_alphanumeric() { p += 1; }
                                    let tok_len = p - tok_start;
                                    if test_mode >= 2 {
                                        let (h, w0, w1, last_b) = hash_token(p_ptr.add(tok_start), tok_len);
                                        let b_idx = (h as usize) & BLOOM_MASK;
                                        if test_mode >= 3 && ((b_ref[b_idx >> 6] >> (b_idx & 63)) & 1) != 0 {
                                            let mut idx = (h as usize) & TABLE_MASK;
                                            loop {
                                                let slot = s_ref.get_unchecked(idx);
                                                if slot.id == 0 { break; }
                                                if slot.tag == h && slot.len as usize == tok_len && slot.w0 == w0 && slot.w1 == w1 && slot.last_byte == last_b {
                                                    total_tid += slot.id as u64;
                                                    break;
                                                }
                                                idx = (idx + 1) & TABLE_MASK;
                                            }
                                        }
                                    }
                                } else {
                                    p += 1;
                                }
                            }
                            if p < len && *p_ptr.add(p) == b'\n' { p += 1; }
                        }
                    }
                    std::hint::black_box(total_tid);
                });
            }
        });
        println!("Test mode {}: {:.2} ms", test_mode, t0.elapsed().as_secs_f64() * 1000.0);
    }
    unsafe { munmap(ptr as *mut _, flen); }
}
