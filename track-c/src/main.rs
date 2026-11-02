use std::arch::x86_64::*;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread;

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, d: i32, o: i64) -> *mut u8;
    fn munmap(a: *mut u8, l: usize) -> i32;
}

#[inline(always)]
fn fast_hash(s: &[u8]) -> u64 {
    if s.is_empty() {
        return 0x517cc1b727220a95;
    }
    let (l, p) = (s.len(), s.as_ptr());
    const K: (u64, u64, u64) = (0x9e3779b97f4a7c15, 0xc6a4a7935bd1e995, 0xbf58476d1ce4e5b9);
    unsafe {
        match l {
            1..=3 => (*p as u64
                | ((*p.add(l >> 1) as u64) << 8)
                | ((*p.add(l - 1) as u64) << 16)
                | ((l as u64) << 24))
                .wrapping_mul(K.0),
            4..=7 => {
                (((p as *const u32).read_unaligned() as u64)
                    | (((p.add(l - 4) as *const u32).read_unaligned() as u64) << 32))
                    .wrapping_mul(K.0)
                    ^ (l as u64).wrapping_mul(K.1)
            }
            8..=15 => {
                (p as *const u64).read_unaligned().wrapping_mul(K.0)
                    ^ (p.add(l - 8) as *const u64)
                        .read_unaligned()
                        .wrapping_mul(K.1)
                    ^ (l as u64).wrapping_mul(K.2)
            }
            _ => {
                let (mut h, mut i) = ((l as u64).wrapping_mul(K.0), 0);
                while i + 8 <= l {
                    h = (h ^ (p.add(i) as *const u64).read_unaligned().wrapping_mul(K.1))
                        .rotate_left(27)
                        .wrapping_mul(K.2);
                    i += 8;
                }
                if i < l {
                    h = (h
                        ^ (p.add(l - 8) as *const u64)
                            .read_unaligned()
                            .wrapping_mul(K.1))
                    .wrapping_mul(K.2);
                }
                h
            }
        }
    }
}

#[inline(always)]
unsafe fn find_char_avx2(p_raw: *const u8, mut p: usize, end: usize, t: u8) -> usize {
    let tv = _mm256_set1_epi8(t as i8);
    while p + 32 <= end {
        let m = _mm256_movemask_epi8(_mm256_cmpeq_epi8(
            _mm256_loadu_si256(p_raw.add(p) as *const _),
            tv,
        )) as u32;
        if m != 0 {
            return p + m.trailing_zeros() as usize;
        }
        p += 32;
    }
    while p < end && *p_raw.add(p) != t {
        p += 1;
    }
    p
}

#[inline(always)]
unsafe fn parse_u64_comma(p_raw: *const u8, mut p: usize) -> (u64, usize) {
    let mut v = 0u64;
    while *p_raw.add(p) != b',' {
        v = v * 10 + (*p_raw.add(p) - b'0') as u64;
        p += 1;
    }
    (v, p + 1)
}

#[inline(always)]
unsafe fn parse_amount(p_raw: *const u8, mut p: usize) -> (i64, usize) {
    let neg = *p_raw.add(p) == b'-';
    if neg {
        p += 1;
    }
    let mut dol = 0i64;
    while *p_raw.add(p) != b'.' {
        dol = dol * 10 + (*p_raw.add(p) - b'0') as i64;
        p += 1;
    }
    let cents = ((*p_raw.add(p + 1) - b'0') as i64) * 10 + ((*p_raw.add(p + 2) - b'0') as i64);
    let tot = dol * 100 + cents;
    p += 3;
    (
        if neg { -tot } else { tot },
        if *p_raw.add(p) == b'\r' { p + 2 } else { p + 1 },
    )
}

fn format_cents(cents: i64) -> String {
    let (s, c) = (if cents < 0 { "-" } else { "" }, cents.abs());
    format!("{}{}.{:02}", s, c / 100, c % 100)
}

