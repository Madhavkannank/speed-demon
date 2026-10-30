use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

fn tokenize(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut tokens = Vec::new();
    let mut current = Vec::new();
    for &b in bytes {
        if b.is_ascii_alphanumeric() {
            current.push(b.to_ascii_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn format_results(ids: &[u32]) -> String {
    let count = ids.len();
    if count == 0 {
        return "0 0".to_string();
    }
    let sum: u64 = ids.iter().map(|&x| x as u64).sum();
    let mut s = format!("{} {}", count, sum);
    let take = count.min(10);
    for &id in &ids[..take] {
        s.push(' ');
        s.push_str(&id.to_string());
    }
    s
}

fn main() -> std::io::Result<()> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string());

    let docs_path = Path::new(&data_dir).join("docs.txt");
    let queries_path = Path::new(&data_dir).join("queries.txt");
    let out_path = Path::new(&out_dir).join("results.txt");

    // Load corpus
    let docs_file = File::open(&docs_path)?;
    let docs_reader = BufReader::with_capacity(512 * 1024, docs_file);

    let mut doc_tokens: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut index: HashMap<Vec<u8>, Vec<u32>> = HashMap::new();

    for (line_idx, line_res) in docs_reader.lines().enumerate() {
        let line = line_res?;
        let tab_pos = line.find('\t').expect("invalid doc format");
        let text = &line[tab_pos + 1..];

        let tokens = tokenize(text.as_bytes());
        let doc_id = line_idx as u32;

        let mut unique_terms: HashSet<&[u8]> = HashSet::new();
        for t in &tokens {
            if unique_terms.insert(t.as_slice()) {
                index.entry(t.clone()).or_default().push(doc_id);
            }
        }
        doc_tokens.push(tokens);
    }

    // Process queries
    let queries_file = File::open(&queries_path)?;
    let queries_reader = BufReader::with_capacity(512 * 1024, queries_file);

    fs::create_dir_all(&out_dir)?;
    let out_file = File::create(&out_path)?;
    let mut writer = BufWriter::with_capacity(512 * 1024, out_file);

    let empty_vec: Vec<u32> = Vec::new();

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

            let first_term = parts[0].to_ascii_lowercase().into_bytes();
            let mut current: HashSet<u32> = index
                .get(&first_term)
                .map(|v| v.iter().copied().collect())
                .unwrap_or_default();

            for chunk in parts[1..].chunks(2) {
                if chunk.len() < 2 {
                    break;
                }
                let op = chunk[0];
                let term = chunk[1].to_ascii_lowercase().into_bytes();
                let postings = index.get(&term).unwrap_or(&empty_vec);

                match op {
                    "AND" => {
                        let post_set: HashSet<u32> = postings.iter().copied().collect();
                        current.retain(|id| post_set.contains(id));
                    }
                    "OR" => {
                        for &id in postings {
                            current.insert(id);
                        }
                    }
                    "NOT" => {
                        for &id in postings {
                            current.remove(&id);
                        }
                    }
                    _ => {}
                }
            }

            let mut sorted_ids: Vec<u32> = current.into_iter().collect();
            sorted_ids.sort_unstable();
            writeln!(writer, "{}", format_results(&sorted_ids))?;
        } else if let Some(rest) = line.strip_prefix("PHRASE \"") {
            let phrase_text = rest.strip_suffix('"').unwrap_or(rest);
            let terms: Vec<Vec<u8>> = phrase_text
                .split(' ')
                .map(|s| s.to_ascii_lowercase().into_bytes())
                .collect();

            if terms.is_empty() {
                writeln!(writer, "0 0")?;
                continue;
            }

            // Find candidate documents containing all phrase terms
            let mut candidate_set: Option<HashSet<u32>> = None;
            for term in &terms {
                let postings = index.get(term).unwrap_or(&empty_vec);
                let post_set: HashSet<u32> = postings.iter().copied().collect();
                if let Some(ref mut c) = candidate_set {
                    c.retain(|id| post_set.contains(id));
                    if c.is_empty() {
                        break;
                    }
                } else {
                    candidate_set = Some(post_set);
                }
            }

            let mut matches: Vec<u32> = Vec::new();
            if let Some(candidates) = candidate_set {
                let k = terms.len();
                let mut sorted_cands: Vec<u32> = candidates.into_iter().collect();
                sorted_cands.sort_unstable();

                for doc_id in sorted_cands {
                    let d_tokens = &doc_tokens[doc_id as usize];
                    if d_tokens.len() < k {
                        continue;
                    }
                    let mut matched = false;
                    for window in d_tokens.windows(k) {
                        if window == terms.as_slice() {
                            matched = true;
                            break;
                        }
                    }
                    if matched {
                        matches.push(doc_id);
                    }
                }
            }

            writeln!(writer, "{}", format_results(&matches))?;
        }
    }

    writer.flush()?;
    Ok(())
}
