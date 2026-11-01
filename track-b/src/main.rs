use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    os::fd::AsRawFd,
    path::Path,
    thread,
};

extern "C" {
    fn mmap(
        addr: *mut std::ffi::c_void,
        length: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
    fn madvise(addr: *mut std::ffi::c_void, length: usize, advice: i32) -> i32;
}

const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const MADV_POPULATE_READ: i32 = 22;

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
        h = (h.rotate_left(5) ^ ((x | 0x20) as u64)).wrapping_mul(0x9e3779b97f4a7c15);
    }
    (h ^ (h >> 32)) as u32
}

impl FastTermTable {
    fn with_capacity(cap: usize) -> Self {
        assert!(cap.is_power_of_two());
        Self {
            slots: vec![
                TableSlot {
                    tag: u32::MAX,
                    id: u32::MAX
                };
                cap
            ],
            mask: cap - 1,
            arena: Vec::with_capacity(1024 * 1024),
            entries: Vec::with_capacity(100000),
        }
    }

    fn resize(&mut self) {
        let new_cap = self.slots.len() * 2;
        let new_mask = new_cap - 1;
        let mut new_slots = vec![
            TableSlot {
                tag: u32::MAX,
                id: u32::MAX,
            };
            new_cap
        ];
        for &slot in &self.slots {
            if slot.tag != u32::MAX {
                let mut idx = (slot.tag as usize) & new_mask;
                while new_slots[idx].tag != u32::MAX {
                    idx = (idx + 1) & new_mask;
                }
                new_slots[idx] = slot;
            }
        }
        self.slots = new_slots;
        self.mask = new_mask;
    }

    #[inline(always)]
    fn get_or_insert(&mut self, term: &[u8]) -> u32 {
        if self.entries.len() * 2 >= self.slots.len() {
            self.resize();
        }
        let h = hash32(term);
        let tag = if h == u32::MAX { 0 } else { h };
        let mut idx = (tag as usize) & self.mask;
        loop {
            let slot = unsafe { self.slots.get_unchecked(idx) };
            if slot.tag == u32::MAX {
                let id = self.entries.len() as u32;
                let offset = self.arena.len() as u32;
                // Store in arena as lowercase
                for &b in term {
                    self.arena.push(b | 0x20);
                }
                self.entries.push(TermEntry {
                    offset,
                    len: term.len() as u16,
                });
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
                    if s.iter().zip(term.iter()).all(|(&a, &b)| a == (b | 0x20)) {
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
                    if s.iter().zip(term.iter()).all(|(&a, &b)| a == (b | 0x20)) {
                        return Some(slot.id);
                    }
                }
            }
            idx = (idx + 1) & self.mask;
        }
    }
}

#[inline(always)]
fn intersect_sorted(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    if a.is_empty() || b.is_empty() {
        return;
    }
    if b.len() > a.len() * 8 {
        let mut s = b;
        for &v in a {
            match s.binary_search(&v) {
                Ok(i) => {
                    out.push(v);
                    s = &s[i + 1..];
                }
                Err(i) => s = &s[i..],
            }
            if s.is_empty() {
                break;
            }
        }
        return;
    }
    if a.len() > b.len() * 8 {
        let mut s = a;
        for &v in b {
            match s.binary_search(&v) {
                Ok(i) => {
                    out.push(v);
                    s = &s[i + 1..];
                }
                Err(i) => s = &s[i..],
            }
            if s.is_empty() {
                break;
            }
        }
        return;
    }
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(a[i]);
            i += 1;
            j += 1;
        } else if a[i] < b[j] {
            i += 1;
        } else {
            j += 1;
        }
    }
}

#[inline(always)]
fn union_sorted(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(a[i]);
            i += 1;
            j += 1;
        } else if a[i] < b[j] {
            out.push(a[i]);
            i += 1;
        } else {
            out.push(b[j]);
            j += 1;
        }
    }
    if i < a.len() {
        out.extend_from_slice(&a[i..]);
    }
    if j < b.len() {
        out.extend_from_slice(&b[j..]);
    }
}

#[inline(always)]
fn difference_sorted(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] {
            out.push(a[i]);
            i += 1;
        } else if a[i] == b[j] {
            i += 1;
            j += 1;
        } else {
            j += 1;
        }
    }
    if i < a.len() {
        out.extend_from_slice(&a[i..]);
    }
}

