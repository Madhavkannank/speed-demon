use std::fs::File;
use std::os::fd::AsRawFd;
use std::thread;
use std::time::Instant;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
}

#[inline(always)]
unsafe fn find_char_avx2(slice: &[u8], mut p: usize, target: u8) -> usize {
    let len = slice.len();
    let needle = _mm256_set1_epi8(target as i8);
    let ptr = slice.as_ptr();

    while p + 32 <= len {
        let chunk = _mm256_loadu_si256(ptr.add(p) as *const __m256i);
        let mask = _mm256_movemask_epi8(_mm256_cmpeq_epi8(chunk, needle)) as u32;
        if mask != 0 {
            return p + mask.trailing_zeros() as usize;
        }
        p += 32;
    }
    while p < len && *ptr.add(p) != target {
        p += 1;
    }
    p
}

fn main() {
    let file = File::open("/tmp/sd-full-c/records.csv").unwrap();
    let flen = file.metadata().unwrap().len() as usize;
    let ptr = unsafe { mmap(std::ptr::null_mut(), flen, 1, 1, file.as_raw_fd(), 0) as *const u8 };
    let slice = unsafe { std::slice::from_raw_parts(ptr, flen) };

    let mut header_offset = 0;
    while header_offset < flen && slice[header_offset] != b'\n' { header_offset += 1; }
    header_offset += 1;

    const NUM_WORKERS: usize = 12;
    let approx_chunk = (flen - header_offset) / NUM_WORKERS;
    let mut chunk_bounds = Vec::with_capacity(NUM_WORKERS + 1);
    chunk_bounds.push(header_offset);

    for i in 1..NUM_WORKERS {
        let approx_pos = header_offset + i * approx_chunk;
        let mut p = approx_pos;
        while p + 16 < flen {
            if slice[p] == b'.' && slice[p + 1].is_ascii_digit() && slice[p + 2].is_ascii_digit() {
                let mut after = p + 3;
                if slice[after] == b'\r' { after += 1; }
                if slice[after] == b'\n' {
                    let mut rec_start = after + 1;
                    if slice[rec_start..].starts_with(b"\xef\xbb\xbf") { rec_start += 3; }
                    if slice[rec_start].is_ascii_digit() && slice[rec_start] != b'0' {
                        let mut d = rec_start + 1;
                        while d < flen && slice[d].is_ascii_digit() { d += 1; }
                        if d < flen && slice[d] == b',' {
                            chunk_bounds.push(rec_start);
                            break;
                        }
                    }
                }
            }
            p += 1;
        }
    }
    chunk_bounds.push(flen);

    let t0 = Instant::now();
    let mut counts = [0usize; NUM_WORKERS];

    thread::scope(|s| {
        for (w_idx, cnt) in counts.iter_mut().enumerate() {
            let (start, end) = (chunk_bounds[w_idx], chunk_bounds[w_idx + 1]);
            s.spawn(move || {
                let sub = &slice[..end];
                let mut p = start;
                let mut c = 0;
                while p < end {
                    if sub[p] == 0xef && p + 3 <= end && &sub[p..p + 3] == b"\xef\xbb\xbf" { p += 3; }
                    if p >= end { break; }

                    // ID up to comma
                    let comma1 = unsafe { find_char_avx2(sub, p, b',') };
                    let mut rec_id = 0u64;
                    for &d in &sub[p..comma1] {
                        rec_id = rec_id * 10 + (d - b'0') as u64;
                    }
                    p = comma1 + 1;

                    // NAME
                    if sub[p] == b'\"' {
                        p += 1;
                        loop {
                            let q = unsafe { find_char_avx2(sub, p, b'\"') };
                            if q + 1 < end && sub[q + 1] == b'\"' {
                                p = q + 2;
                            } else {
                                p = q + 1;
                                break;
                            }
                        }
                    } else {
                        p = unsafe { find_char_avx2(sub, p, b',') };
                    }
                    p += 1; // skip comma

                    // NOTE
                    if sub[p] == b'\"' {
                        p += 1;
                        loop {
                            let q = unsafe { find_char_avx2(sub, p, b'\"') };
                            if q + 1 < end && sub[q + 1] == b'\"' {
                                p = q + 2;
                            } else {
                                p = q + 1;
                                break;
                            }
                        }
                    } else {
                        p = unsafe { find_char_avx2(sub, p, b',') };
                    }
                    p += 1; // skip comma

                    // REGION ID
                    let comma4 = unsafe { find_char_avx2(sub, p, b',') };
                    p = comma4 + 1;

                    // AMOUNT up to newline
                    let nl = unsafe { find_char_avx2(sub, p, b'\n') };
                    p = nl + 1;
                    c += 1;
                }
                *cnt = c;
            });
        }
    });

    let dur = t0.elapsed();
    let total: usize = counts.iter().sum();
    println!("AVX2 parsed {} CSV records in: {:?} ({:.2} Mrec/s)",
        total, dur, (total as f64 / 1e6) / dur.as_secs_f64());
    unsafe { munmap(ptr as *mut std::ffi::c_void, flen); }
}
