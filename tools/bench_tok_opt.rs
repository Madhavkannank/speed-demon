use std::fs::File;
use std::os::fd::AsRawFd;
use std::thread;
use std::time::Instant;

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
    fn madvise(addr: *mut std::ffi::c_void, length: usize, advice: i32) -> i32;
}

fn main() {
    let docs_path = "/tmp/sd-full-b/docs.txt";
    let docs_file = File::open(docs_path).unwrap();
    let flen = docs_file.metadata().unwrap().len() as usize;
    let mmap_ptr = unsafe { mmap(std::ptr::null_mut(), flen, 1, 2, docs_file.as_raw_fd(), 0) };
    assert!(!mmap_ptr.is_null());
    unsafe { madvise(mmap_ptr, flen, 22); }
    let mmap_slice = unsafe { std::slice::from_raw_parts(mmap_ptr as *const u8, flen) };

    let num_threads = thread::available_parallelism().map(|p| p.get()).unwrap_or(12).max(1);
    let approx_chunk = flen / num_threads;
    let mut bounds = Vec::with_capacity(num_threads + 1);
    bounds.push(0);
    for i in 1..num_threads {
        let mut p = (i * approx_chunk).min(flen);
        while p < flen && mmap_slice[p] != b'\n' { p += 1; }
        if p < flen { p += 1; }
        bounds.push(p);
    }
    bounds.push(flen);

    // Test 1: Baseline to_ascii_lowercase()
    let t0 = Instant::now();
    let mut tok_count1 = 0u64;
    thread::scope(|s| {
        let mut handles = Vec::new();
        for w in 0..num_threads {
            let (start, end) = (bounds[w], bounds[w + 1]);
            let slice = &mmap_slice[start..end];
            handles.push(s.spawn(move || {
                let mut p = 0;
                let len = slice.len();
                let mut c = 0u64;
                while p < len {
                    while p < len && slice[p] != b'\t' && slice[p] != b'\n' { p += 1; }
                    if p < len && slice[p] == b'\t' { p += 1; }
                    while p < len && slice[p] != b'\n' {
                        if slice[p].is_ascii_alphanumeric() {
                            let start_t = p;
                            while p < len && slice[p].is_ascii_alphanumeric() { p += 1; }
                            let count = (p - start_t).min(32);
                            let mut h = 0x517cc1b727220a95u64;
                            for k in 0..count {
                                h = (h.rotate_left(5) ^ (slice[start_t + k].to_ascii_lowercase() as u64)).wrapping_mul(0x9e3779b97f4a7c15);
                            }
                            std::hint::black_box(h);
                            c += 1;
                        } else {
                            p += 1;
                        }
                    }
                    if p < len && slice[p] == b'\n' { p += 1; }
                }
                c
            }));
        }
        for h in handles { tok_count1 += h.join().unwrap(); }
    });
    let d1 = t0.elapsed();

    // Test 2: (b | 0x20) branchless
    let t1 = Instant::now();
    let mut tok_count2 = 0u64;
    thread::scope(|s| {
        let mut handles = Vec::new();
        for w in 0..num_threads {
            let (start, end) = (bounds[w], bounds[w + 1]);
            let slice = &mmap_slice[start..end];
            handles.push(s.spawn(move || {
                let mut p = 0;
                let len = slice.len();
                let mut c = 0u64;
                while p < len {
                    while p < len && slice[p] != b'\t' && slice[p] != b'\n' { p += 1; }
                    if p < len && slice[p] == b'\t' { p += 1; }
                    while p < len && slice[p] != b'\n' {
                        if slice[p].is_ascii_alphanumeric() {
                            let start_t = p;
                            while p < len && slice[p].is_ascii_alphanumeric() { p += 1; }
                            let count = (p - start_t).min(32);
                            let mut h = 0x517cc1b727220a95u64;
                            for k in 0..count {
                                h = (h.rotate_left(5) ^ ((slice[start_t + k] | 0x20) as u64)).wrapping_mul(0x9e3779b97f4a7c15);
                            }
                            std::hint::black_box(h);
                            c += 1;
                        } else {
                            p += 1;
                        }
                    }
                    if p < len && slice[p] == b'\n' { p += 1; }
                }
                c
            }));
        }
        for h in handles { tok_count2 += h.join().unwrap(); }
    });
    let d2 = t1.elapsed();

    println!("Baseline to_ascii_lowercase(): {:.2} ms (tokens: {})", d1.as_secs_f64() * 1000.0, tok_count1);
    println!("Branchless (b | 0x20)       : {:.2} ms (tokens: {})", d2.as_secs_f64() * 1000.0, tok_count2);

    unsafe { munmap(mmap_ptr, flen); }
}
