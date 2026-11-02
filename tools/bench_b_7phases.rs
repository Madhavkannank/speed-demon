use std::fs::File;
use std::io::{BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Instant;

static BOOL_TIME_NS: AtomicU64 = AtomicU64::new(0);
static PHRASE_TIME_NS: AtomicU64 = AtomicU64::new(0);
static PREAD_TIME_NS: AtomicU64 = AtomicU64::new(0);
static PREAD_COUNT: AtomicU64 = AtomicU64::new(0);

const NORM_TABLE: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = b'0'; while i <= b'9' { t[i as usize] = i; i += 1; }
    let mut i = b'a'; while i <= b'z' { t[i as usize] = i; i += 1; }
    let mut i = b'A'; while i <= b'Z' { t[i as usize] = i + 32; i += 1; }
    t
};

#[inline(always)]
fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = 0x517cc1b727220a95u64;
    for &x in b { h = (h.rotate_left(5) ^ (x as u64)).wrapping_mul(0x9e3779b97f4a7c15); }
    h
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct CompactEntry {
    hash: u64,
    tid: u32,
    q_offset: u32,
    len: u16,
    _pad: u16,
}

const TABLE_SIZE: usize = 262144;
const TABLE_MASK: usize = TABLE_SIZE - 1;
const NUM_WORKERS: usize = 12;

enum ParsedQuery {
    Bool { first_term: Option<u32>, ops: Vec<(u8, Option<u32>)> },
    Phrase { term_ids: Vec<u32>, words: Vec<Vec<u8>> },
    Empty,
}

#[inline(always)]
fn word_matches(d: &[u8], s: usize, len: usize, t: &[u8]) -> bool {
    if len != t.len() { return false; }
    for k in 0..len {
        if NORM_TABLE[d[s + k] as usize] != t[k] { return false; }
    }
    true
}

#[inline(always)]
fn intersect_sorted(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] { i += 1; }
        else if a[i] == b[j] { out.push(a[i]); i += 1; j += 1; }
        else { j += 1; }
    }
}

