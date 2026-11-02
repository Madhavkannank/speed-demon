use std::{
    arch::x86_64::*,
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

#[inline(always)]
unsafe fn find_newline_avx2(buf: *const u8, mut pos: usize, max: usize) -> usize {
    let target = _mm256_set1_epi8(b'\n' as i8);
    while pos + 32 <= max {
        let chunk = _mm256_loadu_si256(buf.add(pos) as *const __m256i);
        let cmp = _mm256_cmpeq_epi8(chunk, target);
        let mask = _mm256_movemask_epi8(cmp);
        if mask != 0 {
            return pos + mask.trailing_zeros() as usize;
        }
        pos += 32;
    }
    while pos < max && *buf.add(pos) != b'\n' {
        pos += 1;
    }
    pos
}

#[inline(always)]
unsafe fn crc32_hash(slice: &[u8]) -> u32 {
    let mut crc = 0u64;
    let mut p = slice.as_ptr();
    let mut len = slice.len();
    while len >= 8 {
        let w = (p as *const u64).read_unaligned();
        crc = _mm_crc32_u64(crc, w);
        p = p.add(8);
        len -= 8;
    }
    if len > 0 {
        let mut rem = [0u8; 8];
        std::ptr::copy_nonoverlapping(p, rem.as_mut_ptr(), len);
        let w = u64::from_le_bytes(rem);
        crc = _mm_crc32_u64(crc, w);
    }
    crc as u32
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
                        let nl = unsafe { find_newline_avx2(slice.as_ptr(), pos + 35, last_nl) };
                        let mut p_sp = nl - 1;
                        let mut lat = 0u32;
                        let mut mult = 1u32;
                        while slice[p_sp] != b' ' {
                            lat += (slice[p_sp] - b'0') as u32 * mult;
                            mult *= 10;
                            p_sp -= 1;
                        }
                        let is_5xx = (slice[p_sp - 3] == b'5') as u32;
                        let ep_end = p_sp - 4;
                        let ep_len = ep_end - ep_start;
                        let ep_slice = &slice[ep_start..ep_end];
                        pos = nl + 1;

                        let h = unsafe { crc32_hash(ep_slice) };
                        let hl = ((h as u64) << 32) | (ep_len as u64);

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

    let out_handles: Vec<_> = (0..12)
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

    Ok(())
}
