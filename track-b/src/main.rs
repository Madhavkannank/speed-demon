use std::arch::x86_64::*;
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
    fn mmap(
        a: *mut std::ffi::c_void,
        l: usize,
        p: i32,
        f: i32,
        fd: i32,
        o: i64,
    ) -> *mut std::ffi::c_void;
    fn munmap(a: *mut std::ffi::c_void, l: usize) -> i32;
    fn madvise(a: *mut std::ffi::c_void, l: usize, advice: i32) -> i32;
}

const BLOOM_BITS: usize = 1 << 21;
const BLOOM_MASK: usize = BLOOM_BITS - 1;
const TABLE_SIZE: usize = 262144;

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
        let mask = if len == 8 {
            u64::MAX
        } else {
            (1u64 << (len * 8)) - 1
        };
        let raw = unsafe { (ptr as *const u64).read_unaligned() };
        let w0 = (raw | 0x2020202020202020u64) & mask;
        let h = w0.wrapping_mul(0x517cc1b727220a95u64) ^ (len as u64);
        let h32 = (h ^ (h >> 32)) as u32;
        (h32, w0, 0, 0)
    } else {
        let raw0 = unsafe { (ptr as *const u64).read_unaligned() };
        let w0 = raw0 | 0x2020202020202020u64;
        let rem_len = len - 8;
        let mask = if rem_len >= 8 {
            u64::MAX
        } else {
            (1u64 << (rem_len * 8)) - 1
        };
        let raw1 = unsafe { (ptr.add(8) as *const u64).read_unaligned() };
        let w1 = (raw1 | 0x2020202020202020u64) & mask;
        let last_b = if len > 16 {
            unsafe { *ptr.add(16) | 0x20 }
        } else {
            0
        };
        let h = (w0.wrapping_mul(0x517cc1b727220a95u64)
            ^ w1.rotate_left(27)
            ^ (last_b as u64).rotate_left(43))
        .wrapping_mul(0x9e3779b97f4a7c15u64)
            ^ (len as u64);
        let h32 = (h ^ (h >> 32)) as u32;
        (h32, w0, w1, last_b)
    }
}

impl TermTable {
    fn with_capacity(cap: usize) -> Self {
        Self {
            slots: vec![TableSlot::default(); cap],
            mask: cap - 1,
            vocab_size: 1,
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
                        w0,
                        w1,
                        tag,
                        id,
                        len: len as u8,
                        last_byte,
                        _pad: [0; 6],
                    };
                }
                return id;
            }
            if slot.tag == tag
                && slot.len as usize == len
                && slot.w0 == w0
                && slot.w1 == w1
                && slot.last_byte == last_byte
            {
                return slot.id;
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn lookup(&self, tag: u32, w0: u64, w1: u64, len: u8, last_byte: u8) -> u32 {
        let mut idx = (tag as usize) & self.mask;
        loop {
            let slot = unsafe { self.slots.get_unchecked(idx) };
            if slot.id == 0 {
                return 0;
            }
            if slot.tag == tag
                && slot.len == len
                && slot.w0 == w0
                && slot.w1 == w1
                && slot.last_byte == last_byte
            {
                return slot.id;
            }
            idx = (idx + 1) & self.mask;
        }
    }
}

#[derive(Clone, Copy)]
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
    let mut h = k.wrapping_mul(0x517cc1b727220a95);
    h ^= h >> 32;
    h as usize
}