fn main() {
    let t_start = Instant::now();
    let queries_path = "/tmp/sd-full-b/queries.txt";
    let docs_path = "/tmp/sd-full-b/docs.txt";

    // 1. Read queries
    let t_rq0 = Instant::now();
    let q_bytes = std::fs::read(queries_path).unwrap();
    let t_read_queries = t_rq0.elapsed();

    // 2. Build term table
    let t_tt0 = Instant::now();
    let mut bloom = vec![0u64; 32768];
    let mut flat_table = vec![CompactEntry { hash: 0, tid: u32::MAX, q_offset: 0, len: 0, _pad: 0 }; TABLE_SIZE];
    let mut vocab = std::collections::HashMap::new();
    let mut parsed_queries = Vec::with_capacity(50000);

    let mut p = 0;
    while p < q_bytes.len() {
        let mut line_end = p;
        while line_end < q_bytes.len() && q_bytes[line_end] != b'\n' { line_end += 1; }
        let line = &q_bytes[p..line_end];
        let line_off = p;
        p = line_end + 1;
        if line.is_empty() { continue; }

        if line.starts_with(b"BOOL ") {
            let mut words_in_line = Vec::new();
            let mut lp = 5;
            while lp < line.len() {
                let ws = lp;
                while lp < line.len() && line[lp] != b' ' { lp += 1; }
                words_in_line.push(&line[ws..lp]);
                if lp < line.len() && line[lp] == b' ' { lp += 1; }
            }
            if words_in_line.is_empty() {
                parsed_queries.push(ParsedQuery::Empty);
                continue;
            }
            let w0 = words_in_line[0];
            let first_id = if let Some(&id) = vocab.get(w0) { Some(id) } else {
                let id = vocab.len() as u32;
                vocab.insert(w0.to_vec(), id);
                let h = hash_bytes(w0);
                let (b1, b2, b3, b4) = ((h & 0x1FFFFF) as usize, ((h >> 21) & 0x1FFFFF) as usize, (h.rotate_left(17) & 0x1FFFFF) as usize, (h.rotate_left(37) & 0x1FFFFF) as usize);
                bloom[b1 >> 6] |= 1u64 << (b1 & 63); bloom[b2 >> 6] |= 1u64 << (b2 & 63); bloom[b3 >> 6] |= 1u64 << (b3 & 63); bloom[b4 >> 6] |= 1u64 << (b4 & 63);
                let mut slot = (h as usize) & TABLE_MASK;
                while flat_table[slot].hash != 0 { slot = (slot + 1) & TABLE_MASK; }
                flat_table[slot] = CompactEntry { hash: h, tid: id, q_offset: (line_off + 5) as u32, len: w0.len() as u16, _pad: 0 };
                Some(id)
            };
            let mut ops = Vec::new();
            for chunk in words_in_line[1..].chunks(2) {
                if chunk.len() < 2 { break; }
                let op = match chunk[0] { b"OR" => 1, b"NOT" => 2, _ => 0 };
                let w2 = chunk[1];
                let tid2 = if let Some(&id) = vocab.get(w2) { id } else {
                    let id = vocab.len() as u32;
                    vocab.insert(w2.to_vec(), id);
                    let h = hash_bytes(w2);
                    let (b1, b2, b3, b4) = ((h & 0x1FFFFF) as usize, ((h >> 21) & 0x1FFFFF) as usize, (h.rotate_left(17) & 0x1FFFFF) as usize, (h.rotate_left(37) & 0x1FFFFF) as usize);
                    bloom[b1 >> 6] |= 1u64 << (b1 & 63); bloom[b2 >> 6] |= 1u64 << (b2 & 63); bloom[b3 >> 6] |= 1u64 << (b3 & 63); bloom[b4 >> 6] |= 1u64 << (b4 & 63);
                    let mut slot = (h as usize) & TABLE_MASK;
                    while flat_table[slot].hash != 0 { slot = (slot + 1) & TABLE_MASK; }
                    let ws2 = (w2.as_ptr() as usize) - (line.as_ptr() as usize);
                    flat_table[slot] = CompactEntry { hash: h, tid: id, q_offset: (line_off + ws2) as u32, len: w2.len() as u16, _pad: 0 };
                    id
                };
                ops.push((op, Some(tid2)));
            }
            parsed_queries.push(ParsedQuery::Bool { first_term: first_id, ops });
        } else if line.starts_with(b"PHRASE \"") {
            let inner_end = if line.ends_with(b"\"") { line.len() - 1 } else { line.len() };
            let (mut term_ids, mut words, mut lp) = (Vec::new(), Vec::new(), 8);
            while lp < inner_end {
                let ws = lp;
                while lp < inner_end && line[lp] != b' ' { lp += 1; }
                let w = &line[ws..lp];
                let tid = if let Some(&id) = vocab.get(w) { id } else {
                    let id = vocab.len() as u32;
                    vocab.insert(w.to_vec(), id);
                    let h = hash_bytes(w);
                    let (b1, b2, b3, b4) = ((h & 0x1FFFFF) as usize, ((h >> 21) & 0x1FFFFF) as usize, (h.rotate_left(17) & 0x1FFFFF) as usize, (h.rotate_left(37) & 0x1FFFFF) as usize);
                    bloom[b1 >> 6] |= 1u64 << (b1 & 63); bloom[b2 >> 6] |= 1u64 << (b2 & 63); bloom[b3 >> 6] |= 1u64 << (b3 & 63); bloom[b4 >> 6] |= 1u64 << (b4 & 63);
                    let mut slot = (h as usize) & TABLE_MASK;
                    while flat_table[slot].hash != 0 { slot = (slot + 1) & TABLE_MASK; }
                    flat_table[slot] = CompactEntry { hash: h, tid: id, q_offset: (line_off + ws) as u32, len: w.len() as u16, _pad: 0 };
                    id
                };
                term_ids.push(tid);
                words.push(w.to_vec());
                if lp < inner_end && line[lp] == b' ' { lp += 1; }
            }
            parsed_queries.push(ParsedQuery::Phrase { term_ids, words });
        } else {
            parsed_queries.push(ParsedQuery::Empty);
        }
    }
    let vocab_size = vocab.len();
    let t_build_table = t_tt0.elapsed();

    // 3. Scan docs
    let t_sc0 = Instant::now();
    let d_file = File::open(docs_path).unwrap();
    let file_len = d_file.metadata().unwrap().len() as usize;
    let approx_chunk = file_len / NUM_WORKERS;
    let mut chunk_bounds = vec![0usize; NUM_WORKERS + 1];
    let mut scan_buf = vec![0u8; 8192];
    for w in 1..NUM_WORKERS {
        let guess = w * approx_chunk;
        let to_read = (file_len - guess).min(8192);
        d_file.read_exact_at(&mut scan_buf[..to_read], guess as u64).unwrap();
        let mut offset = 0;
        while offset < to_read && scan_buf[offset] != b'\n' { offset += 1; }
        chunk_bounds[w] = (guess + offset + 1).min(file_len);
    }
    chunk_bounds[NUM_WORKERS] = file_len;

    struct WorkerData {
        pairs: Vec<u64>,
        counts: Vec<u32>,
        text_offsets: Vec<u64>,
        text_lens: Vec<u32>,
    }
    let mut worker_results: Vec<WorkerData> = Vec::with_capacity(NUM_WORKERS);

    thread::scope(|s| {
        let (bloom_ref, table_ref, q_bytes_ref) = (&bloom, &flat_table, &q_bytes);
        let mut handles = Vec::with_capacity(NUM_WORKERS);
        for w_idx in 0..NUM_WORKERS {
            let (start_pos, end_pos) = (chunk_bounds[w_idx], chunk_bounds[w_idx + 1]);
            handles.push(s.spawn(move || {
                let d_file = File::open(docs_path).unwrap();
                let mut pairs = Vec::with_capacity(2_200_000);
                let mut counts = vec![0u32; vocab_size];
                let mut last_seen = vec![u32::MAX; vocab_size];
                let mut text_offsets = Vec::with_capacity(25000);
                let mut text_lens = Vec::with_capacity(25000);
                let mut chunk_buf = vec![0u8; 512 * 1024];
                let (mut cur_offset, mut rem_len) = (start_pos, 0);

                while cur_offset < end_pos || rem_len > 0 {
                    let to_read = (512 * 1024 - rem_len).min(end_pos.saturating_sub(cur_offset));
                    let read_bytes = if to_read > 0 {
                        d_file.read_exact_at(&mut chunk_buf[rem_len..rem_len + to_read], cur_offset as u64).unwrap();
                        cur_offset += to_read;
                        to_read
                    } else { 0 };
                    let total_available = rem_len + read_bytes;
                    if total_available == 0 { break; }
                    let mut last_nl = total_available;
                    while last_nl > 0 && chunk_buf[last_nl - 1] != b'\n' { last_nl -= 1; }
                    let (slice, mut p) = (&chunk_buf[..last_nl], 0);

                    while p < last_nl {
                        let mut doc_id = 0u32;
                        while p < last_nl && slice[p] >= b'0' && slice[p] <= b'9' {
                            doc_id = doc_id * 10 + (slice[p] - b'0') as u32;
                            p += 1;
                        }
                        while p < last_nl && slice[p] != b'\t' && slice[p] != b'\n' { p += 1; }
                        let text_start = if p < last_nl && slice[p] == b'\t' { p + 1 } else { p };
                        p = text_start;
                        let text_file_offset = (cur_offset - total_available + text_start) as u64;

                        let (mut word_start, mut cur_len, mut h) = (0, 0, 0x517cc1b727220a95u64);
                        while p < last_nl && slice[p] != b'\n' {
                            let c = NORM_TABLE[slice[p] as usize];
                            if c != 0 {
                                if cur_len == 0 { word_start = p; }
                                cur_len += 1;
                                h = (h.rotate_left(5) ^ (c as u64)).wrapping_mul(0x9e3779b97f4a7c15);
                            } else {
                                if cur_len > 0 {
                                    let (b1, b2, b3, b4) = ((h & 0x1FFFFF) as usize, ((h >> 21) & 0x1FFFFF) as usize, (h.rotate_left(17) & 0x1FFFFF) as usize, (h.rotate_left(37) & 0x1FFFFF) as usize);
                                    if ((bloom_ref[b1 >> 6] >> (b1 & 63)) & (bloom_ref[b2 >> 6] >> (b2 & 63)) & (bloom_ref[b3 >> 6] >> (b3 & 63)) & (bloom_ref[b4 >> 6] >> (b4 & 63))) & 1 != 0 {
                                        let mut slot = (h as usize) & TABLE_MASK;
                                        while table_ref[slot].hash != 0 {
                                            if table_ref[slot].hash == h && table_ref[slot].len as usize == cur_len {
                                                let qw = &q_bytes_ref[table_ref[slot].q_offset as usize..table_ref[slot].q_offset as usize + cur_len];
                                                let mut ok = true;
                                                for k in 0..cur_len {
                                                    if NORM_TABLE[slice[word_start + k] as usize] != qw[k] { ok = false; break; }
                                                }
                                                if ok {
                                                    let tid = table_ref[slot].tid as usize;
                                                    if last_seen[tid] != doc_id {
                                                        pairs.push(((tid as u64) << 32) | (doc_id as u64));
                                                        counts[tid] += 1;
                                                        last_seen[tid] = doc_id;
                                                    }
                                                    break;
                                                }
                                            }
                                            slot = (slot + 1) & TABLE_MASK;
                                        }
                                    }
                                    cur_len = 0;
                                    h = 0x517cc1b727220a95u64;
                                }
                            }
                            p += 1;
                        }
                        if cur_len > 0 {
                            let (b1, b2, b3, b4) = ((h & 0x1FFFFF) as usize, ((h >> 21) & 0x1FFFFF) as usize, (h.rotate_left(17) & 0x1FFFFF) as usize, (h.rotate_left(37) & 0x1FFFFF) as usize);
                            if ((bloom_ref[b1 >> 6] >> (b1 & 63)) & (bloom_ref[b2 >> 6] >> (b2 & 63)) & (bloom_ref[b3 >> 6] >> (b3 & 63)) & (bloom_ref[b4 >> 6] >> (b4 & 63))) & 1 != 0 {
                                let mut slot = (h as usize) & TABLE_MASK;
                                while table_ref[slot].hash != 0 {
                                    if table_ref[slot].hash == h && table_ref[slot].len as usize == cur_len {
                                        let qw = &q_bytes_ref[table_ref[slot].q_offset as usize..table_ref[slot].q_offset as usize + cur_len];
                                        let mut ok = true;
                                        for k in 0..cur_len {
                                            if NORM_TABLE[slice[word_start + k] as usize] != qw[k] { ok = false; break; }
                                        }
                                        if ok {
                                            let tid = table_ref[slot].tid as usize;
                                            if last_seen[tid] != doc_id {
                                                pairs.push(((tid as u64) << 32) | (doc_id as u64));
                                                counts[tid] += 1;
                                                last_seen[tid] = doc_id;
                                            }
                                            break;
                                        }
                                    }
                                    slot = (slot + 1) & TABLE_MASK;
                                }
                            }
                        }
                        text_offsets.push(text_file_offset);
                        text_lens.push((p - text_start) as u32);
                        if p < last_nl && slice[p] == b'\n' { p += 1; }
                    }
                    rem_len = total_available - last_nl;
                    if rem_len > 0 { chunk_buf.copy_within(last_nl..total_available, 0); }
                }
                WorkerData { pairs, counts, text_offsets, text_lens }
            }));
        }
        for h in handles { worker_results.push(h.join().unwrap()); }
    });
    let t_scan = t_sc0.elapsed();

    // 4. Merge / Scatter
    let t_ms0 = Instant::now();
    let (mut doc_text_offsets, mut doc_text_lens) = (Vec::with_capacity(260000), Vec::with_capacity(260000));
    for res in &worker_results {
        doc_text_offsets.extend_from_slice(&res.text_offsets);
        doc_text_lens.extend_from_slice(&res.text_lens);
    }
    let mut term_offsets = vec![0usize; vocab_size + 1];
    for t in 0..vocab_size {
        let mut count = 0usize;
        for w in 0..NUM_WORKERS { count += worker_results[w].counts[t] as usize; }
        term_offsets[t + 1] = term_offsets[t] + count;
    }
    let total_postings = term_offsets[vocab_size];
    let mut worker_base: Vec<Vec<usize>> = vec![vec![0usize; vocab_size]; NUM_WORKERS];
    for t in 0..vocab_size {
        let mut base = term_offsets[t];
        for w in 0..NUM_WORKERS {
            worker_base[w][t] = base;
            base += worker_results[w].counts[t] as usize;
        }
    }
    let mut all_postings = vec![0u32; total_postings];
    let all_ptr = all_postings.as_mut_ptr() as usize;

    thread::scope(|s| {
        for w in 0..NUM_WORKERS {
            let pairs = std::mem::take(&mut worker_results[w].pairs);
            let mut bases = std::mem::take(&mut worker_base[w]);
            s.spawn(move || {
                let ptr = all_ptr as *mut u32;
                for pair in pairs {
                    let tid = (pair >> 32) as usize;
                    let doc_id = pair as u32;
                    let dst = bases[tid];
                    unsafe { *ptr.add(dst) = doc_id; }
                    bases[tid] += 1;
                }
            });
        }
    });
    drop(worker_results);
    drop(worker_base);
    drop(bloom);
    drop(flat_table);
    drop(q_bytes);
    let t_merge = t_ms0.elapsed();

    // 5. Query execution
    let t_qe0 = Instant::now();
    let num_queries = parsed_queries.len();
    let chunk_size = (num_queries + NUM_WORKERS - 1) / NUM_WORKERS;
    let mut thread_outputs: Vec<Vec<u8>> = (0..NUM_WORKERS).map(|_| Vec::new()).collect();

    thread::scope(|s| {
        let (queries_ref, docs_path_ref, doc_to_ref, doc_tl_ref, offsets_ref, postings_ref) = (
            &parsed_queries,
            docs_path,
            &doc_text_offsets,
            &doc_text_lens,
            &term_offsets,
            &all_postings,
        );

        macro_rules! get_p {
            ($tid:expr) => {{
                let t = $tid as usize;
                &postings_ref[offsets_ref[t]..offsets_ref[t + 1]]
            }};
        }

        for (thread_idx, out_buf) in thread_outputs.iter_mut().enumerate() {
            let (start, end) = (thread_idx * chunk_size, (thread_idx * chunk_size + chunk_size).min(num_queries));
            if start >= end { continue; }

            s.spawn(move || {
                let d_file = File::open(docs_path_ref).unwrap();
                let (mut cur_res, mut next_res, mut scratch, mut matches) = (
                    Vec::with_capacity(16384), Vec::with_capacity(16384), Vec::with_capacity(16384), Vec::new(),
                );
                let mut doc_buf = vec![0u8; 8192];

                for q_idx in start..end {
                    match &queries_ref[q_idx] {
                        ParsedQuery::Bool { first_term, ops } => {
                            let t_b0 = Instant::now();
                            cur_res.clear();
                            if let Some(fid) = *first_term { cur_res.extend_from_slice(get_p!(fid)); }
                            for &(op, opt_tid) in ops {
                                let p = if let Some(t) = opt_tid { get_p!(t) } else { &[] };
                                match op {
                                    0 => { intersect_sorted(&cur_res, p, &mut next_res); std::mem::swap(&mut cur_res, &mut next_res); }
                                    _ => {}
                                }
                            }
                            // dummy output formatting write
                            out_buf.extend_from_slice(b"1 100 1\n");
                            BOOL_TIME_NS.fetch_add(t_b0.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        }
                        ParsedQuery::Phrase { term_ids, words } => {
                            let t_ph0 = Instant::now();
                            if term_ids.is_empty() { out_buf.extend_from_slice(b"0 0\n"); continue; }
                            cur_res.clear();
                            cur_res.extend_from_slice(get_p!(term_ids[0]));
                            for &tid in &term_ids[1..] {
                                intersect_sorted(&cur_res, get_p!(tid), &mut scratch);
                                std::mem::swap(&mut cur_res, &mut scratch);
                            }
                            matches.clear();
                            let k = words.len();
                            for &c_doc in &cur_res {
                                let (f_offset, t_len) = (doc_to_ref[c_doc as usize], doc_tl_ref[c_doc as usize] as usize);
                                if t_len > doc_buf.len() { doc_buf.resize(t_len, 0); }
                                let t_pr0 = Instant::now();
                                if d_file.read_exact_at(&mut doc_buf[..t_len], f_offset).is_ok() {
                                    PREAD_COUNT.fetch_add(1, Ordering::Relaxed);
                                    PREAD_TIME_NS.fetch_add(t_pr0.elapsed().as_nanos() as u64, Ordering::Relaxed);
                                    let d_slice = &doc_buf[..t_len];
                                    let (mut dp, mut matched) = (0, 0);
                                    while dp < t_len {
                                        let mut c = NORM_TABLE[d_slice[dp] as usize];
                                        if c != 0 {
                                            let (start_p, mut count) = (dp, 0);
                                            while c != 0 {
                                                count += 1; dp += 1;
                                                if dp >= t_len { break; }
                                                c = NORM_TABLE[d_slice[dp] as usize];
                                            }
                                            if word_matches(d_slice, start_p, count, &words[matched]) {
                                                matched += 1;
                                                if matched == k { matches.push(c_doc); break; }
                                            } else {
                                                matched = if word_matches(d_slice, start_p, count, &words[0]) { 1 } else { 0 };
                                            }
                                        } else { dp += 1; }
                                    }
                                }
                            }
                            out_buf.extend_from_slice(b"1 100 1\n");
                            PHRASE_TIME_NS.fetch_add(t_ph0.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        }
                        ParsedQuery::Empty => out_buf.extend_from_slice(b"0 0\n"),
                    }
                }
            });
        }
    });
    let t_query = t_qe0.elapsed();

    // 6. Format + Write
    let t_fw0 = Instant::now();
    let mut writer = BufWriter::with_capacity(1024 * 1024, File::create("/tmp/test_out.txt").unwrap());
    for b in thread_outputs { writer.write_all(&b).unwrap(); }
    writer.flush().unwrap();
    let t_write = t_fw0.elapsed();

    let total = t_start.elapsed();

    println!("==================================================");
    println!("TRACK B FULL PHASE BREAKDOWN (wall-clock time):");
    println!("==================================================");
    println!("1. read queries:      {:6.2} ms", t_read_queries.as_secs_f64() * 1000.0);
    println!("2. build term table:  {:6.2} ms", t_build_table.as_secs_f64() * 1000.0);
    println!("3. scan (docs.txt):   {:6.2} ms", t_scan.as_secs_f64() * 1000.0);
    println!("4. merge/scatter:     {:6.2} ms", t_merge.as_secs_f64() * 1000.0);
    println!("5. query total:       {:6.2} ms", t_query.as_secs_f64() * 1000.0);
    println!("   -> bool CPU time:   {:6.2} ms (across 12 threads = {:.2} ms/thread)", BOOL_TIME_NS.load(Ordering::Relaxed) as f64 / 1e6, (BOOL_TIME_NS.load(Ordering::Relaxed) as f64 / 1e6) / 12.0);
    println!("   -> phrase CPU time: {:6.2} ms (across 12 threads = {:.2} ms/thread)", PHRASE_TIME_NS.load(Ordering::Relaxed) as f64 / 1e6, (PHRASE_TIME_NS.load(Ordering::Relaxed) as f64 / 1e6) / 12.0);
    println!("      * total pread calls: {}", PREAD_COUNT.load(Ordering::Relaxed));
    println!("      * total pread time:  {:.2} ms CPU time", PREAD_TIME_NS.load(Ordering::Relaxed) as f64 / 1e6);
    println!("6. format + write:    {:6.2} ms", t_write.as_secs_f64() * 1000.0);
    println!("--------------------------------------------------");
    println!("TOTAL WALL-CLOCK:     {:6.2} ms", total.as_secs_f64() * 1000.0);
    println!("==================================================");
}
