use std::fs::File;
use std::os::fd::AsRawFd;
use std::collections::HashMap;

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
}

const NORM_TABLE: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = b'0'; while i <= b'9' { t[i as usize] = i; i += 1; }
    let mut i = b'a'; while i <= b'z' { t[i as usize] = i; i += 1; }
    let mut i = b'A'; while i <= b'Z' { t[i as usize] = i + 32; i += 1; }
    t
};

fn main() {
    let q_bytes = std::fs::read("/tmp/sd-full-b/queries.txt").unwrap();
    let mut query_terms = HashMap::new();
    let mut term_names = Vec::new();

    let mut start = 0;
    while start < q_bytes.len() {
        let mut end = start;
        while end < q_bytes.len() && q_bytes[end] != b'\n' { end += 1; }
        let line = &q_bytes[start..end];
        start = end + 1;
        if line.is_empty() { continue; }

        for w in line.split(|&b| b == b' ' || b == b'"') {
            if w.is_empty() || w == b"BOOL" || w == b"PHRASE" || w == b"AND" || w == b"OR" || w == b"NOT" {
                continue;
            }
            if !query_terms.contains_key(w) {
                let tid = term_names.len();
                query_terms.insert(w.to_vec(), tid);
                term_names.push(w.to_vec());
            }
        }
    }
    println!("Total distinct query terms: {}", term_names.len());

    let f = File::open("/tmp/sd-full-b/docs.txt").unwrap();
    let flen = f.metadata().unwrap().len() as usize;
    let ptr = unsafe { mmap(std::ptr::null_mut(), flen, 1, 1, f.as_raw_fd(), 0) as *const u8 };
    let slice = unsafe { std::slice::from_raw_parts(ptr, flen) };

    let mut df = vec![0u32; term_names.len()];
    let mut last_doc = vec![u32::MAX; term_names.len()];

    let mut p = 0;
    let mut doc_id = 0u32;
    let mut norm_buf = [0u8; 32];

    while p < flen {
        while p < flen && slice[p] != b'\t' && slice[p] != b'\n' { p += 1; }
        if p < flen && slice[p] == b'\t' { p += 1; }

        while p < flen && slice[p] != b'\n' {
            while p < flen && slice[p] != b'\n' && NORM_TABLE[slice[p] as usize] == 0 { p += 1; }
            if p >= flen || slice[p] == b'\n' { break; }
            let mut cur_len = 0;
            while p < flen && slice[p] != b'\n' {
                let c = NORM_TABLE[slice[p] as usize];
                if c == 0 { break; }
                if cur_len < 32 { norm_buf[cur_len] = c; cur_len += 1; }
                p += 1;
            }
            if let Some(&tid) = query_terms.get(&norm_buf[..cur_len]) {
                if last_doc[tid] != doc_id {
                    last_doc[tid] = doc_id;
                    df[tid] += 1;
                }
            }
        }
        if p < flen && slice[p] == b'\n' { p += 1; }
        doc_id += 1;
    }

    unsafe { munmap(ptr as *mut std::ffi::c_void, flen); }

    let total_postings: u64 = df.iter().map(|&d| d as u64).sum();
    println!("Total corpus postings for query terms: {}", total_postings);

    let mut sorted_df = df.clone();
    sorted_df.sort_unstable_by(|a, b| b.cmp(a));

    println!("Top 10 DFs: {:?}", &sorted_df[..10]);
    println!("DF at rank 50: {}", sorted_df[50]);
    println!("DF at rank 100: {}", sorted_df[100]);
    println!("DF at rank 200: {}", sorted_df[200]);
    println!("DF at rank 500: {}", sorted_df[500]);
    println!("DF at rank 1000: {}", sorted_df[1000]);
    println!("DF at rank 2000: {}", sorted_df[2000]);

    for &threshold in &[25000, 10000, 5000, 2048, 1000, 250] {
        let count = df.iter().filter(|&&d| d >= threshold).count();
        let post_sum: u64 = df.iter().filter(|&&d| d >= threshold).map(|&d| d as u64).sum();
        let bitset_mb = (count as f64 * 31250.0) / 1024.0 / 1024.0;
        let u32_mb = (post_sum as f64 * 4.0) / 1024.0 / 1024.0;
        println!("Threshold >= {:5}: {:5} terms | {:8} postings ({:5.1}%) | u32: {:5.2} MB -> bitset: {:5.2} MB",
            threshold, count, post_sum, (post_sum as f64 / total_postings as f64) * 100.0, u32_mb, bitset_mb);
    }
}
