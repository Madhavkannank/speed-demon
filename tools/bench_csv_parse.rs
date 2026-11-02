use std::fs::File;
use std::os::fd::AsRawFd;
use std::thread;
use std::time::Instant;

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
}

#[inline(always)]
fn fast_hash(slice: &[u8]) -> u64 {
    let len = slice.len();
    if len <= 8 {
        let mut b = [0u8; 8];
        b[..len].copy_from_slice(slice);
        u64::from_le_bytes(b).wrapping_mul(0x517cc1b727220a95) ^ (len as u64)
    } else if len <= 16 {
        let mut b1 = [0u8; 8];
        let mut b2 = [0u8; 8];
        b1.copy_from_slice(&slice[..8]);
        b2[..len - 8].copy_from_slice(&slice[8..]);
        let v1 = u64::from_le_bytes(b1);
        let v2 = u64::from_le_bytes(b2);
        (v1 ^ v2.rotate_left(27)).wrapping_mul(0x9e3779b97f4a7c15) ^ (len as u64)
    } else {
        let mut b1 = [0u8; 8];
        let mut b2 = [0u8; 8];
        b1.copy_from_slice(&slice[..8]);
        b2.copy_from_slice(&slice[len - 8..]);
        let v1 = u64::from_le_bytes(b1);
        let v2 = u64::from_le_bytes(b2);
        (v1 ^ v2.rotate_left(31)).wrapping_mul(0x517cc1b727220a95) ^ (len as u64)
    }
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
                let mut p = start;
                let mut c = 0;
                while p < end {
                    if slice[p] == 0xef && p + 3 <= end && &slice[p..p + 3] == b"\xef\xbb\xbf" { p += 3; }
                    if p >= end { break; }
                    // ID
                    let mut rec_id = 0u64;
                    while p < end && slice[p] != b',' {
                        rec_id = rec_id * 10 + (slice[p] - b'0') as u64;
                        p += 1;
                    }
                    p += 1;

                    // NAME
                    let (name_len, name_hash) = if slice[p] == b'\"' {
                        p += 1;
                        let n_start = p;
                        let mut escapes = 0usize;
                        loop {
                            while p < end && slice[p] != b'\"' { p += 1; }
                            if p + 1 < end && slice[p + 1] == b'\"' {
                                escapes += 1;
                                p += 2;
                            } else {
                                break;
                            }
                        }
                        let raw_len = p - n_start;
                        let h = fast_hash(&slice[n_start..p]);
                        p += 1; // skip closing quote
                        (raw_len - escapes, h)
                    } else {
                        let n_start = p;
                        while p < end && slice[p] != b',' { p += 1; }
                        let raw_len = p - n_start;
                        let h = fast_hash(&slice[n_start..p]);
                        (raw_len, h)
                    };
                    p += 1; // skip comma

                    // NOTE
                    let note_len = if slice[p] == b'\"' {
                        p += 1;
                        let n_start = p;
                        let mut escapes = 0usize;
                        loop {
                            while p < end && slice[p] != b'\"' { p += 1; }
                            if p + 1 < end && slice[p + 1] == b'\"' {
                                escapes += 1;
                                p += 2;
                            } else {
                                break;
                            }
                        }
                        let raw_len = p - n_start;
                        p += 1; // skip quote
                        raw_len - escapes
                    } else {
                        let n_start = p;
                        while p < end && slice[p] != b',' { p += 1; }
                        p - n_start
                    };
                    p += 1; // skip comma

                    // REGION_ID
                    let mut reg_id = 0usize;
                    while p < end && slice[p] != b',' {
                        reg_id = reg_id * 10 + (slice[p] - b'0') as usize;
                        p += 1;
                    }
                    p += 1;

                    // AMOUNT
                    let neg = slice[p] == b'-';
                    if neg { p += 1; }
                    let mut dollars = 0i64;
                    while p < end && slice[p] != b'.' {
                        dollars = dollars * 10 + (slice[p] - b'0') as i64;
                        p += 1;
                    }
                    p += 1;
                    let cents = dollars * 100 + ((slice[p] - b'0') as i64 * 10) + (slice[p + 1] - b'0') as i64;
                    let total_cents = if neg { -cents } else { cents };
                    p += 2;
                    if p < end {
                        if slice[p] == b'\r' { p += 2; } else { p += 1; }
                    }
                    c += 1;
                }
                *cnt = c;
            });
        }
    });

    let dur = t0.elapsed();
    let total: usize = counts.iter().sum();
    println!("Parsed {} CSV records across 12 threads in: {:?} ({:.2} Mrec/s)",
        total, dur, (total as f64 / 1e6) / dur.as_secs_f64());
    unsafe { munmap(ptr as *mut std::ffi::c_void, flen); }
}
