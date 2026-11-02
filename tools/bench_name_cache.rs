use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
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

const DICT_SIZE: usize = 262144;
const DICT_MASK: usize = DICT_SIZE - 1;
const LOCAL_CACHE_SIZE: usize = 32768;
const LOCAL_CACHE_MASK: usize = LOCAL_CACHE_SIZE - 1;

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

    let mut dict_vec = Vec::with_capacity(DICT_SIZE);
    for _ in 0..DICT_SIZE { dict_vec.push(AtomicU64::new(0)); }
    let dict_ref: &'static [AtomicU64] = Box::leak(dict_vec.into_boxed_slice());

    let mut bitsets_vec = Vec::with_capacity(241 * (DICT_SIZE / 64));
    for _ in 0..(241 * (DICT_SIZE / 64)) { bitsets_vec.push(AtomicU64::new(0)); }
    let bitsets_ref: &'static [AtomicU64] = Box::leak(bitsets_vec.into_boxed_slice());

    let r_bytes = std::fs::read("/tmp/sd-full-c/regions.csv").unwrap();
    let mut reg_map = vec![0u8; 1_000_000];
    let mut p = 0;
    while p < r_bytes.len() && r_bytes[p] != b'\n' { p += 1; }
    if p < r_bytes.len() { p += 1; }
    while p < r_bytes.len() {
        if r_bytes[p] == b'\r' || r_bytes[p] == b'\n' { p += 1; continue; }
        let mut rid = 0usize;
        while p < r_bytes.len() && r_bytes[p] >= b'0' && r_bytes[p] <= b'9' {
            rid = rid * 10 + (r_bytes[p] - b'0') as usize;
            p += 1;
        }
        if p < r_bytes.len() && r_bytes[p] == b',' { p += 1; }
        let c_start = p;
        while p < r_bytes.len() && r_bytes[p] != b'\r' && r_bytes[p] != b'\n' { p += 1; }
        let c_slice = &r_bytes[c_start..p];
        if p < r_bytes.len() && r_bytes[p] == b'\r' { p += 1; }
        if p < r_bytes.len() && r_bytes[p] == b'\n' { p += 1; }
        let cid = c_slice[0] as u8;
        if rid < 1_000_000 { reg_map[rid] = cid; }
    }
    let reg_map_ref: &'static [u8] = Box::leak(reg_map.into_boxed_slice());

    let t0 = Instant::now();
    let mut total_hits = [0usize; NUM_WORKERS];
    let mut total_records = [0usize; NUM_WORKERS];

    thread::scope(|s| {
        for (w_idx, (th_hits, th_recs)) in total_hits.iter_mut().zip(total_records.iter_mut()).enumerate() {
            let (start, end) = (chunk_bounds[w_idx], chunk_bounds[w_idx + 1]);
            s.spawn(move || {
                let mut local_cache = [(0u64, 0u32); LOCAL_CACHE_SIZE];
                let mut hits = 0;
                let mut recs = 0;
                let mut p = start;

                while p < end {
                    if slice[p] == 0xef && p + 3 <= end && &slice[p..p + 3] == b"\xef\xbb\xbf" { p += 3; }
                    if p >= end { break; }
                    while p < end && slice[p] != b',' { p += 1; }
                    p += 1; // skip ID comma

                    let name_hash = if slice[p] == b'\"' {
                        p += 1;
                        let n_start = p;
                        loop {
                            while p < end && slice[p] != b'\"' { p += 1; }
                            if p + 1 < end && slice[p + 1] == b'\"' {
                                p += 2;
                            } else {
                                break;
                            }
                        }
                        let h = fast_hash(&slice[n_start..p]);
                        p += 1; // skip quote
                        h
                    } else {
                        let n_start = p;
                        while p < end && slice[p] != b',' { p += 1; }
                        fast_hash(&slice[n_start..p])
                    };
                    p += 1; // skip comma

                    // NOTE
                    if slice[p] == b'\"' {
                        p += 1;
                        loop {
                            while p < end && slice[p] != b'\"' { p += 1; }
                            if p + 1 < end && slice[p + 1] == b'\"' {
                                p += 2;
                            } else {
                                break;
                            }
                        }
                        p += 1;
                    } else {
                        while p < end && slice[p] != b',' { p += 1; }
                    }
                    p += 1;

                    // Region ID
                    let mut reg_id = 0usize;
                    while p < end && slice[p] != b',' {
                        reg_id = reg_id * 10 + (slice[p] - b'0') as usize;
                        p += 1;
                    }
                    p += 1;
                    let country_id = if reg_id < 1_000_000 { reg_map_ref[reg_id] as usize } else { 0 };

                    while p < end && slice[p] != b'\n' { p += 1; }
                    if p < end && slice[p] == b'\n' { p += 1; }

                    let non_zero = if name_hash == 0 { 1 } else { name_hash };
                    let slot = (non_zero as usize) & LOCAL_CACHE_MASK;
                    let (cached_h, cached_id) = local_cache[slot];
                    let name_id = if cached_h == non_zero {
                        hits += 1;
                        cached_id
                    } else {
                        let mut slot_idx = (non_zero as usize) & DICT_MASK;
                        let id = loop {
                            let curr = dict_ref[slot_idx].load(Ordering::Relaxed);
                            if curr == non_zero {
                                break slot_idx as u32;
                            }
                            if curr == 0 {
                                if dict_ref[slot_idx].compare_exchange_weak(0, non_zero, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                                    break slot_idx as u32;
                                }
                                continue;
                            }
                            slot_idx = (slot_idx + 1) & DICT_MASK;
                        };
                        local_cache[slot] = (non_zero, id);
                        id
                    };

                    let word_idx = country_id * (DICT_SIZE / 64) + (name_id as usize / 64);
                    let bit_mask = 1u64 << (name_id & 63);
                    let cur = bitsets_ref[word_idx].load(Ordering::Relaxed);
                    if (cur & bit_mask) == 0 {
                        bitsets_ref[word_idx].fetch_or(bit_mask, Ordering::Relaxed);
                    }
                    recs += 1;
                }
                *th_hits = hits;
                *th_recs = recs;
            });
        }
    });

    let dur = t0.elapsed();
    let sum_hits: usize = total_hits.iter().sum();
    let sum_recs: usize = total_records.iter().sum();
    println!("Parsed + Deduped {} names in: {:?}", sum_recs, dur);
    println!("Local Cache Hit Rate: {} / {} ({:.1}%)", sum_hits, sum_recs, (sum_hits as f64 / sum_recs as f64) * 100.0);
    unsafe { munmap(ptr as *mut std::ffi::c_void, flen); }
}
