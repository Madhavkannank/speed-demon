use std::{
    fs::File,
    io::{BufWriter, Write},
    os::fd::AsRawFd,
    path::Path,
    thread,
    time::Instant,
};

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
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

struct FastTermTable {
    slots: Vec<TableSlot>,
    mask: usize,
    arena: Vec<u8>,
    offsets: Vec<(u32, u16)>,
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
            slots: vec![TableSlot { tag: u32::MAX, id: 0 }; cap],
            mask: cap - 1,
            arena: Vec::with_capacity(1024 * 1024),
            offsets: vec![(0, 0)],
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
                let id = self.offsets.len() as u32;
                let offset = self.arena.len() as u32;
                for &b in term {
                    self.arena.push(b | 0x20);
                }
                self.offsets.push((offset, term.len() as u16));
                unsafe {
                    *self.slots.get_unchecked_mut(idx) = TableSlot { tag, id };
                }
                return id;
            }
            if slot.tag == tag {
                let (off, len) = unsafe { *self.offsets.get_unchecked(slot.id as usize) };
                if len as usize == term.len() {
                    let s = &self.arena[off as usize..off as usize + len as usize];
                    if s.iter().zip(term.iter()).all(|(&a, &b)| a == (b | 0x20)) {
                        return slot.id;
                    }
                }
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn lookup(&self, term: &[u8], h: u32) -> u32 {
        let tag = if h == u32::MAX { 0 } else { h };
        let mut idx = (tag as usize) & self.mask;
        loop {
            let slot = unsafe { *self.slots.get_unchecked(idx) };
            if slot.tag == u32::MAX {
                return 0;
            }
            if slot.tag == tag {
                let (off, len) = unsafe { *self.offsets.get_unchecked(slot.id as usize) };
                if len as usize == term.len() {
                    let s = &self.arena[off as usize..off as usize + len as usize];
                    if s.iter().zip(term.iter()).all(|(&a, &b)| a == (b | 0x20)) {
                        return slot.id;
                    }
                }
            }
            idx = (idx + 1) & self.mask;
        }
    }
}

#[derive(Clone, Copy)]
#[repr(C)]
struct PairSlot {
    key: u64,
    head: u32,
}

#[derive(Clone, Copy)]
struct PhraseItem {
    phrase_id: u32,
    len: u8,
    terms_offset: u32,
    next: u32,
}

struct PhraseIndex {
    slots: Vec<PairSlot>,
    mask: usize,
    items: Vec<PhraseItem>,
    terms: Vec<u32>,
}

#[inline(always)]
fn hash_pair(k: u64) -> usize {
    let mut h = k ^ (k >> 33);
    h = h.wrapping_mul(0xff51afd7ed558ccdu64);
    h = h ^ (h >> 33);
    h as usize
}

impl PhraseIndex {
    fn with_capacity(cap: usize) -> Self {
        assert!(cap.is_power_of_two());
        Self {
            slots: vec![PairSlot { key: u64::MAX, head: u32::MAX }; cap],
            mask: cap - 1,
            items: Vec::with_capacity(32768),
            terms: Vec::with_capacity(65536),
        }
    }

