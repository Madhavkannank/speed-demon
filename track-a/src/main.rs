use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

const HOURS: [&str; 72] = [
    // Oct 30: 18..23
    "2026-10-30T18:00:00Z", "2026-10-30T19:00:00Z", "2026-10-30T20:00:00Z",
    "2026-10-30T21:00:00Z", "2026-10-30T22:00:00Z", "2026-10-30T23:00:00Z",
    // Oct 31: 00..23
    "2026-10-31T00:00:00Z", "2026-10-31T01:00:00Z", "2026-10-31T02:00:00Z",
    "2026-10-31T03:00:00Z", "2026-10-31T04:00:00Z", "2026-10-31T05:00:00Z",
    "2026-10-31T06:00:00Z", "2026-10-31T07:00:00Z", "2026-10-31T08:00:00Z",
    "2026-10-31T09:00:00Z", "2026-10-31T10:00:00Z", "2026-10-31T11:00:00Z",
    "2026-10-31T12:00:00Z", "2026-10-31T13:00:00Z", "2026-10-31T14:00:00Z",
    "2026-10-31T15:00:00Z", "2026-10-31T16:00:00Z", "2026-10-31T17:00:00Z",
    "2026-10-31T18:00:00Z", "2026-10-31T19:00:00Z", "2026-10-31T20:00:00Z",
    "2026-10-31T21:00:00Z", "2026-10-31T22:00:00Z", "2026-10-31T23:00:00Z",
    // Nov 01: 00..23
    "2026-11-01T00:00:00Z", "2026-11-01T01:00:00Z", "2026-11-01T02:00:00Z",
    "2026-11-01T03:00:00Z", "2026-11-01T04:00:00Z", "2026-11-01T05:00:00Z",
    "2026-11-01T06:00:00Z", "2026-11-01T07:00:00Z", "2026-11-01T08:00:00Z",
    "2026-11-01T09:00:00Z", "2026-11-01T10:00:00Z", "2026-11-01T11:00:00Z",
    "2026-11-01T12:00:00Z", "2026-11-01T13:00:00Z", "2026-11-01T14:00:00Z",
    "2026-11-01T15:00:00Z", "2026-11-01T16:00:00Z", "2026-11-01T17:00:00Z",
    "2026-11-01T18:00:00Z", "2026-11-01T19:00:00Z", "2026-11-01T20:00:00Z",
    "2026-11-01T21:00:00Z", "2026-11-01T22:00:00Z", "2026-11-01T23:00:00Z",
    // Nov 02: 00..17
    "2026-11-02T00:00:00Z", "2026-11-02T01:00:00Z", "2026-11-02T02:00:00Z",
    "2026-11-02T03:00:00Z", "2026-11-02T04:00:00Z", "2026-11-02T05:00:00Z",
    "2026-11-02T06:00:00Z", "2026-11-02T07:00:00Z", "2026-11-02T08:00:00Z",
    "2026-11-02T09:00:00Z", "2026-11-02T10:00:00Z", "2026-11-02T11:00:00Z",
    "2026-11-02T12:00:00Z", "2026-11-02T13:00:00Z", "2026-11-02T14:00:00Z",
    "2026-11-02T15:00:00Z", "2026-11-02T16:00:00Z", "2026-11-02T17:00:00Z",
];

#[inline(always)]
fn parse_hour_idx(ts: &[u8]) -> usize {
    let day = (ts[8] - b'0') * 10 + (ts[9] - b'0');
    let hour = (ts[11] - b'0') * 10 + (ts[12] - b'0');
    let base = match day {
        30 => 0,
        31 => 6,
        1 => 30,
        2 => 54,
        _ => return 0,
    };
    if day == 30 {
        base + (hour as usize).saturating_sub(18)
    } else {
        base + (hour as usize)
    }
}

