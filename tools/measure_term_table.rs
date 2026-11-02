use std::fs::File;
use std::io::{BufRead, BufReader};
use std::time::Instant;

#[inline(always)]
fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = 0x517cc1b727220a95u64;
    for &x in b {
        h = (h.rotate_left(5) ^ (x as u64)).wrapping_mul(0x9e3779b97f4a7c15);
    }
    h
}

fn main() {
    let queries_path = "/tmp/sd-full-b/queries.txt";

    // Step 1: Read whole file to memory
    let t0 = Instant::now();
    let q_bytes = std::fs::read(queries_path).unwrap();
    let t_read = t0.elapsed();

    // Step 2: Current method (lines() + HashMap + 50k Strings)
    let t1 = Instant::now();
    let queries_reader = BufReader::with_capacity(1024 * 1024, File::open(queries_path).unwrap());
    let mut vocab = std::collections::HashMap::new();
    let mut raw_queries = Vec::with_capacity(50000);
    let mut bloom = vec![0u64; 32768];
    for line_res in queries_reader.lines() {
        let line = line_res.unwrap();
        if let Some(rest) = line.strip_prefix("BOOL ") {
            for chunk in rest.split(' ').step_by(2) {
                let t = chunk.as_bytes();
                if !vocab.contains_key(t) {
                    let id = vocab.len() as u32;
                    vocab.insert(t.to_vec(), id);
                    let h = hash_bytes(t);
                    let (b1, b2, b3, b4) = (
                        (h & 0x1FFFFF) as usize,
                        ((h >> 21) & 0x1FFFFF) as usize,
                        (h.rotate_left(17) & 0x1FFFFF) as usize,
                        (h.rotate_left(37) & 0x1FFFFF) as usize,
                    );
                    bloom[b1 >> 6] |= 1u64 << (b1 & 63);
                    bloom[b2 >> 6] |= 1u64 << (b2 & 63);
                    bloom[b3 >> 6] |= 1u64 << (b3 & 63);
                    bloom[b4 >> 6] |= 1u64 << (b4 & 63);
                }
            }
        } else if let Some(rest) = line.strip_prefix("PHRASE \"") {
            for term_str in rest.strip_suffix('"').unwrap_or(rest).split(' ') {
                let t = term_str.as_bytes();
                if !vocab.contains_key(t) {
                    let id = vocab.len() as u32;
                    vocab.insert(t.to_vec(), id);
                    let h = hash_bytes(t);
                    let (b1, b2, b3, b4) = (
                        (h & 0x1FFFFF) as usize,
                        ((h >> 21) & 0x1FFFFF) as usize,
                        (h.rotate_left(17) & 0x1FFFFF) as usize,
                        (h.rotate_left(37) & 0x1FFFFF) as usize,
                    );
                    bloom[b1 >> 6] |= 1u64 << (b1 & 63);
                    bloom[b2 >> 6] |= 1u64 << (b2 & 63);
                    bloom[b3 >> 6] |= 1u64 << (b3 & 63);
                    bloom[b4 >> 6] |= 1u64 << (b4 & 63);
                }
            }
        }
        raw_queries.push(line);
    }
    let t_current_pass1 = t1.elapsed();

    println!("Queries file size: {} bytes", q_bytes.len());
    println!("Step 1 (fs::read queries.txt): {:.2} ms", t_read.as_secs_f64() * 1000.0);
    println!("Current Pass 1 (BufReader.lines + HashMap + raw_queries): {:.2} ms (terms: {})",
        t_current_pass1.as_secs_f64() * 1000.0, vocab.len());
}