impl PhraseIndex {
    fn with_capacity(cap: usize) -> Self {
        Self {
            slots: vec![
                PairSlot {
                    key: u64::MAX,
                    head: u32::MAX
                };
                cap
            ],
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
                    *self.slots.get_unchecked_mut(slot_idx) = PairSlot {
                        key: pair_key,
                        head: item_idx,
                    };
                }
                self.items.push(PhraseItem {
                    phrase_id,
                    len: k as u8,
                    terms_offset,
                    next: u32::MAX,
                });
                return;
            }
            if slot.key == pair_key {
                let old_head = slot.head;
                unsafe {
                    self.slots.get_unchecked_mut(slot_idx).head = item_idx;
                }
                self.items.push(PhraseItem {
                    phrase_id,
                    len: k as u8,
                    terms_offset,
                    next: old_head,
                });
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

#[inline(always)]
fn append_num(buf: &mut Vec<u8>, mut n: u64) {
    if n == 0 {
        buf.push(b"0"[0]);
        return;
    }
    let mut b = [0u8; 20];
    let mut i = 20;
    while n > 0 {
        i -= 1;
        b[i] = b"0"[0] + (n % 10) as u8;
        n /= 10;
    }
    buf.extend_from_slice(&b[i..]);
}

#[inline(always)]
fn fast_write_results(out: &mut Vec<u8>, doc_ids: &[u32]) {
    let count = doc_ids.len();
    if count == 0 {
        out.extend_from_slice(b"0 0\n");
        return;
    }
    let mut sum: u64 = 0;
    for &d in doc_ids {
        sum += d as u64;
    }
    append_num(out, count as u64);
    out.push(b" "[0]);
    append_num(out, sum);
    let limit = count.min(10);
    for i in 0..limit {
        out.push(b" "[0]);
        append_num(out, doc_ids[i] as u64);
    }
    out.push(b"\n"[0]);
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

struct WorkerState {
    term_counts: Vec<u32>,
    postings: Vec<u64>,
    phrase_matches: Vec<Vec<u32>>,
}

fn main() -> std::io::Result<()> {
    unsafe {
        prctl(41, 1, 0, 0, 0);
    }
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
        mmap(
            std::ptr::null_mut(),
            file_len,
            1,
            2,
            docs_file.as_raw_fd(),
            0,
        )
    };
    if mmap_ptr.is_null() || mmap_ptr == usize::MAX as *mut std::ffi::c_void {
        return Err(std::io::Error::last_os_error());
    }
    unsafe {
        madvise(mmap_ptr, file_len, 22);
    }
    let mmap_slice: &[u8] = unsafe { std::slice::from_raw_parts(mmap_ptr as *const u8, file_len) };

    let q_bytes = std::fs::read(&queries_path)?;
    let mut table = TermTable::with_capacity(TABLE_SIZE);
    let mut bloom = vec![0u64; BLOOM_BITS / 64];
    let mut phrase_index = PhraseIndex::with_capacity(65536);
    let mut pair_bitset = vec![0u64; 16384];
    let mut parsed_queries = Vec::with_capacity(50000);
    let mut num_phrases = 0u32;

    let mut p = 0;
    let q_len = q_bytes.len();
    while p < q_len {
        let line_start = p;
        while p < q_len && q_bytes[p] != b"\n"[0] {
            p += 1;
        }
        let line = &q_bytes[line_start..p];
        if p < q_len && q_bytes[p] == b"\n"[0] {
            p += 1;
        }
        if line.is_empty() {
            continue;
        }

        if line.starts_with(b"BOOL ") {
            let rest = &line[5..];
            let mut parts = rest.split(|&b| b == b" "[0]);
            if let Some(first) = parts.next() {
                let first_id = table.get_or_insert(first);
                let (h, _, _, _) = hash_token(first.as_ptr(), first.len());
                let b1 = (h as usize) & BLOOM_MASK;
                let b2 = (h.rotate_left(31) as usize) & BLOOM_MASK;
                bloom[b1 >> 6] |= 1u64 << (b1 & 63);
                bloom[b2 >> 6] |= 1u64 << (b2 & 63);

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
                        let b1 = (th as usize) & BLOOM_MASK;
                        let b2 = (th.rotate_left(31) as usize) & BLOOM_MASK;
                        bloom[b1 >> 6] |= 1u64 << (b1 & 63);
                        bloom[b2 >> 6] |= 1u64 << (b2 & 63);
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
            for word in inner.split(|&b| b == b" "[0]) {
                if !word.is_empty() {
                    let tid = table.get_or_insert(word);
                    let (h, _, _, _) = hash_token(word.as_ptr(), word.len());
                    let b1 = (h as usize) & BLOOM_MASK;
                    let b2 = (h.rotate_left(31) as usize) & BLOOM_MASK;
                    bloom[b1 >> 6] |= 1u64 << (b1 & 63);
                    bloom[b2 >> 6] |= 1u64 << (b2 & 63);
                    terms.push(tid);
                }
            }
            if terms.len() >= 2 {
                let ph_id = num_phrases;
                num_phrases += 1;
                phrase_index.insert(ph_id, &terms);
                let k = terms.len();
                let pair_key = ((terms[k - 2] as u64) << 32) | (terms[k - 1] as u64);
                let ph_bit = (hash_pair(pair_key) & 0xF_FFFF) as usize;
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
    let num_workers = 12;
    let approx_chunk = file_len / num_workers;
    let mut chunk_bounds = Vec::with_capacity(num_workers + 1);
    chunk_bounds.push(0);
    for i in 1..num_workers {
        let mut pos = (i * approx_chunk).min(file_len);
        while pos < file_len && mmap_slice[pos] != b"\n"[0] {
            pos += 1;
        }
        if pos < file_len {
            pos += 1;
        }
        loop {
            if pos >= file_len {
                break;
            }
            let mut dp = pos;
            let mut did = 0u32;
            while dp < file_len && mmap_slice[dp] >= b"0"[0] && mmap_slice[dp] <= b"9"[0] {
                did = did * 10 + (mmap_slice[dp] - b"0"[0]) as u32;
                dp += 1;
            }
            if did % 64 == 0 {
                break;
            }
            while pos < file_len && mmap_slice[pos] != b"\n"[0] {
                pos += 1;
            }
            if pos < file_len {
                pos += 1;
            }
        }
        chunk_bounds.push(pos.min(file_len));
    }
    chunk_bounds.push(file_len);

    // Parallel fast sampling (1 in 32 docs) to detect dense terms in ~2ms
    let mut sampled_counts = vec![0u16; vocab_size];
    thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_workers);
        for w in 0..num_workers {
            let (start, end) = (chunk_bounds[w], chunk_bounds[w + 1]);
            let slice = &mmap_slice[start..end];
            let table_ref = &table;
            let bloom_ref = &bloom;
            handles.push(s.spawn(move || {
                let mut local_counts = vec![0u16; vocab_size];
                let mut p = 0;
                let len = slice.len();
                while p < len {
                    let mut did = 0u32;
                    while p < len && slice[p] >= b"0"[0] && slice[p] <= b"9"[0] {
                        did = did * 10 + (slice[p] - b"0"[0]) as u32;
                        p += 1;
                    }
                    while p < len && slice[p] != b"\t"[0] && slice[p] != b"\n"[0] {
                        p += 1;
                    }
                    if p < len && slice[p] == b"\t"[0] {
                        p += 1;
                    }
                    if did % 64 == 0 {
                        let mut last_seen = 0u32;
                        while p < len && slice[p] != b"\n"[0] {
                            if slice[p].is_ascii_alphanumeric() {
                                let start_t = p;
                                while p < len && slice[p].is_ascii_alphanumeric() {
                                    p += 1;
                                }
                                let count = p - start_t;
                                let (h, w0, w1, last_b) =
                                    hash_token(slice.as_ptr().wrapping_add(start_t), count);
                                let b_idx = (h as usize) & BLOOM_MASK;
                                if ((bloom_ref[b_idx >> 6] >> (b_idx & 63)) & 1) != 0 {
                                    let tid = table_ref.lookup(h, w0, w1, count as u8, last_b);
                                    if tid > 0 && tid != last_seen {
                                        local_counts[tid as usize] =
                                            local_counts[tid as usize].saturating_add(1);
                                        last_seen = tid;
                                    }
                                }
                            } else {
                                p += 1;
                            }
                        }
                    } else {
                        while p < len && slice[p] != b"\n"[0] {
                            p += 1;
                        }
                    }
                    if p < len && slice[p] == b"\n"[0] {
                        p += 1;
                    }
                }
                local_counts
            }));
        }
        for h in handles {
            let lc = h.join().unwrap();
            for t in 1..vocab_size {
                sampled_counts[t] = sampled_counts[t].saturating_add(lc[t]);
            }
        }
    });

    let mut dense_map = vec![u32::MAX; vocab_size];

    let mut num_dense = 0u32;
    for t in 1..vocab_size {
        if sampled_counts[t] >= 60 {
            dense_map[t] = num_dense;
            num_dense += 1;
        }
    }

    let mut end_p = file_len;
    while end_p > 0 && mmap_slice[end_p - 1] == b"\n"[0] {
        end_p -= 1;
    }
    let mut last_line_start = end_p;
    while last_line_start > 0 && mmap_slice[last_line_start - 1] != b"\n"[0] {
        last_line_start -= 1;
    }
    let mut max_doc_id = 0u32;
    let mut lp = last_line_start;
    while lp < end_p && mmap_slice[lp] >= b"0"[0] && mmap_slice[lp] <= b"9"[0] {
        max_doc_id = max_doc_id * 10 + (mmap_slice[lp] - b"0"[0]) as u32;
        lp += 1;
    }
    #[derive(Copy, Clone)]
    struct BitmapPtr(*mut u64);
    unsafe impl Send for BitmapPtr {}
    unsafe impl Sync for BitmapPtr {}

    let num_words = ((max_doc_id as usize) + 64) / 64;
    let mut dense_bitmaps: Vec<Vec<u64>> = vec![vec![0u64; num_words]; num_dense as usize];
    let dense_bitmaps_ptrs: Vec<BitmapPtr> = dense_bitmaps
        .iter_mut()
        .map(|bm| BitmapPtr(bm.as_mut_ptr()))
        .collect();
    let dense_bitmaps_ptrs_ref = &dense_bitmaps_ptrs;

    let mut worker_states: Vec<WorkerState> = Vec::with_capacity(num_workers);

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

            handles.push(s.spawn(move || unsafe {
                let mut p = 0;
                let len = slice.len();
                let p_ptr = slice.as_ptr();

                let mut term_counts = vec![0u32; vocab_size];
                let mut last_doc = vec![u32::MAX; vocab_size];
                let mut postings = Vec::with_capacity(760_000);
                let mut phrase_matches: Vec<Vec<u32>> = (0..np).map(|_| Vec::new()).collect();
                let mut last_seen_phrase = vec![u32::MAX; np];

                let v_20 = _mm256_set1_epi8(0x20);
                let v_a_minus_1 = _mm256_set1_epi8((b"a"[0] - 1) as i8);
                let v_z_plus_1 = _mm256_set1_epi8((b"z"[0] + 1) as i8);
                let v_0_minus_1 = _mm256_set1_epi8((b"0"[0] - 1) as i8);
                let v_9_plus_1 = _mm256_set1_epi8((b"9"[0] + 1) as i8);
                let v_nl = _mm256_set1_epi8(b"\n"[0] as i8);

                let mut doc_tokens = [0u32; 2048];

                while p < len {
                    let mut did = 0u32;
                    while p < len && *p_ptr.add(p) >= b"0"[0] && *p_ptr.add(p) <= b"9"[0] {
                        did = did * 10 + (*p_ptr.add(p) - b"0"[0]) as u32;
                        p += 1;
                    }
                    let doc_id = did;
                    while p < len && *p_ptr.add(p) != b"\t"[0] && *p_ptr.add(p) != b"\n"[0] {
                        p += 1;
                    }
                    if p < len && *p_ptr.add(p) == b"\t"[0] {
                        p += 1;
                    }

                    let mut dt_len = 0usize;
                    let mut prev_is_alnum = 0u32;
                    let mut doc_ended = false;

                    while p + 32 <= len {
                        let v = _mm256_loadu_si256(p_ptr.add(p) as *const __m256i);
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
                            mask &= (1u32 << nl_off).wrapping_sub(1);
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
                                while p + off + l < len
                                    && (*p_ptr.add(p + off + l)).is_ascii_alphanumeric()
                                {
                                    l += 1;
                                }
                                spanned = true;
                                l
                            };

                            let (h, w0, w1, last_b) = hash_token(p_ptr.add(tok_start), tok_len);
                            let b1 = (h as usize) & BLOOM_MASK;
                            let b2 = (h.rotate_left(31) as usize) & BLOOM_MASK;
                            let mut tid = 0u32;
                            if (((bloom_ref[b1 >> 6] >> (b1 & 63))
                                & (bloom_ref[b2 >> 6] >> (b2 & 63)))
                                & 1)
                                != 0
                            {
                                tid = table_ref.lookup(h, w0, w1, tok_len as u8, last_b);
                            }
                            if tid != 0 {
                                if dt_len < 2048 {
                                    doc_tokens[dt_len] = tid;
                                    dt_len += 1;
                                }
                            } else if dt_len > 0 && doc_tokens[dt_len - 1] != 0 {
                                doc_tokens[dt_len] = 0;
                                dt_len += 1;
                            }

                            if tid > 0 {
                                let d_id = dense_map_ref[tid as usize];
                                if d_id != u32::MAX {
                                    let bm_ptr = dense_bitmaps_ptrs_ref[d_id as usize].0;
                                    *bm_ptr.add((doc_id >> 6) as usize) |= 1u64 << (doc_id & 63);
                                } else {
                                    let tid_idx = tid as usize;
                                    if last_doc[tid_idx] != doc_id {
                                        last_doc[tid_idx] = doc_id;
                                        term_counts[tid_idx] += 1;
                                        postings.push(((tid as u64) << 32) | (doc_id as u64));
                                    }
                                }
                            }

                            if dt_len >= 2 && tid > 0 {
                                let prev_tid = doc_tokens[dt_len - 2];
                                if prev_tid > 0 {
                                    let pair_key = ((prev_tid as u64) << 32) | (tid as u64);
                                    let ph_bit = (hash_pair(pair_key) & 0xF_FFFF) as usize;
                                    if ((pair_bitset_ref[ph_bit >> 6] >> (ph_bit & 63)) & 1) != 0 {
                                        let mut head = phrase_idx_ref.get_head(pair_key);
                                        while head != u32::MAX {
                                            let item =
                                                *phrase_idx_ref.items.get_unchecked(head as usize);
                                            let plen = item.len as usize;
                                            if dt_len >= plen {
                                                let target_slice = &phrase_idx_ref.terms[item
                                                    .terms_offset
                                                    as usize
                                                    ..item.terms_offset as usize + plen];
                                                let candidate_slice =
                                                    &doc_tokens[dt_len - plen..dt_len];
                                                if candidate_slice == target_slice {
                                                    let ph_idx = item.phrase_id as usize;
                                                    if last_seen_phrase[ph_idx] != doc_id {
                                                        last_seen_phrase[ph_idx] = doc_id;
                                                        phrase_matches[ph_idx].push(doc_id);
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
                        while p < len && *p_ptr.add(p) != b"\n"[0] {
                            if (*p_ptr.add(p)).is_ascii_alphanumeric() {
                                let start_t = p;
                                while p < len && (*p_ptr.add(p)).is_ascii_alphanumeric() {
                                    p += 1;
                                }
                                let count = p - start_t;
                                let (h, w0, w1, last_b) = hash_token(p_ptr.add(start_t), count);
                                let b1 = (h as usize) & BLOOM_MASK;
                                let b2 = (h.rotate_left(31) as usize) & BLOOM_MASK;
                                let mut tid = 0u32;
                                if (((bloom_ref[b1 >> 6] >> (b1 & 63))
                                    & (bloom_ref[b2 >> 6] >> (b2 & 63)))
                                    & 1)
                                    != 0
                                {
                                    tid = table_ref.lookup(h, w0, w1, count as u8, last_b);
                                }
                                if dt_len < 2048 {
                                    doc_tokens[dt_len] = tid;
                                    dt_len += 1;
                                }

                                if tid > 0 {
                                    let d_id = dense_map_ref[tid as usize];
                                    if d_id != u32::MAX {
                                        let bm_ptr = dense_bitmaps_ptrs_ref[d_id as usize].0;
                                        *bm_ptr.add((doc_id >> 6) as usize) |=
                                            1u64 << (doc_id & 63);
                                    } else {
                                        let tid_idx = tid as usize;
                                        if last_doc[tid_idx] != doc_id {
                                            last_doc[tid_idx] = doc_id;
                                            term_counts[tid_idx] += 1;
                                            postings.push(((tid as u64) << 32) | (doc_id as u64));
                                        }
                                    }
                                }

                                if dt_len >= 2 && tid > 0 {
                                    let prev_tid = doc_tokens[dt_len - 2];
                                    if prev_tid > 0 {
                                        let pair_key = ((prev_tid as u64) << 32) | (tid as u64);
                                        let ph_bit = (hash_pair(pair_key) & 0xF_FFFF) as usize;
                                        if ((pair_bitset_ref[ph_bit >> 6] >> (ph_bit & 63)) & 1)
                                            != 0
                                        {
                                            let mut head = phrase_idx_ref.get_head(pair_key);
                                            while head != u32::MAX {
                                                let item = *phrase_idx_ref
                                                    .items
                                                    .get_unchecked(head as usize);
                                                let plen = item.len as usize;
                                                if dt_len >= plen {
                                                    let target_slice = &phrase_idx_ref.terms[item
                                                        .terms_offset
                                                        as usize
                                                        ..item.terms_offset as usize + plen];
                                                    let candidate_slice =
                                                        &doc_tokens[dt_len - plen..dt_len];
                                                    if candidate_slice == target_slice {
                                                        let ph_idx = item.phrase_id as usize;
                                                        if last_seen_phrase[ph_idx] != doc_id {
                                                            last_seen_phrase[ph_idx] = doc_id;
                                                            phrase_matches[ph_idx].push(doc_id);
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
                        if p < len && *p_ptr.add(p) == b"\n"[0] {
                            p += 1;
                        }
                    }
                }

                WorkerState {
                    term_counts,
                    postings,
                    phrase_matches,
                }
            }));
        }
        for h in handles {
            worker_states.push(h.join().unwrap());
        }
    });

    // Fast parallel merge of rare postings using prefix sums

    let mut term_offsets: Vec<usize> = Vec::with_capacity(vocab_size + 1);

    term_offsets.push(0);
    let mut total_postings = 0usize;
    for t in 0..vocab_size {
        let mut term_len = 0usize;
        if dense_map[t] == u32::MAX {
            for w in 0..num_workers {
                term_len += worker_states[w].term_counts[t] as usize;
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
                cur += worker_states[w].term_counts[t] as usize;
            }
        }
    }

    thread::scope(|s| {
        for (w, ws) in worker_states.iter().enumerate() {
            let mut w_offsets = cur_offsets[w].clone();
            s.spawn(move || unsafe {
                let out_ptr = all_postings_ptr as *mut u32;
                for &pair in &ws.postings {
                    let tid = (pair >> 32) as usize;
                    let doc_id = pair as u32;
                    let idx = w_offsets[tid];
                    *out_ptr.add(idx) = doc_id;
                    w_offsets[tid] = idx + 1;
                }
            });
        }
    });

    let mut phrase_offsets = vec![0u32; np + 1];
    for w in &worker_states {
        for (ph_id, list) in w.phrase_matches.iter().enumerate() {
            phrase_offsets[ph_id + 1] += list.len() as u32;
        }
    }
    for i in 0..np {
        phrase_offsets[i + 1] += phrase_offsets[i];
    }
    let mut all_phrase_matches = vec![0u32; phrase_offsets[np] as usize];
    let mut cur_phrase = phrase_offsets.clone();
    for w in &worker_states {
        for (ph_id, list) in w.phrase_matches.iter().enumerate() {
            let s = cur_phrase[ph_id] as usize;
            all_phrase_matches[s..s + list.len()].copy_from_slice(list);
            cur_phrase[ph_id] += list.len() as u32;
        }
    }

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
                            let has_dense = f_dense != u32::MAX
                                || ops
                                    .iter()
                                    .any(|&(_, tid)| dense_map_ref[tid as usize] != u32::MAX);

                            if !has_dense {
                                let p0 = get_postings(*first_term);
                                if ops.is_empty() {
                                    fast_write_results(out_buf, p0);
                                } else {
                                    cur_res.clear();
                                    cur_res.extend_from_slice(p0);
                                    for &(op, tid) in ops {
                                        if op == 0 && cur_res.is_empty() {
                                            continue;
                                        }
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
                            } else {
                                if f_dense != u32::MAX {
                                    scratch_bm
                                        .copy_from_slice(&dense_bitmaps_ref[f_dense as usize]);
                                } else {
                                    scratch_bm.fill(0);
                                    for &doc_id in get_postings(*first_term) {
                                        scratch_bm[(doc_id >> 6) as usize] |= 1u64 << (doc_id & 63);
                                    }
                                }

                                for &(op, tid) in ops {
                                    let d_id = dense_map_ref[tid as usize];
                                    if d_id != u32::MAX {
                                        let other = &dense_bitmaps_ref[d_id as usize];
                                        match op {
                                            0 => {
                                                for i in 0..num_words {
                                                    scratch_bm[i] &= other[i];
                                                }
                                            }
                                            1 => {
                                                for i in 0..num_words {
                                                    scratch_bm[i] |= other[i];
                                                }
                                            }
                                            2 => {
                                                for i in 0..num_words {
                                                    scratch_bm[i] &= !other[i];
                                                }
                                            }
                                            _ => {}
                                        }
                                    } else {
                                        let p = get_postings(tid);
                                        match op {
                                            0 => {
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
                                                    scratch_bm[(doc_id >> 6) as usize] |=
                                                        1u64 << (doc_id & 63);
                                                }
                                            }
                                            1 => {
                                                for &doc_id in p {
                                                    scratch_bm[(doc_id >> 6) as usize] |=
                                                        1u64 << (doc_id & 63);
                                                }
                                            }
                                            2 => {
                                                for &doc_id in p {
                                                    scratch_bm[(doc_id >> 6) as usize] &=
                                                        !(1u64 << (doc_id & 63));
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
                                    if w == 0 {
                                        continue;
                                    }
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
                                out_buf.push(b" "[0]);
                                append_num(out_buf, sum);
                                for i in 0..n_first {
                                    out_buf.push(b" "[0]);
                                    append_num(out_buf, first_10[i] as u64);
                                }
                                out_buf.push(b"\n"[0]);
                            }
                        }
                        ParsedQuery::Phrase { phrase_id } => {
                            let ph = *phrase_id as usize;
                            let matches =
                                &p_matches[p_offsets[ph] as usize..p_offsets[ph + 1] as usize];
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
