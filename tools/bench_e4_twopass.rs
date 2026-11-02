use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Instant;

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
    fn madvise(addr: *mut std::ffi::c_void, length: usize, advice: i32) -> i32;
}

#[derive(Clone, Copy)]
#[repr(C)]
struct TableSlot {
    tag: u32,
    id: u32,
}

#[derive(Clone, Copy)]
struct TermEntry {
    offset: u32,
    len: u16,
}

struct FastTermTable {
    slots: Vec<TableSlot>,
    mask: usize,
    arena: Vec<u8>,
    entries: Vec<TermEntry>,
}

#[inline(always)]
fn hash32(b: &[u8]) -> u32 {
    let mut h = 0x517cc1b727220a95u64;
    for &x in b {
        h = (h.rotate_left(5) ^ (x as u64)).wrapping_mul(0x9e3779b97f4a7c15);
    }
    (h ^ (h >> 32)) as u32
}

impl FastTermTable {
    fn with_capacity(cap: usize) -> Self {
        assert!(cap.is_power_of_two());
        Self {
            slots: vec![TableSlot { tag: u32::MAX, id: u32::MAX }; cap],
            mask: cap - 1,
            arena: Vec::with_capacity(1024 * 1024),
            entries: Vec::with_capacity(100000),
        }
    }

