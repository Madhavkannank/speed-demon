use std::{
    cmp::Ordering,
    fs::File,
    io::{BufWriter, Write},
    os::fd::AsRawFd,
    path::Path,
    thread,
};

extern "C" {
    fn prctl(option: i32, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i32;
    fn mmap(a: *mut std::ffi::c_void, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut std::ffi::c_void;
    fn munmap(a: *mut std::ffi::c_void, l: usize) -> i32;
    fn madvise(a: *mut std::ffi::c_void, l: usize, advice: i32) -> i32;
}

const BLOOM_BITS: usize = 1 << 20; // 1M bits = 128 KB
const BLOOM_MASK: usize = BLOOM_BITS - 1;
const TABLE_SIZE: usize = 262144; // 256K slots
const TABLE_MASK: usize = TABLE_SIZE - 1;

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct TableSlot {
    w0: u64,
    w1: u64,
    tag: u32,
    id: u32,
    len: u8,
    last_byte: u8,
    _pad: [u8; 6],
}

struct TermTable {
    slots: Vec<TableSlot>,
    mask: usize,
    vocab_size: u32,
}

#[inline(always)]
fn hash_token(ptr: *const u8, len: usize) -> (u32, u64, u64, u8) {
    if len <= 8 {
        let mask = if len == 8 { u64::MAX } else { (1u64 << (len * 8)) - 1 };
        let raw = unsafe { (ptr as *const u64).read_unaligned() };
        let w0 = (raw | 0x2020202020202020u64) & mask;
        let h = w0.wrapping_mul(0x517cc1b727220a95u64) ^ (len as u64);
        let h32 = (h ^ (h >> 32)) as u32;
        (h32, w0, 0, 0)
    } else {
        let raw0 = unsafe { (ptr as *const u64).read_unaligned() };
        let w0 = raw0 | 0x2020202020202020u64;
        let rem_len = len - 8;
        let mask = if rem_len >= 8 { u64::MAX } else { (1u64 << (rem_len * 8)) - 1 };
        let raw1 = unsafe { (ptr.add(8) as *const u64).read_unaligned() };
        let w1 = (raw1 | 0x2020202020202020u64) & mask;
        let last_b = if len > 16 { unsafe { *ptr.add(16) | 0x20 } } else { 0 };
        let h = (w0.wrapping_mul(0x517cc1b727220a95u64) ^ w1.rotate_left(27) ^ (last_b as u64).rotate_left(43))
            .wrapping_mul(0x9e3779b97f4a7c15u64) ^ (len as u64);
        let h32 = (h ^ (h >> 32)) as u32;
        (h32, w0, w1, last_b)
    }
}

impl TermTable {
    fn with_capacity(cap: usize) -> Self {
        Self {
            slots: vec![TableSlot::default(); cap],
            mask: cap - 1,
            vocab_size: 1, // 0 = not found
        }
    }

    #[inline(always)]
    fn get_or_insert(&mut self, term: &[u8]) -> u32 {
        let mut buf = [0u8; 32];
        let len = term.len();
        buf[..len].copy_from_slice(term);
        let (tag, w0, w1, last_byte) = hash_token(buf.as_ptr(), len);
        let mut idx = (tag as usize) & self.mask;
        loop {
            let slot = unsafe { self.slots.get_unchecked(idx) };
            if slot.id == 0 {
                let id = self.vocab_size;
                self.vocab_size += 1;
                unsafe {
                    *self.slots.get_unchecked_mut(idx) = TableSlot {
                        w0, w1, tag, id, len: len as u8, last_byte, _pad: [0; 6]
                    };
                }
                return id;
            }
            if slot.tag == tag && slot.len as usize == len && slot.w0 == w0 && slot.w1 == w1 && slot.last_byte == last_byte {
                return slot.id;
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn lookup(&self, tag: u32, w0: u64, w1: u64, len: u8, last_byte: u8) -> u32 {
        let mut idx = (tag as usize) & self.mask;
        loop {
            let slot = unsafe { *self.slots.get_unchecked(idx) };
            if slot.id == 0 {
                return 0;
            }
            if slot.tag == tag && slot.len == len && slot.w0 == w0 && slot.w1 == w1 && slot.last_byte == last_byte {
                return slot.id;
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
            chunks: Vec::with_capacity(15_000),
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
        let mut chunk = PostingChunk { next: u32::MAX, len: d_len as u8, data: [0; 27] };
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
    fn decode_term_into(&self, term_id: u32, out_slice: &mut [u32]) {
        let tid = term_id as usize;
        let mut cur = self.heads[tid];
        let mut last = self.base_doc;
        let mut is_first = true;
        let mut out_idx = 0;

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
                    if b < 128 {
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
                out_slice[out_idx] = doc_id;
                out_idx += 1;
                last = doc_id;
            }
            cur = chunk.next;
        }
    }
}

#[inline(always)]
fn append_num(buf: &mut Vec<u8>, mut n: u64) {
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
fn fast_write_results(buf: &mut Vec<u8>, docs: &[u32]) {
    let count = docs.len() as u64;
    let mut sum = 0u64;
    for &d in docs {
        sum += d as u64;
    }
    append_num(buf, count);
    buf.push(b' ');
    append_num(buf, sum);
    let limit = docs.len().min(10);
    for i in 0..limit {
        buf.push(b' ');
        append_num(buf, docs[i] as u64);
    }
    buf.push(b'\n');
}

#[inline(always)]
fn intersect_sorted(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
        }
    }
}

#[inline(always)]
fn union_sorted(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
            Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
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
        match a[i].cmp(&b[j]) {
            Ordering::Equal => {
                i += 1;
                j += 1;
            }
            Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            Ordering::Greater => j += 1,
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

fn main() -> std::io::Result<()> {
    unsafe { prctl(41, 1, 0, 0, 0); }
    let args: Vec<String> = std::env::args().collect();
    let data_dir = if args.len() > 1 {
        args[1].clone()
    } else {
        std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string())
    };
    let out_dir = if args.len() > 2 {
        args[2].clone()
    } else {
        std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string())
    };
    let docs_path = Path::new(&data_dir).join("docs.txt");
    let queries_path = Path::new(&data_dir).join("queries.txt");
    let out_path = Path::new(&out_dir).join("results.txt");

    let docs_file = File::open(&docs_path)?;
    let file_len = docs_file.metadata()?.len() as usize;
    let mmap_ptr = unsafe {
        mmap(std::ptr::null_mut(), file_len, 1, 1, docs_file.as_raw_fd(), 0)
    };
    if mmap_ptr.is_null() || mmap_ptr == usize::MAX as *mut std::ffi::c_void {
        return Err(std::io::Error::last_os_error());
    }
    unsafe { madvise(mmap_ptr, file_len, 22); }
    let mmap_slice: &[u8] = unsafe { std::slice::from_raw_parts(mmap_ptr as *const u8, file_len) };

    let q_bytes = std::fs::read(&queries_path)?;
    let mut table = TermTable::with_capacity(TABLE_SIZE);
    let mut bloom = vec![0u64; BLOOM_BITS / 64];
    let mut phrase_index = PhraseIndex::with_capacity(65536);
    let mut pair_bitset = vec![0u64; 1024];
    let mut parsed_queries = Vec::with_capacity(50000);
    let mut num_phrases = 0u32;

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
                let (h, _, _, _) = hash_token(first.as_ptr(), first.len());
                let b_idx = (h as usize) & BLOOM_MASK;
                bloom[b_idx >> 6] |= 1u64 << (b_idx & 63);

                let mut ops = Vec::new();
                while let Some(op_bytes) = parts.next() {
                    if let Some(term_bytes) = parts.next() {
                        let op_code = match op_bytes {
                            b"OR" => 1,
                            b"NOT" => 2,
                            _ => 0,
                        };
                        let tid = table.get_or_insert(term_bytes);
                        let (th, _, _, _) = hash_token(term_bytes.as_ptr(), term_bytes.len());
                        let tb_idx = (th as usize) & BLOOM_MASK;
                        bloom[tb_idx >> 6] |= 1u64 << (tb_idx & 63);
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
            let mut terms = Vec::new();
            for word in inner.split(|&b| b == b' ') {
                if !word.is_empty() {
                    let tid = table.get_or_insert(word);
                    let (h, _, _, _) = hash_token(word.as_ptr(), word.len());
                    let b_idx = (h as usize) & BLOOM_MASK;
                    bloom[b_idx >> 6] |= 1u64 << (b_idx & 63);
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

    let vocab_size = table.vocab_size as usize;
    let np = num_phrases as usize;

    // Fast sampling to detect dense terms
    let mut sampled_counts = vec![0u16; vocab_size];
    let mut sample_p = 0;
    while sample_p < file_len {
        let mut did = 0u32;
        while sample_p < file_len && mmap_slice[sample_p] >= b'0' && mmap_slice[sample_p] <= b'9' {
            did = did * 10 + (mmap_slice[sample_p] - b'0') as u32;
            sample_p += 1;
        }
        while sample_p < file_len && mmap_slice[sample_p] != b'\t' && mmap_slice[sample_p] != b'\n' {
            sample_p += 1;
        }
        if sample_p < file_len && mmap_slice[sample_p] == b'\t' {
            sample_p += 1;
        }
        if did % 32 == 0 {
            let mut last_seen = 0u32;
            while sample_p < file_len && mmap_slice[sample_p] != b'\n' {
                if mmap_slice[sample_p].is_ascii_alphanumeric() {
                    let start_t = sample_p;
                    while sample_p < file_len && mmap_slice[sample_p].is_ascii_alphanumeric() {
                        sample_p += 1;
                    }
                    let count = sample_p - start_t;
                    let (h, w0, w1, last_b) = hash_token(mmap_slice.as_ptr().wrapping_add(start_t), count);
                    let b_idx = (h as usize) & BLOOM_MASK;
                    if ((bloom[b_idx >> 6] >> (b_idx & 63)) & 1) != 0 {
                        let tid = table.lookup(h, w0, w1, count as u8, last_b);
                        if tid > 0 && tid != last_seen {
                            sampled_counts[tid as usize] = sampled_counts[tid as usize].saturating_add(1);
                            last_seen = tid;
                        }
                    }
                } else {
                    sample_p += 1;
                }
            }
        } else {
            while sample_p < file_len && mmap_slice[sample_p] != b'\n' {
                sample_p += 1;
            }
        }
        if sample_p < file_len && mmap_slice[sample_p] == b'\n' {
            sample_p += 1;
        }
    }

    let mut dense_map = vec![u32::MAX; vocab_size];
    let mut num_dense = 0u32;
    for t in 1..vocab_size {
        if sampled_counts[t] >= 120 {
            dense_map[t] = num_dense;
            num_dense += 1;
        }
    }

    // Find max doc id
    let mut end_p = file_len;
    while end_p > 0 && mmap_slice[end_p - 1] == b'\n' { end_p -= 1; }
    let mut last_line_start = end_p;
    while last_line_start > 0 && mmap_slice[last_line_start - 1] != b'\n' { last_line_start -= 1; }
    let mut max_doc_id = 0u32;
    let mut lp = last_line_start;
    while lp < end_p && mmap_slice[lp] >= b'0' && mmap_slice[lp] <= b'9' {
        max_doc_id = max_doc_id * 10 + (mmap_slice[lp] - b'0') as u32;
        lp += 1;
    }
    #[derive(Copy, Clone)]
    struct BitmapPtr(*mut u64);
    unsafe impl Send for BitmapPtr {}
    unsafe impl Sync for BitmapPtr {}

    let num_words = ((max_doc_id as usize) + 64) / 64;
    let mut dense_bitmaps: Vec<Vec<u64>> = vec![vec![0u64; num_words]; num_dense as usize];
    let dense_bitmaps_ptrs: Vec<BitmapPtr> = dense_bitmaps.iter_mut().map(|bm| BitmapPtr(bm.as_mut_ptr())).collect();
    let dense_bitmaps_ptrs_ref = &dense_bitmaps_ptrs;

    let num_workers = 12;
    let approx_chunk = file_len / num_workers;
    let mut chunk_bounds = Vec::with_capacity(num_workers + 1);
    chunk_bounds.push(0);
    for i in 1..num_workers {
        let mut pos = (i * approx_chunk).min(file_len);
        while pos < file_len && mmap_slice[pos] != b'\n' {
            pos += 1;
        }
        if pos < file_len {
            pos += 1;
        }
        // align to doc_id % 64 == 0
        loop {
            if pos >= file_len { break; }
            let mut dp = pos;
            let mut did = 0u32;
            while dp < file_len && mmap_slice[dp] >= b'0' && mmap_slice[dp] <= b'9' {
                did = did * 10 + (mmap_slice[dp] - b'0') as u32;
                dp += 1;
            }
            if did % 64 == 0 {
                break;
            }
            while pos < file_len && mmap_slice[pos] != b'\n' {
                pos += 1;
            }
            if pos < file_len {
                pos += 1;
            }
        }
        chunk_bounds.push(pos.min(file_len));
    }
    chunk_bounds.push(file_len);

    let mut worker_arenas: Vec<WorkerArena> = Vec::with_capacity(num_workers);

    thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_workers);
        for w in 0..num_workers {
            let (start, end) = (chunk_bounds[w], chunk_bounds[w + 1]);
            let slice = &mmap_slice[start..end];
            let table_ref = &table;
            let bloom_ref = &bloom;
            let phrase_idx_ref = &phrase_index;
            let pair_bitset_ref = &pair_bitset;
            let dense_map_ref = &dense_map;

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
                let mut doc_tokens = Vec::with_capacity(8192);
                let mut last_seen_phrase = vec![u32::MAX; np];

                while p < len {
                    let mut did = 0u32;
                    while p < len && slice[p] >= b'0' && slice[p] <= b'9' {
                        did = did * 10 + (slice[p] - b'0') as u32;
                        p += 1;
                    }
                    let doc_id = did;
                    while p < len && slice[p] != b'\t' && slice[p] != b'\n' {
                        p += 1;
                    }
                    if p < len && slice[p] == b'\t' {
                        p += 1;
                    }

                    doc_tokens.clear();

                    while p < len && slice[p] != b'\n' {
                        if slice[p].is_ascii_alphanumeric() {
                            let start_t = p;
                            while p < len && slice[p].is_ascii_alphanumeric() {
                                p += 1;
                            }
                            let count = p - start_t;
                            let (h, w0, w1, last_b) = hash_token(slice.as_ptr().wrapping_add(start_t), count);
                            let b_idx = (h as usize) & BLOOM_MASK;
                            let mut tid = 0u32;
                            if ((bloom_ref[b_idx >> 6] >> (b_idx & 63)) & 1) != 0 {
                                tid = table_ref.lookup(h, w0, w1, count as u8, last_b);
                            }
                            doc_tokens.push(tid);

                            if tid > 0 {
                                let d_id = dense_map_ref[tid as usize];
                                if d_id != u32::MAX {
                                    let bm_ptr = dense_bitmaps_ptrs_ref[d_id as usize].0;
                                    unsafe {
                                        *bm_ptr.add((doc_id >> 6) as usize) |= 1u64 << (doc_id & 63);
                                    }
                                } else {
                                    arena.add_posting(tid, doc_id);
                                }
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
                                            let item = unsafe {
                                                *phrase_idx_ref.items.get_unchecked(head as usize)
                                            };
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
                    if p < len && slice[p] == b'\n' {
                        p += 1;
                    }
                }

                arena
            }));
        }
        for h in handles {
            worker_arenas.push(h.join().unwrap());
        }
    });

    let mut term_offsets: Vec<usize> = Vec::with_capacity(vocab_size + 1);
    term_offsets.push(0);
    let mut total_postings = 0usize;
    for t in 0..vocab_size {
        let mut term_len = 0usize;
        if dense_map[t] == u32::MAX {
            for w in 0..num_workers {
                term_len += worker_arenas[w].term_counts[t] as usize;
            }
        }
        total_postings += term_len;
        term_offsets.push(total_postings);
    }

    let mut all_postings = vec![0u32; total_postings];
    let all_postings_ptr = all_postings.as_mut_ptr() as usize;

    let mut cur_offsets: Vec<Vec<usize>> = vec![vec![0usize; vocab_size]; num_workers];
    for t in 0..vocab_size {
        let mut cur = term_offsets[t];
        for w in 0..num_workers {
            cur_offsets[w][t] = cur;
            if dense_map[t] == u32::MAX {
                cur += worker_arenas[w].term_counts[t] as usize;
            }
        }
    }

    thread::scope(|s| {
        for (w, arena) in worker_arenas.iter().enumerate() {
            let offsets_w = &cur_offsets[w];
            let dense_map_ref = &dense_map;
            s.spawn(move || {
                let out_ptr = all_postings_ptr as *mut u32;
                for t in 1..vocab_size as u32 {
                    if dense_map_ref[t as usize] == u32::MAX {
                        let cnt = arena.term_counts[t as usize] as usize;
                        if cnt > 0 {
                            let start = offsets_w[t as usize];
                            let slice = unsafe { std::slice::from_raw_parts_mut(out_ptr.add(start), cnt) };
                            arena.decode_term_into(t, slice);
                        }
                    }
                }
            });
        }
    });

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

    drop(worker_arenas);

    let num_queries = parsed_queries.len();
    let chunk_size = (num_queries + num_workers - 1) / num_workers;
    let mut thread_outputs: Vec<Vec<u8>> = (0..num_workers).map(|_| Vec::new()).collect();

    thread::scope(|s| {
        let queries_ref = &parsed_queries;
        let postings_ref = &all_postings;
        let offsets_ref = &term_offsets;
        let p_matches = &all_phrase_matches;
        let p_offsets = &phrase_offsets;
        let dense_map_ref = &dense_map;
        let dense_bitmaps_ref = &dense_bitmaps;

        for (w, out_buf) in thread_outputs.iter_mut().enumerate() {
            let (start, end) = (
                w * chunk_size,
                (w * chunk_size + chunk_size).min(num_queries),
            );
            if start >= end {
                continue;
            }

            s.spawn(move || {
                let get_postings = |tid: u32| -> &[u32] {
                    let t = tid as usize;
                    &postings_ref[offsets_ref[t]..offsets_ref[t + 1]]
                };
                let mut scratch_bm = vec![0u64; num_words];
                let (mut cur_res, mut next_res) = (Vec::new(), Vec::new());

                for q_idx in start..end {
                    match &queries_ref[q_idx] {
                        ParsedQuery::Bool { first_term, ops } => {
                            let f_dense = dense_map_ref[*first_term as usize];
                            let has_dense = f_dense != u32::MAX || ops.iter().any(|&(_, tid)| dense_map_ref[tid as usize] != u32::MAX);

                            if !has_dense {
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
                            } else {
                                // Bitmap path
                                if f_dense != u32::MAX {
                                    scratch_bm.copy_from_slice(&dense_bitmaps_ref[f_dense as usize]);
                                } else {
                                    scratch_bm.fill(0);
                                    for &doc_id in get_postings(*first_term) {
                                        scratch_bm[(doc_id >> 6) as usize] |= 1u64 << (doc_id & 63);
                                    }
                                }

                                for &(op, tid) in ops {
                                    let t_dense = dense_map_ref[tid as usize];
                                    if t_dense != u32::MAX {
                                        let op_bm = &dense_bitmaps_ref[t_dense as usize];
                                        match op {
                                            0 => for k in 0..num_words { scratch_bm[k] &= op_bm[k]; },
                                            1 => for k in 0..num_words { scratch_bm[k] |= op_bm[k]; },
                                            2 => for k in 0..num_words { scratch_bm[k] &= !op_bm[k]; },
                                            _ => {}
                                        }
                                    } else {
                                        let p = get_postings(tid);
                                        match op {
                                            0 => {
                                                // AND with sparse
                                                cur_res.clear();
                                                for &doc_id in p {
                                                    let word = (doc_id >> 6) as usize;
                                                    let bit = 1u64 << (doc_id & 63);
                                                    if (scratch_bm[word] & bit) != 0 {
                                                        cur_res.push(doc_id);
                                                    }
                                                }
                                                scratch_bm.fill(0);
                                                for &doc_id in &cur_res {
                                                    scratch_bm[(doc_id >> 6) as usize] |= 1u64 << (doc_id & 63);
                                                }
                                            }
                                            1 => {
                                                for &doc_id in p {
                                                    scratch_bm[(doc_id >> 6) as usize] |= 1u64 << (doc_id & 63);
                                                }
                                            }
                                            2 => {
                                                for &doc_id in p {
                                                    scratch_bm[(doc_id >> 6) as usize] &= !(1u64 << (doc_id & 63));
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }

                                let mut count = 0usize;
                                let mut sum = 0u64;
                                let mut first_10 = [0u32; 10];
                                let mut n_first = 0;

                                for (w_idx, &w) in scratch_bm.iter().enumerate() {
                                    if w == 0 { continue; }
                                    count += w.count_ones() as usize;
                                    let mut bits = w;
                                    let base_id = (w_idx * 64) as u64;
                                    while bits != 0 {
                                        let bit = bits.trailing_zeros();
                                        let doc_id = (base_id + bit as u64) as u32;
                                        sum += doc_id as u64;
                                        if n_first < 10 {
                                            first_10[n_first] = doc_id;
                                            n_first += 1;
                                        }
                                        bits &= bits - 1;
                                    }
                                }

                                append_num(out_buf, count as u64);
                                out_buf.push(b' ');
                                append_num(out_buf, sum);
                                for i in 0..n_first {
                                    out_buf.push(b' ');
                                    append_num(out_buf, first_10[i] as u64);
                                }
                                out_buf.push(b'\n');
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

    let mut out_file = BufWriter::with_capacity(1024 * 1024, File::create(&out_path)?);
    for buf in &thread_outputs {
        out_file.write_all(buf)?;
    }
    out_file.flush()?;

    unsafe {
        munmap(mmap_ptr, file_len);
    }
    Ok(())
}
