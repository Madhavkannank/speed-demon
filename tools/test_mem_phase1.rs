use std::{
    fs::File,
    io::Write,
    os::unix::fs::FileExt,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
    thread,
    time::Instant,
};

#[inline(always)]
fn get_hour_str(h: usize) -> [u8; 20] {
    let (tot, mut out) = (h + 18, *b"2026-10-00T00:00:00Z");
    let (day, hr) = (tot / 24, tot % 24);
    let (m, d) = if day < 2 { (b'0', 30 + day) } else { (b'1', day - 1) };
    out[6] = m;
    out[8] = b'0' + (d / 10) as u8;
    out[9] = b'0' + (d % 10) as u8;
    out[11] = b'0' + (hr / 10) as u8;
    out[12] = b'0' + (hr % 10) as u8;
    out
}

#[inline(always)]
fn get_hour_idx(ts: &[u8]) -> usize {
    let d = (ts[8] - b'0') * 10 + (ts[9] - b'0');
    let h = (ts[11] - b'0') as usize * 10 + (ts[12] - b'0') as usize;
    let day = if d >= 30 { d - 30 } else { d + 1 } as usize;
    day * 24 + h - 18
}

#[inline(always)]
fn write_u32(buf: &mut Vec<u8>, mut n: u32) {
    let (mut b, mut i) = ([0u8; 10], 10);
    loop {
        i -= 1;
        b[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    buf.extend_from_slice(&b[i..]);
}

struct AtomicSlot {
    hash_len: AtomicU64,
    id: AtomicU32,
}

#[derive(Clone, Copy)]
struct GroupEntry {
    stat: u32,
    head: u32,
    lat0: u32,
    lat1: u32,
}

#[derive(Clone, Copy)]
struct LatChunk {
    lats: [u32; 6],
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
    let t0 = Instant::now();
    let (in_dir, out_dir) = (
        std::env::var("DATA_DIR").unwrap_or_else(|_| "/tmp/sd-full-a".into()),
        std::env::var("OUT_DIR").unwrap_or_else(|_| "/tmp".into()),
    );
    let (input_path, out_path) = (
        format!("{in_dir}/requests.log"),
        format!("{out_dir}/summary.txt"),
    );
    let file = File::open(&input_path)?;
    let file_len = file.metadata()?.len() as usize;

    let (approx_chunk, mut chunk_bounds) = (file_len / NUM_WORKERS, vec![0]);
    let mut scan_buf = [0u8; 1024];
    for i in 1..NUM_WORKERS {
        let approx_pos = i * approx_chunk;
        file.read_exact_at(&mut scan_buf, approx_pos as u64)?;
        let offset = scan_buf.iter().position(|&b| b == b'\n').unwrap_or(0);
        chunk_bounds.push(approx_pos + offset + 1);
    }
    chunk_bounds.push(file_len);

    let slots: Vec<AtomicSlot> = (0..TABLE_SIZE)
        .map(|_| AtomicSlot {
            hash_len: AtomicU64::new(0),
            id: AtomicU32::new(u32::MAX),
        })
        .collect();
    let table_ref: &'static [AtomicSlot] = Box::leak(slots.into_boxed_slice());
    let next_id: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(0)));
    let mut ep_storage: Vec<Vec<u8>> = (0..2048).map(|_| Vec::new()).collect();
    let ep_ptrs: Vec<usize> = ep_storage.iter_mut().map(|v| v as *mut _ as usize).collect();
    let ep_ptrs_ref: &'static [usize] = Box::leak(ep_ptrs.into_boxed_slice());

    let t_init = Instant::now();

    let mut handles = Vec::with_capacity(NUM_WORKERS);
    for i in 0..NUM_WORKERS {
        let (start_pos, end_pos, path) = (chunk_bounds[i], chunk_bounds[i + 1], input_path.clone());
        let handle = thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(move || {
                let t_file = File::open(&path).unwrap();
                const EMPTY: Option<Box<[GroupEntry; SLAB_LEN]>> = None;
                let mut slabs = [EMPTY; NUM_HOURS];
                let mut chunks = Vec::with_capacity(44_000);
                let (mut buf, mut cur_offset, mut rem_len) =
                    (vec![0u8; CHUNK_SIZE + 4096], start_pos, 0);
                let mut local_cache = [(0u64, 0u32); 1024];

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
                            let ptr = slabs[h_idx]
                                .get_or_insert_with(|| {
                                    Box::new([GroupEntry { stat: 0, head: 0, lat0: 0, lat1: 0 }; SLAB_LEN])
                                })
                                .as_mut_ptr();
                            cached_slab_ptr = ptr;
                            ptr
                        };
                        let ep_start = pos
                            + match slice[pos + 28] {
                                b'G' => 32,
                                b'P' => {
                                    32 + (slice[pos + 29] != b'U') as usize
                                        + (slice[pos + 29] == b'A') as usize
                                }
                                b'H' => 33,
                                b'D' => 35,
                                _ => 36,
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
                        let hl = ((h as u64) << 32) | (ep_len as u64);
                        let ep_slice = &slice[ep_start..idx];

                        let cache_slot = (h as usize) & 1023;
                        let (c_hl, c_id) = local_cache[cache_slot];
                        let ep_id = if c_hl == hl {
                            c_id as usize
                        } else {
                            let mut slot = (h as usize) & TABLE_MASK;
                            let id = loop {
                                let s = &table_ref[slot];
                                let curr_hl = s.hash_len.load(Ordering::Relaxed);
                                if curr_hl == hl {
                                    while s.id.load(Ordering::Relaxed) == u32::MAX {
                                        std::hint::spin_loop();
                                    }
                                    let cand_id = s.id.load(Ordering::Acquire);
                                    let cand_ep = unsafe {
                                        &*(ep_ptrs_ref[cand_id as usize] as *const Vec<u8>)
                                    };
                                    if cand_ep == ep_slice {
                                        break cand_id;
                                    }
                                }
                                if curr_hl == 0 {
                                    if s.hash_len
                                        .compare_exchange_weak(
                                            0,
                                            hl,
                                            Ordering::AcqRel,
                                            Ordering::Relaxed,
                                        )
                                        .is_ok()
                                    {
                                        let new_id = next_id.fetch_add(1, Ordering::Relaxed);
                                        let ep_vec = unsafe {
                                            &mut *(ep_ptrs_ref[new_id as usize] as *mut Vec<u8>)
                                        };
                                        *ep_vec = ep_slice.to_vec();
                                        s.id.store(new_id, Ordering::Release);
                                        break new_id;
                                    }
                                    continue;
                                }
                                slot = (slot + 1) & TABLE_MASK;
                            };
                            local_cache[cache_slot] = (hl, id);
                            id as usize
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
                            g.lat0 = lat;
                        } else if cnt == 1 {
                            g.lat1 = lat;
                        } else if cnt == 2 {
                            let new_idx = chunks.len() as u32;
                            let mut chunk = LatChunk {
                                lats: [0; 6],
                                count: 1,
                                _pad: [0; 3],
                                next: u32::MAX,
                            };
                            chunk.lats[0] = lat;
                            chunks.push(chunk);
                            g.head = new_idx;
                        } else {
                            let c_idx = g.head as usize;
                            if chunks[c_idx].count < 6 {
                                let n = chunks[c_idx].count as usize;
                                chunks[c_idx].lats[n] = lat;
                                chunks[c_idx].count += 1;
                            } else {
                                let new_idx = chunks.len() as u32;
                                let mut chunk = LatChunk {
                                    lats: [0; 6],
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
    let t_ingest = Instant::now();

    let workers_ref: &'static [WorkerOutput] = Box::leak(workers.into_boxed_slice());

    let mut perm: Vec<usize> = (0..next_id.load(Ordering::Relaxed) as usize).collect();
    perm.sort_unstable_by_key(|&i| &ep_storage[i]);
    let perm_ref: &'static [usize] = Box::leak(perm.into_boxed_slice());
    let ep_storage_ref: &'static [Vec<u8>] = Box::leak(ep_storage.into_boxed_slice());

    let t_sort = Instant::now();

    return Ok(()); let out_handles: Vec<_> = (0..12)
        .map(|t| {
            thread::Builder::new()
                .stack_size(64 * 1024)
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
                        for &ep_id in perm_ref.iter() {
                            let ep = &ep_storage_ref[ep_id];
                            let (mut count, mut errors) = (0u32, 0u32);
                            scratch.clear();
                            for w in &active_w {
                                let g = &w.slabs[hour_idx].as_ref().unwrap()[ep_id];
                                let w_cnt = g.stat & 0xFFFF;
                                if w_cnt == 0 {
                                    continue;
                                }
                                count += w_cnt;
                                errors += g.stat >> 16;
                                if w_cnt == 1 {
                                    scratch.push(g.lat0);
                                } else if w_cnt == 2 {
                                    scratch.push(g.lat0);
                                    scratch.push(g.lat1);
                                } else {
                                    scratch.push(g.lat0);
                                    scratch.push(g.lat1);
                                    let mut curr = g.head;
                                    while curr != u32::MAX {
                                        let c = &w.chunks[curr as usize];
                                        scratch.extend_from_slice(&c.lats[..c.count as usize]);
                                        curr = c.next;
                                    }
                                }
                            }
                            if count == 0 {
                                continue;
                            }
                            let n = scratch.len();
                            scratch.sort_unstable();
                            let (p50, p95, p99) = (
                                scratch[(50 * n + 99) / 100 - 1],
                                scratch[(95 * n + 99) / 100 - 1],
                                scratch[(99 * n + 99) / 100 - 1],
                            );
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

    let bufs: Vec<Vec<u8>> = out_handles.into_iter().map(|h| h.join().unwrap()).collect();
    let t_format = Instant::now();

    let mut out_file = File::create(&out_path)?;
    for b in &bufs {
        out_file.write_all(b)?;
    }
    out_file.flush()?;
    let t_write = Instant::now();

    println!("TIMING BREAKDOWN:");
    println!("  Init:    {:6.2} ms", t_init.duration_since(t0).as_secs_f64() * 1000.0);
    println!("  Ingest:  {:6.2} ms", t_ingest.duration_since(t_init).as_secs_f64() * 1000.0);
    println!("  Sort:    {:6.2} ms", t_sort.duration_since(t_ingest).as_secs_f64() * 1000.0);
    println!("  Format:  {:6.2} ms", t_format.duration_since(t_sort).as_secs_f64() * 1000.0);
    println!("  Write:   {:6.2} ms", t_write.duration_since(t_format).as_secs_f64() * 1000.0);
    println!("  TOTAL:   {:6.2} ms", t_write.duration_since(t0).as_secs_f64() * 1000.0);

    let mut max_chunks = 0;
    for (w_i, w) in workers_ref.iter().enumerate() {
        println!("  Worker {}: chunks={}", w_i, w.chunks.len());
        if w.chunks.len() > max_chunks { max_chunks = w.chunks.len(); }
    }
    println!("Max chunks per worker: {}", max_chunks);

    Ok(())
}