    #[inline(always)]
    fn get_or_insert(&mut self, term: &[u8]) -> u32 {
        let h = hash32(term);
        let tag = if h == u32::MAX { 0 } else { h };
        let mut idx = (tag as usize) & self.mask;
        loop {
            let slot = unsafe { self.slots.get_unchecked(idx) };
            if slot.tag == u32::MAX {
                let id = self.entries.len() as u32;
                let offset = self.arena.len() as u32;
                self.arena.extend_from_slice(term);
                self.entries.push(TermEntry { offset, len: term.len() as u16 });
                unsafe {
                    *self.slots.get_unchecked_mut(idx) = TableSlot { tag, id };
                }
                return id;
            }
            if slot.tag == tag {
                let entry = unsafe { *self.entries.get_unchecked(slot.id as usize) };
                if entry.len as usize == term.len() {
                    let s = unsafe {
                        std::slice::from_raw_parts(
                            self.arena.as_ptr().add(entry.offset as usize),
                            entry.len as usize,
                        )
                    };
                    if s == term {
                        return slot.id;
                    }
                }
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn lookup(&self, term: &[u8], h: u32) -> Option<u32> {
        let tag = if h == u32::MAX { 0 } else { h };
        let mut idx = (tag as usize) & self.mask;
        loop {
            let slot = unsafe { *self.slots.get_unchecked(idx) };
            if slot.tag == u32::MAX {
                return None;
            }
            if slot.tag == tag {
                let entry = unsafe { *self.entries.get_unchecked(slot.id as usize) };
                if entry.len as usize == term.len() {
                    let s = unsafe {
                        std::slice::from_raw_parts(
                            self.arena.as_ptr().add(entry.offset as usize),
                            entry.len as usize,
                        )
                    };
                    if s == term {
                        return Some(slot.id);
                    }
                }
            }
            idx = (idx + 1) & self.mask;
        }
    }
}

fn main() {
    let docs_path = "/tmp/sd-full-b/docs.txt";
    let queries_path = "/tmp/sd-full-b/queries.txt";

    let t_start_total = Instant::now();
    let q_bytes = std::fs::read(queries_path).unwrap();
    let mut table = FastTermTable::with_capacity(131072);
    let mut bitset = vec![0u64; 8192];

    let mut p = 0;
    let len = q_bytes.len();
    while p < len {
        let start = p;
        while p < len && q_bytes[p] != b'\n' { p += 1; }
        let line = &q_bytes[start..p];
        if p < len && q_bytes[p] == b'\n' { p += 1; }
        if line.starts_with(b"BOOL ") {
            for chunk in line[5..].split(|&b| b == b' ').step_by(2) {
                let tid = table.get_or_insert(chunk);
                let h = hash32(chunk);
                let bit = (h & 0xFFFF) as usize;
                bitset[bit >> 6] |= 1u64 << (bit & 63);
            }
        } else if line.starts_with(b"PHRASE \"") {
            let mut inner = &line[8..];
            if inner.ends_with(b"\"") { inner = &inner[..inner.len() - 1]; }
            for chunk in inner.split(|&b| b == b' ') {
                if !chunk.is_empty() {
                    let tid = table.get_or_insert(chunk);
                    let h = hash32(chunk);
                    let bit = (h & 0xFFFF) as usize;
                    bitset[bit >> 6] |= 1u64 << (bit & 63);
                }
            }
        }
    }
    let t_table_done = Instant::now();

    let docs_file = File::open(docs_path).unwrap();
    let flen = docs_file.metadata().unwrap().len() as usize;
    let mmap_ptr = unsafe { mmap(std::ptr::null_mut(), flen, 1, 2, docs_file.as_raw_fd(), 0) };
    assert!(!mmap_ptr.is_null());
    unsafe { madvise(mmap_ptr, flen, 22); }
    let mmap_slice = unsafe { std::slice::from_raw_parts(mmap_ptr as *const u8, flen) };

    let num_threads = thread::available_parallelism().map(|p| p.get()).unwrap_or(12).max(1);
    let vocab_size = table.entries.len();
    println!("=== E4 (a) TWO-PASS ARENA POSTINGS (threads: {}, terms: {}) ===", num_threads, vocab_size);

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

    // PASS 1: Count postings per (worker, term)
    let t_p1_start = Instant::now();
    let mut worker_counts: Vec<Vec<u32>> = (0..num_threads).map(|_| vec![0u32; vocab_size]).collect();

    thread::scope(|s| {
        for (w, counts_ref) in worker_counts.iter_mut().enumerate() {
            let (start, end) = (bounds[w], bounds[w + 1]);
            let slice = &mmap_slice[start..end];
            let table_ref = &table;
            let bitset_ref = &bitset;

            s.spawn(move || {
                let mut p = 0;
                let len = slice.len();
                let mut doc_id = 0u32;
                let mut last_seen = vec![u32::MAX; vocab_size];

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
                            while p < len && slice[p].is_ascii_alphanumeric() { p += 1; }
                            let count = (p - start_t).min(32);
                            let mut h = 0x517cc1b727220a95u64;
                            for k in 0..count {
                                h = (h.rotate_left(5) ^ (slice[start_t + k].to_ascii_lowercase() as u64)).wrapping_mul(0x9e3779b97f4a7c15);
                            }
                            let h32 = (h ^ (h >> 32)) as u32;
                            let bit = (h32 & 0xFFFF) as usize;
                            if ((bitset_ref[bit >> 6] >> (bit & 63)) & 1) != 0 {
                                let mut lower = [0u8; 32];
                                for k in 0..count {
                                    lower[k] = slice[start_t + k].to_ascii_lowercase();
                                }
                                if let Some(id) = table_ref.lookup(&lower[..count], h32) {
                                    let id_usize = id as usize;
                                    if last_seen[id_usize] != doc_id {
                                        last_seen[id_usize] = doc_id;
                                        counts_ref[id_usize] += 1;
                                    }
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
    let t_p1_done = Instant::now();

    // PREFIX SUMS: compute global offsets for each term and worker
    // Layout in all_postings: for each term t:
    // [worker 0 postings for t][worker 1 postings for t] ...
    // Term t total length = sum_w worker_counts[w][t].
    let t_prefix_start = Instant::now();
    let mut term_offsets: Vec<usize> = Vec::with_capacity(vocab_size + 1);
    term_offsets.push(0);
    let mut total_postings = 0usize;
    for t in 0..vocab_size {
        let mut term_len = 0usize;
        for w in 0..num_threads {
            term_len += worker_counts[w][t] as usize;
        }
        total_postings += term_len;
        term_offsets.push(total_postings);
    }

    // Per-worker cursor: write_cursor[w][t] is the starting index in all_postings
    let mut write_cursor: Vec<Vec<u32>> = vec![vec![0u32; vocab_size]; num_threads];
    for t in 0..vocab_size {
        let mut cur = term_offsets[t] as u32;
        for w in 0..num_threads {
            write_cursor[w][t] = cur;
            cur += worker_counts[w][t];
        }
    }

    // Allocate all_postings arena once
    let mut all_postings = vec![0u32; total_postings];
    let all_postings_ptr = all_postings.as_mut_ptr() as usize;
    let t_prefix_done = Instant::now();

    // PASS 2: Directly write postings into all_postings
    let t_p2_start = Instant::now();
    thread::scope(|s| {
        for w in 0..num_threads {
            let (start, end) = (bounds[w], bounds[w + 1]);
            let slice = &mmap_slice[start..end];
            let table_ref = &table;
            let bitset_ref = &bitset;
            let mut cursors = write_cursor[w].clone();

            s.spawn(move || {
                let mut p = 0;
                let len = slice.len();
                let mut doc_id = 0u32;
                let mut last_seen = vec![u32::MAX; vocab_size];
                let out_ptr = all_postings_ptr as *mut u32;

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
                            while p < len && slice[p].is_ascii_alphanumeric() { p += 1; }
                            let count = (p - start_t).min(32);
                            let mut h = 0x517cc1b727220a95u64;
                            for k in 0..count {
                                h = (h.rotate_left(5) ^ (slice[start_t + k].to_ascii_lowercase() as u64)).wrapping_mul(0x9e3779b97f4a7c15);
                            }
                            let h32 = (h ^ (h >> 32)) as u32;
                            let bit = (h32 & 0xFFFF) as usize;
                            if ((bitset_ref[bit >> 6] >> (bit & 63)) & 1) != 0 {
                                let mut lower = [0u8; 32];
                                for k in 0..count {
                                    lower[k] = slice[start_t + k].to_ascii_lowercase();
                                }
                                if let Some(id) = table_ref.lookup(&lower[..count], h32) {
                                    let id_usize = id as usize;
                                    if last_seen[id_usize] != doc_id {
                                        last_seen[id_usize] = doc_id;
                                        unsafe {
                                            let pos = cursors.get_unchecked_mut(id_usize);
                                            *out_ptr.add(*pos as usize) = doc_id;
                                            *pos += 1;
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
            });
        }
    });
    let t_p2_done = Instant::now();

    println!("--- TIMING BREAKDOWN ---");
    println!("Term Table + Bitset : {:6.2} ms", (t_table_done - t_start_total).as_secs_f64() * 1000.0);
    println!("Pass 1 (Counting)   : {:6.2} ms", (t_p1_done - t_p1_start).as_secs_f64() * 1000.0);
    println!("Prefix Sums & Alloc : {:6.2} ms", (t_prefix_done - t_prefix_start).as_secs_f64() * 1000.0);
    println!("Pass 2 (Writing)    : {:6.2} ms", (t_p2_done - t_p2_start).as_secs_f64() * 1000.0);
    let total_indexing_ms = (t_p2_done - t_start_total).as_secs_f64() * 1000.0;
    println!("Total Indexing Time : {:6.2} ms", total_indexing_ms);
    println!("Total Postings      : {} ({:.2} MB)", total_postings, (total_postings * 4) as f64 / 1e6);

    // Verify postings are strictly sorted for sample terms
    let mut sorted_ok = true;
    for t in 0..vocab_size {
        let (s, e) = (term_offsets[t], term_offsets[t + 1]);
        let slice = &all_postings[s..e];
        for w in 1..slice.len() {
            if slice[w] <= slice[w - 1] {
                sorted_ok = false;
                println!("MISMATCH for term {}: not sorted at {} ({} <= {})", t, w, slice[w], slice[w - 1]);
                break;
            }
        }
        if !sorted_ok { break; }
    }
    println!("Postings strictly sorted check: {}", if sorted_ok { "PASS" } else { "FAIL" });

    unsafe { munmap(mmap_ptr, flen); }
}
