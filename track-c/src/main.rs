use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::thread;

#[inline(always)]
fn hash_init() -> u64 {
    0x517cc1b727220a95u64
}

#[inline(always)]
fn hash_byte(h: u64, b: u8) -> u64 {
    (h.rotate_left(5) ^ (b as u64)).wrapping_mul(0x9e3779b97f4a7c15)
}

#[derive(Clone, Copy)]
struct LocalAgg {
    count: u64,
    id_sum: i64,
    amount_sum: i64,
    amount_min: i64,
    amount_max: i64,
    name_bytes: usize,
    note_bytes: usize,
}

impl LocalAgg {
    fn new() -> Self {
        Self {
            count: 0,
            id_sum: 0,
            amount_sum: 0,
            amount_min: i64::MAX,
            amount_max: i64::MIN,
            name_bytes: 0,
            note_bytes: 0,
        }
    }
}

struct WorkerOutput {
    aggs: Vec<LocalAgg>,
    names: Vec<Vec<u64>>,
}

fn format_cents(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let c = cents.abs();
    format!("{}{}.{:02}", sign, c / 100, c % 100)
}

fn load_regions(path: &Path) -> std::io::Result<(Vec<u8>, Vec<String>)> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    let _header = lines.next();

    let mut country_names = vec!["_UNMATCHED".to_string()];
    let mut country_to_id = std::collections::HashMap::new();
    country_to_id.insert("_UNMATCHED".to_string(), 0u8);

    let mut region_map = vec![0u8; 1_000_000];

    for line_res in lines {
        let line = line_res?;
        if line.is_empty() {
            continue;
        }
        if let Some((id_str, country)) = line.split_once(',') {
            let reg_id: usize = id_str.trim().parse().unwrap_or(0);
            let c_str = country.trim();
            let c_id = match country_to_id.get(c_str) {
                Some(&id) => id,
                None => {
                    let id = country_names.len() as u8;
                    country_names.push(c_str.to_string());
                    country_to_id.insert(c_str.to_string(), id);
                    id
                }
            };
            if reg_id < region_map.len() {
                region_map[reg_id] = c_id;
            }
        }
    }

    Ok((region_map, country_names))
}

const NUM_WORKERS: usize = 12;
const CHUNK_SIZE: usize = 1024 * 1024;

