use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::thread;

const HOURS: [&str; 72] = [
    "2026-10-30T18:00:00Z", "2026-10-30T19:00:00Z", "2026-10-30T20:00:00Z",
    "2026-10-30T21:00:00Z", "2026-10-30T22:00:00Z", "2026-10-30T23:00:00Z",
    "2026-10-31T00:00:00Z", "2026-10-31T01:00:00Z", "2026-10-31T02:00:00Z",
    "2026-10-31T03:00:00Z", "2026-10-31T04:00:00Z", "2026-10-31T05:00:00Z",
    "2026-10-31T06:00:00Z", "2026-10-31T07:00:00Z", "2026-10-31T08:00:00Z",
    "2026-10-31T09:00:00Z", "2026-10-31T10:00:00Z", "2026-10-31T11:00:00Z",
    "2026-10-31T12:00:00Z", "2026-10-31T13:00:00Z", "2026-10-31T14:00:00Z",
    "2026-10-31T15:00:00Z", "2026-10-31T16:00:00Z", "2026-10-31T17:00:00Z",
    "2026-10-31T18:00:00Z", "2026-10-31T19:00:00Z", "2026-10-31T20:00:00Z",
    "2026-10-31T21:00:00Z", "2026-10-31T22:00:00Z", "2026-10-31T23:00:00Z",
    "2026-11-01T00:00:00Z", "2026-11-01T01:00:00Z", "2026-11-01T02:00:00Z",
    "2026-11-01T03:00:00Z", "2026-11-01T04:00:00Z", "2026-11-01T05:00:00Z",
    "2026-11-01T06:00:00Z", "2026-11-01T07:00:00Z", "2026-11-01T08:00:00Z",
    "2026-11-01T09:00:00Z", "2026-11-01T10:00:00Z", "2026-11-01T11:00:00Z",
    "2026-11-01T12:00:00Z", "2026-11-01T13:00:00Z", "2026-11-01T14:00:00Z",
    "2026-11-01T15:00:00Z", "2026-11-01T16:00:00Z", "2026-11-01T17:00:00Z",
    "2026-11-01T18:00:00Z", "2026-11-01T19:00:00Z", "2026-11-01T20:00:00Z",
    "2026-11-01T21:00:00Z", "2026-11-01T22:00:00Z", "2026-11-01T23:00:00Z",
    "2026-11-02T00:00:00Z", "2026-11-02T01:00:00Z", "2026-11-02T02:00:00Z",
    "2026-11-02T03:00:00Z", "2026-11-02T04:00:00Z", "2026-11-02T05:00:00Z",
    "2026-11-02T06:00:00Z", "2026-11-02T07:00:00Z", "2026-11-02T08:00:00Z",
    "2026-11-02T09:00:00Z", "2026-11-02T10:00:00Z", "2026-11-02T11:00:00Z",
    "2026-11-02T12:00:00Z", "2026-11-02T13:00:00Z", "2026-11-02T14:00:00Z",
    "2026-11-02T15:00:00Z", "2026-11-02T16:00:00Z", "2026-11-02T17:00:00Z",
];

// Fast FxHasher implementation without external crates
struct FxHasher(u64);
impl std::hash::Hasher for FxHasher {
    #[inline(always)]
    fn finish(&self) -> u64 { self.0 }
    #[inline(always)]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = self.0.rotate_left(5) ^ (b as u64) ^ 0x517cc1b727220a95;
        }
    }
}
#[derive(Clone, Copy, Default)]
struct BuildFxHasher;
impl std::hash::BuildHasher for BuildFxHasher {
    type Hasher = FxHasher;
    #[inline(always)]
    fn build_hasher(&self) -> FxHasher { FxHasher(0) }
}

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

const CHUNK_SIZE: usize = 2 * 1024 * 1024;
const NUM_BUFFERS: usize = 6;
const NUM_WORKERS: usize = 10;

struct WorkerResult {
    endpoints: Vec<Box<[u8]>>,
    hourly_events: Vec<Vec<u64>>,
}