#[inline(always)]
fn append_u32(buf: &mut Vec<u8>, mut n: u32) {
    if n == 0 {
        buf.push(b'0');
        return;
    }
    let mut t = [0u8; 10];
    let mut i = 10;
    while n > 0 {
        i -= 1;
        t[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    buf.extend_from_slice(&t[i..]);
}

#[inline(always)]
fn append_u64(buf: &mut Vec<u8>, mut n: u64) {
    if n == 0 {
        buf.push(b'0');
        return;
    }
    let mut t = [0u8; 20];
    let mut i = 20;
    while n > 0 {
        i -= 1;
        t[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    buf.extend_from_slice(&t[i..]);
}

#[inline(always)]
fn fast_write_results(buf: &mut Vec<u8>, ids: &[u32]) {
    if ids.is_empty() {
        buf.extend_from_slice(b"0 0\n");
        return;
    }
    let sum: u64 = ids.iter().map(|&x| x as u64).sum();
    append_u32(buf, ids.len() as u32);
    buf.push(b' ');
    append_u64(buf, sum);
    for &id in &ids[..ids.len().min(10)] {
        buf.push(b' ');
        append_u32(buf, id);
    }
    buf.push(b'\n');
}

enum ParsedQuery {
    Bool {
        first_term: u32,
        ops: Vec<(u8, u32)>, // op: 0=AND, 1=OR, 2=NOT
    },
    Phrase {
        term_ids: Vec<u32>,
        words: Vec<Vec<u8>>,
    },
    Empty,
}

fn main() -> std::io::Result<()> {
    let (data_dir, out_dir) = (
        std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string()),
        std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string()),
    );
    let (docs_path, queries_path, out_path) = (
        Path::new(&data_dir).join("docs.txt"),
        Path::new(&data_dir).join("queries.txt"),
        Path::new(&out_dir).join("results.txt"),
    );

    let docs_file = File::open(&docs_path)?;
    let file_len = docs_file.metadata()?.len() as usize;
    // SAFETY: docs_file is a valid read-only file descriptor. file_len matches its size.
    let mmap_ptr = unsafe {
        mmap(
            std::ptr::null_mut(),
            file_len,
            PROT_READ,
            MAP_PRIVATE,
            docs_file.as_raw_fd(),
            0,
        )
    };
    if mmap_ptr == usize::MAX as *mut std::ffi::c_void || mmap_ptr.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: mmap_ptr is non-null and valid for file_len bytes.
    unsafe {
        madvise(mmap_ptr, file_len, MADV_POPULATE_READ);
    }
    // SAFETY: mmap_slice is guaranteed valid for read access during the lifetime of main.
    let mmap_slice: &[u8] = unsafe { std::slice::from_raw_parts(mmap_ptr as *const u8, file_len) };

    // Read queries.txt directly from disk
    let q_bytes = std::fs::read(&queries_path)?;
    let mut table = FastTermTable::with_capacity(131072);
    let mut bitset = vec![0u64; 8192];
    let mut parsed_queries = Vec::with_capacity(50000);

    let mut p = 0;
    let q_len = q_bytes.len();
    while p < q_len {
        let line_start = p;
        while p < q_len && q_bytes[p] != b'\n' {
            p += 1;
        }
        let line = &q_bytes[line_start..p];
        if p < q_len && q_bytes[p] == b'\n' {
            p += 1;
        }
        if line.is_empty() {
            continue;
        }

        if line.starts_with(b"BOOL ") {
            let rest = &line[5..];
            let mut parts = rest.split(|&b| b == b' ');
            if let Some(first) = parts.next() {
                let first_id = table.get_or_insert(first);
                let h = hash32(first);
                let bit = (h & 0xFFFF) as usize;
                bitset[bit >> 6] |= 1u64 << (bit & 63);

                let mut ops = Vec::new();
                while let Some(op_bytes) = parts.next() {
                    if let Some(term_bytes) = parts.next() {
                        let op_code = match op_bytes {
                            b"OR" => 1,
                            b"NOT" => 2,
                            _ => 0,
                        };
                        let tid = table.get_or_insert(term_bytes);
                        let th = hash32(term_bytes);
                        let tbit = (th & 0xFFFF) as usize;
                        bitset[tbit >> 6] |= 1u64 << (tbit & 63);
                        ops.push((op_code, tid));
                    }
                }
                parsed_queries.push(ParsedQuery::Bool {
                    first_term: first_id,
                    ops,
                });
            } else {
                parsed_queries.push(ParsedQuery::Empty);
            }
        } else if line.starts_with(b"PHRASE \"") {
            let mut inner = &line[8..];
            if inner.ends_with(b"\"") {
                inner = &inner[..inner.len() - 1];
            }
            let mut term_ids = Vec::new();
            let mut words = Vec::new();
            for word in inner.split(|&b| b == b' ') {
                if !word.is_empty() {
                    let tid = table.get_or_insert(word);
                    let h = hash32(word);
                    let bit = (h & 0xFFFF) as usize;
                    bitset[bit >> 6] |= 1u64 << (bit & 63);
                    term_ids.push(tid);
                    let mut lower_word = Vec::with_capacity(word.len());
                    for &b in word {
                        lower_word.push(b | 0x20);
                    }
                    words.push(lower_word);
                }
            }
            parsed_queries.push(ParsedQuery::Phrase { term_ids, words });
        } else {
            parsed_queries.push(ParsedQuery::Empty);
        }
    }

    let vocab_size = table.entries.len();
    let num_workers = thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(12)
        .min(12)
        .max(1);

    let approx_chunk = file_len / num_workers;
    let mut chunk_bounds = Vec::with_capacity(num_workers + 1);
    chunk_bounds.push(0);
    for i in 1..num_workers {
        let mut p = (i * approx_chunk).min(file_len);
        while p < file_len && mmap_slice[p] != b'\n' {
            p += 1;
        }
        if p < file_len {
            p += 1;
        }
        chunk_bounds.push(p);
    }
    chunk_bounds.push(file_len);

    // Build line_offsets table: 250,001 entries (starts of documents)
    // We can populate line_offsets efficiently during pass 1
    let mut worker_counts: Vec<Vec<u32>> =
        (0..num_workers).map(|_| vec![0u32; vocab_size]).collect();
    let mut worker_doc_starts: Vec<Vec<u32>> = (0..num_workers).map(|_| Vec::new()).collect();

    // PASS 1: Count postings per (worker, term) and record doc start offsets
    thread::scope(|s| {
        for (w, (counts_ref, doc_starts_ref)) in worker_counts
            .iter_mut()
            .zip(worker_doc_starts.iter_mut())
            .enumerate()
        {
            let (start, end) = (chunk_bounds[w], chunk_bounds[w + 1]);
            let slice = &mmap_slice[start..end];
            let table_ref = &table;
            let bitset_ref = &bitset;

            s.spawn(move || {
                let mut p = 0;
                let len = slice.len();
                let mut doc_id;
                let mut last_seen = vec![u32::MAX; vocab_size];

                while p < len {
                    let d_start = (start + p) as u32;
                    let mut did = 0u32;
                    while p < len && slice[p] >= b'0' && slice[p] <= b'9' {
                        did = did * 10 + (slice[p] - b'0') as u32;
                        p += 1;
                    }
                    doc_id = did;
                    while p < len && slice[p] != b'\t' && slice[p] != b'\n' {
                        p += 1;
                    }
                    if p < len && slice[p] == b'\t' {
                        p += 1;
                    }
                    doc_starts_ref.push(d_start);

                    while p < len && slice[p] != b'\n' {
                        if slice[p].is_ascii_alphanumeric() {
                            let start_t = p;
                            while p < len && slice[p].is_ascii_alphanumeric() {
                                p += 1;
                            }
                            let count = (p - start_t).min(32);
                            let mut h = 0x517cc1b727220a95u64;
                            for k in 0..count {
                                h = (h.rotate_left(5) ^ ((slice[start_t + k] | 0x20) as u64))
                                    .wrapping_mul(0x9e3779b97f4a7c15);
                            }
                            let h32 = (h ^ (h >> 32)) as u32;
                            let bit = (h32 & 0xFFFF) as usize;
                            if ((bitset_ref[bit >> 6] >> (bit & 63)) & 1) != 0 {
                                if let Some(id) =
                                    table_ref.lookup(&slice[start_t..start_t + count], h32)
                                {
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
                    if p < len && slice[p] == b'\n' {
                        p += 1;
                    }
                }
            });
        }
    });

    // Flatten line_offsets
    let mut line_offsets: Vec<u32> = Vec::with_capacity(260_000);
    for w in &worker_doc_starts {
        line_offsets.extend_from_slice(w);
    }
    line_offsets.push(file_len as u32);
    drop(worker_doc_starts);

    // PREFIX SUMS: Global offsets for each term and worker
    let mut term_offsets: Vec<usize> = Vec::with_capacity(vocab_size + 1);
    term_offsets.push(0);
    let mut total_postings = 0usize;
    for t in 0..vocab_size {
        let mut term_len = 0usize;
        for w in 0..num_workers {
            term_len += worker_counts[w][t] as usize;
        }
        total_postings += term_len;
        term_offsets.push(total_postings);
    }

    let mut write_cursor: Vec<Vec<u32>> = vec![vec![0u32; vocab_size]; num_workers];
    for t in 0..vocab_size {
        let mut cur = term_offsets[t] as u32;
        for w in 0..num_workers {
            write_cursor[w][t] = cur;
            cur += worker_counts[w][t];
        }
    }
    drop(worker_counts);

    let mut all_postings = vec![0u32; total_postings];
    let all_postings_ptr = all_postings.as_mut_ptr() as usize;

    // PASS 2: Directly write postings into all_postings
    thread::scope(|s| {
        for w in 0..num_workers {
            let (start, end) = (chunk_bounds[w], chunk_bounds[w + 1]);
            let slice = &mmap_slice[start..end];
            let table_ref = &table;
            let bitset_ref = &bitset;
            let mut cursors = write_cursor[w].clone();

            s.spawn(move || {
                let mut p = 0;
                let len = slice.len();
                let mut doc_id;
                let mut last_seen = vec![u32::MAX; vocab_size];
                let out_ptr = all_postings_ptr as *mut u32;

                while p < len {
                    let mut did = 0u32;
                    while p < len && slice[p] >= b'0' && slice[p] <= b'9' {
                        did = did * 10 + (slice[p] - b'0') as u32;
                        p += 1;
                    }
                    doc_id = did;
                    while p < len && slice[p] != b'\t' && slice[p] != b'\n' {
                        p += 1;
                    }
                    if p < len && slice[p] == b'\t' {
                        p += 1;
                    }

                    while p < len && slice[p] != b'\n' {
                        if slice[p].is_ascii_alphanumeric() {
                            let start_t = p;
                            while p < len && slice[p].is_ascii_alphanumeric() {
                                p += 1;
                            }
                            let count = (p - start_t).min(32);
                            let mut h = 0x517cc1b727220a95u64;
                            for k in 0..count {
                                h = (h.rotate_left(5) ^ ((slice[start_t + k] | 0x20) as u64))
                                    .wrapping_mul(0x9e3779b97f4a7c15);
                            }
                            let h32 = (h ^ (h >> 32)) as u32;
                            let bit = (h32 & 0xFFFF) as usize;
                            if ((bitset_ref[bit >> 6] >> (bit & 63)) & 1) != 0 {
                                if let Some(id) =
                                    table_ref.lookup(&slice[start_t..start_t + count], h32)
                                {
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
                    if p < len && slice[p] == b'\n' {
                        p += 1;
                    }
                }
            });
        }
    });

    drop(write_cursor);

    // Parallel Query Execution
    let num_queries = parsed_queries.len();
    let num_threads = thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(12)
        .min(12)
        .max(1);
    let chunk_size = (num_queries + num_threads - 1) / num_threads;
    let mut thread_outputs: Vec<Vec<u8>> = (0..num_threads).map(|_| Vec::new()).collect();
    let (offsets_ref, postings_ref) = (&term_offsets, &all_postings);

    thread::scope(|s| {
        let (queries_ref, mmap_ref, line_offsets_ref) =
            (&parsed_queries, &mmap_slice, &line_offsets);
        let get_postings = |tid: u32| -> &[u32] {
            let t = tid as usize;
            &postings_ref[offsets_ref[t]..offsets_ref[t + 1]]
        };
        for (thread_idx, out_buf) in thread_outputs.iter_mut().enumerate() {
            let (start, end) = (
                thread_idx * chunk_size,
                (thread_idx * chunk_size + chunk_size).min(num_queries),
            );
            if start >= end {
                continue;
            }
            s.spawn(move || {
                let (mut cur_res, mut next_res, mut scratch, mut matches) =
                    (Vec::new(), Vec::new(), Vec::new(), Vec::new());
                for q_idx in start..end {
                    match &queries_ref[q_idx] {
                        ParsedQuery::Bool { first_term, ops } => {
                            let p0 = get_postings(*first_term);
                            if ops.is_empty() {
                                fast_write_results(out_buf, p0);
                            } else {
                                cur_res.clear();
                                let (first_op, first_tid) = ops[0];
                                let p1 = get_postings(first_tid);
                                match first_op {
                                    0 => intersect_sorted(p0, p1, &mut cur_res),
                                    1 => union_sorted(p0, p1, &mut cur_res),
                                    2 => difference_sorted(p0, p1, &mut cur_res),
                                    _ => {}
                                }
                                for &(op, tid) in &ops[1..] {
                                    let p = get_postings(tid);
                                    match op {
                                        0 => {
                                            intersect_sorted(&cur_res, p, &mut next_res);
                                            std::mem::swap(&mut cur_res, &mut next_res);
                                        }
                                        1 => {
                                            union_sorted(&cur_res, p, &mut next_res);
                                            std::mem::swap(&mut cur_res, &mut next_res);
                                        }
                                        2 => {
                                            difference_sorted(&cur_res, p, &mut next_res);
                                            std::mem::swap(&mut cur_res, &mut next_res);
                                        }
                                        _ => {}
                                    }
                                }
                                fast_write_results(out_buf, &cur_res);
                            }
                        }
                        ParsedQuery::Phrase { term_ids, words } => {
                            if term_ids.is_empty()
                                || term_ids.iter().any(|&t| get_postings(t).is_empty())
                            {
                                out_buf.extend_from_slice(b"0 0\n");
                                continue;
                            }
                            let (num_tids, mut order) = (term_ids.len().min(16), [0usize; 16]);
                            for idx in 0..num_tids {
                                order[idx] = idx;
                            }
                            order[..num_tids]
                                .sort_unstable_by_key(|&idx| get_postings(term_ids[idx]).len());
                            cur_res.clear();
                            cur_res.extend_from_slice(get_postings(term_ids[order[0]]));
                            for &idx in &order[1..num_tids] {
                                if cur_res.is_empty() {
                                    break;
                                }
                                intersect_sorted(
                                    &cur_res,
                                    get_postings(term_ids[idx]),
                                    &mut scratch,
                                );
                                std::mem::swap(&mut cur_res, &mut scratch);
                            }
                            let k = words.len();
                            let mut pi = [0usize; 16];
                            let mut j = 0;
                            for i in 1..k {
                                while j > 0 && words[i] != words[j] {
                                    j = pi[j - 1];
                                }
                                if words[i] == words[j] {
                                    j += 1;
                                }
                                pi[i] = j;
                            }
                            matches.clear();
                            for &c_doc in &cur_res {
                                let c_idx = c_doc as usize;
                                if c_idx + 1 >= line_offsets_ref.len() {
                                    continue;
                                }
                                let f_offset = line_offsets_ref[c_idx] as usize;
                                let f_end = line_offsets_ref[c_idx + 1] as usize;
                                if f_offset >= f_end || f_end > mmap_ref.len() {
                                    continue;
                                }
                                let line_slice = &mmap_ref[f_offset..f_end];
                                // Skip doc_id and tab
                                let mut dp = 0;
                                while dp < line_slice.len()
                                    && line_slice[dp] != b'\t'
                                    && line_slice[dp] != b'\n'
                                {
                                    dp += 1;
                                }
                                if dp < line_slice.len() && line_slice[dp] == b'\t' {
                                    dp += 1;
                                }
                                let t_len = line_slice.len();
                                let mut matched = 0;
                                while dp < t_len {
                                    if line_slice[dp].is_ascii_alphanumeric() {
                                        let t_start = dp;
                                        while dp < t_len && line_slice[dp].is_ascii_alphanumeric() {
                                            dp += 1;
                                        }
                                        let token_slice = &line_slice[t_start..dp];
                                        let matches_word = |target: &[u8]| -> bool {
                                            if token_slice.len() != target.len() {
                                                return false;
                                            }
                                            token_slice
                                                .iter()
                                                .zip(target.iter())
                                                .all(|(&a, &b)| (a | 0x20) == b)
                                        };
                                        while matched > 0
                                            && !matches_word(words[matched].as_slice())
                                        {
                                            matched = pi[matched - 1];
                                        }
                                        if matches_word(words[matched].as_slice()) {
                                            matched += 1;
                                            if matched == k {
                                                matches.push(c_doc);
                                                break;
                                            }
                                        }
                                    } else {
                                        dp += 1;
                                    }
                                }
                            }
                            fast_write_results(out_buf, &matches);
                        }
                        ParsedQuery::Empty => out_buf.extend_from_slice(b"0 0\n"),
                    }
                }
            });
        }
    });

    fs::create_dir_all(&out_dir)?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, File::create(&out_path)?);
    for out_buf in thread_outputs {
        writer.write_all(&out_buf)?;
    }
    writer.flush()?;

    // SAFETY: mmap_ptr is non-null and valid; munmap cleans up the virtual memory mapping.
    unsafe {
        munmap(mmap_ptr, file_len);
    }
    Ok(())
}