fn main() -> std::io::Result<()> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string());

    let regions_path = Path::new(&data_dir).join("regions.csv");
    let records_path = Path::new(&data_dir).join("records.csv");
    let out_path = Path::new(&out_dir).join("aggregates.txt");

    let (region_map, country_names) = load_regions(&regions_path)?;
    let num_countries = country_names.len();

    let file = File::open(&records_path)?;
    let file_len = file.metadata()?.len() as usize;

    if file_len == 0 {
        File::create(&out_path)?;
        return Ok(());
    }

    // Skip initial BOM + header
    let mut header_buf = [0u8; 128];
    file.read_exact_at(&mut header_buf, 0)?;
    let header_offset = if header_buf.starts_with(b"\xef\xbb\xbf") {
        let mut idx = 3;
        while idx < header_buf.len() && header_buf[idx] != b'\n' {
            idx += 1;
        }
        idx + 1
    } else {
        let mut idx = 0;
        while idx < header_buf.len() && header_buf[idx] != b'\n' {
            idx += 1;
        }
        idx + 1
    };

    // Find reliable boundaries for worker chunks
    let data_bytes = file_len - header_offset;
    let approx_chunk = data_bytes / NUM_WORKERS;
    let mut chunk_bounds = Vec::with_capacity(NUM_WORKERS + 1);
    chunk_bounds.push(header_offset);

    let mut scan_buf = vec![0u8; 4096];
    for i in 1..NUM_WORKERS {
        let approx_pos = header_offset + i * approx_chunk;
        file.read_exact_at(&mut scan_buf, approx_pos as u64)?;

        // Search for unambiguous record boundary:
        // .[0-9][0-9]\r?\n (\xEF\xBB\xBF)? [1-9][0-9]*,
        let mut found_pos = None;
        let mut p = 0;
        while p + 16 < scan_buf.len() {
            if scan_buf[p] == b'.'
                && scan_buf[p + 1].is_ascii_digit()
                && scan_buf[p + 2].is_ascii_digit()
            {
                let mut after = p + 3;
                if scan_buf[after] == b'\r' {
                    after += 1;
                }
                if scan_buf[after] == b'\n' {
                    let mut rec_start = after + 1;
                    if scan_buf[rec_start..].starts_with(b"\xef\xbb\xbf") {
                        rec_start += 3;
                    }
                    if scan_buf[rec_start].is_ascii_digit() && scan_buf[rec_start] != b'0' {
                        let mut d = rec_start + 1;
                        while d < scan_buf.len() && scan_buf[d].is_ascii_digit() {
                            d += 1;
                        }
                        if d < scan_buf.len() && scan_buf[d] == b',' {
                            found_pos = Some(approx_pos + rec_start);
                            break;
                        }
                    }
                }
            }
            p += 1;
        }

        chunk_bounds.push(found_pos.unwrap_or(approx_pos));
    }
    chunk_bounds.push(file_len);

    let region_map_ref: &'static [u8] = Box::leak(region_map.into_boxed_slice());

    // Spawn workers to parse chunks
    let mut handles = Vec::with_capacity(NUM_WORKERS);
    for i in 0..NUM_WORKERS {
        let start_pos = chunk_bounds[i];
        let end_pos = chunk_bounds[i + 1];
        let path = records_path.clone();

        let handle = thread::spawn(move || {
            let t_file = File::open(&path).unwrap();
            let mut buf = vec![0u8; CHUNK_SIZE + 8192];
            let mut aggs: Vec<LocalAgg> = vec![LocalAgg::new(); num_countries];
            let mut names: Vec<Vec<u64>> = (0..num_countries).map(|_| Vec::new()).collect();

            let mut cur_offset = start_pos;
            let mut rem_len = 0;

            while cur_offset < end_pos || rem_len > 0 {
                let to_read = (CHUNK_SIZE).min(end_pos.saturating_sub(cur_offset));
                let read_bytes = if to_read > 0 {
                    t_file.read_exact_at(&mut buf[rem_len..rem_len + to_read], cur_offset as u64).unwrap();
                    cur_offset += to_read;
                    to_read
                } else {
                    0
                };

                let total_available = rem_len + read_bytes;
                if total_available == 0 {
                    break;
                }

                // If not last chunk, find last record boundary
                let valid_len = if cur_offset >= end_pos {
                    total_available
                } else {
                    let mut p = total_available.saturating_sub(1);
                    let mut boundary = 0;
                    while p >= 16 {
                        if buf[p] == b'\n' {
                            let mut before = p;
                            if before > 0 && buf[before - 1] == b'\r' {
                                before -= 1;
                            }
                            if before >= 3 && buf[before - 3] == b'.'
                                && buf[before - 2].is_ascii_digit()
                                && buf[before - 1].is_ascii_digit()
                            {
                                boundary = p + 1;
                                break;
                            }
                        }
                        p -= 1;
                    }
                    if boundary == 0 { total_available } else { boundary }
                };

                let slice = &buf[..valid_len];
                let mut p = 0;

                while p < valid_len {
                    // 1. Skip BOM if present
                    if p + 3 <= valid_len && &slice[p..p + 3] == b"\xef\xbb\xbf" {
                        p += 3;
                    }

                    if p >= valid_len {
                        break;
                    }

                    // 2. id
                    let mut rec_id = 0i64;
                    while p < valid_len && slice[p] != b',' {
                        rec_id = rec_id * 10 + (slice[p] - b'0') as i64;
                        p += 1;
                    }
                    p += 1; // skip ','

                    // 3. name
                    let mut name_len = 0usize;
                    let mut name_hash = hash_init();
                    if p < valid_len && slice[p] == b'"' {
                        p += 1;
                        while p < valid_len {
                            if slice[p] == b'"' {
                                if p + 1 < valid_len && slice[p + 1] == b'"' {
                                    name_len += 1;
                                    name_hash = hash_byte(name_hash, b'"');
                                    p += 2;
                                } else {
                                    p += 1;
                                    break;
                                }
                            } else {
                                name_len += 1;
                                name_hash = hash_byte(name_hash, slice[p]);
                                p += 1;
                            }
                        }
                    } else {
                        while p < valid_len && slice[p] != b',' {
                            name_len += 1;
                            name_hash = hash_byte(name_hash, slice[p]);
                            p += 1;
                        }
                    }
                    p += 1; // skip ','

                    // 4. note
                    let mut note_len = 0usize;
                    if p < valid_len && slice[p] == b'"' {
                        p += 1;
                        while p < valid_len {
                            if slice[p] == b'"' {
                                if p + 1 < valid_len && slice[p + 1] == b'"' {
                                    note_len += 1;
                                    p += 2;
                                } else {
                                    p += 1;
                                    break;
                                }
                            } else {
                                note_len += 1;
                                p += 1;
                            }
                        }
                    } else {
                        while p < valid_len && slice[p] != b',' {
                            note_len += 1;
                            p += 1;
                        }
                    }
                    p += 1; // skip ','

                    // 5. region_id
                    let mut reg_id = 0usize;
                    while p < valid_len && slice[p] != b',' {
                        reg_id = reg_id * 10 + (slice[p] - b'0') as usize;
                        p += 1;
                    }
                    p += 1; // skip ','

                    // 6. amount
                    let neg = p < valid_len && slice[p] == b'-';
                    if neg {
                        p += 1;
                    }
                    let mut dollars = 0i64;
                    while p < valid_len && slice[p] != b'.' {
                        dollars = dollars * 10 + (slice[p] - b'0') as i64;
                        p += 1;
                    }
                    p += 1; // skip '.'
                    let cents_val = ((slice[p] - b'0') as i64) * 10 + ((slice[p + 1] - b'0') as i64);
                    let total_cents = dollars * 100 + cents_val;
                    let cents = if neg { -total_cents } else { total_cents };
                    p += 2;

                    if p < valid_len && slice[p] == b'\r' {
                        p += 1;
                    }
                    if p < valid_len && slice[p] == b'\n' {
                        p += 1;
                    }

                    let c_id = if reg_id < 1_000_000 {
                        region_map_ref[reg_id] as usize
                    } else {
                        0
                    };

                    let agg = &mut aggs[c_id];
                    agg.count += 1;
                    agg.id_sum += rec_id;
                    agg.amount_sum += cents;
                    if cents < agg.amount_min {
                        agg.amount_min = cents;
                    }
                    if cents > agg.amount_max {
                        agg.amount_max = cents;
                    }
                    agg.name_bytes += name_len;
                    agg.note_bytes += note_len;
                    names[c_id].push(name_hash);
                }

                rem_len = total_available - valid_len;
                if rem_len > 0 {
                    buf.copy_within(valid_len..total_available, 0);
                }
            }

            WorkerOutput { aggs, names }
        });
        handles.push(handle);
    }

    let workers: Vec<WorkerOutput> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let mut all_aggs = vec![LocalAgg::new(); num_countries];
    for w in &workers {
        for (c_id, agg_w) in w.aggs.iter().enumerate() {
            let agg = &mut all_aggs[c_id];
            agg.count += agg_w.count;
            agg.id_sum += agg_w.id_sum;
            agg.amount_sum += agg_w.amount_sum;
            if agg_w.amount_min < agg.amount_min {
                agg.amount_min = agg_w.amount_min;
            }
            if agg_w.amount_max > agg.amount_max {
                agg.amount_max = agg_w.amount_max;
            }
            agg.name_bytes += agg_w.name_bytes;
            agg.note_bytes += agg_w.note_bytes;
        }
    }

    // Parallel distinct name counting across countries
    let workers_ref: &'static [WorkerOutput] = Box::leak(workers.into_boxed_slice());
    let mut distinct_counts = vec![0usize; num_countries];
    let countries_per_worker = (num_countries + NUM_WORKERS - 1) / NUM_WORKERS;

    thread::scope(|s| {
        let mut handles = Vec::with_capacity(NUM_WORKERS);
        for t in 0..NUM_WORKERS {
            let start_c = t * countries_per_worker;
            let end_c = (start_c + countries_per_worker).min(num_countries);
            if start_c >= end_c {
                continue;
            }

            handles.push(s.spawn(move || {
                let mut results = Vec::with_capacity(end_c - start_c);
                let mut combined = Vec::new();
                for c_id in start_c..end_c {
                    let total_cap: usize = workers_ref.iter().map(|w| w.names[c_id].len()).sum();
                    if total_cap == 0 {
                        results.push((c_id, 0));
                        continue;
                    }
                    combined.clear();
                    combined.reserve(total_cap);
                    for w in workers_ref {
                        combined.extend_from_slice(&w.names[c_id]);
                    }
                    combined.sort_unstable();
                    combined.dedup();
                    results.push((c_id, combined.len()));
                }
                results
            }));
        }

        for h in handles {
            for (c_id, cnt) in h.join().unwrap() {
                distinct_counts[c_id] = cnt;
            }
        }
    });

    // Sort by country name ascending
    let mut sorted_indices: Vec<usize> = (0..num_countries).collect();
    sorted_indices.sort_unstable_by(|&a, &b| country_names[a].cmp(&country_names[b]));

    fs::create_dir_all(&out_dir)?;
    let out_file = File::create(&out_path)?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, out_file);

    for &c_id in &sorted_indices {
        let agg = &all_aggs[c_id];
        if agg.count == 0 {
            continue;
        }

        writeln!(
            writer,
            "{} {} {} {} {} {} {} {} {}",
            country_names[c_id],
            agg.count,
            agg.id_sum,
            format_cents(agg.amount_sum),
            format_cents(agg.amount_min),
            format_cents(agg.amount_max),
            distinct_counts[c_id],
            agg.name_bytes,
            agg.note_bytes
        )?;
    }
    writer.flush()?;

    Ok(())
}
