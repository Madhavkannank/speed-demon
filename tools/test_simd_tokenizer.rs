use std::arch::x86_64::*;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::time::Instant;

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
    fn madvise(addr: *mut std::ffi::c_void, length: usize, advice: i32) -> i32;
}

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

fn build_table(q_bytes: &[u8]) -> (Vec<TableSlot>, u32) {
    let mut table = vec![TableSlot::default(); TABLE_SIZE];
    let mut vocab_size = 0u32;
    let mut start = 0;
    while start < q_bytes.len() {
        let mut end = start;
        while end < q_bytes.len() && q_bytes[end] != b'\n' { end += 1; }
        let line = &q_bytes[start..end];
        start = end + 1;
        for w in line.split(|&b| b == b' ' || b == b'"') {
            if w.is_empty() || w == b"BOOL" || w == b"PHRASE" || w == b"AND" || w == b"OR" || w == b"NOT" {
                continue;
            }
            let mut buf = [0u8; 32];
            buf[..w.len()].copy_from_slice(w);
            let (h, w0, w1, ext) = unsafe { hash_and_key(buf.as_ptr(), w.len()) };
            let tag = if h == 0 { 1 } else { h };
            let mut slot = (tag as usize) & TABLE_MASK;
            loop {
                let e = &table[slot];
                if e.hash == 0 {
                    table[slot] = TableSlot {
                        key_low: w0,
                        key_high: w1,
                        hash: tag,
                        id: vocab_size,
                        len: w.len() as u8,
                        extra: ext,
                        _pad: [0; 6],
                    };
                    vocab_size += 1;
                    break;
                }
                if e.hash == tag && e.len as usize == w.len() && e.key_low == w0 && e.key_high == w1 && e.extra == ext {
                    break;
                }
                slot = (slot + 1) & TABLE_MASK;
            }
        }
    }
    (table, vocab_size)
}

#[target_feature(enable = "avx2")]
unsafe fn scan_chunk_avx2(
    slice: &[u8],
    table: &[TableSlot],
    vocab_size: usize,
) -> (u64, Vec<u32>) {
    let mut token_counts = vec![0u32; vocab_size];
    let mut last_seen = vec![u32::MAX; vocab_size];
    let mut total_tokens = 0u64;

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
        // Read doc_id
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

        // Tokenize document text
        let mut prev_is_alnum = 0u32;
        while p + 32 <= len {
            let v = _mm256_loadu_si256(ptr.add(p) as *const __m256i);

            // Check for newline
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

            let doc_ended = nl_mask != 0;
            if doc_ended {
                let nl_off = nl_mask.trailing_zeros();
                let valid_mask = (1u32 << nl_off).wrapping_sub(1);
                mask &= valid_mask;
            }

            let mut start_bits = mask & !((mask << 1) | prev_is_alnum);
            prev_is_alnum = (mask >> 31) & 1;

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
                    l
                };

                total_tokens += 1;

                // Lookup in query term table
                if tok_len <= 17 && tok_start + 17 <= len {
                    let (h, w0, w1, ext) = hash_and_key(ptr.add(tok_start), tok_len);
                    let tag = if h == 0 { 1 } else { h };
                    let mut slot = (tag as usize) & TABLE_MASK;
                    loop {
                        let e = table.get_unchecked(slot);
                        if e.hash == 0 {
                            break;
                        }
                        if e.hash == tag && e.len as usize == tok_len && e.key_low == w0 && e.key_high == w1 && e.extra == ext {
                            let tid = e.id as usize;
                            if *last_seen.get_unchecked(tid) != doc_id {
                                *last_seen.get_unchecked_mut(tid) = doc_id;
                                *token_counts.get_unchecked_mut(tid) += 1;
                            }
                            break;
                        }
                        slot = (slot + 1) & TABLE_MASK;
                    }
                }
            }

            if doc_ended {
                let nl_off = nl_mask.trailing_zeros() as usize;
                p += nl_off + 1;
                break;
            } else {
                p += 32;
            }
        }

        // Tail processing if doc didn't end within 32-byte chunks
        while p < len && *ptr.add(p) != b'\n' {
            if (*ptr.add(p)).is_ascii_alphanumeric() {
                let tok_start = p;
                while p < len && (*ptr.add(p)).is_ascii_alphanumeric() {
                    p += 1;
                }
                let tok_len = p - tok_start;
                total_tokens += 1;
                if tok_len <= 17 {
                    let mut buf = [0u8; 32];
                    buf[..tok_len].copy_from_slice(&slice[tok_start..tok_start + tok_len]);
                    let (h, w0, w1, ext) = hash_and_key(buf.as_ptr(), tok_len);
                    let tag = if h == 0 { 1 } else { h };
                    let mut slot = (tag as usize) & TABLE_MASK;
                    loop {
                        let e = table.get_unchecked(slot);
                        if e.hash == 0 { break; }
                        if e.hash == tag && e.len as usize == tok_len && e.key_low == w0 && e.key_high == w1 && e.extra == ext {
                            let tid = e.id as usize;
                            if *last_seen.get_unchecked(tid) != doc_id {
                                *last_seen.get_unchecked_mut(tid) = doc_id;
                                *token_counts.get_unchecked_mut(tid) += 1;
                            }
                            break;
                        }
                        slot = (slot + 1) & TABLE_MASK;
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

    (total_tokens, token_counts)
}

fn main() {
    let q_bytes = std::fs::read("/tmp/sd-full-b/queries.txt").unwrap();
    let (table, vocab_size) = build_table(&q_bytes);
    println!("Built query table: {} unique terms", vocab_size);

    let docs_file = File::open("/tmp/sd-full-b/docs.txt").unwrap();
    let flen = docs_file.metadata().unwrap().len() as usize;
    let mmap_ptr = unsafe { mmap(std::ptr::null_mut(), flen, 1, 2, docs_file.as_raw_fd(), 0) };
    unsafe { madvise(mmap_ptr, flen, 22); }
    let mmap_slice = unsafe { std::slice::from_raw_parts(mmap_ptr as *const u8, flen) };

    const NUM_THREADS: usize = 12;
    let approx_chunk = flen / NUM_THREADS;
    let mut bounds = Vec::with_capacity(NUM_THREADS + 1);
    bounds.push(0);
    for i in 1..NUM_THREADS {
        let mut p = (i * approx_chunk).min(flen);
        while p < flen && mmap_slice[p] != b'\n' { p += 1; }
        if p < flen { p += 1; }
        bounds.push(p);
    }
    bounds.push(flen);

    println!("Running 12-thread AVX2 start-bit tokenizer on full docs.txt (254 MB)...");
    for iter in 0..5 {
        let t0 = Instant::now();
        let mut total_tokens = 0u64;
        let mut total_matches = 0u64;

        std::thread::scope(|s| {
            let mut handles = Vec::new();
            for w in 0..NUM_THREADS {
                let slice = &mmap_slice[bounds[w]..bounds[w + 1]];
                let table_ref = &table;
                handles.push(s.spawn(move || unsafe {
                    scan_chunk_avx2(slice, table_ref, vocab_size as usize)
                }));
            }
            for h in handles {
                let (toks, counts) = h.join().unwrap();
                total_tokens += toks;
                total_matches += counts.iter().map(|&c| c as u64).sum::<u64>();
            }
        });

        let d = t0.elapsed();
        println!("Iter {}: {:?} ({} tokens, {} query matches, {:.1} MB/s)",
            iter + 1, d, total_tokens, total_matches, (flen as f64 / 1e6) / d.as_secs_f64());
    }

    unsafe { munmap(mmap_ptr, flen); }
}