fn worker_loop(
    chunk_rx: std::sync::Arc<std::sync::Mutex<Receiver<Option<(Vec<u8>, usize)>>>>,
    free_tx: SyncSender<Vec<u8>>,
) -> WorkerResult {
    let mut ep_map: HashMap<Box<[u8]>, u16, BuildFxHasher> = HashMap::with_capacity_and_hasher(2048, BuildFxHasher);
    let mut endpoints: Vec<Box<[u8]>> = Vec::with_capacity(2048);
    let mut hourly_events: Vec<Vec<u64>> = (0..72).map(|_| Vec::with_capacity(4096)).collect();

    loop {
        let msg = {
            let rx = chunk_rx.lock().unwrap();
            match rx.recv() {
                Ok(m) => m,
                Err(_) => break,
            }
        };
        let (buf, len) = match msg {
            Some(payload) => payload,
            None => break,
        };

        let slice = &buf[..len];
        let mut pos = 0;
        while pos + 35 <= len {
            // 1. Hour
            let hour_idx = parse_hour_idx(&slice[pos..pos + 13]);

            // 2. Method and Endpoint
            let mut idx = pos + 28;
            while idx < len && slice[idx] != b' ' {
                idx += 1;
            }
            idx += 1; // skip SP after method

            let ep_start = idx;
            while idx < len && slice[idx] != b' ' {
                idx += 1;
            }
            let ep_end = idx;
            idx += 1; // skip SP after endpoint

            let endpoint_bytes = &slice[ep_start..ep_end];

            // 3. Status
            let is_5xx = idx < len && slice[idx] == b'5';
            while idx < len && slice[idx] != b' ' {
                idx += 1;
            }
            idx += 1; // skip SP after status

            // 4. Latency
            let mut lat = 0u32;
            while idx < len && slice[idx] != b'\n' {
                let b = slice[idx];
                if b >= b'0' && b <= b'9' {
                    lat = lat * 10 + (b - b'0') as u32;
                }
                idx += 1;
            }
            pos = idx + 1; // advance past \n

            // 5. Endpoint mapping
            let ep_id = match ep_map.get(endpoint_bytes) {
                Some(&id) => id,
                None => {
                    let boxed: Box<[u8]> = endpoint_bytes.into();
                    let id = endpoints.len() as u16;
                    endpoints.push(boxed.clone());
                    ep_map.insert(boxed, id);
                    id
                }
            };

            let packed = (ep_id as u64) | ((lat as u64) << 16) | ((is_5xx as u64) << 48);
            hourly_events[hour_idx].push(packed);
        }

        let _ = free_tx.send(buf);
    }

    WorkerResult {
        endpoints,
        hourly_events,
    }
}

