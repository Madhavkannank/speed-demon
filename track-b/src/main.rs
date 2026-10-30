use std::collections::HashMap;
use std::fs::{self, File};
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::thread;

#[derive(Default)]
struct FastHasher(u64);

impl Hasher for FastHasher {
    #[inline(always)]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline(always)]
    fn write(&mut self, bytes: &[u8]) {
        let mut hash = self.0.wrapping_add(0x517cc1b727220a95);
        for &b in bytes {
            hash = hash.rotate_left(5) ^ (b as u64);
            hash = hash.wrapping_mul(0x9e3779b97f4a7c15);
        }
        self.0 = hash;
    }
}
#[inline(always)]
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash = 0x517cc1b727220a95u64;
    for &b in bytes {
        hash = hash.rotate_left(5) ^ (b as u64);
        hash = hash.wrapping_mul(0x9e3779b97f4a7c15);
    }
    hash
}
type FastBuildHasher = BuildHasherDefault<FastHasher>;


#[inline(always)]
fn intersect_sorted(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    if a.is_empty() || b.is_empty() {
        return;
    }
    if b.len() > a.len() * 32 {
        let mut b_slice = b;
        for &val in a {
            match b_slice.binary_search(&val) {
                Ok(idx) => {
                    out.push(val);
                    b_slice = &b_slice[idx + 1..];
                }
                Err(idx) => {
                    b_slice = &b_slice[idx..];
                }
            }
            if b_slice.is_empty() {
                break;
            }
        }
        return;
    }
    if a.len() > b.len() * 32 {
        let mut a_slice = a;
        for &val in b {
            match a_slice.binary_search(&val) {
                Ok(idx) => {
                    out.push(val);
                    a_slice = &a_slice[idx + 1..];
                }
                Err(idx) => {
                    a_slice = &a_slice[idx..];
                }
            }
            if a_slice.is_empty() {
                break;
            }
        }
        return;
    }
    let mut i = 0;
    let mut j = 0;
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
    let mut i = 0;
    let mut j = 0;
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
    let mut i = 0;
    let mut j = 0;
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
fn append_u64(buf: &mut Vec<u8>, mut n: u64) {
    if n == 0 {
        buf.push(b'0');
        return;
    }
    let mut temp = [0u8; 20];
    let mut i = 20;
    while n > 0 {
        i -= 1;
        temp[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    buf.extend_from_slice(&temp[i..]);
}

#[inline(always)]
fn append_u32(buf: &mut Vec<u8>, mut n: u32) {
    if n == 0 {
        buf.push(b'0');
        return;
    }
    let mut temp = [0u8; 10];
    let mut i = 10;
    while n > 0 {
        i -= 1;
        temp[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    buf.extend_from_slice(&temp[i..]);
}

#[inline(always)]
fn fast_write_results(buf: &mut Vec<u8>, ids: &[u32]) {
    let count = ids.len();
    if count == 0 {
        buf.extend_from_slice(b"0 0\n");
        return;
    }
    let sum: u64 = ids.iter().map(|&x| x as u64).sum();
    append_u32(buf, count as u32);
    buf.push(b' ');
    append_u64(buf, sum);
    let take = count.min(10);
    for &id in &ids[..take] {
        buf.push(b' ');
        append_u32(buf, id);
    }
    buf.push(b'\n');
}

enum ParsedQuery {
    Bool {
        first_term: Option<u32>,
        ops: Vec<(u8, Option<u32>)>, // 0: AND, 1: OR, 2: NOT
    },
    Phrase {
        term_ids: Vec<u32>,
    },
    Empty,
}

fn main() -> std::io::Result<()> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string());

    let docs_path = Path::new(&data_dir).join("docs.txt");
    let queries_path = Path::new(&data_dir).join("queries.txt");
    let out_path = Path::new(&out_dir).join("results.txt");

    // Phase 1: Read queries FIRST to extract exact needed vocabulary
    let queries_file = File::open(&queries_path)?;
    let queries_reader = BufReader::with_capacity(1024 * 1024, queries_file);

    let mut vocab: HashMap<Box<[u8]>, u32, FastBuildHasher> =
        HashMap::with_capacity_and_hasher(32768, FastBuildHasher::default());
    let mut raw_queries = Vec::with_capacity(50000);
    let mut bloom = vec![0u64; 8192];

    for line_res in queries_reader.lines() {
        let line = line_res?;
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix("BOOL ") {
            let parts: Vec<&str> = rest.split(' ').collect();
            for chunk in parts.iter().step_by(2) {
                let term = chunk.as_bytes();
                if !vocab.contains_key(term) {
                    let id = vocab.len() as u32;
                    vocab.insert(term.into(), id);
                    let h = hash_bytes(term);
                    let b1 = (h & 0x7FFFF) as usize;
                    let b2 = ((h >> 20) & 0x7FFFF) as usize;
                    bloom[b1 >> 6] |= 1u64 << (b1 & 63);
                    bloom[b2 >> 6] |= 1u64 << (b2 & 63);
                }
            }
        } else if let Some(rest) = line.strip_prefix("PHRASE \"") {
            let phrase_text = rest.strip_suffix('"').unwrap_or(rest);
            for term_str in phrase_text.split(' ') {
                let term = term_str.as_bytes();
                if !vocab.contains_key(term) {
                    let id = vocab.len() as u32;
                    vocab.insert(term.into(), id);
                    let h = hash_bytes(term);
                    let b1 = (h & 0x7FFFF) as usize;
                    let b2 = ((h >> 20) & 0x7FFFF) as usize;
                    bloom[b1 >> 6] |= 1u64 << (b1 & 63);
                    bloom[b2 >> 6] |= 1u64 << (b2 & 63);
                }
            }
        }

        raw_queries.push(line);
    }

    // Parse queries into integer token IDs
    let mut parsed_queries = Vec::with_capacity(raw_queries.len());
    for line in &raw_queries {
        if let Some(rest) = line.strip_prefix("BOOL ") {
            let parts: Vec<&str> = rest.split(' ').collect();
            if parts.is_empty() {
                parsed_queries.push(ParsedQuery::Empty);
                continue;
            }

            let first_term = vocab.get(parts[0].as_bytes()).copied();
            let mut ops = Vec::new();
            for chunk in parts[1..].chunks(2) {
                if chunk.len() < 2 {
                    break;
                }
                let op_code = match chunk[0] {
                    "AND" => 0,
                    "OR" => 1,
                    "NOT" => 2,
                    _ => 0,
                };
                let term_id = vocab.get(chunk[1].as_bytes()).copied();
                ops.push((op_code, term_id));
            }
            parsed_queries.push(ParsedQuery::Bool {
                first_term,
                ops,
            });
        } else if let Some(rest) = line.strip_prefix("PHRASE \"") {
            let phrase_text = rest.strip_suffix('"').unwrap_or(rest);
            let phrase_terms: Vec<&str> = phrase_text.split(' ').collect();
            let mut term_ids = Vec::with_capacity(phrase_terms.len());
            for pt in phrase_terms {
                if let Some(&id) = vocab.get(pt.as_bytes()) {
                    term_ids.push(id);
                }
            }
            parsed_queries.push(ParsedQuery::Phrase { term_ids });
        } else {
            parsed_queries.push(ParsedQuery::Empty);
        }
    }

    // Phase 2: Parallel document indexing across 6 workers
    let vocab_size = vocab.len();
    let docs_file = File::open(&docs_path)?;
    let file_len = docs_file.metadata()?.len() as usize;

    const NUM_INDEX_WORKERS: usize = 6;
    let approx_chunk = file_len / NUM_INDEX_WORKERS;
    let mut chunk_bounds = Vec::with_capacity(NUM_INDEX_WORKERS + 1);
    chunk_bounds.push(0);

    let mut scan_buf = vec![0u8; 4096];
    for i in 1..NUM_INDEX_WORKERS {
        let approx_pos = i * approx_chunk;
        docs_file.read_exact_at(&mut scan_buf, approx_pos as u64)?;
        let mut offset = 0;
        while offset < scan_buf.len() && scan_buf[offset] != b'\n' {
            offset += 1;
        }
        chunk_bounds.push(approx_pos + offset + 1);
    }
    chunk_bounds.push(file_len);
    drop(scan_buf);

    struct WorkerIndexData {
        index: Vec<Vec<u32>>,
        doc_text_offsets: Vec<u64>,
        doc_text_lens: Vec<u32>,
    }

    let mut worker_data: Vec<WorkerIndexData> = Vec::with_capacity(NUM_INDEX_WORKERS);
    thread::scope(|s| {
        let mut handles = Vec::with_capacity(NUM_INDEX_WORKERS);
        let bloom_ref = &bloom;
        let vocab_ref = &vocab;
        let docs_path_ref = &docs_path;

        for w_idx in 0..NUM_INDEX_WORKERS {
            let start_pos = chunk_bounds[w_idx];
            let end_pos = chunk_bounds[w_idx + 1];

            handles.push(s.spawn(move || {
                let d_file = File::open(docs_path_ref).unwrap();
                let mut local_index: Vec<Vec<u32>> = (0..vocab_size).map(|_| Vec::new()).collect();
                let mut last_seen = vec![u32::MAX; vocab_size];
                let mut text_offsets = Vec::with_capacity(45_000);
                let mut text_lens = Vec::with_capacity(45_000);

                let mut chunk_buf = vec![0u8; 4 * 1024 * 1024];
                let mut cur_offset = start_pos;
                let mut rem_len = 0;

                while cur_offset < end_pos || rem_len > 0 {
                    let to_read = (4 * 1024 * 1024 - rem_len).min(end_pos.saturating_sub(cur_offset));
                    let read_bytes = if to_read > 0 {
                        d_file.read_exact_at(&mut chunk_buf[rem_len..rem_len + to_read], cur_offset as u64).unwrap();
                        cur_offset += to_read;
                        to_read
                    } else {
                        0
                    };

                    let total_available = rem_len + read_bytes;
                    if total_available == 0 {
                        break;
                    }

                    let mut last_nl = total_available;
                    while last_nl > 0 && chunk_buf[last_nl - 1] != b'\n' {
                        last_nl -= 1;
                    }

                    let slice = &chunk_buf[..last_nl];
                    let mut p = 0;
                    while p < last_nl {
                        // Scan doc_id up to TAB
                        let mut doc_id = 0u32;
                        while p < last_nl && slice[p] >= b'0' && slice[p] <= b'9' {
                            doc_id = doc_id * 10 + (slice[p] - b'0') as u32;
                            p += 1;
                        }
                        while p < last_nl && slice[p] != b'\t' && slice[p] != b'\n' {
                            p += 1;
                        }
                        if p >= last_nl {
                            break;
                        }
                        let text_start = if slice[p] == b'\t' {
                            p += 1;
                            p
                        } else {
                            p
                        };

                        let text_file_offset = (cur_offset - total_available + text_start) as u64;

                        // Tokenize document text until '\n'
                        while p < last_nl && slice[p] != b'\n' {
                            let b = slice[p];
                            if b.is_ascii_alphanumeric() {
                                let start = p;
                                let mut hash = 0x517cc1b727220a95u64;
                                let mut count = 0;
                                while p < last_nl && slice[p].is_ascii_alphanumeric() {
                                    if count < 32 {
                                        let lb = slice[p].to_ascii_lowercase();
                                        hash = hash.rotate_left(5) ^ (lb as u64);
                                        hash = hash.wrapping_mul(0x9e3779b97f4a7c15);
                                        count += 1;
                                    }
                                    p += 1;
                                }

                                let b1 = (hash & 0x7FFFF) as usize;
                                let b2 = ((hash >> 20) & 0x7FFFF) as usize;
                                let in_bloom = ((bloom_ref[b1 >> 6] >> (b1 & 63)) & 1) != 0
                                            && ((bloom_ref[b2 >> 6] >> (b2 & 63)) & 1) != 0;

                                if in_bloom {
                                    let mut lower = [0u8; 32];
                                    for k in 0..count {
                                        lower[k] = slice[start + k].to_ascii_lowercase();
                                    }
                                    let key = &lower[..count];
                                    if let Some(&id) = vocab_ref.get(key) {
                                        let id_usize = id as usize;
                                        if last_seen[id_usize] != doc_id {
                                            last_seen[id_usize] = doc_id;
                                            local_index[id_usize].push(doc_id);
                                        }
                                    }
                                }
                            } else {
                                p += 1;
                            }
                        }

                        let text_end = p;
                        let text_len = (text_end - text_start) as u32;
                        text_offsets.push(text_file_offset);
                        text_lens.push(text_len);

                        if p < last_nl && slice[p] == b'\n' {
                            p += 1;
                        }
                    }

                    rem_len = total_available - last_nl;
                    if rem_len > 0 {
                        chunk_buf.copy_within(last_nl..total_available, 0);
                    }
                }

                WorkerIndexData {
                    index: local_index,
                    doc_text_offsets: text_offsets,
                    doc_text_lens: text_lens,
                }
            }));
        }

        for h in handles {
            worker_data.push(h.join().unwrap());
        }
    });

    // Merge worker indices into a single Compressed Sparse Row (CSR) flat array
    let mut offsets = vec![0u32; vocab_size + 1];
    for w in &worker_data {
        for (tid, list) in w.index.iter().enumerate() {
            offsets[tid + 1] += list.len() as u32;
        }
    }
    for i in 0..vocab_size {
        offsets[i + 1] += offsets[i];
    }
    let total_postings = offsets[vocab_size] as usize;
    let mut all_postings = vec![0u32; total_postings];

    let mut cur_pos = offsets.clone();
    for w in &worker_data {
        for (tid, list) in w.index.iter().enumerate() {
            let dest_start = cur_pos[tid] as usize;
            let dest_end = dest_start + list.len();
            all_postings[dest_start..dest_end].copy_from_slice(list);
            cur_pos[tid] += list.len() as u32;
        }
    }

    let mut doc_text_offsets = Vec::with_capacity(260_000);
    let mut doc_text_lens = Vec::with_capacity(260_000);
    for w in &worker_data {
        doc_text_offsets.extend_from_slice(&w.doc_text_offsets);
        doc_text_lens.extend_from_slice(&w.doc_text_lens);
    }
    drop(worker_data);

    // Phase 3: Parallel query execution
    let num_queries = parsed_queries.len();
    let num_threads = thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(12)
        .min(12)
        .max(1);

    let chunk_size = (num_queries + num_threads - 1) / num_threads;
    let mut thread_outputs: Vec<Vec<u8>> = (0..num_threads).map(|_| Vec::new()).collect();

    let offsets_ref = &offsets;
    let postings_ref = &all_postings;

    thread::scope(|s| {
        let queries_ref = &parsed_queries;
        let docs_file_ref = &docs_file;
        let doc_text_offsets_ref = &doc_text_offsets;
        let doc_text_lens_ref = &doc_text_lens;
        let vocab_ref = &vocab;

        let get_postings = |tid: u32| -> &[u32] {
            let t = tid as usize;
            let start = offsets_ref[t] as usize;
            let end = offsets_ref[t + 1] as usize;
            &postings_ref[start..end]
        };

        for (thread_idx, out_buf) in thread_outputs.iter_mut().enumerate() {
            let start = thread_idx * chunk_size;
            let end = (start + chunk_size).min(num_queries);
            if start >= end {
                continue;
            }

            s.spawn(move || {
                let mut cur_res = Vec::new();
                let mut next_res = Vec::new();
                let mut scratch = Vec::new();
                let mut doc_buf = vec![0u8; 8192];
                let mut doc_tokens = Vec::with_capacity(256);

                for q_idx in start..end {
                    match &queries_ref[q_idx] {
                        ParsedQuery::Bool { first_term, ops } => {
                            cur_res.clear();
                            if let Some(fid) = *first_term {
                                cur_res.extend_from_slice(get_postings(fid));
                            }

                            for &(op, opt_tid) in ops {
                                let postings: &[u32] = match opt_tid {
                                    Some(tid) => get_postings(tid),
                                    None => &[],
                                };

                                match op {
                                    0 => {
                                        intersect_sorted(&cur_res, postings, &mut next_res);
                                        std::mem::swap(&mut cur_res, &mut next_res);
                                    }
                                    1 => {
                                        union_sorted(&cur_res, postings, &mut next_res);
                                        std::mem::swap(&mut cur_res, &mut next_res);
                                    }
                                    2 => {
                                        difference_sorted(&cur_res, postings, &mut next_res);
                                        std::mem::swap(&mut cur_res, &mut next_res);
                                    }
                                    _ => {}
                                }
                            }

                            fast_write_results(out_buf, &cur_res);
                        }
                        ParsedQuery::Phrase { term_ids } => {
                            if term_ids.is_empty() {
                                out_buf.extend_from_slice(b"0 0\n");
                                continue;
                            }

                            // Check if any term is absent
                            let mut any_empty = false;
                            for &tid in term_ids {
                                if get_postings(tid).is_empty() {
                                    any_empty = true;
                                    break;
                                }
                            }
                            if any_empty {
                                out_buf.extend_from_slice(b"0 0\n");
                                continue;
                            }

                            // Order term checks by posting length ascending
                            let mut order: Vec<usize> = (0..term_ids.len()).collect();
                            order.sort_unstable_by_key(|&idx| {
                                get_postings(term_ids[idx]).len()
                            });

                            cur_res.clear();
                            let rarest_tid = term_ids[order[0]];
                            cur_res.extend_from_slice(get_postings(rarest_tid));

                            for &idx in &order[1..] {
                                if cur_res.is_empty() {
                                    break;
                                }
                                let tid = term_ids[idx];
                                intersect_sorted(&cur_res, get_postings(tid), &mut scratch);
                                std::mem::swap(&mut cur_res, &mut scratch);
                            }

                            // Verify consecutive positions on candidate docs
                            let k = term_ids.len();
                            let mut matches = Vec::new();
                            for &c_doc in &cur_res {
                                let f_offset = doc_text_offsets_ref[c_doc as usize];
                                let t_len = doc_text_lens_ref[c_doc as usize] as usize;
                                if t_len == 0 {
                                    continue;
                                }
                                if t_len > doc_buf.len() {
                                    doc_buf.resize(t_len, 0);
                                }
                                if docs_file_ref.read_exact_at(&mut doc_buf[..t_len], f_offset).is_err() {
                                    continue;
                                }

                                doc_tokens.clear();
                                let d_slice = &doc_buf[..t_len];
                                let mut dp = 0;
                                while dp < t_len {
                                    if d_slice[dp].is_ascii_alphanumeric() {
                                        let t_start = dp;
                                        let mut count = 0;
                                        while dp < t_len && d_slice[dp].is_ascii_alphanumeric() {
                                            count += 1;
                                            dp += 1;
                                        }
                                        let mut lower = [0u8; 32];
                                        let c = count.min(32);
                                        for k_idx in 0..c {
                                            lower[k_idx] = d_slice[t_start + k_idx].to_ascii_lowercase();
                                        }
                                        let tid = match vocab_ref.get(&lower[..c]) {
                                            Some(&id) => id,
                                            None => u32::MAX,
                                        };
                                        doc_tokens.push(tid);
                                    } else {
                                        dp += 1;
                                    }
                                }

                                if doc_tokens.windows(k).any(|w| w == term_ids.as_slice()) {
                                    matches.push(c_doc);
                                }
                            }

                            fast_write_results(out_buf, &matches);
                        }
                        ParsedQuery::Empty => {
                            out_buf.extend_from_slice(b"0 0\n");
                        }
                    }
                }
            });
        }
    });

    // Write aggregated outputs sequentially to results.txt
    fs::create_dir_all(&out_dir)?;
    let out_file = File::create(&out_path)?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, out_file);

    for out_buf in thread_outputs {
        writer.write_all(&out_buf)?;
    }

    writer.flush()?;

    Ok(())
}
