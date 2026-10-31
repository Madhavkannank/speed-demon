use std::fs::File;
use std::io::Write;
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

#[inline(always)]
fn write_u32(buf: &mut Vec<u8>, mut n: u32) {
    if n == 0 {
        buf.push(b'0');
        return;
    }
    let mut temp = [0u8; 10];
    let mut i = 10;
    while n > 0 {
        i -= 1;
        temp[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    buf.extend_from_slice(&temp[i..]);
}

#[inline(always)]
fn fast_hash(b: &[u8]) -> u64 {
    let ep_len = b.len();
    let mut h = (ep_len as u64).wrapping_mul(0x517cc1b727220a95);
    let ptr = b.as_ptr();
    if ep_len < 8 {
        for i in 0..ep_len {
            let byte = unsafe { *ptr.add(i) };
            h = (h.rotate_left(5) ^ (byte as u64)).wrapping_mul(0x517cc1b727220a95);
        }
    } else {
        let mut k = 0;
        while k + 8 <= ep_len {
            let word = unsafe { (ptr.add(k) as *const u64).read_unaligned() };
            h = (h.rotate_left(7) ^ word).wrapping_mul(0x517cc1b727220a95);
            k += 8;
        }
        if k < ep_len {
            let word = unsafe { (ptr.add(ep_len - 8) as *const u64).read_unaligned() };
            h = (h.rotate_left(7) ^ word).wrapping_mul(0x517cc1b727220a95);
        }
    }
    h
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

const TABLE_SIZE: usize = 8192;
const TABLE_MASK: usize = TABLE_SIZE - 1;
const NUM_WORKERS: usize = 6;
const CHUNK_SIZE: usize = 512 * 1024;

fn main() -> std::io::Result<()> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string());

    let input_path = Path::new(&data_dir).join("requests.log");
    let out_path = Path::new(&out_dir).join("summary.txt");

    let file = File::open(&input_path)?;
    let file_len = file.metadata()?.len() as usize;

    if file_len == 0 {
        File::create(&out_path)?;
        return Ok(());
    }

    // 1. Discover unique endpoints by streaming in 256 KB chunks until all 1842 are found
    let mut probe_table = vec![HashSlot { hash: 0, len: 0, id: 0 }; TABLE_SIZE];
    let mut ep_strings: Vec<Vec<u8>> = Vec::with_capacity(2048);

    let mut head_buf = vec![0u8; 256 * 1024 + 4096];
    let mut h_offset = 0;
    let mut h_rem = 0;

    while h_offset < file_len && ep_strings.len() < 1842 {
        let to_read = (256 * 1024).min(file_len - h_offset);
        file.read_exact_at(&mut head_buf[h_rem..h_rem + to_read], h_offset as u64)?;
        h_offset += to_read;
        let available = h_rem + to_read;

        let mut last_nl = available;
        while last_nl > 0 && head_buf[last_nl - 1] != b'\n' {
            last_nl -= 1;
        }

        let mut pos = 0;
        while pos + 35 <= last_nl {
            let mut idx = pos + 28;
            while idx < last_nl && head_buf[idx] != b' ' {
                idx += 1;
            }
            idx += 1;
            let ep_start = idx;
            while idx < last_nl && head_buf[idx] != b' ' {
                idx += 1;
            }
            let ep_len = idx - ep_start;
            let ep_slice = &head_buf[ep_start..idx];
            let h = fast_hash(ep_slice);

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

            while idx < last_nl && head_buf[idx] != b'\n' {
                idx += 1;
            }
            pos = idx + 1;
        }

        h_rem = available - last_nl;
        if h_rem > 0 {
            head_buf.copy_within(last_nl..available, 0);
        }
    }
    drop(head_buf);

    // 2. Sort endpoints for deterministic byte ordering
    ep_strings.sort_unstable();
    let num_eps = ep_strings.len();
    let total_groups = 72 * num_eps;

    // 3. Build fast lookup table
    let mut ep_table = vec![HashSlot { hash: 0, len: 0, id: 0 }; TABLE_SIZE];
    for (i, ep) in ep_strings.iter().enumerate() {
        let h = fast_hash(ep);
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
                let mut last_hour_str = [0u8; 13];
                let mut cached_hour_idx = 0usize;

                while pos + 35 <= last_nl {
                    let hour_prefix = &slice[pos..pos + 13];
                    let hour_idx = if hour_prefix == &last_hour_str {
                        cached_hour_idx
                    } else {
                        last_hour_str.copy_from_slice(hour_prefix);
                        cached_hour_idx = get_hour_idx(hour_prefix);
                        cached_hour_idx
                    };

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
                while idx < last_nl && slice[idx] != b' ' {
                    idx += 1;
                }
                let ep_len = idx - ep_start;
                let ep_slice = &slice[ep_start..idx];
                let h = fast_hash(ep_slice);

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

    // 5. Parallel sort and format across 6 threads
    let workers_ref: &'static [WorkerOutput] = Box::leak(workers.into_boxed_slice());
    let ep_strings_ref: &'static [Vec<u8>] = Box::leak(ep_strings.into_boxed_slice());

    let mut sort_handles = Vec::with_capacity(NUM_WORKERS);
    let hours_per_worker = 72 / NUM_WORKERS;

    for t in 0..NUM_WORKERS {
        let start_h = t * hours_per_worker;
        let end_h = (t + 1) * hours_per_worker;

        let handle = thread::spawn(move || {
            let mut buf = Vec::with_capacity(900 * 1024);
            let mut scratch = Vec::with_capacity(4096);

            for hour_idx in start_h..end_h {
                let hour_bytes = HOURS[hour_idx].as_bytes();
                for (ep_id, ep) in ep_strings_ref.iter().enumerate() {
                    let group_id = hour_idx * num_eps + ep_id;

                    let mut count = 0u32;
                    let mut errors = 0u32;
                    for w in workers_ref {
                        let s = w.stats[group_id];
                        count += s & 0xFFFF;
                        errors += s >> 16;
                    }

                    if count == 0 {
                        continue;
                    }

                    scratch.clear();
                    for w in workers_ref {
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

                    buf.extend_from_slice(hour_bytes);
                    buf.push(b' ');
                    buf.extend_from_slice(ep);
                    buf.push(b' ');
                    write_u32(&mut buf, count);
                    buf.push(b' ');
                    write_u32(&mut buf, p50);
                    buf.push(b' ');
                    write_u32(&mut buf, p95);
                    buf.push(b' ');
                    write_u32(&mut buf, p99);
                    buf.push(b' ');
                    write_u32(&mut buf, errors);
                    buf.push(b' ');
                    write_u32(&mut buf, error_ppm);
                    buf.push(b'\n');
                }
            }
            buf
        });
        sort_handles.push(handle);
    }

    let mut out_file = File::create(&out_path)?;
    for handle in sort_handles {
        let buf = handle.join().unwrap();
        out_file.write_all(&buf)?;
    }
    out_file.flush()?;

    Ok(())
}
