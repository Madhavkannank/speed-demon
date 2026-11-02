import os
import subprocess
import json

# Compile and run a timing probe for Track B baseline and current
timing_probe_src = """
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

fn main() {
    let t0 = Instant::now();
    let q_bytes = std::fs::read("/tmp/sd-full-b/queries.txt").unwrap();
    let t_read_queries = Instant::now();

    // count lines and terms
    let mut num_queries = 0;
    let mut bool_cnt = 0;
    let mut phrase_cnt = 0;
    for line in q_bytes.split(|&b| b == b'\\n') {
        if line.is_empty() { continue; }
        num_queries += 1;
        if line.starts_with(b"BOOL ") { bool_cnt += 1; }
        else if line.starts_with(b"PHRASE ") { phrase_cnt += 1; }
    }
    let t_parse_queries = Instant::now();

    println!("Queries: total={}, bool={}, phrase={}", num_queries, bool_cnt, phrase_cnt);
    println!("read_queries_time: {:.2} ms", (t_read_queries - t0).as_secs_f64() * 1000.0);
    println!("parse_queries_time: {:.2} ms", (t_parse_queries - t_read_queries).as_secs_f64() * 1000.0);
}
"""
with open("/tmp/probe.rs", "w") as f:
    f.write(timing_probe_src)
subprocess.run(["rustc", "-O", "-C", "target-cpu=x86-64-v3", "/tmp/probe.rs", "-o", "/tmp/probe"], check=True)
p = subprocess.run(["taskset", "-c", "0-11", "/tmp/probe"], capture_output=True, text=True, check=True)
print(p.stdout)
