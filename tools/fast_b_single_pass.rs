use std::{
    arch::x86_64::*,
    cmp::Ordering,
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

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct TableSlot {
    key_low: u64,
    key_high: u64,
    hash: u32,
    id: u32,
    len: u8,
    extra: u8,
    _pad: [u8; 6],
}

const TABLE_SIZE: usize = 262144;
const TABLE_MASK: usize = TABLE_SIZE - 1;

#[inline(always)]
unsafe fn hash_and_key(ptr: *const u8, len: usize) -> (u32, u64, u64, u8) {
    if len <= 8 {
        let mask = if len == 8 { u64::MAX } else { (1u64 << (len * 8)) - 1 };
        let raw = (ptr as *const u64).read_unaligned();
        let w0 = (raw | 0x2020202020202020u64) & mask;
        let h = w0.wrapping_mul(0x517cc1b727220a95u64) ^ (len as u64);
        let h32 = (h ^ (h >> 32)) as u32;
        (h32, w0, 0, 0)
    } else if len <= 16 {
        let raw0 = (ptr as *const u64).read_unaligned();
        let w0 = raw0 | 0x2020202020202020u64;
        let rem_len = len - 8;
        let mask = if rem_len == 8 { u64::MAX } else { (1u64 << (rem_len * 8)) - 1 };
        let raw1 = (ptr.add(8) as *const u64).read_unaligned();
        let w1 = (raw1 | 0x2020202020202020u64) & mask;
        let h = (w0.wrapping_mul(0x517cc1b727220a95u64) ^ w1.rotate_left(27)).wrapping_mul(0x9e3779b97f4a7c15u64) ^ (len as u64);
        let h32 = (h ^ (h >> 32)) as u32;
        (h32, w0, w1, 0)
    } else {
        let raw0 = (ptr as *const u64).read_unaligned();
        let w0 = raw0 | 0x2020202020202020u64;
        let raw1 = (ptr.add(8) as *const u64).read_unaligned();
        let w1 = raw1 | 0x2020202020202020u64;
        let ext = (*ptr.add(16)) | 0x20;
        let h = (w0.wrapping_mul(0x517cc1b727220a95u64) ^ w1.rotate_left(27) ^ ((ext as u64).rotate_left(43))).wrapping_mul(0x9e3779b97f4a7c15u64) ^ (len as u64);
        let h32 = (h ^ (h >> 32)) as u32;
        (h32, w0, w1, ext)
    }
}

struct TermTable {
    slots: Vec<TableSlot>,
    vocab_size: u32,
}

impl TermTable {
    fn new() -> Self {
        Self {
            slots: vec![TableSlot::default(); TABLE_SIZE],
            vocab_size: 1, // 0 = empty
        }
    }

    #[inline(always)]
    fn get_or_insert(&mut self, term: &[u8]) -> u32 {
        let mut buf = [0u8; 32];
        let len = term.len().min(32);
        buf[..len].copy_from_slice(&term[..len]);
        let (h, w0, w1, ext) = unsafe { hash_and_key(buf.as_ptr(), len) };
        let tag = if h == 0 { 1 } else { h };
        let mut idx = (tag as usize) & TABLE_MASK;
        loop {
            let slot = unsafe { self.slots.get_unchecked_mut(idx) };
            if slot.hash == 0 {
                let id = self.vocab_size;
                self.vocab_size += 1;
                *slot = TableSlot { key_low: w0, key_high: w1, hash: tag, id, len: len as u8, extra: ext, _pad: [0; 6] };
                return id;
            }
            if slot.hash == tag && slot.len as usize == len && slot.key_low == w0 && slot.key_high == w1 && slot.extra == ext {
                return slot.id;
            }
            idx = (idx + 1) & TABLE_MASK;
        }
    }

    #[inline(always)]
    fn lookup(&self, ptr: *const u8, len: usize) -> u32 {
        let (h, w0, w1, ext) = unsafe { hash_and_key(ptr, len) };
        let tag = if h == 0 { 1 } else { h };
        let mut idx = (tag as usize) & TABLE_MASK;
        loop {
            let slot = unsafe { self.slots.get_unchecked(idx) };
            if slot.hash == 0 {
                return 0;
            }
            if slot.hash == tag && slot.len as usize == len && slot.key_low == w0 && slot.key_high == w1 && slot.extra == ext {
                return slot.id;
            }
            idx = (idx + 1) & TABLE_MASK;
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
        let pair_key = ((phrase_terms[k - 2] as u64) << 32) | (phrase_terms[k - 1] as u64);
        let terms_offset = self.terms.len() as u32;
        self.terms.extend_from_slice(phrase_terms);

        let item_idx = self.items.len() as u32;
        let mut slot_idx = hash_pair(pair_key) & self.mask;
        loop {
            let slot = unsafe { self.slots.get_unchecked(slot_idx) };
            if slot.key == u64::MAX {
                unsafe {
                    *self.slots.get_unchecked_mut(slot_idx) = PairSlot { key: pair_key, head: item_idx };
                }
                self.items.push(PhraseItem { phrase_id, len: k as u8, terms_offset, next: u32::MAX });
                return;
            }
            if slot.key == pair_key {
                let old_head = slot.head;
                unsafe {
                    self.slots.get_unchecked_mut(slot_idx).head = item_idx;
                }
                self.items.push(PhraseItem { phrase_id, len: k as u8, terms_offset, next: old_head });
                return;
            }
            slot_idx = (slot_idx + 1) & self.mask;
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
    term_counts: Vec<u32>,
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
            term_counts: vec![0u32; vocab_size],
            chunks: Vec::with_capacity(30_000),
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
        let delta = if last == u32::MAX {
            doc_id - self.base_doc
        } else {
            doc_id - last - 1
        };
        self.last_doc[tid] = doc_id;
        self.term_counts[tid] += 1;

        let mut d_bytes = [0u8; 4];
        let mut d_len = 0;
        let mut v = delta;
        while v >= 128 {
            d_bytes[d_len] = (v as u8 & 0x7F) | 0x80;
            v >>= 7;
            d_len += 1;
        }
        d_bytes[d_len] = v as u8;
        d_len += 1;

        let tail = self.tails[tid];
        if tail != u32::MAX {
            let chunk = &mut self.chunks[tail as usize];
            let rem = 27 - chunk.len as usize;
            if rem >= d_len {
                let start = chunk.len as usize;
                chunk.data[start..start + d_len].copy_from_slice(&d_bytes[..d_len]);
                chunk.len += d_len as u8;
                return;
            }
        }

        let new_idx = self.chunks.len() as u32;
        let mut new_chunk = PostingChunk { next: u32::MAX, len: d_len as u8, data: [0u8; 27] };
        new_chunk.data[..d_len].copy_from_slice(&d_bytes[..d_len]);
        self.chunks.push(new_chunk);

        if tail == u32::MAX {
            self.heads[tid] = new_idx;
        } else {
            self.chunks[tail as usize].next = new_idx;
        }
        self.tails[tid] = new_idx;
    }

    fn decode_postings_into(&self, term_id: u32, out: &mut Vec<u32>) {
        let tid = term_id as usize;
        let mut cur = self.heads[tid];
        let mut is_first = true;
        let mut last = 0u32;

        while cur != u32::MAX {
            let chunk = &self.chunks[cur as usize];
            let len = chunk.len as usize;
            let mut p = 0;
            while p < len {
                let mut delta = 0u32;
                let mut shift = 0;
                while p < len {
                    let b = chunk.data[p];
                    p += 1;
                    delta |= ((b & 0x7F) as u32) << shift;
                    if (b & 0x80) == 0 {
                        break;
                    }
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
    if n == 0 { buf.push(b'0'); return; }
    let mut t = [0u8; 10];
    let mut i = 10;
    while n > 0 { i -= 1; t[i] = b'0' + (n % 10) as u8; n /= 10; }
    buf.extend_from_slice(&t[i..]);
}

#[inline(always)]
fn append_u64(buf: &mut Vec<u8>, mut n: u64) {
    if n == 0 { buf.push(b'0'); return; }
    let mut t = [0u8; 20];
    let mut i = 20;
    while n > 0 { i -= 1; t[i] = b'0' + (n % 10) as u8; n /= 10; }
    buf.extend_from_slice(&t[i..]);
}

#[inline(always)]
fn fast_write_results(buf: &mut Vec<u8>, ids: &[u32]) {
    if ids.is_empty() { buf.extend_from_slice(b"0 0\n"); return; }
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
        if a[i] == b[j] { out.push(a[i]); i += 1; j += 1; }
        else if a[i] < b[j] { i += 1; }
        else { j += 1; }
    }
}

#[inline(always)]
fn union_sorted(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] { out.push(a[i]); i += 1; j += 1; }
        else if a[i] < b[j] { out.push(a[i]); i += 1; }
        else { out.push(b[j]); j += 1; }
    }
    if i < a.len() { out.extend_from_slice(&a[i..]); }
    if j < b.len() { out.extend_from_slice(&b[j..]); }
}

#[inline(always)]
fn difference_sorted(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] { out.push(a[i]); i += 1; }
        else if a[i] == b[j] { i += 1; j += 1; }
        else { j += 1; }
    }
    if i < a.len() { out.extend_from_slice(&a[i..]); }
}

enum ParsedQuery {
    Bool { first_term: u32, ops: Vec<(u8, u32)> },
    Phrase { phrase_id: u32 },
    Empty,
}

fn main() -> std::io::Result<()> {
    let t_all = Instant::now();
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string());
    let docs_path = Path::new(&data_dir).join("docs.txt");
    let queries_path = Path::new(&data_dir).join("queries.txt");
    let out_path = Path::new(&out_dir).join("results.txt");

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

    // 1. Parse all queries
    let t_q = Instant::now();
    let q_bytes = std::fs::read(&queries_path)?;
    let mut table = TermTable::new();
    let mut bitset = vec![0u64; 8192];
    let mut phrase_index = PhraseIndex::with_capacity(65536);
    let mut pair_bitset = vec![0u64; 1024];
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
                let (h, _, _, _) = unsafe { hash_and_key(first.as_ptr(), first.len()) };
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
                        let (th, _, _, _) = unsafe { hash_and_key(term_bytes.as_ptr(), term_bytes.len()) };
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
                    let (h, _, _, _) = unsafe { hash_and_key(word.as_ptr(), word.len()) };
                    bitset[(h & 0xFFFF) as usize >> 6] |= 1u64 << ((h & 0xFFFF) as usize & 63);
                    terms.push(tid);
                }
            }
            if terms.len() >= 2 {
                let ph_id = num_phrases;
                num_phrases += 1;
                phrase_index.insert(ph_id, &terms);
                let k = terms.len();
                let pair_key = ((terms[k - 2] as u64) << 32) | (terms[k - 1] as u64);
                let ph_bit = (hash_pair(pair_key) & 0xFFFF) as usize;
                pair_bitset[ph_bit >> 6] |= 1u64 << (ph_bit & 63);
                parsed_queries.push(ParsedQuery::Phrase { phrase_id: ph_id });
            } else {
                parsed_queries.push(ParsedQuery::Empty);
            }
        } else {
            parsed_queries.push(ParsedQuery::Empty);
        }
    }
    eprintln!("Parse queries: {:.2} ms", t_q.elapsed().as_secs_f64() * 1000.0);

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

    let vocab_size = table.vocab_size as usize;
    let np = num_phrases as usize;

    let mut worker_arenas: Vec<WorkerArena> = Vec::with_capacity(num_workers);

    // 2. Parallel AVX2 worker scan
    let t_scan = Instant::now();
    thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_workers);
        for w in 0..num_workers {
            let (start, end) = (chunk_bounds[w], chunk_bounds[w + 1]);
            let slice = &mmap_slice[start..end];
            let table_ref = &table;
            let bitset_ref = &bitset;
            let phrase_idx_ref = &phrase_index;
            let pair_bitset_ref = &pair_bitset;

            handles.push(s.spawn(move || unsafe {
                let mut p = 0;
                let len = slice.len();
                let ptr = slice.as_ptr();
                let mut base_doc = 0u32;
                let mut sp = 0;
                while sp < len && *ptr.add(sp) >= b'0' && *ptr.add(sp) <= b'9' {
                    base_doc = base_doc * 10 + (*ptr.add(sp) - b'0') as u32;
                    sp += 1;
                }
                let mut arena = WorkerArena::new(base_doc, vocab_size, np);
                let mut doc_tokens = Vec::with_capacity(256);
                let mut last_seen_phrase = vec![u32::MAX; np];

                let v_20 = _mm256_set1_epi8(0x20);
                let v_a_minus_1 = _mm256_set1_epi8((b'a' - 1) as i8);
                let v_z_plus_1 = _mm256_set1_epi8((b'z' + 1) as i8);
                let v_0_minus_1 = _mm256_set1_epi8((b'0' - 1) as i8);
                let v_9_plus_1 = _mm256_set1_epi8((b'9' + 1) as i8);
                let v_nl = _mm256_set1_epi8(b'\n' as i8);

                while p < len {
                    let mut doc_id = 0u32;
                    while p < len && *ptr.add(p) >= b'0' && *ptr.add(p) <= b'9' {
                        doc_id = doc_id * 10 + (*ptr.add(p) - b'0') as u32;
                        p += 1;
                    }
                    while p < len && *ptr.add(p) != b'\t' && *ptr.add(p) != b'\n' { p += 1; }
                    if p < len && *ptr.add(p) == b'\t' { p += 1; }

                    doc_tokens.clear();

                    let mut prev_is_alnum = 0u32;
                    let mut doc_ended = false;

                    while p + 32 <= len {
                        let v = _mm256_loadu_si256(ptr.add(p) as *const __m256i);
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
                            let valid_mask = (1u32 << nl_off).wrapping_sub(1);
                            mask &= valid_mask;
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
                                while p + off + l < len && (*ptr.add(p + off + l)).is_ascii_alphanumeric() {
                                    l += 1;
                                }
                                spanned = true;
                                l
                            };

                            let eff_len = tok_len.min(32);
                            let mut tid = 0u32;
                            if eff_len <= 17 {
                                let (h, _, _, _) = hash_and_key(ptr.add(tok_start), eff_len);
                                let bit = (h & 0xFFFF) as usize;
                                if ((bitset_ref[bit >> 6] >> (bit & 63)) & 1) != 0 {
                                    tid = table_ref.lookup(ptr.add(tok_start), eff_len);
                                }
                            }
                            doc_tokens.push(tid);

                            if tid > 0 {
                                arena.add_posting(tid, doc_id);
                            }

                            // INLINE PHRASE CHECK
                            let dt_len = doc_tokens.len();
                            if dt_len >= 2 && tid > 0 {
                                let prev_tid = doc_tokens[dt_len - 2];
                                if prev_tid > 0 {
                                    let pair_key = ((prev_tid as u64) << 32) | (tid as u64);
                                    let ph_bit = (hash_pair(pair_key) & 0xFFFF) as usize;
                                    if ((pair_bitset_ref[ph_bit >> 6] >> (ph_bit & 63)) & 1) != 0 {
                                        let mut head = phrase_idx_ref.get_head(pair_key);
                                        while head != u32::MAX {
                                            let item = *phrase_idx_ref.items.get_unchecked(head as usize);
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
                        while p < len && *ptr.add(p) != b'\n' {
                            if (*ptr.add(p)).is_ascii_alphanumeric() {
                                let tok_start = p;
                                while p < len && (*ptr.add(p)).is_ascii_alphanumeric() { p += 1; }
                                let tok_len = p - tok_start;
                                let eff_len = tok_len.min(32);
                                let mut tid = 0u32;
                                if eff_len <= 17 {
                                    let (h, _, _, _) = hash_and_key(ptr.add(tok_start), eff_len);
                                    let bit = (h & 0xFFFF) as usize;
                                    if ((bitset_ref[bit >> 6] >> (bit & 63)) & 1) != 0 {
                                        tid = table_ref.lookup(ptr.add(tok_start), eff_len);
                                    }
                                }
                                doc_tokens.push(tid);

                                if tid > 0 {
                                    arena.add_posting(tid, doc_id);
                                }

                                let dt_len = doc_tokens.len();
                                if dt_len >= 2 && tid > 0 {
                                    let prev_tid = doc_tokens[dt_len - 2];
                                    if prev_tid > 0 {
                                        let pair_key = ((prev_tid as u64) << 32) | (tid as u64);
                                        let ph_bit = (hash_pair(pair_key) & 0xFFFF) as usize;
                                        if ((pair_bitset_ref[ph_bit >> 6] >> (ph_bit & 63)) & 1) != 0 {
                                            let mut head = phrase_idx_ref.get_head(pair_key);
                                            while head != u32::MAX {
                                                let item = *phrase_idx_ref.items.get_unchecked(head as usize);
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
                                }
                            } else {
                                p += 1;
                            }
                        }
                        if p < len && *ptr.add(p) == b'\n' { p += 1; }
                    }
                }

                arena
            }));
        }
        for h in handles { worker_arenas.push(h.join().unwrap()); }
    });
    eprintln!("Workers scan: {:.2} ms", t_scan.elapsed().as_secs_f64() * 1000.0);

    // 3. Merge Phrase matches
    let mut phrase_offsets = vec![0u32; np + 1];
    for w in &worker_arenas {
        for (ph_id, list) in w.phrase_matches.iter().enumerate() {
            phrase_offsets[ph_id + 1] += list.len() as u32;
        }
    }
    for i in 0..np { phrase_offsets[i + 1] += phrase_offsets[i]; }
    let mut all_phrase_matches = vec![0u32; phrase_offsets[np] as usize];
    let mut cur_phrase = phrase_offsets.clone();
    for w in &worker_arenas {
        for (ph_id, list) in w.phrase_matches.iter().enumerate() {
            let s = cur_phrase[ph_id] as usize;
            all_phrase_matches[s..s + list.len()].copy_from_slice(list);
            cur_phrase[ph_id] += list.len() as u32;
        }
    }

    // 4. Merge Postings
    let t_merge = Instant::now();
    let mut term_offsets: Vec<usize> = Vec::with_capacity(vocab_size + 1);
    term_offsets.push(0);
    let mut total_postings = 0usize;
    for t in 0..vocab_size {
        let mut term_len = 0usize;
        for w in 0..num_workers {
            term_len += worker_arenas[w].term_counts[t] as usize;
        }
        total_postings += term_len;
        term_offsets.push(total_postings);
    }

    let mut all_postings = vec![0u32; total_postings];
    let mut cur_offsets = term_offsets.clone();
    for w in 0..num_workers {
        let arena = &worker_arenas[w];
        let mut decode_buf = Vec::with_capacity(8192);
        for t in 1..vocab_size {
            if arena.term_counts[t] > 0 {
                decode_buf.clear();
                arena.decode_postings_into(t as u32, &mut decode_buf);
                let start = cur_offsets[t];
                all_postings[start..start + decode_buf.len()].copy_from_slice(&decode_buf);
                cur_offsets[t] += decode_buf.len();
            }
        }
    }
    drop(worker_arenas);
    eprintln!("Merge postings: {:.2} ms (total: {})", t_merge.elapsed().as_secs_f64() * 1000.0, total_postings);

    // 5. Parallel Query Evaluation
    let t_eval = Instant::now();
    let num_queries = parsed_queries.len();
    let chunk_size = (num_queries + num_workers - 1) / num_workers;
    let mut thread_outputs: Vec<Vec<u8>> = (0..num_workers).map(|_| Vec::new()).collect();

    thread::scope(|s| {
        let queries_ref = &parsed_queries;
        let postings_ref = &all_postings;
        let offsets_ref = &term_offsets;
        let p_matches = &all_phrase_matches;
        let p_offsets = &phrase_offsets;

        for (w, out_buf) in thread_outputs.iter_mut().enumerate() {
            let (start, end) = (w * chunk_size, (w * chunk_size + chunk_size).min(num_queries));
            if start >= end { continue; }

            s.spawn(move || {
                let get_postings = |tid: u32| -> &[u32] {
                    let t = tid as usize;
                    &postings_ref[offsets_ref[t]..offsets_ref[t + 1]]
                };
                let (mut cur_res, mut next_res) = (Vec::new(), Vec::new());

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
                                    next_res.clear();
                                    match op {
                                        0 => intersect_sorted(&cur_res, p, &mut next_res),
                                        1 => union_sorted(&cur_res, p, &mut next_res),
                                        2 => difference_sorted(&cur_res, p, &mut next_res),
                                        _ => {}
                                    }
                                    std::mem::swap(&mut cur_res, &mut next_res);
                                }
                                fast_write_results(out_buf, &cur_res);
                            }
                        }
                        ParsedQuery::Phrase { phrase_id } => {
                            let ph = *phrase_id as usize;
                            let matches = &p_matches[p_offsets[ph] as usize..p_offsets[ph + 1] as usize];
                            fast_write_results(out_buf, matches);
                        }
                        ParsedQuery::Empty => {
                            out_buf.extend_from_slice(b"0 0\n");
                        }
                    }
                }
            });
        }
    });
    eprintln!("Query eval: {:.2} ms", t_eval.elapsed().as_secs_f64() * 1000.0);

    // 6. Write output
    let t_out = Instant::now();
    let mut out_file = BufWriter::with_capacity(1024 * 1024, File::create(&out_path)?);
    for buf in &thread_outputs {
        out_file.write_all(buf)?;
    }
    out_file.flush()?;
    eprintln!("Write output: {:.2} ms", t_out.elapsed().as_secs_f64() * 1000.0);
    eprintln!("ALL DONE: {:.2} ms", t_all.elapsed().as_secs_f64() * 1000.0);

    unsafe { munmap(mmap_ptr, file_len); }
    Ok(())
}
