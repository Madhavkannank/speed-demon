use std::{
    arch::x86_64::*,
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
        let mask = if len == 8 {
            u64::MAX
        } else {
            (1u64 << (len * 8)) - 1
        };
        let raw = (ptr as *const u64).read_unaligned();
        let w0 = (raw | 0x2020202020202020u64) & mask;
        let h = w0.wrapping_mul(0x517cc1b727220a95u64) ^ (len as u64);
        let h32 = (h ^ (h >> 32)) as u32;
        (h32, w0, 0, 0)
    } else if len <= 16 {
        let raw0 = (ptr as *const u64).read_unaligned();
        let w0 = raw0 | 0x2020202020202020u64;
        let rem_len = len - 8;
        let mask = if rem_len == 8 {
            u64::MAX
        } else {
            (1u64 << (rem_len * 8)) - 1
        };
        let raw1 = (ptr.add(8) as *const u64).read_unaligned();
        let w1 = (raw1 | 0x2020202020202020u64) & mask;
        let h = (w0.wrapping_mul(0x517cc1b727220a95u64) ^ w1.rotate_left(27))
            .wrapping_mul(0x9e3779b97f4a7c15u64)
            ^ (len as u64);
        let h32 = (h ^ (h >> 32)) as u32;
        (h32, w0, w1, 0)
    } else {
        let raw0 = (ptr as *const u64).read_unaligned();
        let w0 = raw0 | 0x2020202020202020u64;
        let raw1 = (ptr.add(8) as *const u64).read_unaligned();
        let w1 = raw1 | 0x2020202020202020u64;
        let ext = (*ptr.add(16)) | 0x20;
        let h = (w0.wrapping_mul(0x517cc1b727220a95u64)
            ^ w1.rotate_left(27)
            ^ ((ext as u64).rotate_left(43)))
        .wrapping_mul(0x9e3779b97f4a7c15u64)
            ^ (len as u64);
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
            vocab_size: 0,
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
                *slot = TableSlot {
                    key_low: w0,
                    key_high: w1,
                    hash: tag,
                    id,
                    len: len as u8,
                    extra: ext,
                    _pad: [0; 6],
                };
                return id;
            }
            if slot.hash == tag
                && slot.len as usize == len
                && slot.key_low == w0
                && slot.key_high == w1
                && slot.extra == ext
            {
                return slot.id;
            }
            idx = (idx + 1) & TABLE_MASK;
        }
    }

    #[inline(always)]
    fn lookup(&self, ptr: *const u8, len: usize) -> Option<u32> {
        let (h, w0, w1, ext) = unsafe { hash_and_key(ptr, len) };
        let tag = if h == 0 { 1 } else { h };
        let mut idx = (tag as usize) & TABLE_MASK;
        loop {
            let slot = unsafe { self.slots.get_unchecked(idx) };
            if slot.hash == 0 {
                return None;
            }
            if slot.hash == tag
                && slot.len as usize == len
                && slot.key_low == w0
                && slot.key_high == w1
                && slot.extra == ext
            {
                return Some(slot.id);
            }
            idx = (idx + 1) & TABLE_MASK;
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
        let (va, vb) = (unsafe { *a.get_unchecked(i) }, unsafe {
            *b.get_unchecked(j)
        });
        if va == vb {
            out.push(va);
            i += 1;
            j += 1;
        } else if va < vb {
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
        let (va, vb) = (unsafe { *a.get_unchecked(i) }, unsafe {
            *b.get_unchecked(j)
        });
        if va == vb {
            out.push(va);
            i += 1;
            j += 1;
        } else if va < vb {
            out.push(va);
            i += 1;
        } else {
            out.push(vb);
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
    if b.is_empty() {
        out.extend_from_slice(a);
        return;
    }
    if b.len() > a.len() * 8 {
        let mut s = b;
        for &v in a {
            match s.binary_search(&v) {
                Ok(i) => s = &s[i + 1..],
                Err(i) => {
                    out.push(v);
                    s = &s[i..];
                }
            }
        }
        return;
    }
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (va, vb) = (unsafe { *a.get_unchecked(i) }, unsafe {
            *b.get_unchecked(j)
        });
        if va == vb {
            i += 1;
            j += 1;
        } else if va < vb {
            out.push(va);
            i += 1;
        } else {
            j += 1;
        }
    }
    if i < a.len() {
        out.extend_from_slice(&a[i..]);
    }
}

#[inline(always)]
fn fast_write_results(buf: &mut Vec<u8>, ids: &[u32]) {
    if ids.is_empty() {
        buf.extend_from_slice(b"0 0\n");
        return;
    }
    let sum: u64 = ids.iter().map(|&x| x as u64).sum();
    write_u64(buf, ids.len() as u64);
    buf.push(b' ');
    write_u64(buf, sum);
    for &id in &ids[..ids.len().min(10)] {
        buf.push(b' ');
        write_u64(buf, id as u64);
    }
    buf.push(b'\n');
}

#[derive(Clone)]
enum ParsedQuery {
    Bool {
        first_term: u32,
        ops: Vec<(u8, u32)>,
    },
    Phrase {
        term_ids: Vec<u32>,
        words: Vec<Vec<u8>>,
    },
    Empty,
}

#[inline(always)]
fn write_u64(buf: &mut Vec<u8>, mut n: u64) {
    if n == 0 {
        buf.push(b'0');
        return;
    }
    let mut b = [0u8; 20];
    let mut i = 20;
    while n > 0 {
        i -= 1;
        b[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    buf.extend_from_slice(&b[i..]);
}

#[target_feature(enable = "avx2")]
unsafe fn pass1_scan(
    slice: &[u8],
    slice_start: usize,
    table: &TermTable,
    vocab_size: usize,
    counts: &mut [u32],
    doc_starts: &mut Vec<u32>,
) {
    let mut last_seen = vec![u32::MAX; vocab_size];
    let len = slice.len();
    let ptr = slice.as_ptr();
    let mut p = 0;

    let v_20 = _mm256_set1_epi8(0x20);
    let v_a_minus_1 = _mm256_set1_epi8((b'a' - 1) as i8);
    let v_z_plus_1 = _mm256_set1_epi8((b'z' + 1) as i8);
    let v_0_minus_1 = _mm256_set1_epi8((b'0' - 1) as i8);
    let v_9_plus_1 = _mm256_set1_epi8((b'9' + 1) as i8);
    let v_nl = _mm256_set1_epi8(b'\n' as i8);

    while p < len {
        let d_start = (slice_start + p) as u32;
        let mut doc_id = 0u32;
        while p < len && *ptr.add(p) >= b'0' && *ptr.add(p) <= b'9' {
            doc_id = doc_id * 10 + (*ptr.add(p) - b'0') as u32;
            p += 1;
        }
        while p < len && *ptr.add(p) != b'\t' && *ptr.add(p) != b'\n' {
            p += 1;
        }
        if p < len && *ptr.add(p) == b'\t' {
            p += 1;
        }
        doc_starts.push(d_start);

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
                if eff_len <= 17 {
                    let tid_opt = if tok_start + 17 <= len {
                        table.lookup(ptr.add(tok_start), eff_len)
                    } else {
                        let mut buf = [0u8; 32];
                        buf[..eff_len].copy_from_slice(std::slice::from_raw_parts(
                            ptr.add(tok_start),
                            eff_len,
                        ));
                        table.lookup(buf.as_ptr(), eff_len)
                    };
                    if let Some(tid) = tid_opt {
                        let id = tid as usize;
                        if *last_seen.get_unchecked(id) != doc_id {
                            *last_seen.get_unchecked_mut(id) = doc_id;
                            *counts.get_unchecked_mut(id) += 1;
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
                    while p < len && (*ptr.add(p)).is_ascii_alphanumeric() {
                        p += 1;
                    }
                    let tok_len = p - tok_start;
                    let eff_len = tok_len.min(32);
                    if eff_len <= 17 {
                        let mut buf = [0u8; 32];
                        buf[..eff_len].copy_from_slice(std::slice::from_raw_parts(
                            ptr.add(tok_start),
                            eff_len,
                        ));
                        if let Some(tid) = table.lookup(buf.as_ptr(), eff_len) {
                            let id = tid as usize;
                            if *last_seen.get_unchecked(id) != doc_id {
                                *last_seen.get_unchecked_mut(id) = doc_id;
                                *counts.get_unchecked_mut(id) += 1;
                            }
                        }
                    }
                } else {
                    p += 1;
                }
            }
            if p < len && *ptr.add(p) == b'\n' {
                p += 1;
            }
        }
    }
}

#[target_feature(enable = "avx2")]
unsafe fn pass2_scan(
    slice: &[u8],
    table: &TermTable,
    vocab_size: usize,
    cursors: &mut [u32],
    out_ptr: *mut u32,
) {
    let mut last_seen = vec![u32::MAX; vocab_size];
    let len = slice.len();
    let ptr = slice.as_ptr();
    let mut p = 0;

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
        while p < len && *ptr.add(p) != b'\t' && *ptr.add(p) != b'\n' {
            p += 1;
        }
        if p < len && *ptr.add(p) == b'\t' {
            p += 1;
        }

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
                if eff_len <= 17 {
                    let tid_opt = if tok_start + 17 <= len {
                        table.lookup(ptr.add(tok_start), eff_len)
                    } else {
                        let mut buf = [0u8; 32];
                        buf[..eff_len].copy_from_slice(std::slice::from_raw_parts(
                            ptr.add(tok_start),
                            eff_len,
                        ));
                        table.lookup(buf.as_ptr(), eff_len)
                    };
                    if let Some(tid) = tid_opt {
                        let id = tid as usize;
                        if *last_seen.get_unchecked(id) != doc_id {
                            *last_seen.get_unchecked_mut(id) = doc_id;
                            let pos = cursors.get_unchecked_mut(id);
                            *out_ptr.add(*pos as usize) = doc_id;
                            *pos += 1;
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
                    while p < len && (*ptr.add(p)).is_ascii_alphanumeric() {
                        p += 1;
                    }
                    let tok_len = p - tok_start;
                    let eff_len = tok_len.min(32);
                    if eff_len <= 17 {
                        let mut buf = [0u8; 32];
                        buf[..eff_len].copy_from_slice(std::slice::from_raw_parts(
                            ptr.add(tok_start),
                            eff_len,
                        ));
                        if let Some(tid) = table.lookup(buf.as_ptr(), eff_len) {
                            let id = tid as usize;
                            if *last_seen.get_unchecked(id) != doc_id {
                                *last_seen.get_unchecked_mut(id) = doc_id;
                                let pos = cursors.get_unchecked_mut(id);
                                *out_ptr.add(*pos as usize) = doc_id;
                                *pos += 1;
                            }
                        }
                    }
                } else {
                    p += 1;
                }
            }
            if p < len && *ptr.add(p) == b'\n' {
                p += 1;
            }
        }
    }
}

fn main() -> std::io::Result<()> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string());
    let docs_path = Path::new(&data_dir).join("docs.txt");
    let queries_path = Path::new(&data_dir).join("queries.txt");
    let out_path = Path::new(&out_dir).join("results.txt");
    fs::create_dir_all(&out_dir)?;

    let docs_file = File::open(&docs_path)?;
    let file_len = docs_file.metadata()?.len() as usize;
    if file_len == 0 {
        File::create(&out_path)?;
        return Ok(());
    }

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
    unsafe {
        madvise(mmap_ptr, file_len, MADV_POPULATE_READ);
    }
    let mmap_slice: &[u8] = unsafe { std::slice::from_raw_parts(mmap_ptr as *const u8, file_len) };

    let q_bytes = fs::read(&queries_path)?;
    let mut table = TermTable::new();
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
                let mut ops = Vec::new();
                while let Some(op_bytes) = parts.next() {
                    if let Some(term_bytes) = parts.next() {
                        let op_code = match op_bytes {
                            b"OR" => 1,
                            b"NOT" => 2,
                            _ => 0,
                        };
                        let tid = table.get_or_insert(term_bytes);
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

    let vocab_size = table.vocab_size as usize;
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

            s.spawn(move || unsafe {
                pass1_scan(
                    slice,
                    start,
                    table_ref,
                    vocab_size,
                    counts_ref,
                    doc_starts_ref,
                );
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
            let mut cursors = write_cursor[w].clone();

            s.spawn(move || unsafe {
                pass2_scan(
                    slice,
                    table_ref,
                    vocab_size,
                    &mut cursors,
                    all_postings_ptr as *mut u32,
                );
            });
        }
    });
    drop(write_cursor);

    let term_offsets_ref: &'static [usize] = Box::leak(term_offsets.into_boxed_slice());
    let all_postings_ref: &'static [u32] = Box::leak(all_postings.into_boxed_slice());

    // QUERY EXECUTION: Parallel across 12 worker threads
    let num_queries = parsed_queries.len();
    let q_chunk = (num_queries + num_workers - 1) / num_workers;
    let mut out_parts: Vec<Vec<u8>> = (0..num_workers).map(|_| Vec::new()).collect();

    thread::scope(|s| {
        let (queries_ref, mmap_ref, line_offsets_ref) =
            (&parsed_queries, &mmap_slice, &line_offsets);

        for (w, out_buf) in out_parts.iter_mut().enumerate() {
            let q_start = w * q_chunk;
            let q_end = (q_start + q_chunk).min(num_queries);
            if q_start >= q_end {
                continue;
            }

            s.spawn(move || {
                let mut cur_res = Vec::with_capacity(32768);
                let mut temp_res = Vec::with_capacity(32768);
                let mut matches = Vec::with_capacity(4096);
                let mut out = Vec::with_capacity((q_end - q_start) * 64);

                for q in &queries_ref[q_start..q_end] {
                    match q {
                        ParsedQuery::Bool { first_term, ops } => {
                            let ft = *first_term as usize;
                            let p0 =
                                &all_postings_ref[term_offsets_ref[ft]..term_offsets_ref[ft + 1]];
                            if ops.is_empty() {
                                fast_write_results(&mut out, p0);
                            } else {
                                cur_res.clear();
                                cur_res.extend_from_slice(p0);

                                for &(op, tid) in ops {
                                    if op == 0 && cur_res.is_empty() {
                                        continue;
                                    }
                                    let t_idx = tid as usize;
                                    let t_range =
                                        term_offsets_ref[t_idx]..term_offsets_ref[t_idx + 1];
                                    let t_slice = &all_postings_ref[t_range];

                                    match op {
                                        0 => {
                                            intersect_sorted(&cur_res, t_slice, &mut temp_res);
                                            std::mem::swap(&mut cur_res, &mut temp_res);
                                        }
                                        1 => {
                                            union_sorted(&cur_res, t_slice, &mut temp_res);
                                            std::mem::swap(&mut cur_res, &mut temp_res);
                                        }
                                        2 => {
                                            difference_sorted(&cur_res, t_slice, &mut temp_res);
                                            std::mem::swap(&mut cur_res, &mut temp_res);
                                        }
                                        _ => {}
                                    }
                                }
                                fast_write_results(&mut out, &cur_res);
                            }
                        }
                        ParsedQuery::Phrase { term_ids, words } => {
                            if term_ids.is_empty() {
                                out.extend_from_slice(b"0 0\n");
                                continue;
                            }
                            let mut min_len = usize::MAX;
                            let mut min_tid = 0;
                            for &tid in term_ids {
                                let t_idx = tid as usize;
                                let len = term_offsets_ref[t_idx + 1] - term_offsets_ref[t_idx];
                                if len < min_len {
                                    min_len = len;
                                    min_tid = tid;
                                }
                            }

                            if min_len == 0 {
                                out.extend_from_slice(b"0 0\n");
                                continue;
                            }

                            let mt_idx = min_tid as usize;
                            cur_res.clear();
                            cur_res.extend_from_slice(
                                &all_postings_ref
                                    [term_offsets_ref[mt_idx]..term_offsets_ref[mt_idx + 1]],
                            );

                            for &tid in term_ids {
                                if tid == min_tid {
                                    continue;
                                }
                                if cur_res.is_empty() {
                                    break;
                                }
                                let t_idx = tid as usize;
                                let t_range = term_offsets_ref[t_idx]..term_offsets_ref[t_idx + 1];
                                intersect_sorted(
                                    &cur_res,
                                    &all_postings_ref[t_range],
                                    &mut temp_res,
                                );
                                std::mem::swap(&mut cur_res, &mut temp_res);
                            }

                            if cur_res.is_empty() {
                                out.extend_from_slice(b"0 0\n");
                                continue;
                            }

                            let k = words.len();
                            let mut pi = vec![0usize; k];
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
                            fast_write_results(&mut out, &matches);
                        }
                        ParsedQuery::Empty => {
                            out.extend_from_slice(b"0 0\n");
                        }
                    }
                }
                *out_buf = out;
            });
        }
    });

    let mut out_file = BufWriter::with_capacity(512 * 1024, File::create(&out_path)?);
    for part in &out_parts {
        out_file.write_all(part)?;
    }
    out_file.flush()?;

    unsafe {
        munmap(mmap_ptr, file_len);
    }

    Ok(())
}
