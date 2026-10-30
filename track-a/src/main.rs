use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
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

#[inline(always)]
fn get_hour_idx(ts: &[u8]) -> usize {
    let day = (ts[8] - b'0') * 10 + (ts[9] - b'0');
    let hour = ((ts[11] - b'0') as usize) * 10 + ((ts[12] - b'0') as usize);
    let base = match day {
        30 => 0,
        31 => 6,
        1 => 30,
        2 => 54,
        _ => return 0,
    };
    if day == 30 {
        base + hour.saturating_sub(18)
    } else {
        base + hour
    }
}

#[derive(Clone, Copy)]
struct HashSlot {
    hash: u64,
    len: u16,
    id: u16,
}

#[derive(Clone, Copy)]
struct LatChunk {
    lats: [u32; 14],
    count: u8,
    _pad: [u8; 3],
    next: u32,
}

struct WorkerOutput {
    stats: Vec<u32>,
    heads: Vec<u32>,
    chunks: Vec<LatChunk>,
}

const TABLE_SIZE: usize = 4096;
const TABLE_MASK: usize = TABLE_SIZE - 1;
const NUM_WORKERS: usize = 6;
const CHUNK_SIZE: usize = 512 * 1024;

fn main() -> std::io::Result<()> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string());

    let input_path = Path::new(&data_dir).join("requests.log");
    let out_path = Path::new(&out_dir).join("summary.txt");

    let mut file = File::open(&input_path)?;
    let file_len = file.metadata()?.len() as usize;

    if file_len == 0 {
        File::create(&out_path)?;
        return Ok(());
    }

    // 1. Discover unique endpoints from first 8 MB
    let mut probe_table = vec![HashSlot { hash: 0, len: 0, id: 0 }; TABLE_SIZE];
    let mut ep_strings: Vec<Vec<u8>> = Vec::with_capacity(2048);

    let head_read_size = file_len.min(8 * 1024 * 1024);
    let mut head_buf = vec![0u8; head_read_size];
    file.read_exact(&mut head_buf)?;

    let mut pos = 0;
    while pos + 35 <= head_read_size {
        let mut idx = pos + 28;
        while idx < head_read_size && head_buf[idx] != b' ' {
            idx += 1;
        }
        idx += 1;
        let ep_start = idx;
        let mut h = 0x517cc1b727220a95u64;
        while idx < head_read_size && head_buf[idx] != b' ' {
            let b = head_buf[idx];
            h = (h.rotate_left(5) ^ (b as u64)).wrapping_mul(0x517cc1b727220a95);
            idx += 1;
        }
        let ep_len = idx - ep_start;
        let ep_slice = &head_buf[ep_start..idx];

        let mut slot = (h as usize) & TABLE_MASK;
        loop {
            let entry = &probe_table[slot];
            if entry.hash == h && entry.len as usize == ep_len {
                break;
            }
            if entry.len == 0 {
                let id = ep_strings.len() as u16;
                probe_table[slot] = HashSlot { hash: h, len: ep_len as u16, id };
                ep_strings.push(ep_slice.to_vec());
                break;
            }
            slot = (slot + 1) & TABLE_MASK;
        }

        if ep_strings.len() == 1842 {
            break;
        }

        while idx < head_read_size && head_buf[idx] != b'\n' {
            idx += 1;
        }
        pos = idx + 1;
    }
    drop(head_buf);

    // 2. Sort endpoints for deterministic byte ordering
    ep_strings.sort_unstable();
    let num_eps = ep_strings.len();
    let total_groups = 72 * num_eps;

    // 3. Build fast lookup table
    let mut ep_table = vec![HashSlot { hash: 0, len: 0, id: 0 }; TABLE_SIZE];
    for (i, ep) in ep_strings.iter().enumerate() {
        let mut h = 0x517cc1b727220a95u64;
        for &b in ep {
            h = (h.rotate_left(5) ^ (b as u64)).wrapping_mul(0x517cc1b727220a95);
        }
        let mut slot = (h as usize) & TABLE_MASK;
        loop {
            if ep_table[slot].len == 0 {
                ep_table[slot] = HashSlot { hash: h, len: ep.len() as u16, id: i as u16 };
                break;
            }
            slot = (slot + 1) & TABLE_MASK;
        }
    }

    // 4. Partition work across threads using pread chunks
    let ep_table_ref: &'static [HashSlot] = Box::leak(ep_table.into_boxed_slice());
    let mut handles = Vec::with_capacity(NUM_WORKERS);

    let approx_chunk = file_len / NUM_WORKERS;
    let mut chunk_bounds = Vec::with_capacity(NUM_WORKERS + 1);
    chunk_bounds.push(0);

    let mut scan_buf = vec![0u8; 1024];
    for i in 1..NUM_WORKERS {
        let approx_pos = i * approx_chunk;
        file.read_exact_at(&mut scan_buf, approx_pos as u64)?;
        let mut offset = 0;
        while offset < scan_buf.len() && scan_buf[offset] != b'\n' {
            offset += 1;
        }
        chunk_bounds.push(approx_pos + offset + 1);
    }
    chunk_bounds.push(file_len);
    drop(scan_buf);

    for i in 0..NUM_WORKERS {
        let start_pos = chunk_bounds[i];
        let end_pos = chunk_bounds[i + 1];
        let path = input_path.clone();

        let handle = thread::spawn(move || {
            let t_file = File::open(&path).unwrap();
            let mut buf = vec![0u8; CHUNK_SIZE + 4096];
            let mut heads = vec![u32::MAX; total_groups];
            let mut stats = vec![0u32; total_groups];
            let mut chunks: Vec<LatChunk> = Vec::with_capacity(((end_pos - start_pos) / 500) + 1024);

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

                let mut last_nl = total_available;
                while last_nl > 0 && buf[last_nl - 1] != b'\n' {
                    last_nl -= 1;
                }

                let slice = &buf[..last_nl];
                let mut pos = 0;
                while pos + 35 <= last_nl {
                    let hour_idx = get_hour_idx(&slice[pos..pos + 13]);

                // Method starts at pos + 28
                let ep_start = if slice[pos + 31] == b' ' {
                    pos + 32
                } else if slice[pos + 32] == b' ' {
                    pos + 33
                } else if slice[pos + 33] == b' ' {
                    pos + 34
                } else if slice[pos + 34] == b' ' {
                    pos + 35
                } else {
                    pos + 36
                };

                let mut idx = ep_start;
                let mut h = 0x517cc1b727220a95u64;
                while idx < last_nl && slice[idx] != b' ' {
                    let b = slice[idx];
                    h = (h.rotate_left(5) ^ (b as u64)).wrapping_mul(0x517cc1b727220a95);
                    idx += 1;
                }
                let ep_len = idx - ep_start;

                let mut slot = (h as usize) & TABLE_MASK;
                let ep_id = loop {
                    let entry = &ep_table_ref[slot];
                    if entry.hash == h && entry.len as usize == ep_len {
                        break entry.id as usize;
                    }
                    slot = (slot + 1) & TABLE_MASK;
                };

                // Status is 3 digits at idx + 1
                let is_5xx = (slice[idx + 1] == b'5') as u32;

                // Latency starts at idx + 5
                let mut p = idx + 5;
                let mut lat = (slice[p] - b'0') as u32;
                p += 1;
                while slice[p] != b'\n' {
                    lat = lat * 10 + (slice[p] - b'0') as u32;
                    p += 1;
                }
                pos = p + 1;

                    let group_id = hour_idx * num_eps + ep_id;
                    let head_idx = heads[group_id];
                    if head_idx != u32::MAX && chunks[head_idx as usize].count < 14 {
                        let chunk = &mut chunks[head_idx as usize];
                        chunk.lats[chunk.count as usize] = lat;
                        chunk.count += 1;
                    } else {
                        let new_idx = chunks.len() as u32;
                        let mut chunk = LatChunk {
                            lats: [0; 14],
                            count: 1,
                            _pad: [0; 3],
                            next: head_idx,
                        };
                        chunk.lats[0] = lat;
                        chunks.push(chunk);
                        heads[group_id] = new_idx;
                    }
                    stats[group_id] += 1 + (is_5xx << 16);
                }

                rem_len = total_available - last_nl;
                if rem_len > 0 {
                    buf.copy_within(last_nl..total_available, 0);
                }
            }

            WorkerOutput { stats, heads, chunks }
        });
        handles.push(handle);
    }

    let workers: Vec<WorkerOutput> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // 5. Stream output directly in sorted order
    let out_file = File::create(&out_path)?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, out_file);
    let mut scratch: Vec<u32> = Vec::with_capacity(4096);

    for hour_idx in 0..72 {
        let hour_str = HOURS[hour_idx];
        for (ep_id, ep) in ep_strings.iter().enumerate() {
            let group_id = hour_idx * num_eps + ep_id;

            let mut count = 0u32;
            let mut errors = 0u32;
            for w in &workers {
                let s = w.stats[group_id];
                count += s & 0xFFFF;
                errors += s >> 16;
            }

            if count == 0 {
                continue;
            }

            scratch.clear();
            for w in &workers {
                let mut curr = w.heads[group_id];
                while curr != u32::MAX {
                    let chunk = &w.chunks[curr as usize];
                    let chunk_cnt = chunk.count as usize;
                    scratch.extend_from_slice(&chunk.lats[..chunk_cnt]);
                    curr = chunk.next;
                }
            }

            scratch.sort_unstable();
            let n = scratch.len();
            let p50 = scratch[(50 * n + 99) / 100 - 1];
            let p95 = scratch[(95 * n + 99) / 100 - 1];
            let p99 = scratch[(99 * n + 99) / 100 - 1];
            let error_ppm = (errors as u64 * 1_000_000 / count as u64) as u32;

            write!(writer, "{} ", hour_str)?;
            writer.write_all(ep)?;
            writeln!(writer, " {} {} {} {} {} {}", count, p50, p95, p99, errors, error_ppm)?;
        }
    }
    writer.flush()?;

    Ok(())
}