fn load_regions(path: &std::path::Path) -> std::io::Result<(Vec<u8>, Vec<String>)> {
    let bytes = fs::read(path)?;
    let (mut region_map, mut country_names, mut country_table) = (
        vec![0u8; 1_000_000],
        vec!["_UNMATCHED".to_string()],
        vec![(0u32, 0u8); 1024],
    );
    let (len, mut p) = (bytes.len(), 0);
    while p < len && bytes[p] != b'\n' {
        p += 1;
    }
    if p < len {
        p += 1;
    }
    while p < len {
        if bytes[p] == b'\r' || bytes[p] == b'\n' {
            p += 1;
            continue;
        }
        let (mut reg_id, c_start) = (0usize, 0);
        while p < len && bytes[p].is_ascii_digit() {
            reg_id = reg_id * 10 + (bytes[p] - b'0') as usize;
            p += 1;
        }
        if reg_id >= region_map.len() {
            region_map.resize(reg_id + 1, 0);
        }
        if p < len && bytes[p] == b',' {
            p += 1;
        }
        let (c_start, mut h) = (p, 0x517cc1b7u32);
        while p < len && bytes[p] != b'\r' && bytes[p] != b'\n' {
            h = (h.rotate_left(5) ^ (bytes[p] as u32)).wrapping_mul(0x9e3779b9);
            p += 1;
        }
        let c_slice = &bytes[c_start..p];
        if p < len && bytes[p] == b'\r' {
            p += 1;
        }
        if p < len && bytes[p] == b'\n' {
            p += 1;
        }
        let mut slot = (h as usize) & 1023;
        let c_id = loop {
            let (sh, id) = country_table[slot];
            if sh == h && country_names[id as usize].as_bytes() == c_slice {
                break id;
            }
            if sh == 0 {
                let id = country_names.len() as u8;
                country_names.push(String::from_utf8_lossy(c_slice).to_string());
                country_table[slot] = (h, id);
                break id;
            }
            slot = (slot + 1) & 1023;
        };
        if reg_id < 1_000_000 {
            region_map[reg_id] = c_id;
        }
    }
    Ok((region_map, country_names))
}

const DICT_SIZE: usize = 262144;
const DICT_MASK: usize = DICT_SIZE - 1;
const WORDS_PER_COUNTRY: usize = 1571;
const LOCAL_CACHE_SIZE: usize = 4096;
const LOCAL_CACHE_MASK: usize = LOCAL_CACHE_SIZE - 1;

