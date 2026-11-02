use std::{fs::File, io::Write, sync::atomic::Ordering::*, sync::atomic::*, thread};

extern "C" {
    fn mmap(
        addr: *mut std::ffi::c_void,
        length: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut std::ffi::c_void;
}

#[inline(always)]
fn get_hour_str(h: usize) -> [u8; 20] {
    let (tot, mut out) = (h + 18, *b"2026-10-00T00:00:00Z");
    let (day, hr) = (tot / 24, tot % 24);
    let d = if day < 2 { 30 + day } else { day - 1 };
    if day >= 2 {
        out[6] = b'1';
    }
    out[8..10].copy_from_slice(&[b'0' + (d / 10) as u8, b'0' + (d % 10) as u8]);
    out[11..13].copy_from_slice(&[b'0' + (hr / 10) as u8, b'0' + (hr % 10) as u8]);
    out
}

#[inline(always)]
fn get_hour_idx(ts: &[u8]) -> usize {
    let d = (ts[8] - b'0') * 10 + (ts[9] - b'0');
    let h = (ts[11] - b'0') as usize * 10 + (ts[12] - b'0') as usize;
    (if d >= 30 { d - 30 } else { d + 1 } as usize) * 24 + h - 18
}

#[inline(always)]
fn write_u32(buf: &mut Vec<u8>, mut n: u32) {
    if n == 0 {
        return buf.push(b'0');
    }
    let (mut b, mut i) = ([0u8; 10], 10);
    while n > 0 {
        i -= 1;
        b[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    buf.extend_from_slice(&b[i..]);
}

struct AtomicSlot(AtomicU64, AtomicU32);
#[derive(Clone, Copy)]
struct GroupEntry(u32, u32);
#[derive(Clone, Copy)]
struct LatChunk {
    lats: [u32; 14],
    count: u32,
    next: u32,
}

const NUM_WORKERS: usize = 12;
const NUM_HOURS: usize = 72;
const SLAB_LEN: usize = 2048;
const TABLE_MASK: usize = 4095;

struct WorkerOutput {
    slabs: [Option<Box<[GroupEntry; SLAB_LEN]>>; NUM_HOURS],
    chunks: Vec<LatChunk>,
}

fn main() -> std::io::Result<()> {
    let (d_dir, o_dir) = (
        std::env::var("DATA_DIR").unwrap_or("/data".into()),
        std::env::var("OUT_DIR").unwrap_or("/out".into()),
    );
    let (in_p, out_p) = (
        format!("{d_dir}/requests.log"),
        format!("{o_dir}/summary.txt"),
    );
    let file = File::open(&in_p)?;
    let file_len = file.metadata()?.len() as usize;
    if file_len == 0 {
        return File::create(&out_p).map(|_| ());
    }

    let mmap_ptr = unsafe {
        mmap(
            std::ptr::null_mut(),
            file_len,
            1, // PROT_READ
            1, // MAP_SHARED
            std::os::unix::io::AsRawFd::as_raw_fd(&file),
            0,
        )
    };
    if mmap_ptr == usize::MAX as *mut std::ffi::c_void {
        panic!("mmap failed");
    }
    let slice: &'static [u8] =
        unsafe { std::slice::from_raw_parts(mmap_ptr as *const u8, file_len) };

    let chunk_sz = file_len / NUM_WORKERS;
    let mut chunk_bounds = vec![0];
    for i in 1..NUM_WORKERS {
        let mut pos = i * chunk_sz;
        while pos < file_len && slice[pos] != b'\n' {
            pos += 1;
        }
        chunk_bounds.push(pos + 1);
    }
    chunk_bounds.push(file_len);

    let table: &'static [AtomicSlot] = Box::leak(
        (0..4096)
            .map(|_| AtomicSlot(AtomicU64::new(0), AtomicU32::new(u32::MAX)))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let next_id: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(0)));
    let mut ep_store = vec![Vec::new(); 2048];
    let ep_ptrs: &'static [usize] = Box::leak(
        ep_store
            .iter_mut()
            .map(|v| v as *mut Vec<u8> as usize)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );

    let workers: Vec<WorkerOutput> = (0..NUM_WORKERS)
        .map(|i| {
            let (mut pos, end_pos) = (chunk_bounds[i], chunk_bounds[i + 1]);
            thread::Builder::new()
                .stack_size(256 * 1024)
                .spawn(move || {
                    const EMPTY: Option<Box<[GroupEntry; SLAB_LEN]>> = None;
                    let mut slabs = [EMPTY; NUM_HOURS];
                    let mut chunks = Vec::with_capacity(29_000);
                    let mut local_cache = [(0u64, 0u32); 1024];
                    let (mut last_hour_str, mut cached_slab_ptr) =
                        ([0u8; 13], std::ptr::null_mut::<GroupEntry>());

                    while pos + 35 <= end_pos {
                        let hp = &slice[pos..pos + 13];
                        let slab_ptr = if hp == &last_hour_str {
                            cached_slab_ptr
                        } else {
                            last_hour_str.copy_from_slice(hp);
                            let h = get_hour_idx(hp);
                            if slabs[h].is_none() {
                                slabs[h] = Some(Box::new([GroupEntry(0, 0); SLAB_LEN]));
                            }
                            cached_slab_ptr = slabs[h].as_mut().unwrap().as_mut_ptr();
                            cached_slab_ptr
                        };
                        let ep_start = pos
                            + match slice[pos + 28] {
                                b'G' => 32,
                                b'H' => 33,
                                b'D' => 35,
                                b'P' => match slice[pos + 29] {
                                    b'U' => 32,
                                    b'O' => 33,
                                    _ => 34,
                                },
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
                                let s = &table[slot];
                                let curr_hl = s.0.load(Relaxed);
                                if curr_hl == hl {
                                    while s.1.load(Relaxed) == u32::MAX {
                                        std::hint::spin_loop();
                                    }
                                    let cand_id = s.1.load(Acquire);
                                    let cand_ep =
                                        unsafe { &*(ep_ptrs[cand_id as usize] as *const Vec<u8>) };
                                    if cand_ep == ep_slice {
                                        break cand_id;
                                    }
                                }
                                if curr_hl == 0 {
                                    if s.0.compare_exchange_weak(0, hl, AcqRel, Relaxed).is_ok() {
                                        let new_id = next_id.fetch_add(1, Relaxed);
                                        let ep_vec = unsafe {
                                            &mut *(ep_ptrs[new_id as usize] as *mut Vec<u8>)
                                        };
                                        *ep_vec = ep_slice.to_vec();
                                        s.1.store(new_id, Release);
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
                        let (mut p, mut lat) = (idx + 6, (slice[idx + 5] - b'0') as u32);
                        while slice[p] != b'\n' {
                            lat = lat * 10 + (slice[p] - b'0') as u32;
                            p += 1;
                        }
                        pos = p + 1;
                        let g = unsafe { &mut *slab_ptr.add(ep_id) };
                        let cnt = g.0 & 0xFFFF;
                        g.0 += 1 + (is_5xx << 16);
                        if cnt == 0 {
                            g.1 = lat;
                        } else if cnt == 1 {
                            let (prev_lat, new_idx) = (g.1, chunks.len() as u32);
                            let mut chunk = LatChunk {
                                lats: [0; 14],
                                count: 2,
                                next: u32::MAX,
                            };
                            chunk.lats[0] = prev_lat;
                            chunk.lats[1] = lat;
                            chunks.push(chunk);
                            g.1 = new_idx;
                        } else {
                            let c_idx = g.1 as usize;
                            if chunks[c_idx].count < 14 {
                                let n = chunks[c_idx].count as usize;
                                chunks[c_idx].lats[n] = lat;
                                chunks[c_idx].count += 1;
                            } else {
                                let new_idx = chunks.len() as u32;
                                let mut chunk = LatChunk {
                                    lats: [0; 14],
                                    count: 1,
                                    next: g.1,
                                };
                                chunk.lats[0] = lat;
                                chunks.push(chunk);
                                g.1 = new_idx;
                            }
                        }
                    }
                    WorkerOutput { slabs, chunks }
                })
                .unwrap()
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect();
    let workers_ref: &'static [WorkerOutput] = Box::leak(workers.into_boxed_slice());

    let mut perm: Vec<usize> = (0..next_id.load(Relaxed) as usize).collect();
    perm.sort_unstable_by_key(|&i| &ep_store[i]);
    let (perm_ref, ep_store_ref): (&'static [usize], &'static [Vec<u8>]) = (
        Box::leak(perm.into_boxed_slice()),
        Box::leak(ep_store.into_boxed_slice()),
    );

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
                        for &ep_id in perm_ref.iter() {
                            let ep = &ep_store_ref[ep_id];
                            let (mut count, mut errors) = (0u32, 0u32);
                            for w in &active_w {
                                let g = &w.slabs[hour_idx].as_ref().unwrap()[ep_id];
                                count += g.0 & 0xFFFF;
                                errors += g.0 >> 16;
                            }
                            if count == 0 {
                                continue;
                            }
                            let (p50, p95, p99) = if count == 1 {
                                let mut v = 0;
                                for w in &active_w {
                                    let g = &w.slabs[hour_idx].as_ref().unwrap()[ep_id];
                                    if g.0 & 0xFFFF > 0 {
                                        v = g.1;
                                        break;
                                    }
                                }
                                (v, v, v)
                            } else {
                                scratch.clear();
                                for w in &active_w {
                                    let g = &w.slabs[hour_idx].as_ref().unwrap()[ep_id];
                                    let w_cnt = g.0 & 0xFFFF;
                                    if w_cnt == 1 {
                                        scratch.push(g.1);
                                    } else if w_cnt > 1 {
                                        let mut curr = g.1;
                                        while curr != u32::MAX {
                                            let c = &w.chunks[curr as usize];
                                            scratch.extend_from_slice(&c.lats[..c.count as usize]);
                                            curr = c.next;
                                        }
                                    }
                                }
                                let n = scratch.len();
                                if n == 2 {
                                    (
                                        scratch[0].min(scratch[1]),
                                        scratch[0].max(scratch[1]),
                                        scratch[0].max(scratch[1]),
                                    )
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
    let mut out_file = File::create(&out_p)?;
    for h in out_handles {
        out_file.write_all(&h.join().unwrap())?;
    }
    out_file.flush()
}
