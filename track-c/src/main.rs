use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread;

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

fn format_cents(cents: i64) -> String {
    let (s, c) = (if cents < 0 { "-" } else { "" }, cents.abs());
    format!("{}{}.{:02}", s, c / 100, c % 100)
}

fn load_regions(path: &Path) -> std::io::Result<(Vec<u8>, Vec<String>)> {
    let bytes = fs::read(path)?;
    let mut region_map = vec![0u8; 1_000_000];
    let mut country_names = vec!["_UNMATCHED".to_string()];
    let mut country_table = vec![(0u32, 0u8); 1024];
    let len = bytes.len();
    let mut p = 0;
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
        let mut reg_id = 0usize;
        while p < len && bytes[p] >= b'0' && bytes[p] <= b'9' {
            reg_id = reg_id * 10 + (bytes[p] - b'0') as usize;
            p += 1;
        }
        if reg_id >= region_map.len() {
            region_map.resize(reg_id + 1, 0);
        }
        if p < len && bytes[p] == b',' {
            p += 1;
        }
        let c_start = p;
        let mut h = 0x517cc1b7u32;
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

const NUM_WORKERS: usize = 12;
const CHUNK_SIZE: usize = 256 * 1024;
const DICT_SIZE: usize = 262144;
const DICT_MASK: usize = DICT_SIZE - 1;
const MAX_NAMES: usize = 131072;
const WORDS_PER_COUNTRY: usize = MAX_NAMES / 64; // 2048 words = 16KB per country
const LOCAL_CACHE_SIZE: usize = 8192;
const LOCAL_CACHE_MASK: usize = LOCAL_CACHE_SIZE - 1;

struct NameDictSlot {
    hash: AtomicU64,
    id: AtomicU32,
}

fn main() -> std::io::Result<()> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string());
    let (regions_path, records_path, out_path) = (
        Path::new(&data_dir).join("regions.csv"),
        Path::new(&data_dir).join("records.csv"),
        Path::new(&out_dir).join("aggregates.txt"),
    );

    let (region_map, country_names) = load_regions(&regions_path)?;
    let num_countries = country_names.len();
    let file = File::open(&records_path)?;
    let file_len = file.metadata()?.len() as usize;
    if file_len == 0 {
        File::create(&out_path)?;
        return Ok(());
    }

    let mut header_buf = [0u8; 128];
    file.read_exact_at(&mut header_buf, 0)?;
    let header_offset = {
        let mut idx = if header_buf.starts_with(b"\xef\xbb\xbf") {
            3
        } else {
            0
        };
        while idx < header_buf.len() && header_buf[idx] != b'\n' {
            idx += 1;
        }
        idx + 1
    };

    let approx_chunk = (file_len - header_offset) / NUM_WORKERS;
    let mut chunk_bounds = Vec::with_capacity(NUM_WORKERS + 1);
    chunk_bounds.push(header_offset);
    let mut scan_buf = vec![0u8; 65536];
    for i in 1..NUM_WORKERS {
        let mut cur_search = header_offset + i * approx_chunk;
        let mut found_pos = None;
        while cur_search < file_len && found_pos.is_none() {
            let to_read = scan_buf.len().min(file_len - cur_search);
            if file
                .read_exact_at(&mut scan_buf[..to_read], cur_search as u64)
                .is_err()
            {
                break;
            }
            let mut p = 0;
            while p + 16 < to_read {
                if scan_buf[p] == b'.'
                    && scan_buf[p + 1].is_ascii_digit()
                    && scan_buf[p + 2].is_ascii_digit()
                {
                    let mut after = p + 3;
                    if after < to_read && scan_buf[after] == b'\r' {
                        after += 1;
                    }
                    if after < to_read && scan_buf[after] == b'\n' {
                        let mut rec_start = after + 1;
                        if rec_start + 3 <= to_read
                            && scan_buf[rec_start..].starts_with(b"\xef\xbb\xbf")
                        {
                            rec_start += 3;
                        }
                        if rec_start < to_read
                            && scan_buf[rec_start].is_ascii_digit()
                            && scan_buf[rec_start] != b'0'
                        {
                            let mut d = rec_start + 1;
                            while d < to_read && scan_buf[d].is_ascii_digit() {
                                d += 1;
                            }
                            if d < to_read && scan_buf[d] == b',' {
                                found_pos = Some(cur_search + rec_start);
                                break;
                            }
                        }
                    }
                }
                p += 1;
            }
            if found_pos.is_none() {
                cur_search += to_read.saturating_sub(64);
            }
        }
        chunk_bounds.push(found_pos.unwrap_or(file_len));
    }
    chunk_bounds.push(file_len);
    drop(scan_buf);

    let region_map_ref: &'static [u8] = Box::leak(region_map.into_boxed_slice());

    // Allocate Lock-Free Global Name Dictionary and Country Bitsets
    let mut dict_vec = Vec::with_capacity(DICT_SIZE);
    for _ in 0..DICT_SIZE {
        dict_vec.push(NameDictSlot {
            hash: AtomicU64::new(0),
            id: AtomicU32::new(u32::MAX),
        });
    }
    let dict_ref: &'static [NameDictSlot] = Box::leak(dict_vec.into_boxed_slice());
    let next_name_id: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(0)));

    let total_words = num_countries * WORDS_PER_COUNTRY;
    let mut bitsets_vec = Vec::with_capacity(total_words);
    for _ in 0..total_words {
        bitsets_vec.push(AtomicU64::new(0));
    }
    let bitsets_ref: &'static [AtomicU64] = Box::leak(bitsets_vec.into_boxed_slice());

    let mut worker_aggs: Vec<Vec<LocalAgg>> = (0..NUM_WORKERS).map(|_| Vec::new()).collect();

    thread::scope(|s| {
        let mut worker_aggs_ptrs = Vec::with_capacity(NUM_WORKERS);
        for i in 0..NUM_WORKERS {
            worker_aggs_ptrs.push(&mut worker_aggs[i] as *mut Vec<LocalAgg> as usize);
        }

        for w_idx in 0..NUM_WORKERS {
            let (start_pos, end_pos, path) = (
                chunk_bounds[w_idx],
                chunk_bounds[w_idx + 1],
                records_path.clone(),
            );
            let aggs_ptrs = worker_aggs_ptrs.clone();
            s.spawn(move || {
                let t_file = File::open(&path).unwrap();
                let mut aggs = vec![LocalAgg::new(); num_countries];
                let mut buf = vec![0u8; CHUNK_SIZE + 8192];
                let mut local_cache = [(0u64, 0u32); LOCAL_CACHE_SIZE];
                let (mut cur_offset, mut rem_len) = (start_pos, 0);

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
                    let valid_len = if cur_offset >= end_pos {
                        total_available
                    } else {
                        let (mut p, mut boundary) = (total_available.saturating_sub(1), 0);
                        while p >= 16 {
                            if buf[p] == b'\n' {
                                let mut before = p;
                                if before > 0 && buf[before - 1] == b'\r' {
                                    before -= 1;
                                }
                                if before >= 3
                                    && buf[before - 3] == b'.'
                                    && buf[before - 2].is_ascii_digit()
                                    && buf[before - 1].is_ascii_digit()
                                {
                                    boundary = p + 1;
                                    break;
                                }
                            }
                            p -= 1;
                        }
                        if boundary == 0 {
                            total_available
                        } else {
                            boundary
                        }
                    };
                    let (slice, mut p) = (&buf[..valid_len], 0);

                    while p < valid_len {
                        if slice[p] == 0xef
                            && p + 3 <= valid_len
                            && &slice[p..p + 3] == b"\xef\xbb\xbf"
                        {
                            p += 3;
                        }
                        if p >= valid_len {
                            break;
                        }
                        let mut rec_id = 0i64;
                        while p < valid_len && slice[p] != b',' {
                            rec_id = rec_id * 10 + (slice[p] - b'0') as i64;
                            p += 1;
                        }
                        p += 1;

                        let (mut name_len, mut name_hash) = (0usize, 0x517cc1b727220a95u64);
                        if p < valid_len && slice[p] == b'\"' {
                            p += 1;
                            while p < valid_len {
                                if slice[p] == b'\"' {
                                    if p + 1 < valid_len && slice[p + 1] == b'\"' {
                                        name_len += 1;
                                        name_hash = hash_byte(name_hash, b'\"');
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
                            let n_start = p;
                            while p < valid_len && slice[p] != b',' {
                                name_hash = hash_byte(name_hash, slice[p]);
                                p += 1;
                            }
                            name_len = p - n_start;
                        }
                        p += 1;

                        let mut note_len = 0usize;
                        if p < valid_len && slice[p] == b'\"' {
                            p += 1;
                            loop {
                                while p + 8 <= valid_len {
                                    let w = unsafe {
                                        (slice.as_ptr().add(p) as *const u64).read_unaligned()
                                    };
                                    let has_q = (w ^ 0x2222222222222222u64)
                                        .wrapping_sub(0x0101010101010101u64)
                                        & !(w ^ 0x2222222222222222u64)
                                        & 0x8080808080808080u64;
                                    if has_q != 0 {
                                        let off = (has_q.trailing_zeros() >> 3) as usize;
                                        note_len += off;
                                        p += off;
                                        break;
                                    }
                                    note_len += 8;
                                    p += 8;
                                }
                                if p >= valid_len {
                                    break;
                                }
                                if slice[p] == b'\"' {
                                    if p + 1 < valid_len && slice[p + 1] == b'\"' {
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
                            let not_start = p;
                            while p + 8 <= valid_len {
                                let w = unsafe {
                                    (slice.as_ptr().add(p) as *const u64).read_unaligned()
                                };
                                let has_comma = (w ^ 0x2c2c2c2c2c2c2c2cu64)
                                    .wrapping_sub(0x0101010101010101u64)
                                    & !(w ^ 0x2c2c2c2c2c2c2c2cu64)
                                    & 0x8080808080808080u64;
                                if has_comma != 0 {
                                    p += (has_comma.trailing_zeros() >> 3) as usize;
                                    break;
                                }
                                p += 8;
                            }
                            while p < valid_len && slice[p] != b',' {
                                p += 1;
                            }
                            note_len = p - not_start;
                        }
                        p += 1;

                        let mut reg_id = 0usize;
                        while p < valid_len && slice[p] != b',' {
                            reg_id = reg_id * 10 + (slice[p] - b'0') as usize;
                            p += 1;
                        }
                        p += 1;

                        let neg = p < valid_len && slice[p] == b'-';
                        if neg {
                            p += 1;
                        }
                        let mut dollars = 0i64;
                        while p < valid_len && slice[p] != b'.' {
                            dollars = dollars * 10 + (slice[p] - b'0') as i64;
                            p += 1;
                        }
                        p += 1;
                        let cents_val =
                            ((slice[p] - b'0') as i64) * 10 + ((slice[p + 1] - b'0') as i64);
                        let total_cents = dollars * 100 + cents_val;
                        let cents = if neg { -total_cents } else { total_cents };
                        p += 2;
                        if p < valid_len {
                            if slice[p] == b'\r' {
                                p += 2;
                            } else {
                                p += 1;
                            }
                        }

                        let c_id = if reg_id < region_map_ref.len() {
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

                        // Lock-Free Lookup in Name Dictionary with L1 Cache
                        let non_zero_hash = if name_hash == 0 { 1 } else { name_hash };
                        let cache_slot = (non_zero_hash as usize) & LOCAL_CACHE_MASK;
                        let (cached_h, cached_id) = local_cache[cache_slot];
                        let name_id = if cached_h == non_zero_hash {
                            cached_id
                        } else {
                            let mut slot_idx = (non_zero_hash as usize) & DICT_MASK;
                            let id = loop {
                                let slot = &dict_ref[slot_idx];
                                let curr_h = slot.hash.load(Ordering::Relaxed);
                                if curr_h == non_zero_hash {
                                    loop {
                                        let id = slot.id.load(Ordering::Relaxed);
                                        if id != u32::MAX {
                                            break;
                                        }
                                        std::hint::spin_loop();
                                    }
                                    break slot.id.load(Ordering::Relaxed);
                                }
                                if curr_h == 0 {
                                    if slot
                                        .hash
                                        .compare_exchange_weak(
                                            0,
                                            non_zero_hash,
                                            Ordering::AcqRel,
                                            Ordering::Relaxed,
                                        )
                                        .is_ok()
                                    {
                                        let id = next_name_id.fetch_add(1, Ordering::Relaxed);
                                        slot.id.store(id, Ordering::Release);
                                        break id;
                                    }
                                    continue;
                                }
                                slot_idx = (slot_idx + 1) & DICT_MASK;
                            };
                            local_cache[cache_slot] = (non_zero_hash, id);
                            id
                        };

                        // Set bit in country bitset
                        let word_idx = c_id * WORDS_PER_COUNTRY + (name_id as usize / 64);
                        let bit_mask = 1u64 << (name_id & 63);
                        bitsets_ref[word_idx].fetch_or(bit_mask, Ordering::Relaxed);
                    }
                    rem_len = total_available - valid_len;
                    if rem_len > 0 {
                        buf.copy_within(valid_len..total_available, 0);
                    }
                }
                unsafe {
                    *(aggs_ptrs[w_idx] as *mut Vec<LocalAgg>) = aggs;
                }
            });
        }
    });

    let mut all_aggs = vec![LocalAgg::new(); num_countries];
    for w in 0..NUM_WORKERS {
        for (c_id, a) in worker_aggs[w].iter().enumerate() {
            let agg = &mut all_aggs[c_id];
            agg.count += a.count;
            agg.id_sum += a.id_sum;
            agg.amount_sum += a.amount_sum;
            if a.amount_min < agg.amount_min {
                agg.amount_min = a.amount_min;
            }
            if a.amount_max > agg.amount_max {
                agg.amount_max = a.amount_max;
            }
            agg.name_bytes += a.name_bytes;
            agg.note_bytes += a.note_bytes;
        }
    }

    let mut distinct_counts = vec![0usize; num_countries];
    for c_id in 0..num_countries {
        let start_word = c_id * WORDS_PER_COUNTRY;
        let mut count = 0;
        for w in 0..WORDS_PER_COUNTRY {
            count += bitsets_ref[start_word + w]
                .load(Ordering::Relaxed)
                .count_ones() as usize;
        }
        distinct_counts[c_id] = count;
    }

    let mut sorted_indices: Vec<usize> = (0..num_countries).collect();
    sorted_indices.sort_unstable_by(|&a, &b| country_names[a].cmp(&country_names[b]));

    fs::create_dir_all(&out_dir)?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, File::create(&out_path)?);
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
