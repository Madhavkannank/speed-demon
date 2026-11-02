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

    // Build FastTermTable & 64 KB bitset prefilter
    let q_bytes = std::fs::read(queries_path).unwrap();
    let mut table = FastTermTable::with_capacity(131072);
    let mut bitset = vec![0u64; 8192]; // 64 KB bitset = 65,536 bits!

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

    let docs_file = File::open(docs_path).unwrap();
    let flen = docs_file.metadata().unwrap().len() as usize;
    let mmap_ptr = unsafe { mmap(std::ptr::null_mut(), flen, 1, 2, docs_file.as_raw_fd(), 0) };
    assert!(!mmap_ptr.is_null());
    unsafe { madvise(mmap_ptr, flen, 22); }
    let mmap_slice = unsafe { std::slice::from_raw_parts(mmap_ptr as *const u8, flen) };

    let num_threads = thread::available_parallelism().map(|p| p.get()).unwrap_or(12).max(1);
    println!("=== FAST SCAN LADDER (threads: {}, terms: {}) ===", num_threads, table.entries.len());

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
                let table_ref = &table;
                let bitset_ref = &bitset;
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
                    let mut last_seen = vec![u32::MAX; table_ref.entries.len()];
                    let mut local_index: Vec<Vec<u32>> = (0..table_ref.entries.len()).map(|_| Vec::new()).collect();

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
                                tok_count += 1;

                                if stage == 1 {
                                    std::hint::black_box(count);
                                } else if stage == 2 {
                                    let mut h = 0x517cc1b727220a95u64;
                                    for k in 0..count {
                                        h = (h.rotate_left(5) ^ (slice[start_t + k].to_ascii_lowercase() as u64)).wrapping_mul(0x9e3779b97f4a7c15);
                                    }
                                    let h32 = (h ^ (h >> 32)) as u32;
                                    std::hint::black_box(h32);
                                } else if stage >= 3 {
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
                                            if stage >= 4 {
                                                let id_usize = id as usize;
                                                if last_seen[id_usize] != doc_id {
                                                    last_seen[id_usize] = doc_id;
                                                    if stage >= 5 {
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
            "3. + 64KB Bitset & Flat Table",
            "4. + Dedupe Stamp",
            "5. + Posting Write (Vectors)",
        ];
        println!(
            "{:<32}: wall={:6.2} ms | thread_agg={:7.2} ms | {:6.2} ns/tok | {:6.2} cyc/tok",
            stage_names[stage - 1], wall_ms, agg_thread_ms, ns_per_tok, cyc_per_tok
        );
    }

    unsafe { munmap(mmap_ptr, flen); }
}
