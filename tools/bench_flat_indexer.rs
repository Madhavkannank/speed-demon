use std::fs::File;
use std::os::unix::fs::FileExt;
use std::thread;
use std::time::Instant;

const NORM_TABLE: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = b'0'; while i <= b'9' { t[i as usize] = i; i += 1; }
    let mut i = b'a'; while i <= b'z' { t[i as usize] = i; i += 1; }
    let mut i = b'A'; while i <= b'Z' { t[i as usize] = i + 32; i += 1; }
    t
};

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct CompactEntry {
    hash: u64,
    tid: u32,
    q_offset: u32,
    len: u16,
    _pad: u16,
}

const TABLE_SIZE: usize = 262144;
const TABLE_MASK: usize = TABLE_SIZE - 1;
const BLOOM_WORDS: usize = 32768;
const NUM_WORKERS: usize = 12;

#[inline(always)]
fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = 0x517cc1b727220a95u64;
    for &x in b { h = (h.rotate_left(5) ^ (x as u64)).wrapping_mul(0x9e3779b97f4a7c15); }
    h
}

fn main() {
    let docs_path = "/tmp/sd-full-b/docs.txt";
    let queries_path = "/tmp/sd-full-b/queries.txt";
    let q_bytes = std::fs::read(queries_path).unwrap();

    let mut bloom = vec![0u64; BLOOM_WORDS];
    let mut flat_table = vec![CompactEntry::default(); TABLE_SIZE];
    let mut vocab = std::collections::HashMap::new();

    let mut p = 0;
    while p < q_bytes.len() {
        let mut line_end = p;
        while line_end < q_bytes.len() && q_bytes[line_end] != b'\n' { line_end += 1; }
        let line = &q_bytes[p..line_end];
        let line_off = p;
        p = line_end + 1;
        if line.is_empty() { continue; }

        for w in line.split(|&b| b == b' ' || b == b'"') {
            if w.is_empty() || w == b"BOOL" || w == b"PHRASE" || w == b"AND" || w == b"OR" || w == b"NOT" {
                continue;
            }
            if !vocab.contains_key(w) {
                let id = vocab.len() as u32;
                vocab.insert(w.to_vec(), id);
                let h = hash_bytes(w);
                let (b1, b2, b3, b4) = (
                    (h & 0x1FFFFF) as usize,
                    ((h >> 21) & 0x1FFFFF) as usize,
                    (h.rotate_left(17) & 0x1FFFFF) as usize,
                    (h.rotate_left(37) & 0x1FFFFF) as usize,
                );
                bloom[b1 >> 6] |= 1u64 << (b1 & 63);
                bloom[b2 >> 6] |= 1u64 << (b2 & 63);
                bloom[b3 >> 6] |= 1u64 << (b3 & 63);
                bloom[b4 >> 6] |= 1u64 << (b4 & 63);
                let mut slot = (h as usize) & TABLE_MASK;
                while flat_table[slot].hash != 0 { slot = (slot + 1) & TABLE_MASK; }
                let w_off = (w.as_ptr() as usize) - (q_bytes.as_ptr() as usize);
                flat_table[slot] = CompactEntry {
                    hash: h,
                    tid: id,
                    q_offset: w_off as u32,
                    len: w.len() as u16,
                    _pad: 0,
                };
            }
        }
    }
    let vocab_size = vocab.len();
    println!("Vocab size: {}", vocab_size);

    let d_file = File::open(docs_path).unwrap();
    let file_len = d_file.metadata().unwrap().len() as usize;

    let chunk_approx = file_len / NUM_WORKERS;
    let mut chunk_bounds = vec![0usize; NUM_WORKERS + 1];
    let mut scan_buf = vec![0u8; 8192];
    for w in 1..NUM_WORKERS {
        let guess = w * chunk_approx;
        let to_read = (file_len - guess).min(8192);
        d_file.read_exact_at(&mut scan_buf[..to_read], guess as u64).unwrap();
        let mut offset = 0;
        while offset < to_read && scan_buf[offset] != b'\n' { offset += 1; }
        chunk_bounds[w] = (guess + offset + 1).min(file_len);
    }
    chunk_bounds[NUM_WORKERS] = file_len;

    // Benchmark Flat Indexing Approach
    let t0 = Instant::now();
    let mut worker_pairs: Vec<Vec<u64>> = (0..NUM_WORKERS).map(|_| Vec::new()).collect();
    let mut worker_counts: Vec<Vec<u32>> = (0..NUM_WORKERS).map(|_| Vec::new()).collect();

    thread::scope(|s| {
        let (bloom_ref, table_ref, q_bytes_ref) = (&bloom, &flat_table, &q_bytes);
        for (w_idx, (wp, wc)) in worker_pairs.iter_mut().zip(worker_counts.iter_mut()).enumerate() {
            let (start_pos, end_pos) = (chunk_bounds[w_idx], chunk_bounds[w_idx + 1]);
            s.spawn(move || {
                let d_file = File::open(docs_path).unwrap();
                let mut pairs = Vec::with_capacity(2_200_000);
                let mut counts = vec![0u32; vocab_size];
                let mut last_seen = vec![u32::MAX; vocab_size];
                let mut chunk_buf = vec![0u8; 512 * 1024];
                let (mut cur_offset, mut rem_len) = (start_pos, 0);

                while cur_offset < end_pos || rem_len > 0 {
                    let to_read = (512 * 1024 - rem_len).min(end_pos.saturating_sub(cur_offset));
                    let read_bytes = if to_read > 0 {
                        d_file.read_exact_at(&mut chunk_buf[rem_len..rem_len + to_read], cur_offset as u64).unwrap();
                        cur_offset += to_read;
                        to_read
                    } else { 0 };
                    let total_available = rem_len + read_bytes;
                    if total_available == 0 { break; }
                    let mut last_nl = total_available;
                    while last_nl > 0 && chunk_buf[last_nl - 1] != b'\n' { last_nl -= 1; }
                    let (slice, mut p) = (&chunk_buf[..last_nl], 0);

                    while p < last_nl {
                        let mut doc_id = 0u32;
                        while p < last_nl && slice[p] >= b'0' && slice[p] <= b'9' {
                            doc_id = doc_id * 10 + (slice[p] - b'0') as u32;
                            p += 1;
                        }
                        while p < last_nl && slice[p] != b'\t' && slice[p] != b'\n' { p += 1; }
                        let text_start = if p < last_nl && slice[p] == b'\t' { p + 1 } else { p };
                        p = text_start;

                        let (mut word_start, mut cur_len, mut h) = (0, 0, 0x517cc1b727220a95u64);
                        while p < last_nl && slice[p] != b'\n' {
                            let c = NORM_TABLE[slice[p] as usize];
                            if c != 0 {
                                if cur_len == 0 { word_start = p; }
                                cur_len += 1;
                                h = (h.rotate_left(5) ^ (c as u64)).wrapping_mul(0x9e3779b97f4a7c15);
                            } else {
                                if cur_len > 0 {
                                    let (b1, b2, b3, b4) = (
                                        (h & 0x1FFFFF) as usize,
                                        ((h >> 21) & 0x1FFFFF) as usize,
                                        (h.rotate_left(17) & 0x1FFFFF) as usize,
                                        (h.rotate_left(37) & 0x1FFFFF) as usize,
                                    );
                                    if ((bloom_ref[b1 >> 6] >> (b1 & 63)) & (bloom_ref[b2 >> 6] >> (b2 & 63)) & (bloom_ref[b3 >> 6] >> (b3 & 63)) & (bloom_ref[b4 >> 6] >> (b4 & 63))) & 1 != 0 {
                                        let mut slot = (h as usize) & TABLE_MASK;
                                        while table_ref[slot].hash != 0 {
                                            if table_ref[slot].hash == h && table_ref[slot].len as usize == cur_len {
                                                let qw = &q_bytes_ref[table_ref[slot].q_offset as usize..table_ref[slot].q_offset as usize + cur_len];
                                                let mut ok = true;
                                                for k in 0..cur_len {
                                                    if NORM_TABLE[slice[word_start + k] as usize] != qw[k] { ok = false; break; }
                                                }
                                                if ok {
                                                    let tid = table_ref[slot].tid as usize;
                                                    if last_seen[tid] != doc_id {
                                                        pairs.push(((tid as u64) << 32) | (doc_id as u64));
                                                        counts[tid] += 1;
                                                        last_seen[tid] = doc_id;
                                                    }
                                                    break;
                                                }
                                            }
                                            slot = (slot + 1) & TABLE_MASK;
                                        }
                                    }
                                    cur_len = 0;
                                    h = 0x517cc1b727220a95u64;
                                }
                            }
                            p += 1;
                        }
                        if cur_len > 0 {
                            let (b1, b2, b3, b4) = (
                                (h & 0x1FFFFF) as usize,
                                ((h >> 21) & 0x1FFFFF) as usize,
                                (h.rotate_left(17) & 0x1FFFFF) as usize,
                                (h.rotate_left(37) & 0x1FFFFF) as usize,
                            );
                            if ((bloom_ref[b1 >> 6] >> (b1 & 63)) & (bloom_ref[b2 >> 6] >> (b2 & 63)) & (bloom_ref[b3 >> 6] >> (b3 & 63)) & (bloom_ref[b4 >> 6] >> (b4 & 63))) & 1 != 0 {
                                let mut slot = (h as usize) & TABLE_MASK;
                                while table_ref[slot].hash != 0 {
                                    if table_ref[slot].hash == h && table_ref[slot].len as usize == cur_len {
                                        let qw = &q_bytes_ref[table_ref[slot].q_offset as usize..table_ref[slot].q_offset as usize + cur_len];
                                        let mut ok = true;
                                        for k in 0..cur_len {
                                            if NORM_TABLE[slice[word_start + k] as usize] != qw[k] { ok = false; break; }
                                        }
                                        if ok {
                                            let tid = table_ref[slot].tid as usize;
                                            if last_seen[tid] != doc_id {
                                                pairs.push(((tid as u64) << 32) | (doc_id as u64));
                                                counts[tid] += 1;
                                                last_seen[tid] = doc_id;
                                            }
                                            break;
                                        }
                                    }
                                    slot = (slot + 1) & TABLE_MASK;
                                }
                            }
                        }
                        if p < last_nl && slice[p] == b'\n' { p += 1; }
                    }
                    rem_len = total_available - last_nl;
                    if rem_len > 0 { chunk_buf.copy_within(last_nl..total_available, 0); }
                }
                *wp = pairs;
                *wc = counts;
            });
        }
    });

    let t_scan = t0.elapsed();
    println!("Scan phase elapsed: {:?}", t_scan);

    // Compute prefix sums
    let t_merge_start = Instant::now();
    let mut term_offsets = vec![0usize; vocab_size + 1];
    for t in 0..vocab_size {
        let mut total = 0usize;
        for w in 0..NUM_WORKERS {
            total += worker_counts[w][t] as usize;
        }
        term_offsets[t + 1] = term_offsets[t] + total;
    }
    let total_postings = term_offsets[vocab_size];
    println!("Total postings: {}", total_postings);

    let mut worker_base: Vec<Vec<usize>> = vec![vec![0usize; vocab_size]; NUM_WORKERS];
    for t in 0..vocab_size {
        let mut base = term_offsets[t];
        for w in 0..NUM_WORKERS {
            worker_base[w][t] = base;
            base += worker_counts[w][t] as usize;
        }
    }

    let mut all_postings = vec![0u32; total_postings];
    let all_ptr = all_postings.as_mut_ptr() as usize;

    thread::scope(|s| {
        for w in 0..NUM_WORKERS {
            let pairs = &worker_pairs[w];
            let mut bases = std::mem::take(&mut worker_base[w]);
            s.spawn(move || {
                let ptr = all_ptr as *mut u32;
                for &pair in pairs {
                    let tid = (pair >> 32) as usize;
                    let doc_id = pair as u32;
                    let dst = bases[tid];
                    unsafe { *ptr.add(dst) = doc_id; }
                    bases[tid] += 1;
                }
            });
        }
    });

    let t_merge = t_merge_start.elapsed();
    println!("Merge phase elapsed: {:?}", t_merge);
    println!("Total index build: {:?}", t0.elapsed());

    // Verify sortedness
    let mut sorted_ok = true;
    for t in 0..vocab_size {
        let slice = &all_postings[term_offsets[t]..term_offsets[t + 1]];
        for i in 1..slice.len() {
            if slice[i] <= slice[i - 1] {
                println!("FAIL: term {} not sorted: {} <= {}", t, slice[i], slice[i - 1]);
                sorted_ok = false;
                break;
            }
        }
        if !sorted_ok { break; }
    }
    println!("All postings sorted strictly ascending: {}", sorted_ok);
}
