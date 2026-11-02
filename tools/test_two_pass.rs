use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Instant;

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
    fn madvise(addr: *mut std::ffi::c_void, length: usize, advice: i32) -> i32;
}

#[inline(always)]
fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = 0x517cc1b727220a95u64;
    for &x in b { h = (h.rotate_left(5) ^ (x as u64)).wrapping_mul(0x9e3779b97f4a7c15); }
    h
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct TermEntry {
    offset: u32,
    len: u8,
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct TableSlot {
    tag: u32,
    id: u32,
}

const TABLE_CAP: usize = 131072;
const TABLE_MASK: usize = TABLE_CAP - 1;

fn main() {
    let docs_path = "/tmp/sd-full-b/docs.txt";
    let queries_path = "/tmp/sd-full-b/queries.txt";

    let t_start = Instant::now();

    // 1. Build term table
    let queries_reader = BufReader::with_capacity(1024 * 1024, File::open(queries_path).unwrap());
    let mut table = vec![TableSlot { tag: 0, id: u32::MAX }; TABLE_CAP];
    let mut terms: Vec<TermEntry> = Vec::with_capacity(80_000);
    let mut term_bytes: Vec<u8> = Vec::with_capacity(800_000);
    let mut filter = vec![0u64; 8192]; // 64 KB 2-hash bloom

    let mut add_term = |token: &[u8]| -> u32 {
        let h = hash_bytes(token);
        let b1 = (h & 0x7FFFF) as usize;
        let b2 = ((h >> 20) & 0x7FFFF) as usize;
        filter[b1 >> 6] |= 1u64 << (b1 & 63);
        filter[b2 >> 6] |= 1u64 << (b2 & 63);

        let tag = ((h >> 32) as u32) | 1;
        let mut idx = (h as usize) & TABLE_MASK;
        loop {
            let slot = table[idx];
            if slot.id == u32::MAX {
                let id = terms.len() as u32;
                let offset = term_bytes.len() as u32;
                term_bytes.extend_from_slice(token);
                terms.push(TermEntry { offset, len: token.len() as u8 });
                table[idx] = TableSlot { tag, id };
                return id;
            }
            if slot.tag == tag {
                let entry = terms[slot.id as usize];
                if entry.len as usize == token.len() && &term_bytes[entry.offset as usize..entry.offset as usize + entry.len as usize] == token {
                    return slot.id;
                }
            }
            idx = (idx + 1) & TABLE_MASK;
        }
    };

    for line_res in queries_reader.lines() {
        let line = line_res.unwrap();
        if let Some(rest) = line.strip_prefix("BOOL ") {
            for chunk in rest.split(' ').step_by(2) {
                add_term(chunk.as_bytes());
            }
        } else if let Some(rest) = line.strip_prefix("PHRASE \"") {
            for term_str in rest.strip_suffix('"').unwrap_or(rest).split(' ') {
                add_term(term_str.as_bytes());
            }
        }
    }
    let t_table = t_start.elapsed();
    let num_terms = terms.len();
    println!("Built term table in {:?}: {} terms, table size: {:.2} MB", t_table, num_terms, (TABLE_CAP * 8 + term_bytes.len() + terms.len() * 5) as f64 / 1e6);

    // 2. Mmap docs
    let docs_file = File::open(docs_path).unwrap();
    let flen = docs_file.metadata().unwrap().len() as usize;
    let mmap_ptr = unsafe { mmap(std::ptr::null_mut(), flen, 1, 2, docs_file.as_raw_fd(), 0) };
    assert!(!mmap_ptr.is_null());
    unsafe { madvise(mmap_ptr, flen, 22); }
    let mmap_slice = unsafe { std::slice::from_raw_parts(mmap_ptr as *const u8, flen) };

    let num_threads = thread::available_parallelism().map(|p| p.get()).unwrap_or(12).max(1);
    let approx_chunk = flen / num_threads;
    let mut bounds = Vec::with_capacity(num_threads + 1);
    bounds.push(0);
    for i in 1..num_threads {
        let mut p = (i * approx_chunk).min(flen);
        while p < flen && mmap_slice[p] != b'\n' { p += 1; }
        if p < flen { p += 1; }
        bounds.push(p);
    }
    bounds.push(flen);

    // 3. Pass 1: Count df per (thread, term)
    let t_p1 = Instant::now();
    let mut thread_counts: Vec<Vec<u32>> = (0..num_threads).map(|_| vec![0u32; num_terms]).collect();
    let (table_ref, terms_ref, bytes_ref, filter_ref) = (&table, &terms, &term_bytes, &filter);

    thread::scope(|s| {
        for (w_idx, counts) in thread_counts.iter_mut().enumerate() {
            let (start, end) = (bounds[w_idx], bounds[w_idx + 1]);
            let slice = &mmap_slice[start..end];
            s.spawn(move || {
                let mut p = 0;
                let len = slice.len();
                let mut doc_id = 0u32;
                let mut last_seen = vec![u32::MAX; num_terms];

                while p < len {
                    let mut did = 0u32;
                    while p < len && slice[p] >= b'0' && slice[p] <= b'9' {
                        did = did * 10 + (slice[p] - b'0') as u32;
                        p += 1;
                    }
                    doc_id = did;
                    while p < len && slice[p] != b'\t' && slice[p] != b'\n' { p += 1; }
                    if p < len && slice[p] == b'\t' { p += 1; }

                    while p < len && slice[p] != b'\n' {
                        if slice[p].is_ascii_alphanumeric() {
                            let start_t = p;
                            let mut h = 0x517cc1b727220a95u64;
                            let mut count = 0;
                            while p < len && slice[p].is_ascii_alphanumeric() {
                                if count < 32 {
                                    h = (h.rotate_left(5) ^ (slice[p].to_ascii_lowercase() as u64)).wrapping_mul(0x9e3779b97f4a7c15);
                                    count += 1;
                                }
                                p += 1;
                            }
                            let b1 = (h & 0x7FFFF) as usize;
                            let b2 = ((h >> 20) & 0x7FFFF) as usize;
                            if ((filter_ref[b1 >> 6] >> (b1 & 63)) & 1) != 0 && ((filter_ref[b2 >> 6] >> (b2 & 63)) & 1) != 0 {
                                let tag = ((h >> 32) as u32) | 1;
                                let mut s_idx = (h as usize) & TABLE_MASK;
                                while table_ref[s_idx].id != u32::MAX {
                                    if table_ref[s_idx].tag == tag {
                                        let id = table_ref[s_idx].id as usize;
                                        let entry = terms_ref[id];
                                        if entry.len as usize == count && &bytes_ref[entry.offset as usize..entry.offset as usize + count] == &slice[start_t..start_t + count].iter().map(|b| b.to_ascii_lowercase()).collect::<Vec<u8>>()[..] {
                                            if last_seen[id] != doc_id {
                                                last_seen[id] = doc_id;
                                                counts[id] += 1;
                                            }
                                            break;
                                        }
                                    }
                                    s_idx = (s_idx + 1) & TABLE_MASK;
                                }
                            }
                        } else {
                            p += 1;
                        }
                    }
                    if p < len && slice[p] == b'\n' { p += 1; }
                }
            });
        }
    });
    let dur_p1 = t_p1.elapsed();

    // 4. Prefix Sum
    let t_prefix = Instant::now();
    let mut term_offsets = vec![0u32; num_terms + 1];
    let mut thread_slice_offsets: Vec<Vec<u32>> = (0..num_threads).map(|_| vec![0u32; num_terms]).collect();
    
    let mut total_postings = 0u32;
    for tid in 0..num_terms {
        term_offsets[tid] = total_postings;
        for w in 0..num_threads {
            thread_slice_offsets[w][tid] = total_postings;
            total_postings += thread_counts[w][tid];
        }
    }
    term_offsets[num_terms] = total_postings;
    let dur_prefix = t_prefix.elapsed();

    println!("Pass 1: {:?}, Prefix sum: {:?}, Total postings: {} ({:.2} MB)", dur_p1, dur_prefix, total_postings, (total_postings * 4) as f64 / 1e6);

    unsafe { munmap(mmap_ptr, flen); }
}