#[inline(always)]
fn parse_u32(bytes: &[u8]) -> u32 {
    let mut val = 0u32;
    for &b in bytes {
        if b.is_ascii_digit() {
            val = val * 10 + (b - b'0') as u32;
        }
    }
    val
}

#[derive(Default)]
struct Group {
    latencies: Vec<u32>,
    errors: u32,
}

#[inline(always)]
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
    let mut reader = BufReader::with_capacity(1024 * 1024, file);

    let mut endpoints: Vec<Box<[u8]>> = Vec::with_capacity(2048);
    let mut endpoint_map: HashMap<Box<[u8]>, usize> = HashMap::with_capacity(2048);
    let mut groups: Vec<Vec<Group>> = (0..72).map(|_| Vec::with_capacity(2048)).collect();

    let mut line = Vec::with_capacity(256);

    while reader.read_until(b'\n', &mut line)? > 0 {
        let len = line.len();
        if len < 35 {
            line.clear();
            continue;
        }

        // 1. Hour index from 27-byte timestamp
        let hour_idx = parse_hour_idx(&line[0..13]);

        // 2. Parse method and endpoint
        // line format: ts SP method SP endpoint SP status SP latency LF
        // ts is 27 bytes, so line[27] == b' '
        let mut idx = 28;
        while idx < len && line[idx] != b' ' {
            idx += 1;
        }
        idx += 1; // skip SP after method

        let ep_start = idx;
        while idx < len && line[idx] != b' ' {
            idx += 1;
        }
        let ep_end = idx;
        idx += 1; // skip SP after endpoint

        let endpoint_bytes = &line[ep_start..ep_end];

        // 3. Status
        let is_5xx = idx < len && line[idx] == b'5';
        while idx < len && line[idx] != b' ' {
            idx += 1;
        }
        idx += 1; // skip SP after status

        // 4. Latency
        let lat_start = idx;
        let mut lat_end = len;
        while lat_end > lat_start && (line[lat_end - 1] == b'\n' || line[lat_end - 1] == b'\r') {
            lat_end -= 1;
        }
        let latency = parse_u32(&line[lat_start..lat_end]);

        // 5. Endpoint intern
        let ep_id = match endpoint_map.get(endpoint_bytes) {
            Some(&id) => id,
            None => {
                let boxed: Box<[u8]> = endpoint_bytes.into();
                let id = endpoints.len();
                endpoints.push(boxed.clone());
                endpoint_map.insert(boxed, id);
                for h in 0..72 {
                    groups[h].push(Group::default());
                }
                id
            }
        };

        let group = &mut groups[hour_idx][ep_id];
        group.latencies.push(latency);
        if is_5xx {
            group.errors += 1;
        }

        line.clear();
    }

    // Sort endpoints lexicographically by unsigned byte string
    let num_endpoints = endpoints.len();
    let mut sorted_ep_ids: Vec<usize> = (0..num_endpoints).collect();
    sorted_ep_ids.sort_unstable_by(|&a, &b| endpoints[a].as_ref().cmp(endpoints[b].as_ref()));

    fs::create_dir_all(&out_dir)?;
    let out_file = File::create(&out_path)?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, out_file);

    for h in 0..72 {
        let hour_str = HOURS[h];
        for &ep_id in &sorted_ep_ids {
            let group = &mut groups[h][ep_id];
            let count = group.latencies.len();
            if count == 0 {
                continue;
            }

            group.latencies.sort_unstable();

            let p50 = percentile(&group.latencies, 50);
            let p95 = percentile(&group.latencies, 95);
            let p99 = percentile(&group.latencies, 99);
            let errors = group.errors;
            let ppm = (errors as u64 * 1_000_000) / count as u64;

            let ep_str = std::str::from_utf8(&endpoints[ep_id]).unwrap();
            writeln!(
                writer,
                "{} {} {} {} {} {} {} {}",
                hour_str, ep_str, count, p50, p95, p99, errors, ppm
            )?;
        }
    }

    writer.flush()?;
    Ok(())
}
