use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

struct Group {
    latencies: Vec<u32>,
    errors: usize,
}

fn percentile(sorted: &[u32], p: usize) -> u32 {
    let n = sorted.len();
    let rank = (p * n + 99) / 100;
    sorted[rank - 1]
}

fn main() -> std::io::Result<()> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string());

    let input_path = Path::new(&data_dir).join("requests.log");
    let out_path = Path::new(&out_dir).join("summary.txt");

    let file = File::open(&input_path)?;
    let reader = BufReader::with_capacity(512 * 1024, file);

    let mut groups: HashMap<(String, String), Group> = HashMap::new();

    for line_res in reader.lines() {
        let line = line_res?;
        if line.is_empty() {
            continue;
        }

        let mut parts = line.split(' ');
        let ts = match parts.next() {
            Some(t) => t,
            None => continue,
        };
        let _method = parts.next();
        let endpoint = match parts.next() {
            Some(e) => e,
            None => continue,
        };
        let status_str = match parts.next() {
            Some(s) => s,
            None => continue,
        };
        let latency_str = match parts.next() {
            Some(l) => l,
            None => continue,
        };

        if ts.len() < 13 {
            continue;
        }
        let mut hour = String::with_capacity(20);
        hour.push_str(&ts[0..13]);
        hour.push_str(":00:00Z");

        let status: u16 = status_str.parse().unwrap_or(0);
        let latency: u32 = latency_str.parse().unwrap_or(0);

        let entry = groups.entry((hour, endpoint.to_string())).or_insert_with(|| Group {
            latencies: Vec::new(),
            errors: 0,
        });

        entry.latencies.push(latency);
        if (500..=599).contains(&status) {
            entry.errors += 1;
        }
    }

    let mut keys: Vec<(String, String)> = groups.keys().cloned().collect();
    keys.sort_unstable();

    fs::create_dir_all(&out_dir)?;
    let out_file = File::create(&out_path)?;
    let mut writer = BufWriter::with_capacity(512 * 1024, out_file);

    for key in keys {
        let group = groups.get_mut(&key).unwrap();
        group.latencies.sort_unstable();

        let count = group.latencies.len();
        let p50 = percentile(&group.latencies, 50);
        let p95 = percentile(&group.latencies, 95);
        let p99 = percentile(&group.latencies, 99);
        let errors = group.errors;
        let ppm = (errors as u64 * 1_000_000) / count as u64;

        writeln!(
            writer,
            "{} {} {} {} {} {} {} {}",
            key.0, key.1, count, p50, p95, p99, errors, ppm
        )?;
    }

    writer.flush()?;
    Ok(())
}