fn main() -> std::io::Result<()> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/tmp/sd-full-c".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/tmp/sd-eval-c".to_string());
    let (regions_path, records_path, out_path) = (
        std::path::Path::new(&data_dir).join("regions.csv"),
        std::path::Path::new(&data_dir).join("records.csv"),
        std::path::Path::new(&out_dir).join("aggregates.txt"),
    );

    let (region_map, country_names) = load_regions(&regions_path)?;
    let num_countries = country_names.len();

    let file = File::open(&records_path)?;
    let flen = file.metadata()?.len() as usize;
    let ptr = unsafe { mmap(std::ptr::null_mut(), flen, 1, 1, file.as_raw_fd(), 0) };
    let slice = unsafe { std::slice::from_raw_parts(ptr, flen) };

    let mut header = 0;
    while header < flen && slice[header] != b'\n' {
        header += 1;
    }
    header += 1;

    let num_workers = 8;
    let approx = (flen - header) / num_workers;
    let mut bounds = vec![header];
    for i in 1..num_workers {
        let mut cur = header + i * approx;
        while cur < flen {
            if slice[cur] == b'\n' {
                let mut b = cur;
                if b > 0 && slice[b - 1] == b'\r' {
                    b -= 1;
                }
                if b >= 3
                    && slice[b - 3] == b'.'
                    && slice[b - 2].is_ascii_digit()
                    && slice[b - 1].is_ascii_digit()
                {
                    bounds.push(cur + 1);
                    break;
                }
            }
            cur += 1;
        }
    }
    bounds.push(flen);

    let region_map_ref: &'static [u8] = Box::leak(region_map.into_boxed_slice());
    let reg_usize = region_map_ref.as_ptr() as usize;

    let dict_hashes_ref: &'static [AtomicU64] = Box::leak(
        (0..DICT_SIZE)
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let dict_ids_ref: &'static [AtomicU32] = Box::leak(
        (0..DICT_SIZE)
            .map(|_| AtomicU32::new(u32::MAX))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let next_name_id: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(0)));

    let total_words = num_countries * WORDS_PER_COUNTRY;
    let bitsets_ref: &'static [AtomicU64] = Box::leak(
        (0..total_words)
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );

    let mut worker_aggs =
        vec![
            vec![(0u64, 0i64, 0i64, i64::MAX, i64::MIN, 0usize, 0usize); num_countries];
            num_workers
        ];

    thread::scope(|s| {
        for (w, aggs_ref) in worker_aggs.iter_mut().enumerate() {
            let (start, end) = (bounds[w], bounds[w + 1]);
            thread::Builder::new()
                .stack_size(131072)
                .spawn_scoped(s, move || {
                    let reg_ptr = reg_usize as *const u8;
                    let raw_ptr = slice.as_ptr();
                    let (mut local_cache, mut unescape_buf, mut p) =
                        ([(0u64, 0u32); LOCAL_CACHE_SIZE], [0u8; 128], start);

                    while p < end {
                        if p + 3 <= end
                            && unsafe {
                                *raw_ptr.add(p) == 0xef
                                    && *raw_ptr.add(p + 1) == 0xbb
                                    && *raw_ptr.add(p + 2) == 0xbf
                            }
                        {
                            p += 3;
                        }
                        if p >= end {
                            break;
                        }

                        let (rec_id, next_p) = unsafe { parse_u64_comma(raw_ptr, p) };
                        p = next_p;

                        let (name_len, name_hash) = if p < end && unsafe { *raw_ptr.add(p) == b'"' }
                        {
                            p += 1;
                            let start_n = p;
                            p = unsafe { find_char_avx2(raw_ptr, p, end, b'"') };
                            if p + 1 < end && unsafe { *raw_ptr.add(p + 1) != b'"' } {
                                let (h, l) = (
                                    fast_hash(unsafe {
                                        std::slice::from_raw_parts(
                                            raw_ptr.add(start_n),
                                            p - start_n,
                                        )
                                    }),
                                    p - start_n,
                                );
                                p += 2;
                                (l, h)
                            } else {
                                let mut d = p - start_n;
                                unescape_buf[..d].copy_from_slice(unsafe {
                                    std::slice::from_raw_parts(raw_ptr.add(start_n), d)
                                });
                                loop {
                                    if p >= end || unsafe { *raw_ptr.add(p) != b'"' } {
                                        break;
                                    }
                                    if p + 1 < end && unsafe { *raw_ptr.add(p + 1) == b'"' } {
                                        unescape_buf[d] = b'"';
                                        d += 1;
                                        p += 2;
                                        let mid = p;
                                        p = unsafe { find_char_avx2(raw_ptr, p, end, b'"') };
                                        let part_len = p - mid;
                                        unescape_buf[d..d + part_len].copy_from_slice(unsafe {
                                            std::slice::from_raw_parts(raw_ptr.add(mid), part_len)
                                        });
                                        d += part_len;
                                    } else {
                                        p += 2;
                                        break;
                                    }
                                }
                                (d, fast_hash(&unescape_buf[..d]))
                            }
                        } else {
                            let start_n = p;
                            p = unsafe { find_char_avx2(raw_ptr, p, end, b',') };
                            let (h, l) = (
                                fast_hash(unsafe {
                                    std::slice::from_raw_parts(raw_ptr.add(start_n), p - start_n)
                                }),
                                p - start_n,
                            );
                            p += 1;
                            (l, h)
                        };

                        let note_len = if p < end && unsafe { *raw_ptr.add(p) == b'"' } {
                            p += 1;
                            let start_not = p;
                            p = unsafe { find_char_avx2(raw_ptr, p, end, b'"') };
                            if p + 1 < end && unsafe { *raw_ptr.add(p + 1) != b'"' } {
                                let l = p - start_not;
                                p += 2;
                                l
                            } else {
                                let mut d = p - start_not;
                                loop {
                                    if p >= end || unsafe { *raw_ptr.add(p) != b'"' } {
                                        break;
                                    }
                                    if p + 1 < end && unsafe { *raw_ptr.add(p + 1) == b'"' } {
                                        d += 1;
                                        p += 2;
                                        let mid = p;
                                        p = unsafe { find_char_avx2(raw_ptr, p, end, b'"') };
                                        d += p - mid;
                                    } else {
                                        p += 2;
                                        break;
                                    }
                                }
                                d
                            }
                        } else {
                            let start_not = p;
                            p = unsafe { find_char_avx2(raw_ptr, p, end, b',') };
                            let l = p - start_not;
                            p += 1;
                            l
                        };

                        let (reg_id, next_p) = unsafe { parse_u64_comma(raw_ptr, p) };
                        p = next_p;

                        let (cents, next_p) = unsafe { parse_amount(raw_ptr, p) };
                        p = next_p;

                        let c_id = if reg_id < 1_000_000 {
                            unsafe { *reg_ptr.add(reg_id as usize) as usize }
                        } else {
                            0
                        };
                        let agg = &mut aggs_ref[c_id];
                        agg.0 += 1;
                        agg.1 += rec_id as i64;
                        agg.2 += cents;
                        agg.3 = agg.3.min(cents);
                        agg.4 = agg.4.max(cents);
                        agg.5 += name_len;
                        agg.6 += note_len;

                        let cache_slot = (name_hash as usize) & LOCAL_CACHE_MASK;
                        let (cached_h, cached_id) = local_cache[cache_slot];
                        let name_id = if cached_h == name_hash {
                            cached_id
                        } else {
                            let mut slot_idx = (name_hash as usize) & DICT_MASK;
                            let id = loop {
                                let curr_h = dict_hashes_ref[slot_idx].load(Ordering::Relaxed);
                                if curr_h == name_hash {
                                    while dict_ids_ref[slot_idx].load(Ordering::Relaxed) == u32::MAX
                                    {
                                        std::hint::spin_loop();
                                    }
                                    break dict_ids_ref[slot_idx].load(Ordering::Relaxed);
                                }
                                if curr_h == 0 {
                                    if dict_hashes_ref[slot_idx]
                                        .compare_exchange_weak(
                                            0,
                                            name_hash,
                                            Ordering::AcqRel,
                                            Ordering::Relaxed,
                                        )
                                        .is_ok()
                                    {
                                        let id = next_name_id.fetch_add(1, Ordering::Relaxed);
                                        dict_ids_ref[slot_idx].store(id, Ordering::Release);
                                        break id;
                                    }
                                    continue;
                                }
                                slot_idx = (slot_idx + 1) & DICT_MASK;
                            };
                            local_cache[cache_slot] = (name_hash, id);
                            id
                        };

                        let word_idx = c_id * WORDS_PER_COUNTRY + (name_id as usize / 64);
                        bitsets_ref[word_idx].fetch_or(1u64 << (name_id & 63), Ordering::Relaxed);
                    }
                })
                .unwrap();
        }
    });

    let mut all_aggs = vec![(0u64, 0i64, 0i64, i64::MAX, i64::MIN, 0usize, 0usize); num_countries];
    for w in &worker_aggs {
        for (i, a) in w.iter().enumerate() {
            let g = &mut all_aggs[i];
            g.0 += a.0;
            g.1 += a.1;
            g.2 += a.2;
            g.3 = g.3.min(a.3);
            g.4 = g.4.max(a.4);
            g.5 += a.5;
            g.6 += a.6;
        }
    }

    let distinct_counts: Vec<usize> = (0..num_countries)
        .map(|c| {
            (0..WORDS_PER_COUNTRY)
                .map(|w| {
                    bitsets_ref[c * WORDS_PER_COUNTRY + w]
                        .load(Ordering::Relaxed)
                        .count_ones() as usize
                })
                .sum()
        })
        .collect();

    let mut sorted_indices: Vec<usize> = (0..num_countries).collect();
    sorted_indices.sort_unstable_by(|&a, &b| country_names[a].cmp(&country_names[b]));

    fs::create_dir_all(&out_dir)?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, File::create(&out_path)?);
    for &c_id in &sorted_indices {
        let agg = &all_aggs[c_id];
        if agg.0 == 0 {
            continue;
        }
        writeln!(
            writer,
            "{} {} {} {} {} {} {} {} {}",
            country_names[c_id],
            agg.0,
            agg.1,
            format_cents(agg.2),
            format_cents(agg.3),
            format_cents(agg.4),
            distinct_counts[c_id],
            agg.5,
            agg.6
        )?;
    }
    writer.flush()?;
    unsafe {
        munmap(ptr, flen);
    }
    Ok(())
}
