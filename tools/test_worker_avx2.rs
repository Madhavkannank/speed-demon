use std::arch::x86_64::*;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::time::Instant;

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
    fn madvise(addr: *mut std::ffi::c_void, length: usize, advice: i32) -> i32;
}

fn main() {
    let f = File::open("/tmp/sd-full-b/docs.txt").unwrap();
    let flen = f.metadata().unwrap().len() as usize;
    let ptr = unsafe { mmap(std::ptr::null_mut(), flen, 1, 2, f.as_raw_fd(), 0) as *const u8 };
    unsafe { madvise(ptr as *mut _, flen, 22); }
    let slice = unsafe { std::slice::from_raw_parts(ptr, flen) };

    println!("Starting 12-thread AVX2 doc scan test...");
    let t0 = Instant::now();
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

    let mut total_tokens = 0u64;
    std::thread::scope(|s| {
        let mut handles = Vec::new();
        for w in 0..num_threads {
            let (st, en) = (bounds[w], bounds[w + 1]);
            let s_slice = &slice[st..en];
            handles.push(s.spawn(move || unsafe {
                let mut p = 0;
                let len = s_slice.len();
                let p_ptr = s_slice.as_ptr();
                let mut n_tok = 0u64;

                let v_20 = _mm256_set1_epi8(0x20);
                let v_a_minus_1 = _mm256_set1_epi8((b'a' - 1) as i8);
                let v_z_plus_1 = _mm256_set1_epi8((b'z' + 1) as i8);
                let v_0_minus_1 = _mm256_set1_epi8((b'0' - 1) as i8);
                let v_9_plus_1 = _mm256_set1_epi8((b'9' + 1) as i8);
                let v_nl = _mm256_set1_epi8(b'\n' as i8);

                let mut doc_tokens = [0u32; 1024];
                let mut dt_len = 0usize;

                while p < len {
                    while p < len && *p_ptr.add(p) != b'\t' && *p_ptr.add(p) != b'\n' { p += 1; }
                    if p < len && *p_ptr.add(p) == b'\t' { p += 1; }
                    dt_len = 0;

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
                            n_tok += 1;
                            if dt_len < 1024 {
                                doc_tokens[dt_len] = tok_len as u32;
                                dt_len += 1;
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
                                n_tok += 1;
                                if dt_len < 1024 {
                                    doc_tokens[dt_len] = tok_len as u32;
                                    dt_len += 1;
                                }
                            } else {
                                p += 1;
                            }
                        }
                        if p < len && *p_ptr.add(p) == b'\n' { p += 1; }
                    }
                }
                n_tok
            }));
        }
        for h in handles { total_tokens += h.join().unwrap(); }
    });
    let d = t0.elapsed();
    println!("AVX2 Scan finished in {:.2} ms (tokens: {})", d.as_secs_f64() * 1000.0, total_tokens);
    unsafe { munmap(ptr as *mut _, flen); }
}
