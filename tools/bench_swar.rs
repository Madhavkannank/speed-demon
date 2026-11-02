use std::fs::File;
use std::os::fd::AsRawFd;
use std::thread;
use std::time::Instant;

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
}

// Byte classification table: 1 for [a-zA-Z0-9], 0 otherwise
const IS_ALNUM: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = b'0'; while i <= b'9' { t[i as usize] = 1; i += 1; }
    let mut i = b'a'; while i <= b'z' { t[i as usize] = 1; i += 1; }
    let mut i = b'A'; while i <= b'Z' { t[i as usize] = 1; i += 1; }
    t
};

fn main() {
    let f = File::open("/tmp/sd-full-b/docs.txt").unwrap();
    let flen = f.metadata().unwrap().len() as usize;
    let ptr = unsafe { mmap(std::ptr::null_mut(), flen, 1, 1, f.as_raw_fd(), 0) as *const u8 };
    let slice = unsafe { std::slice::from_raw_parts(ptr, flen) };

    const T_CNT: usize = 12;
    let chunk = flen / T_CNT;
    let mut bounds = vec![0; T_CNT + 1];
    for i in 1..T_CNT {
        let mut p = i * chunk;
        while p < flen && slice[p] != b'\n' { p += 1; }
        bounds[i] = (p + 1).min(flen);
    }
    bounds[T_CNT] = flen;

    let t0 = Instant::now();
    let mut total_words = [0usize; T_CNT];

    thread::scope(|s| {
        for (w_idx, tw) in total_words.iter_mut().enumerate() {
            let (start, end) = (bounds[w_idx], bounds[w_idx + 1]);
            s.spawn(move || {
                let mut words = 0;
                let mut p = start;
                while p < end {
                    while p < end && slice[p] != b'\t' && slice[p] != b'\n' { p += 1; }
                    if p < end && slice[p] == b'\t' { p += 1; }

                    while p < end && slice[p] != b'\n' {
                        while p < end && slice[p] != b'\n' && IS_ALNUM[slice[p] as usize] == 0 {
                            p += 1;
                        }
                        if p >= end || slice[p] == b'\n' { break; }
                        let w_start = p;
                        while p < end && IS_ALNUM[slice[p] as usize] != 0 {
                            p += 1;
                        }
                        let w_len = p - w_start;
                        // fast hash
                        words += 1;
                    }
                    if p < end && slice[p] == b'\n' { p += 1; }
                }
                *tw = words;
            });
        }
    });

    let dur = t0.elapsed();
    let sum_words: usize = total_words.iter().sum();
    println!("IS_ALNUM scanned {} words in: {:?}", sum_words, dur);
    unsafe { munmap(ptr as *mut std::ffi::c_void, flen); }
}
