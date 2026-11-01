use std::{fs::File, io::Write, os::unix::fs::FileExt, thread};
#[inline(always)]
fn get_hour_str(h: usize) -> [u8; 20] {
    let (d, hr) = match h {
        0..=5 => (30, 18 + h),
        6..=29 => (31, h - 6),
        30..=53 => (1, h - 30),
        _ => (2, h - 54),
    };
    let mut out = *b"2026-10-00T00:00:00Z";
    if d < 30 {
        out[6] = b'1';
    }
    out[8] = b'0' + (d / 10) as u8;
    out[9] = b'0' + (d % 10) as u8;
    out[11] = b'0' + (hr / 10) as u8;
    out[12] = b'0' + (hr % 10) as u8;
    out
}
#[inline(always)]
fn get_hour_idx(ts: &[u8]) -> usize {
    let (d, h) = (
        (ts[8] - b'0') * 10 + (ts[9] - b'0'),
        ((ts[11] - b'0') as usize) * 10 + ((ts[12] - b'0') as usize),
    );
    match d {
        30 => h.saturating_sub(18),
        31 => 6 + h,
        1 => 30 + h,
        _ => 54 + h,
    }
}
#[inline(always)]
fn write_u32(buf: &mut Vec<u8>, mut n: u32) {
    if n == 0 {
        buf.push(b'0');
        return;
    }
    let mut b = [0u8; 10];
    let mut i = 10;
    while n > 0 {
        i -= 1;
        b[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    buf.extend_from_slice(&b[i..]);
}
#[inline(always)]
fn fast_hash(b: &[u8]) -> u32 {
    let mut h = 0x517cc1b7u32;
    let mut p = 0;
    let len = b.len();
    while p + 8 <= len {
        let w = unsafe { (b.as_ptr().add(p) as *const u64).read_unaligned() };
        h = (h.rotate_left(7) ^ (w as u32) ^ ((w >> 32) as u32)).wrapping_mul(0x27220a95);
        p += 8;
    }
    if p < len {
        let mut rem = [0u8; 8];
        rem[..len - p].copy_from_slice(&b[p..]);
        let w = u64::from_le_bytes(rem);
        h = (h.rotate_left(7) ^ (w as u32) ^ ((w >> 32) as u32)).wrapping_mul(0x27220a95);
    }
    (h ^ ((len as u32).wrapping_mul(0x9e3779b9))).wrapping_mul(0x27220a95)
}
#[derive(Clone, Copy)]
struct HashSlot {
    hash: u32,
    len: u16,
    id: u16,
}
#[derive(Clone, Copy)]
struct GroupEntry {
    stat: u32,
    head: u32,
}
#[derive(Clone, Copy)]
struct LatChunk {
    lats: [u32; 14],
    count: u8,
    _pad: [u8; 3],
    next: u32,
}
const TABLE_SIZE: usize = 4096;
const TABLE_MASK: usize = TABLE_SIZE - 1;
const NUM_WORKERS: usize = 12;
const CHUNK_SIZE: usize = 128 * 1024;
const NUM_HOURS: usize = 72;
const SLAB_LEN: usize = 2048;

struct WorkerOutput {
    slabs: [Option<Box<[GroupEntry; SLAB_LEN]>>; NUM_HOURS],
    chunks: Vec<LatChunk>,
}
fn main() -> std::io::Result<()> {
    let (data_dir, out_dir) = (
        std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string()),
        std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string()),
    );
    let (input_path, out_path) = (
        std::path::Path::new(&data_dir).join("requests.log"),
        std::path::Path::new(&out_dir).join("summary.txt"),
    );
    let file = File::open(&input_path)?;
    let file_len = file.metadata()?.len() as usize;
    if file_len == 0 {
        File::create(&out_path)?;
        return Ok(());
    }
    let mut probe_table = vec![
        HashSlot {
            hash: 0,
            len: 0,
            id: 0
        };
        TABLE_SIZE
    ];
    let mut ep_strings: Vec<Vec<u8>> = Vec::with_capacity(2048);
    let (mut s_buf, mut s_offset, mut s_rem) = (vec![0u8; 128 * 1024 + 512], 0u64, 0);
    while ep_strings.len() < 1842 && s_offset < file_len as u64 {
        let to_read = (128 * 1024).min(file_len - s_offset as usize);
        file.read_exact_at(&mut s_buf[s_rem..s_rem + to_read], s_offset)?;
        s_offset += to_read as u64;
        let avail = s_rem + to_read;
        let mut last_nl = avail;
        while last_nl > 0 && s_buf[last_nl - 1] != b'\n' {
            last_nl -= 1;
        }
        let mut pos = 0;
        while pos + 35 <= last_nl {
            let mut idx = pos + 28;
            while idx < last_nl && s_buf[idx] != b' ' {
                idx += 1;
            }
            let ep_start = idx + 1;
            idx = ep_start;
            while idx < last_nl && s_buf[idx] != b' ' {
                idx += 1;
            }
            let ep_slice = &s_buf[ep_start..idx];
            let h = fast_hash(ep_slice);
            let mut slot = (h as usize) & TABLE_MASK;
            loop {
                let e = &probe_table[slot];
                if e.hash == h
                    && e.len as usize == ep_slice.len()
                    && ep_strings[e.id as usize] == ep_slice
                {
                    break;
                }
                if e.len == 0 {
                    let id = ep_strings.len() as u16;
                    probe_table[slot] = HashSlot {
                        hash: h,
                        len: ep_slice.len() as u16,
                        id,
                    };
                    ep_strings.push(ep_slice.to_vec());
                    break;
                }
                slot = (slot + 1) & TABLE_MASK;
            }
            if ep_strings.len() == 1842 {
                break;
            }
            while idx < last_nl && s_buf[idx] != b'\n' {
                idx += 1;
            }
            pos = idx + 1;
        }
        s_rem = avail - last_nl;
        if s_rem > 0 {
            s_buf.copy_within(last_nl..avail, 0);
        }
    }
    drop(s_buf);
    drop(probe_table);
    ep_strings.sort_unstable();
    let _num_eps = ep_strings.len();
    let mut ep_table = vec![
        HashSlot {
            hash: 0,
            len: 0,
            id: 0
        };
        TABLE_SIZE
    ];
    for (i, ep) in ep_strings.iter().enumerate() {
        let h = fast_hash(ep);
        let mut slot = (h as usize) & TABLE_MASK;
        loop {
            if ep_table[slot].len == 0 {
                ep_table[slot] = HashSlot {
                    hash: h,
                    len: ep.len() as u16,
                    id: i as u16,
                };
                break;
            }
            slot = (slot + 1) & TABLE_MASK;
        }
    }
    let ep_table_ref: &'static [HashSlot] = Box::leak(ep_table.into_boxed_slice());
    let ep_strings_ref: &'static [Vec<u8>] = Box::leak(ep_strings.into_boxed_slice());
    let (approx_chunk, mut chunk_bounds) =
        (file_len / NUM_WORKERS, Vec::with_capacity(NUM_WORKERS + 1));
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
    let mut handles = Vec::with_capacity(NUM_WORKERS);
    for i in 0..NUM_WORKERS {
        let (start_pos, end_pos, path) = (chunk_bounds[i], chunk_bounds[i + 1], input_path.clone());
        let handle = thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let t_file = File::open(&path).unwrap();
                const EMPTY: Option<Box<[GroupEntry; SLAB_LEN]>> = None;
                let mut slabs = [EMPTY; NUM_HOURS];
                let mut chunks = Vec::with_capacity(29_000);
                let (mut buf, mut cur_offset, mut rem_len) =
                    (vec![0u8; CHUNK_SIZE + 4096], start_pos, 0);
                while cur_offset < end_pos || rem_len > 0 {
                    let to_read = (CHUNK_SIZE).min(end_pos.saturating_sub(cur_offset));
                    let read_bytes = if to_read > 0 {
                        t_file
                            .read_exact_at(&mut buf[rem_len..rem_len + to_read], cur_offset as u64)
                            .unwrap();
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
                    let (slice, mut pos, mut last_hour_str, mut cached_slab_ptr) = (
                        &buf[..last_nl],
                        0,
                        [0u8; 13],
                        std::ptr::null_mut::<GroupEntry>(),
                    );
                    while pos + 35 <= last_nl {
                        let hour_prefix = &slice[pos..pos + 13];
                        let slab_ptr = if hour_prefix == &last_hour_str {
                            cached_slab_ptr
                        } else {
                            last_hour_str.copy_from_slice(hour_prefix);
                            let h_idx = get_hour_idx(hour_prefix);
                            if slabs[h_idx].is_none() {
                                slabs[h_idx] =
                                    Some(Box::new([GroupEntry { stat: 0, head: 0 }; SLAB_LEN]));
                            }
                            let ptr = slabs[h_idx].as_mut().unwrap().as_mut_ptr();
                            cached_slab_ptr = ptr;
                            ptr
                        };
                        let ep_start = match slice[pos + 28] {
                            b'G' => pos + 32,
                            b'P' => {
                                if slice[pos + 29] == b'U' {
                                    pos + 32
                                } else if slice[pos + 29] == b'O' {
                                    pos + 33
                                } else {
                                    pos + 34
                                }
                            }
                            b'H' => pos + 33,
                            b'D' => pos + 35,
                            _ => pos + 36,
                        };
                        let (mut idx, mut h) = (ep_start, 0x517cc1b7u32);
                        loop {
                            let w =
                                unsafe { (slice.as_ptr().add(idx) as *const u64).read_unaligned() };
                            let has_space = (w ^ 0x2020202020202020u64)
                                .wrapping_sub(0x0101010101010101u64)
                                & !(w ^ 0x2020202020202020u64)
                                & 0x8080808080808080u64;
                            if has_space != 0 {
                                let sp = (has_space.trailing_zeros() >> 3) as usize;
                                if sp > 0 {
                                    let rem_w = w & ((1u64 << (sp * 8)).wrapping_sub(1));
                                    h = (h.rotate_left(7)
                                        ^ (rem_w as u32)
                                        ^ ((rem_w >> 32) as u32))
                                        .wrapping_mul(0x27220a95);
                                }
                                idx += sp;
                                break;
                            }
                            h = (h.rotate_left(7) ^ (w as u32) ^ ((w >> 32) as u32))
                                .wrapping_mul(0x27220a95);
                            idx += 8;
                        }
                        let ep_len = idx - ep_start;
                        h = (h ^ ((ep_len as u32).wrapping_mul(0x9e3779b9)))
                            .wrapping_mul(0x27220a95);
                        let mut slot = (h as usize) & TABLE_MASK;
                        let ep_id = loop {
                            let entry = &ep_table_ref[slot];
                            if entry.hash == h
                                && entry.len as usize == ep_len
                                && ep_strings_ref[entry.id as usize] == &slice[ep_start..idx]
                            {
                                break entry.id as usize;
                            }
                            slot = (slot + 1) & TABLE_MASK;
                        };
                        let is_5xx = (slice[idx + 1] == b'5') as u32;
                        let mut p = idx + 5;
                        let mut lat = (slice[p] - b'0') as u32;
                        p += 1;
                        while slice[p] != b'\n' {
                            lat = lat * 10 + (slice[p] - b'0') as u32;
                            p += 1;
                        }
                        pos = p + 1;
                        let g = unsafe { &mut *slab_ptr.add(ep_id) };
                        let cnt = g.stat & 0xFFFF;
                        g.stat += 1 + (is_5xx << 16);
                        if cnt == 0 {
                            g.head = lat;
                        } else if cnt == 1 {
                            let (prev_lat, new_idx) = (g.head, chunks.len() as u32);
                            let mut chunk = LatChunk {
                                lats: [0; 14],
                                count: 2,
                                _pad: [0; 3],
                                next: u32::MAX,
                            };
                            chunk.lats[0] = prev_lat;
                            chunk.lats[1] = lat;
                            chunks.push(chunk);
                            g.head = new_idx;
                        } else {
                            let c_idx = g.head as usize;
                            if chunks[c_idx].count < 14 {
                                let n = chunks[c_idx].count as usize;
                                chunks[c_idx].lats[n] = lat;
                                chunks[c_idx].count += 1;
                            } else {
                                let new_idx = chunks.len() as u32;
                                let mut chunk = LatChunk {
                                    lats: [0; 14],
                                    count: 1,
                                    _pad: [0; 3],
                                    next: g.head,
                                };
                                chunk.lats[0] = lat;
                                chunks.push(chunk);
                                g.head = new_idx;
                            }
                        }
                    }
                    rem_len = total_available - last_nl;
                    if rem_len > 0 {
                        buf.copy_within(last_nl..total_available, 0);
                    }
                }
                WorkerOutput { slabs, chunks }
            })
            .unwrap();
        handles.push(handle);
    }
    let workers: Vec<WorkerOutput> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let workers_ref: &'static [WorkerOutput] = Box::leak(workers.into_boxed_slice());
    let out_handles: Vec<_> = (0..12)
        .map(|t| {
            thread::Builder::new()
                .stack_size(256 * 1024)
                .spawn(move || {
                    let (h_start, h_end) = (t * 6, (t + 1) * 6);
                    let mut buf = Vec::with_capacity(900_000);
                    let mut scratch = Vec::with_capacity(4096);
                    for hour_idx in h_start..h_end {
                        let hour_bytes = get_hour_str(hour_idx);
                        let active_w: Vec<&WorkerOutput> = workers_ref
                            .iter()
                            .filter(|w| w.slabs[hour_idx].is_some())
                            .collect();
                        if active_w.is_empty() {
                            continue;
                        }
                        for (ep_id, ep) in ep_strings_ref.iter().enumerate() {
                            let (mut count, mut errors) = (0u32, 0u32);
                            for w in &active_w {
                                let g = &w.slabs[hour_idx].as_ref().unwrap()[ep_id];
                                count += g.stat & 0xFFFF;
                                errors += g.stat >> 16;
                            }
                            if count == 0 {
                                continue;
                            }
                            let (p50, p95, p99) = if count == 1 {
                                let mut v = 0;
                                for w in &active_w {
                                    let g = &w.slabs[hour_idx].as_ref().unwrap()[ep_id];
                                    if g.stat & 0xFFFF > 0 {
                                        v = g.head;
                                        break;
                                    }
                                }
                                (v, v, v)
                            } else {
                                scratch.clear();
                                for w in &active_w {
                                    let g = &w.slabs[hour_idx].as_ref().unwrap()[ep_id];
                                    let w_cnt = g.stat & 0xFFFF;
                                    if w_cnt == 1 {
                                        scratch.push(g.head);
                                    } else if w_cnt > 1 {
                                        let mut curr = g.head;
                                        while curr != u32::MAX {
                                            let c = &w.chunks[curr as usize];
                                            scratch.extend_from_slice(&c.lats[..c.count as usize]);
                                            curr = c.next;
                                        }
                                    }
                                }
                                let n = scratch.len();
                                if n == 2 {
                                    let (a, b) = if scratch[0] <= scratch[1] {
                                        (scratch[0], scratch[1])
                                    } else {
                                        (scratch[1], scratch[0])
                                    };
                                    (a, b, b)
                                } else {
                                    scratch.sort_unstable();
                                    (
                                        scratch[(50 * n + 99) / 100 - 1],
                                        scratch[(95 * n + 99) / 100 - 1],
                                        scratch[(99 * n + 99) / 100 - 1],
                                    )
                                }
                            };
                            let error_ppm = (errors as u64 * 1_000_000 / count as u64) as u32;
                            buf.extend_from_slice(&hour_bytes);
                            buf.push(b' ');
                            buf.extend_from_slice(ep);
                            for val in [count, p50, p95, p99, errors, error_ppm] {
                                buf.push(b' ');
                                write_u32(&mut buf, val);
                            }
                            buf.push(b'\n');
                        }
                    }
                    buf
                })
                .unwrap()
        })
        .collect();
    let mut out_file = File::create(&out_path)?;
    for h in out_handles {
        let b = h.join().unwrap();
        out_file.write_all(&b)?;
    }
    out_file.flush()?;
    Ok(())
}
