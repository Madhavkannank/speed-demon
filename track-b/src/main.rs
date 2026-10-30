use std::collections::HashMap;
use std::fs::{self, File};
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
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
fn write_results<W: Write>(writer: &mut W, ids: &[u32]) -> std::io::Result<()> {
    let count = ids.len();
    if count == 0 {
        return writeln!(writer, "0 0");
    }
    let sum: u64 = ids.iter().map(|&x| x as u64).sum();
    write!(writer, "{} {}", count, sum)?;
    let take = count.min(10);
    for &id in &ids[..take] {
        write!(writer, " {}", id)?;
    }
    writeln!(writer)
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

    // Phase 2: Index documents using streaming chunks
    let vocab_size = vocab.len();
    let mut index: Vec<Vec<u32>> = (0..vocab_size).map(|_| Vec::new()).collect();
    let mut last_seen_doc = vec![u32::MAX; vocab_size];

    let mut doc_text_offsets: Vec<u64> = Vec::with_capacity(260_000);
    let mut doc_text_lens: Vec<u32> = Vec::with_capacity(260_000);

    let docs_file = File::open(&docs_path)?;
    let mut chunk_buf = vec![0u8; 4 * 1024 * 1024];
    let mut remainder = Vec::new();
    let mut reader = BufReader::with_capacity(1024 * 1024, &docs_file);
    let mut current_file_offset = 0u64;
    let mut doc_id = 0u32;

    loop {
        let rem_len = remainder.len();
        if rem_len > 0 {
            chunk_buf[..rem_len].copy_from_slice(&remainder);
            remainder.clear();
        }
        let n = reader.read(&mut chunk_buf[rem_len..])?;
        if n == 0 && rem_len == 0 {
            break;
        }
        let total_len = rem_len + n;
        let mut last_nl = total_len;
        while last_nl > 0 && chunk_buf[last_nl - 1] != b'\n' {
            last_nl -= 1;
        }

        let valid_len = if n == 0 {
            total_len
        } else if last_nl > 0 {
            remainder.extend_from_slice(&chunk_buf[last_nl..total_len]);
            last_nl
        } else {
            remainder.extend_from_slice(&chunk_buf[..total_len]);
            current_file_offset += total_len as u64;
            continue;
        };

        let slice = &chunk_buf[..valid_len];
        let mut p = 0;
        while p < valid_len {
            // Scan doc_id up to TAB
            while p < valid_len && slice[p] != b'\t' && slice[p] != b'\n' {
                p += 1;
            }
            if p >= valid_len {
                break;
            }
            let text_start = if slice[p] == b'\t' {
                p += 1;
                p
            } else {
                p
            };

            let text_file_offset = current_file_offset + text_start as u64;

            // Tokenize document text until '\n'
            while p < valid_len && slice[p] != b'\n' {
                let b = slice[p];
                if b.is_ascii_alphanumeric() {
                    let start = p;
                    let mut hash = 0x517cc1b727220a95u64;
                    let mut count = 0;
                    while p < valid_len && slice[p].is_ascii_alphanumeric() {
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
                    let in_bloom = ((bloom[b1 >> 6] >> (b1 & 63)) & 1) != 0
                                && ((bloom[b2 >> 6] >> (b2 & 63)) & 1) != 0;

                    if in_bloom {
                        let mut lower = [0u8; 32];
                        for k in 0..count {
                            lower[k] = slice[start + k].to_ascii_lowercase();
                        }
                        let key = &lower[..count];
                        if let Some(&id) = vocab.get(key) {
                            let id_usize = id as usize;
                            if last_seen_doc[id_usize] != doc_id {
                                last_seen_doc[id_usize] = doc_id;
                                index[id_usize].push(doc_id);
                            }
                        }
                    }
                } else {
                    p += 1;
                }
            }

            let text_end = p;
            let text_len = (text_end - text_start) as u32;
            doc_text_offsets.push(text_file_offset);
            doc_text_lens.push(text_len);
            doc_id += 1;

            if p < valid_len && slice[p] == b'\n' {
                p += 1;
            }
        }

        current_file_offset += valid_len as u64;
    }

    // Phase 3: Parallel query execution
    let num_queries = parsed_queries.len();
    let num_threads = thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(12)
        .min(12)
        .max(1);

    let chunk_size = (num_queries + num_threads - 1) / num_threads;
    let mut thread_outputs: Vec<Vec<u8>> = (0..num_threads).map(|_| Vec::new()).collect();

    thread::scope(|s| {
        let index_ref = &index;
        let queries_ref = &parsed_queries;
        let docs_file_ref = &docs_file;
        let doc_text_offsets_ref = &doc_text_offsets;
        let doc_text_lens_ref = &doc_text_lens;
        let vocab_ref = &vocab;

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
                                cur_res.extend_from_slice(&index_ref[fid as usize]);
                            }

                            for &(op, opt_tid) in ops {
                                let postings: &[u32] = match opt_tid {
                                    Some(tid) => &index_ref[tid as usize],
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

                            let _ = write_results(out_buf, &cur_res);
                        }
                        ParsedQuery::Phrase { term_ids } => {
                            if term_ids.is_empty() {
                                let _ = writeln!(out_buf, "0 0");
                                continue;
                            }

                            // Check if any term is absent
                            let mut any_empty = false;
                            for &tid in term_ids {
                                if index_ref[tid as usize].is_empty() {
                                    any_empty = true;
                                    break;
                                }
                            }
                            if any_empty {
                                let _ = writeln!(out_buf, "0 0");
                                continue;
                            }

                            // Order term checks by posting length ascending
                            let mut order: Vec<usize> = (0..term_ids.len()).collect();
                            order.sort_unstable_by_key(|&idx| {
                                index_ref[term_ids[idx] as usize].len()
                            });

                            cur_res.clear();
                            let rarest_tid = term_ids[order[0]];
                            cur_res.extend_from_slice(&index_ref[rarest_tid as usize]);

                            for &idx in &order[1..] {
                                if cur_res.is_empty() {
                                    break;
                                }
                                let tid = term_ids[idx];
                                intersect_sorted(&cur_res, &index_ref[tid as usize], &mut scratch);
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

                            let _ = write_results(out_buf, &matches);
                        }
                        ParsedQuery::Empty => {
                            let _ = writeln!(out_buf, "0 0");
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