fn main() -> std::io::Result<()> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string());

    let input_path = Path::new(&data_dir).join("requests.log");
    let out_path = Path::new(&out_dir).join("summary.txt");

    let (free_tx, free_rx) = sync_channel::<Vec<u8>>(NUM_BUFFERS);
    let (chunk_tx, chunk_rx) = sync_channel::<Option<(Vec<u8>, usize)>>(NUM_BUFFERS);

    // Populate buffer pool
    for _ in 0..NUM_BUFFERS {
        free_tx.send(vec![0u8; CHUNK_SIZE]).unwrap();
    }

    let chunk_rx = std::sync::Arc::new(std::sync::Mutex::new(chunk_rx));

    // Spawn workers
    let mut worker_handles = Vec::with_capacity(NUM_WORKERS);
    for _ in 0..NUM_WORKERS {
        let c_rx = chunk_rx.clone();
        let f_tx = free_tx.clone();
        let handle = thread::spawn(move || worker_loop(c_rx, f_tx));
        worker_handles.push(handle);
    }

    // Producer reads from file in chunks
    let mut file = File::open(&input_path)?;
    let mut remainder = Vec::new();

    loop {
        let mut buf = free_rx.recv().unwrap();
        let rem_len = remainder.len();
        if rem_len > 0 {
            buf[..rem_len].copy_from_slice(&remainder);
            remainder.clear();
        }

        let n = file.read(&mut buf[rem_len..])?;
        if n == 0 && rem_len == 0 {
            break;
        }

        let total_bytes = rem_len + n;
        if n == 0 {
            // EOF reached, push all remaining
            chunk_tx.send(Some((buf, total_bytes))).unwrap();
            break;
        }

        // Find last newline
        let mut last_nl = total_bytes;
        while last_nl > 0 && buf[last_nl - 1] != b'\n' {
            last_nl -= 1;
        }

        if last_nl == 0 {
            // No newline found in entire buffer, save all to remainder
            remainder.extend_from_slice(&buf[..total_bytes]);
            free_tx.send(buf).unwrap();
        } else {
            remainder.extend_from_slice(&buf[last_nl..total_bytes]);
            chunk_tx.send(Some((buf, last_nl))).unwrap();
        }
    }

    // Signal workers to terminate
    for _ in 0..NUM_WORKERS {
        chunk_tx.send(None).unwrap();
    }

    // Collect worker results
    let mut worker_results = Vec::with_capacity(NUM_WORKERS);
    for handle in worker_handles {
        worker_results.push(handle.join().unwrap());
    }

    // Build global endpoint dictionary
    let mut global_endpoints: Vec<Box<[u8]>> = Vec::with_capacity(2048);
    let mut global_map: HashMap<Box<[u8]>, u16, BuildFxHasher> = HashMap::with_capacity_and_hasher(2048, BuildFxHasher);

    for res in &worker_results {
        for ep in &res.endpoints {
            if !global_map.contains_key(ep) {
                let id = global_endpoints.len() as u16;
                global_endpoints.push(ep.clone());
                global_map.insert(ep.clone(), id);
            }
        }
    }

    // Sort endpoints lexicographically
    let mut sorted_indices: Vec<u16> = (0..global_endpoints.len() as u16).collect();
    sorted_indices.sort_unstable_by(|&a, &b| global_endpoints[a as usize].as_ref().cmp(global_endpoints[b as usize].as_ref()));

    // rank_of_global_ep[global_id] -> sorted_order (0..num_endpoints)
    let num_endpoints = global_endpoints.len();
    let mut rank_of_global_ep = vec![0u16; num_endpoints];
    for (rank, &gid) in sorted_indices.iter().enumerate() {
        rank_of_global_ep[gid as usize] = rank as u16;
    }

    // Build remap tables: remap[worker_idx][worker_ep_id] -> rank
    let mut remap: Vec<Vec<u16>> = Vec::with_capacity(NUM_WORKERS);
    for res in &worker_results {
        let mut table = Vec::with_capacity(res.endpoints.len());
        for ep in &res.endpoints {
            let gid = global_map[ep];
            table.push(rank_of_global_ep[gid as usize]);
        }
        remap.push(table);
    }

    // Process all 72 hours across threads
    let worker_results = std::sync::Arc::new(worker_results);
    let remap = std::sync::Arc::new(remap);
    let sorted_endpoints: std::sync::Arc<Vec<Box<[u8]>>> = std::sync::Arc::new(
        sorted_indices.iter().map(|&idx| global_endpoints[idx as usize].clone()).collect()
    );

    let mut hour_handles = Vec::with_capacity(NUM_WORKERS);
    let hours_per_thread = (72 + NUM_WORKERS - 1) / NUM_WORKERS;

    for t in 0..NUM_WORKERS {
        let start_h = t * hours_per_thread;
        let end_h = (start_h + hours_per_thread).min(72);
        if start_h >= end_h {
            continue;
        }

        let w_res = worker_results.clone();
        let r_map = remap.clone();
        let s_eps = sorted_endpoints.clone();

        hour_handles.push(thread::spawn(move || {
            let mut formatted_hours: Vec<(usize, Vec<u8>)> = Vec::with_capacity(end_h - start_h);
            let n_eps = s_eps.len();

            let mut counts = vec![0u32; n_eps];
            let mut errors = vec![0u32; n_eps];
            let mut offsets = vec![0u32; n_eps];
            let mut write_pos = vec![0u32; n_eps];
            let mut latencies: Vec<u32> = Vec::with_capacity(65536);

            for h in start_h..end_h {
                counts.fill(0);
                errors.fill(0);

                // Pass 1: Count per endpoint
                for (w_idx, w) in w_res.iter().enumerate() {
                    let events = &w.hourly_events[h];
                    let r_table = &r_map[w_idx];
                    for &ev in events {
                        let local_ep = (ev & 0xFFFF) as usize;
                        let is_err = (ev >> 48) != 0;
                        let rank = r_table[local_ep] as usize;
                        counts[rank] += 1;
                        if is_err {
                            errors[rank] += 1;
                        }
                    }
                }

                // Prefix sums
                let mut total = 0u32;
                for rank in 0..n_eps {
                    offsets[rank] = total;
                    write_pos[rank] = total;
                    total += counts[rank];
                }

                latencies.resize(total as usize, 0);

                // Pass 2: Place latencies contiguously
                for (w_idx, w) in w_res.iter().enumerate() {
                    let events = &w.hourly_events[h];
                    let r_table = &r_map[w_idx];
                    for &ev in events {
                        let local_ep = (ev & 0xFFFF) as usize;
                        let lat = ((ev >> 16) & 0xFFFFFFFF) as u32;
                        let rank = r_table[local_ep] as usize;
                        let pos = write_pos[rank] as usize;
                        latencies[pos] = lat;
                        write_pos[rank] += 1;
                    }
                }

                // Format hour h
                let mut out_buf = Vec::with_capacity(128 * 1024);
                let hour_str = HOURS[h];

                for rank in 0..n_eps {
                    let count = counts[rank] as usize;
                    if count == 0 {
                        continue;
                    }

                    let start = offsets[rank] as usize;
                    let slice = &mut latencies[start..start + count];

                    let r50 = (50 * count + 99) / 100 - 1;
                    let r95 = (95 * count + 99) / 100 - 1;
                    let r99 = (99 * count + 99) / 100 - 1;

                    let (p50, p95, p99) = if count <= 32 {
                        slice.sort_unstable();
                        (slice[r50], slice[r95], slice[r99])
                    } else {
                        slice.select_nth_unstable(r99);
                        let p99 = slice[r99];
                        let p95 = if r95 == r99 {
                            p99
                        } else {
                            slice[..r99].select_nth_unstable(r95);
                            slice[r95]
                        };
                        let p50 = if r50 == r95 {
                            p95
                        } else {
                            slice[..r95].select_nth_unstable(r50);
                            slice[r50]
                        };
                        (p50, p95, p99)
                    };

                    let err_count = errors[rank];
                    let ppm = (err_count as u64 * 1_000_000) / count as u64;

                    use std::io::Write as _;
                    let _ = writeln!(
                        out_buf,
                        "{} {} {} {} {} {} {} {}",
                        hour_str,
                        std::str::from_utf8(&s_eps[rank]).unwrap(),
                        count,
                        p50,
                        p95,
                        p99,
                        err_count,
                        ppm
                    );
                }

                formatted_hours.push((h, out_buf));
            }

            formatted_hours
        }));
    }

    let mut hour_outputs: Vec<Option<Vec<u8>>> = (0..72).map(|_| None).collect();
    for handle in hour_handles {
        let results = handle.join().unwrap();
        for (h, buf) in results {
            hour_outputs[h] = Some(buf);
        }
    }

    fs::create_dir_all(&out_dir)?;
    let mut out_file = File::create(&out_path)?;
    for h in 0..72 {
        if let Some(buf) = &hour_outputs[h] {
            out_file.write_all(buf)?;
        }
    }
    out_file.flush()?;

    Ok(())
}
