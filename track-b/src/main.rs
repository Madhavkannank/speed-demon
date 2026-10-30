use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

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

#[inline(always)]
fn tokenize_to_interned(
    bytes: &[u8],
    vocab: &mut HashMap<Box<[u8]>, u32>,
    tokens_out: &mut Vec<u32>,
) {
    tokens_out.clear();
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        let b = bytes[i];
        if b.is_ascii_alphanumeric() {
            let start = i;
            while i < len && bytes[i].is_ascii_alphanumeric() {
                i += 1;
            }
            let slice = &bytes[start..i];
            let mut lower = [0u8; 32];
            let tok_len = slice.len().min(32);
            for k in 0..tok_len {
                lower[k] = slice[k].to_ascii_lowercase();
            }
            let key = &lower[..tok_len];
            let id = match vocab.get(key) {
                Some(&id) => id,
                None => {
                    let id = vocab.len() as u32;
                    vocab.insert(key.into(), id);
                    id
                }
            };
            tokens_out.push(id);
        } else {
            i += 1;
        }
    }
}

fn main() -> std::io::Result<()> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string());

    let docs_path = Path::new(&data_dir).join("docs.txt");
    let queries_path = Path::new(&data_dir).join("queries.txt");
    let out_path = Path::new(&out_dir).join("results.txt");

    let docs_file = File::open(&docs_path)?;
    let mut docs_reader = BufReader::with_capacity(1024 * 1024, docs_file);

    let mut vocab: HashMap<Box<[u8]>, u32> = HashMap::with_capacity(65536);
    let mut doc_tokens: Vec<Vec<u32>> = Vec::with_capacity(30000);
    let mut index: HashMap<u32, Vec<u32>> = HashMap::with_capacity(65536);

    let mut line = Vec::with_capacity(4096);
    let mut doc_toks = Vec::with_capacity(512);

    let mut doc_id = 0u32;
    while docs_reader.read_until(b'\n', &mut line)? > 0 {
        let tab_pos = line.iter().position(|&b| b == b'\t').unwrap_or(line.len());
        let text = if tab_pos < line.len() {
            &line[tab_pos + 1..]
        } else {
            &[]
        };

        tokenize_to_interned(text, &mut vocab, &mut doc_toks);

        // Record postings for distinct terms in this document
        // Sort and dedup to ensure one doc_id per term
        let mut unique_toks = doc_toks.clone();
        unique_toks.sort_unstable();
        unique_toks.dedup();

        for &t in &unique_toks {
            index.entry(t).or_default().push(doc_id);
        }

        doc_tokens.push(std::mem::take(&mut doc_toks));
        doc_id += 1;
        line.clear();
    }

    // Process queries
    let queries_file = File::open(&queries_path)?;
    let queries_reader = BufReader::with_capacity(1024 * 1024, queries_file);

    fs::create_dir_all(&out_dir)?;
    let out_file = File::create(&out_path)?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, out_file);

    let mut cur_res = Vec::new();
    let mut next_res = Vec::new();
    let mut scratch = Vec::new();

    for line_res in queries_reader.lines() {
        let line = line_res?;
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix("BOOL ") {
            let parts: Vec<&str> = rest.split(' ').collect();
            if parts.is_empty() {
                writeln!(writer, "0 0")?;
                continue;
            }

            let first_term = parts[0].as_bytes();
            let first_id = vocab.get(first_term).copied();
            cur_res.clear();
            if let Some(fid) = first_id {
                if let Some(postings) = index.get(&fid) {
                    cur_res.extend_from_slice(postings);
                }
            }

            for chunk in parts[1..].chunks(2) {
                if chunk.len() < 2 {
                    break;
                }
                let op = chunk[0];
                let term = chunk[1].as_bytes();
                let term_id = vocab.get(term).copied();
                let postings: &[u32] = match term_id {
                    Some(tid) => index.get(&tid).map(|v| v.as_slice()).unwrap_or(&[]),
                    None => &[],
                };

                match op {
                    "AND" => {
                        intersect_sorted(&cur_res, postings, &mut next_res);
                        std::mem::swap(&mut cur_res, &mut next_res);
                    }
                    "OR" => {
                        union_sorted(&cur_res, postings, &mut next_res);
                        std::mem::swap(&mut cur_res, &mut next_res);
                    }
                    "NOT" => {
                        difference_sorted(&cur_res, postings, &mut next_res);
                        std::mem::swap(&mut cur_res, &mut next_res);
                    }
                    _ => {}
                }
            }

            write_results(&mut writer, &cur_res)?;
        } else if let Some(rest) = line.strip_prefix("PHRASE \"") {
            let phrase_text = rest.strip_suffix('"').unwrap_or(rest);
            let phrase_terms: Vec<&str> = phrase_text.split(' ').collect();

            if phrase_terms.is_empty() {
                writeln!(writer, "0 0")?;
                continue;
            }

            let mut phrase_ids: Vec<u32> = Vec::with_capacity(phrase_terms.len());
            let mut all_found = true;
            for pt in &phrase_terms {
                if let Some(&id) = vocab.get(pt.as_bytes()) {
                    phrase_ids.push(id);
                } else {
                    all_found = false;
                    break;
                }
            }

            if !all_found {
                writeln!(writer, "0 0")?;
                continue;
            }

            // Order term checks by posting length ascending
            let mut term_indices: Vec<usize> = (0..phrase_ids.len()).collect();
            term_indices.sort_unstable_by_key(|&idx| {
                index.get(&phrase_ids[idx]).map(|v| v.len()).unwrap_or(0)
            });

            cur_res.clear();
            let rarest_id = phrase_ids[term_indices[0]];
            if let Some(postings) = index.get(&rarest_id) {
                cur_res.extend_from_slice(postings);
            }

            for &idx in &term_indices[1..] {
                if cur_res.is_empty() {
                    break;
                }
                let tid = phrase_ids[idx];
                let postings: &[u32] = index.get(&tid).map(|v| v.as_slice()).unwrap_or(&[]);
                intersect_sorted(&cur_res, postings, &mut scratch);
                std::mem::swap(&mut cur_res, &mut scratch);
            }

            // Verify consecutive match on candidates
            let mut matches = Vec::new();
            let k = phrase_ids.len();
            for &c_doc in &cur_res {
                let d_tokens = &doc_tokens[c_doc as usize];
                if d_tokens.windows(k).any(|w| w == phrase_ids.as_slice()) {
                    matches.push(c_doc);
                }
            }

            write_results(&mut writer, &matches)?;
        }
    }

    writer.flush()?;
    Ok(())
}
