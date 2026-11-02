use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::fd::AsRawFd;
use std::path::Path;
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
    for &x in b {
        h = (h.rotate_left(5) ^ (x as u64)).wrapping_mul(0x9e3779b97f4a7c15);
    }
    h
}

fn main() {
    let docs_path = "/tmp/sd-full-b/docs.txt";
    let queries_path = "/tmp/sd-full-b/queries.txt";

    // Build vocab & bloom
    let queries_reader = BufReader::with_capacity(1024 * 1024, File::open(queries_path).unwrap());
    let mut vocab = std::collections::HashMap::new();
    let mut bloom = vec![0u64; 32768];
    for line_res in queries_reader.lines() {
        let line = line_res.unwrap();
        if let Some(rest) = line.strip_prefix("BOOL ") {
            for chunk in rest.split(' ').step_by(2) {
                let t = chunk.as_bytes();
                if !vocab.contains_key(t) {
                    let id = vocab.len() as u32;
                    vocab.insert(t.to_vec(), id);
                    let h = hash_bytes(t);
                    let (b1, b2, b3, b4) = (
                        (h & 0x1FFFFF) as usize,
                        ((h >> 21) & 0x1FFFFF) as usize,
                        (h.rotate_left(17) & 0x1FFFFF) as usize,
                        (h.rotate_left(37) & 0x1FFFFF) as usize,
                    );
                    bloom[b1 >> 6] |= 1u64 << (b1 & 63);
                    bloom[b2 >> 6] |= 1u64 << (b2 & 63);
                    bloom[b3 >> 6] |= 1u64 << (b3 & 63);
                    bloom[b4 >> 6] |= 1u64 << (b4 & 63);
                }
            }
        } else if let Some(rest) = line.strip_prefix("PHRASE \"") {
            for term_str in rest.strip_suffix('"').unwrap_or(rest).split(' ') {
                let t = term_str.as_bytes();
                if !vocab.contains_key(t) {
                    let id = vocab.len() as u32;
                    vocab.insert(t.to_vec(), id);
                    let h = hash_bytes(t);
                    let (b1, b2, b3, b4) = (
                        (h & 0x1FFFFF) as usize,
                        ((h >> 21) & 0x1FFFFF) as usize,
                        (h.rotate_left(17) & 0x1FFFFF) as usize,
                        (h.rotate_left(37) & 0x1FFFFF) as usize,
                    );
                    bloom[b1 >> 6] |= 1u64 << (b1 & 63);
                    bloom[b2 >> 6] |= 1u64 << (b2 & 63);
                    bloom[b3 >> 6] |= 1u64 << (b3 & 63);
                    bloom[b4 >> 6] |= 1u64 << (b4 & 63);
                }
            }
        }
    }

    let docs_file = File::open(docs_path).unwrap();
    let flen = docs_file.metadata().unwrap().len() as usize;
    let mmap_ptr = unsafe { mmap(std::ptr::null_mut(), flen, 1, 2, docs_file.as_raw_fd(), 0) };
    assert!(!mmap_ptr.is_null());
    unsafe { madvise(mmap_ptr, flen, 22); }
    let mmap_slice = unsafe { std::slice::from_raw_parts(mmap_ptr as *const u8, flen) };

    let num_threads = thread::available_parallelism().map(|p| p.get()).unwrap_or(12).max(1);
    println!("=== STAGE LADDER BENCHMARK (threads: {}) ===", num_threads);

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

    for stage in 1..=5 {
        let total_tokens = AtomicU64::new(0);
        let total_thread_time_ns = AtomicU64::new(0);
        let total_thread_cycles = AtomicU64::new(0);
        let t0 = Instant::now();

        thread::scope(|s| {
            for w in 0..num_threads {
                let (start, end) = (bounds[w], bounds[w + 1]);
                let slice = &mmap_slice[start..end];
                let vocab_ref = &vocab;
                let bloom_ref = &bloom;
                let tot_tok = &total_tokens;
                let tot_time = &total_thread_time_ns;
                let tot_cyc = &total_thread_cycles;

                s.spawn(move || {
                    let cyc_start = unsafe { core::arch::x86_64::_rdtsc() };
                    let w_start = Instant::now();
                    let mut p = 0;
                    let len = slice.len();
                    let mut tok_count = 0u64;
                    let mut doc_id = 0u32;
                    let mut last_seen = vec![u32::MAX; vocab_ref.len()];
                    let mut local_index: Vec<Vec<u32>> = (0..vocab_ref.len()).map(|_| Vec::new()).collect();

                    while p < len {
                        // parse doc_id
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
                                while p < len && slice[p].is_ascii_alphanumeric() { p += 1; }
                                let count = (p - start_t).min(32);
                                tok_count += 1;

                                if stage == 1 {
                                    // Stage 1: tokenize only
                                    std::hint::black_box(count);
                                } else if stage == 2 {
                                    // Stage 2: tokenize + hash
                                    let mut h = 0x517cc1b727220a95u64;
                                    for k in 0..count {
                                        h = (h.rotate_left(5) ^ (slice[start_t + k].to_ascii_lowercase() as u64)).wrapping_mul(0x9e3779b97f4a7c15);
                                    }
                                    std::hint::black_box(h);
                                } else if stage >= 3 {
                                    // Stage 3: + bloom / table lookup
                                    let mut h = 0x517cc1b727220a95u64;
                                    for k in 0..count {
                                        h = (h.rotate_left(5) ^ (slice[start_t + k].to_ascii_lowercase() as u64)).wrapping_mul(0x9e3779b97f4a7c15);
                                    }
                                    let (b1, b2, b3, b4) = (
                                        (h & 0x1FFFFF) as usize,
                                        ((h >> 21) & 0x1FFFFF) as usize,
                                        (h.rotate_left(17) & 0x1FFFFF) as usize,
                                        (h.rotate_left(37) & 0x1FFFFF) as usize,
                                    );
                                    if ((bloom_ref[b1 >> 6] >> (b1 & 63)) & 1) != 0
                                        && ((bloom_ref[b2 >> 6] >> (b2 & 63)) & 1) != 0
                                        && ((bloom_ref[b3 >> 6] >> (b3 & 63)) & 1) != 0
                                        && ((bloom_ref[b4 >> 6] >> (b4 & 63)) & 1) != 0
                                    {
                                        let mut lower = [0u8; 32];
                                        for k in 0..count {
                                            lower[k] = slice[start_t + k].to_ascii_lowercase();
                                        }
                                        if let Some(&id) = vocab_ref.get(&lower[..count]) {
                                            if stage >= 4 {
                                                // Stage 4: dedupe stamp
                                                let id_usize = id as usize;
                                                if last_seen[id_usize] != doc_id {
                                                    last_seen[id_usize] = doc_id;
                                                    if stage >= 5 {
                                                        // Stage 5: posting write
                                                        local_index[id_usize].push(doc_id);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                p += 1;
                            }
                        }
                        if p < len && slice[p] == b'\n' { p += 1; }
                    }
                    tot_tok.fetch_add(tok_count, Ordering::Relaxed);
                    tot_time.fetch_add(w_start.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    let cyc_end = unsafe { core::arch::x86_64::_rdtsc() };
                    tot_cyc.fetch_add(cyc_end.saturating_sub(cyc_start), Ordering::Relaxed);
                });
            }
        });

        let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let toks = total_tokens.load(Ordering::Relaxed);
        let agg_thread_ms = total_thread_time_ns.load(Ordering::Relaxed) as f64 / 1e6;
        let ns_per_tok = (total_thread_time_ns.load(Ordering::Relaxed) as f64) / (toks as f64);
        let cyc_per_tok = (total_thread_cycles.load(Ordering::Relaxed) as f64) / (toks as f64);
        let stage_names = [
            "1. Tokenize only",
            "2. Tokenize + Hash",
            "3. + Bloom / Table Lookup",
            "4. + Dedupe Stamp",
            "5. + Posting Write (Full)",
        ];
        println!(
            "{:<28}: wall={:6.2} ms | thread_agg={:7.2} ms | {:6.2} ns/tok | {:6.2} cyc/tok",
            stage_names[stage - 1], wall_ms, agg_thread_ms, ns_per_tok, cyc_per_tok
        );
    }

    unsafe { munmap(mmap_ptr, flen); }
}