    fn insert(&mut self, phrase_id: u32, phrase_terms: &[u32]) {
        let k = phrase_terms.len();
        assert!(k >= 2);
        let key = ((phrase_terms[k - 2] as u64) << 32) | (phrase_terms[k - 1] as u64);
        let terms_offset = self.terms.len() as u32;
        self.terms.extend_from_slice(phrase_terms);

        let mut idx = hash_pair(key) & self.mask;
        loop {
            let slot = unsafe { self.slots.get_unchecked(idx) };
            if slot.key == u64::MAX || slot.key == key {
                let item_idx = self.items.len() as u32;
                let next = if slot.key == key { slot.head } else { u32::MAX };
                self.items.push(PhraseItem {
                    phrase_id,
                    len: k as u8,
                    terms_offset,
                    next,
                });
                unsafe {
                    *self.slots.get_unchecked_mut(idx) = PairSlot { key, head: item_idx };
                }
                return;
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn get_head(&self, key: u64) -> u32 {
        let mut idx = hash_pair(key) & self.mask;
        loop {
            let slot = unsafe { *self.slots.get_unchecked(idx) };
            if slot.key == u64::MAX {
                return u32::MAX;
            }
            if slot.key == key {
                return slot.head;
            }
            idx = (idx + 1) & self.mask;
        }
    }
}

// 32-byte chunk for posting lists: 27 bytes payload, 5 bytes metadata
#[derive(Clone, Copy)]
#[repr(C)]
struct PostingChunk {
    next: u32,
    len: u8,
    data: [u8; 27],
}

struct WorkerArena {
    base_doc: u32,
    heads: Vec<u32>,
    tails: Vec<u32>,
    last_doc: Vec<u32>,
    chunks: Vec<PostingChunk>,
    phrase_matches: Vec<Vec<u32>>,
}

impl WorkerArena {
    fn new(base_doc: u32, vocab_size: usize, num_phrases: usize) -> Self {
        Self {
            base_doc,
            heads: vec![u32::MAX; vocab_size],
            tails: vec![u32::MAX; vocab_size],
            last_doc: vec![u32::MAX; vocab_size],
            chunks: Vec::with_capacity(180_000),
            phrase_matches: (0..num_phrases).map(|_| Vec::new()).collect(),
        }
    }

    #[inline(always)]
    fn add_posting(&mut self, term_id: u32, doc_id: u32) {
        let tid = term_id as usize;
        let last = self.last_doc[tid];
        if last == doc_id {
            return;
        }
        let delta = if last == u32::MAX { doc_id - self.base_doc } else { doc_id - last - 1 };
        self.last_doc[tid] = doc_id;

        let mut d_bytes = [0u8; 4];
        let mut d_len = 0;
        let mut v = delta;
        while v >= 128 {
            d_bytes[d_len] = (v & 0x7F) as u8 | 0x80;
            v >>= 7;
            d_len += 1;
        }
        d_bytes[d_len] = v as u8;
        d_len += 1;

        let tail = self.tails[tid];
        if tail != u32::MAX {
            let chunk = unsafe { self.chunks.get_unchecked_mut(tail as usize) };
            let rem = 27 - chunk.len as usize;
            if rem >= d_len {
                for i in 0..d_len {
                    chunk.data[chunk.len as usize + i] = d_bytes[i];
                }
                chunk.len += d_len as u8;
                return;
            }
        }

        let new_idx = self.chunks.len() as u32;
        let mut chunk = PostingChunk {
            next: u32::MAX,
            len: d_len as u8,
            data: [0; 27],
        };
        for i in 0..d_len {
            chunk.data[i] = d_bytes[i];
        }
        self.chunks.push(chunk);

        if tail != u32::MAX {
            unsafe {
                self.chunks.get_unchecked_mut(tail as usize).next = new_idx;
            }
            self.tails[tid] = new_idx;
        } else {
            self.heads[tid] = new_idx;
            self.tails[tid] = new_idx;
        }
    }

    #[inline(always)]
    fn decode_term(&self, term_id: u32, out: &mut Vec<u32>) {
        let tid = term_id as usize;
        let mut cur = self.heads[tid];
        let mut last = self.base_doc;
        let mut is_first = true;

        while cur != u32::MAX {
            let chunk = unsafe { self.chunks.get_unchecked(cur as usize) };
            let mut p = 0;
            let len = chunk.len as usize;
            while p < len {
                let mut delta = 0u32;
                let mut shift = 0;
                loop {
                    let b = chunk.data[p];
                    p += 1;
                    delta |= ((b & 0x7F) as u32) << shift;
                    if b < 128 { break; }
                    shift += 7;
                }
                let doc_id = if is_first {
                    is_first = false;
                    self.base_doc + delta
                } else {
                    last + delta + 1
                };
                out.push(doc_id);
                last = doc_id;
            }
            cur = chunk.next;
        }
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

#[inline(always)]
fn intersect_sorted(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
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

enum ParsedQuery {
    Bool {
        first_term: u32,
        ops: Vec<(u8, u32)>,
    },
    Phrase {
        phrase_id: u32,
    },
    Empty,
}

fn run_compact(data_dir: &str, out_dir: &str) -> std::io::Result<()> {
    let docs_path = Path::new(data_dir).join("docs.txt");
    let queries_path = Path::new(data_dir).join("queries.txt");
    let out_path = Path::new(out_dir).join("results.txt");

    let docs_file = File::open(&docs_path)?;
    let file_len = docs_file.metadata()?.len() as usize;
    let mmap_ptr = unsafe {
        mmap(std::ptr::null_mut(), file_len, PROT_READ, MAP_PRIVATE, docs_file.as_raw_fd(), 0)
    };
    if mmap_ptr.is_null() || mmap_ptr == usize::MAX as *mut std::ffi::c_void {
        return Err(std::io::Error::last_os_error());
    }
    unsafe { madvise(mmap_ptr, file_len, MADV_POPULATE_READ); }
    let mmap_slice: &[u8] = unsafe { std::slice::from_raw_parts(mmap_ptr as *const u8, file_len) };

    // 1. Parse all queries up front
    let q_bytes = std::fs::read(&queries_path)?;
    let mut table = FastTermTable::with_capacity(131072);
    let mut bitset = vec![0u64; 8192];
    let mut phrase_index = PhraseIndex::with_capacity(65536);
    let mut parsed_queries = Vec::with_capacity(50000);
    let mut num_phrases = 0u32;

    let mut p = 0;
    let q_len = q_bytes.len();
    while p < q_len {
        let line_start = p;
        while p < q_len && q_bytes[p] != b'\n' { p += 1; }
        let line = &q_bytes[line_start..p];
        if p < q_len && q_bytes[p] == b'\n' { p += 1; }
        if line.is_empty() { continue; }

        if line.starts_with(b"BOOL ") {
            let rest = &line[5..];
            let mut parts = rest.split(|&b| b == b' ');
            if let Some(first) = parts.next() {
                let first_id = table.get_or_insert(first);
                let h = hash32(first);
                bitset[(h & 0xFFFF) as usize >> 6] |= 1u64 << ((h & 0xFFFF) as usize & 63);

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
                        bitset[(th & 0xFFFF) as usize >> 6] |= 1u64 << ((th & 0xFFFF) as usize & 63);
                        ops.push((op_code, tid));
                    }
                }
                parsed_queries.push(ParsedQuery::Bool { first_term: first_id, ops });
            } else {
                parsed_queries.push(ParsedQuery::Empty);
            }
        } else if line.starts_with(b"PHRASE \"") {
            let mut inner = &line[8..];
            if inner.ends_with(b"\"") { inner = &inner[..inner.len() - 1]; }
            let mut terms = Vec::new();
            for word in inner.split(|&b| b == b' ') {
                if !word.is_empty() {
                    let tid = table.get_or_insert(word);
                    let h = hash32(word);
                    bitset[(h & 0xFFFF) as usize >> 6] |= 1u64 << ((h & 0xFFFF) as usize & 63);
                    terms.push(tid);
                }
            }
            if terms.len() >= 2 {
                let ph_id = num_phrases;
                num_phrases += 1;
                phrase_index.insert(ph_id, &terms);
                parsed_queries.push(ParsedQuery::Phrase { phrase_id: ph_id });
            } else {
                parsed_queries.push(ParsedQuery::Empty);
            }
        } else {
            parsed_queries.push(ParsedQuery::Empty);
        }
    }

    let num_workers = thread::available_parallelism().map(|p| p.get()).unwrap_or(12).min(12).max(1);
    let approx_chunk = file_len / num_workers;
    let mut chunk_bounds = Vec::with_capacity(num_workers + 1);
    chunk_bounds.push(0);
    for i in 1..num_workers {
        let mut pos = (i * approx_chunk).min(file_len);
        while pos < file_len && mmap_slice[pos] != b'\n' { pos += 1; }
        if pos < file_len { pos += 1; }
        chunk_bounds.push(pos);
    }
    chunk_bounds.push(file_len);

    let vocab_size = table.offsets.len();
    let np = num_phrases as usize;

    let mut worker_arenas: Vec<WorkerArena> = Vec::with_capacity(num_workers);

    thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_workers);
        for w in 0..num_workers {
            let (start, end) = (chunk_bounds[w], chunk_bounds[w + 1]);
            let slice = &mmap_slice[start..end];
            let table_ref = &table;
            let bitset_ref = &bitset;
            let phrase_idx_ref = &phrase_index;

            handles.push(s.spawn(move || {
                let mut p = 0;
                let len = slice.len();
                let mut base_doc = 0u32;
                let mut sp = 0;
                while sp < len && slice[sp] >= b'0' && slice[sp] <= b'9' {
                    base_doc = base_doc * 10 + (slice[sp] - b'0') as u32;
                    sp += 1;
                }
                let mut arena = WorkerArena::new(base_doc, vocab_size, np);
                let mut doc_tokens = Vec::with_capacity(256);
                let mut last_seen_phrase = vec![u32::MAX; np];

                while p < len {
                    let mut did = 0u32;
                    while p < len && slice[p] >= b'0' && slice[p] <= b'9' {
                        did = did * 10 + (slice[p] - b'0') as u32;
                        p += 1;
                    }
                    let doc_id = did;
                    while p < len && slice[p] != b'\t' && slice[p] != b'\n' { p += 1; }
                    if p < len && slice[p] == b'\t' { p += 1; }

                    doc_tokens.clear();

                    while p < len && slice[p] != b'\n' {
                        if slice[p].is_ascii_alphanumeric() {
                            let start_t = p;
                            while p < len && slice[p].is_ascii_alphanumeric() { p += 1; }
                            let count = (p - start_t).min(32);
                            let mut h = 0x517cc1b727220a95u64;
                            for k in 0..count {
                                h = (h.rotate_left(5) ^ ((slice[start_t + k] | 0x20) as u64)).wrapping_mul(0x9e3779b97f4a7c15);
                            }
                            let h32 = (h ^ (h >> 32)) as u32;
                            let bit = (h32 & 0xFFFF) as usize;
                            let mut tid = 0u32;
                            if ((bitset_ref[bit >> 6] >> (bit & 63)) & 1) != 0 {
                                tid = table_ref.lookup(&slice[start_t..start_t + count], h32);
                            }
                            doc_tokens.push(tid);

                            // Record for BOOL posting list if in vocabulary
                            if tid > 0 {
                                arena.add_posting(tid, doc_id);
                            }

                            // INLINE PHRASE CHECK: check last two tokens
                            let dt_len = doc_tokens.len();
                            if dt_len >= 2 && tid > 0 {
                                let prev_tid = doc_tokens[dt_len - 2];
                                if prev_tid > 0 {
                                    let pair_key = ((prev_tid as u64) << 32) | (tid as u64);
                                    let mut head = phrase_idx_ref.get_head(pair_key);
                                    while head != u32::MAX {
                                        let item = unsafe { *phrase_idx_ref.items.get_unchecked(head as usize) };
                                        let plen = item.len as usize;
                                        if dt_len >= plen {
                                            let target_slice = &phrase_idx_ref.terms[item.terms_offset as usize..item.terms_offset as usize + plen];
                                            let candidate_slice = &doc_tokens[dt_len - plen..dt_len];
                                            if candidate_slice == target_slice {
                                                let ph_idx = item.phrase_id as usize;
                                                if last_seen_phrase[ph_idx] != doc_id {
                                                    last_seen_phrase[ph_idx] = doc_id;
                                                    arena.phrase_matches[ph_idx].push(doc_id);
                                                }
                                            }
                                        }
                                        head = item.next;
                                    }
                                }
                            }
                        } else {
                            p += 1;
                        }
                    }
                    if p < len && slice[p] == b'\n' { p += 1; }
                }

                arena
            }));
        }
        for h in handles {
            worker_arenas.push(h.join().unwrap());
        }
    });

    // Merge Phrase matches into flat CSR (fast and tiny, < 1 MB total)
    let mut phrase_offsets = vec![0u32; np + 1];
    for w in &worker_arenas {
        for (ph_id, list) in w.phrase_matches.iter().enumerate() {
            phrase_offsets[ph_id + 1] += list.len() as u32;
        }
    }
    for i in 0..np {
        phrase_offsets[i + 1] += phrase_offsets[i];
    }
    let mut all_phrase_matches = vec![0u32; phrase_offsets[np] as usize];
    let mut cur_phrase = phrase_offsets.clone();
    for w in &worker_arenas {
        for (ph_id, list) in w.phrase_matches.iter().enumerate() {
            let s = cur_phrase[ph_id] as usize;
            all_phrase_matches[s..s + list.len()].copy_from_slice(list);
            cur_phrase[ph_id] += list.len() as u32;
        }
    }

    // Answer all queries
    let num_queries = parsed_queries.len();
    let chunk_size = (num_queries + num_workers - 1) / num_workers;
    let mut thread_outputs: Vec<Vec<u8>> = (0..num_workers).map(|_| Vec::new()).collect();

    thread::scope(|s| {
        let queries_ref = &parsed_queries;
        let arenas_ref = &worker_arenas;
        let p_matches = &all_phrase_matches;
        let p_offsets = &phrase_offsets;

        for (w, out_buf) in thread_outputs.iter_mut().enumerate() {
            let (start, end) = (w * chunk_size, (w * chunk_size + chunk_size).min(num_queries));
            if start >= end { continue; }

            s.spawn(move || {
                let get_postings = |tid: u32, out: &mut Vec<u32>| {
                    out.clear();
                    for a in arenas_ref {
                        a.decode_term(tid, out);
                    }
                };
                let (mut p0, mut p1, mut cur_res, mut next_res) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
                for q_idx in start..end {
                    match &queries_ref[q_idx] {
                        ParsedQuery::Bool { first_term, ops } => {
                            get_postings(*first_term, &mut p0);
                            if ops.is_empty() {
                                fast_write_results(out_buf, &p0);
                            } else {
                                cur_res.clear();
                                let (first_op, first_tid) = ops[0];
                                get_postings(first_tid, &mut p1);
                                match first_op {
                                    0 => intersect_sorted(&p0, &p1, &mut cur_res),
                                    1 => union_sorted(&p0, &p1, &mut cur_res),
                                    2 => difference_sorted(&p0, &p1, &mut cur_res),
                                    _ => {}
                                }
                                for &(op, tid) in &ops[1..] {
                                    get_postings(tid, &mut p1);
                                    match op {
                                        0 => {
                                            intersect_sorted(&cur_res, &p1, &mut next_res);
                                            std::mem::swap(&mut cur_res, &mut next_res);
                                        }
                                        1 => {
                                            union_sorted(&cur_res, &p1, &mut next_res);
                                            std::mem::swap(&mut cur_res, &mut next_res);
                                        }
                                        2 => {
                                            difference_sorted(&cur_res, &p1, &mut next_res);
                                            std::mem::swap(&mut cur_res, &mut next_res);
                                        }
                                        _ => {}
                                    }
                                }
                                fast_write_results(out_buf, &cur_res);
                            }
                        }
                        ParsedQuery::Phrase { phrase_id } => {
                            let pid = *phrase_id as usize;
                            let matches = &p_matches[p_offsets[pid] as usize..p_offsets[pid + 1] as usize];
                            fast_write_results(out_buf, matches);
                        }
                        ParsedQuery::Empty => out_buf.extend_from_slice(b"0 0\n"),
                    }
                }
            });
        }
    });

    std::fs::create_dir_all(out_dir)?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, File::create(&out_path)?);
    for out_buf in thread_outputs {
        writer.write_all(&out_buf)?;
    }
    writer.flush()?;

    unsafe { munmap(mmap_ptr, file_len); }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = if args.len() > 1 { &args[1] } else { "/tmp/sd-test-tiny-b" };
    let out_dir = if args.len() > 2 { &args[2] } else { "/tmp/sd-cand-proto" };
    let t0 = Instant::now();
    run_compact(data_dir, out_dir).unwrap();
    println!("Finished in {:.2} ms", t0.elapsed().as_secs_f64() * 1000.0);
}
