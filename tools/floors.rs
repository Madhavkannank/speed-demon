use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::thread;
use std::time::Instant;

fn main() {
    println!("=== SPEED DEMON 2026 — HARDWARE & RUNTIME FLOORS ===");

    // 1. Thread scope spawn overhead
    for &num_threads in &[6, 8, 10, 12] {
        let t0 = Instant::now();
        thread::scope(|s| {
            for _ in 0..num_threads {
                s.spawn(|| {
                    std::hint::black_box(1 + 1);
                });
            }
        });
        println!("thread::scope spawn {} threads: {:?}", num_threads, t0.elapsed());
    }

    // 2. Write 10 MB to /tmp and check time
    let out_path = "/tmp/floor_10mb.tmp";
    let data = vec![b'A'; 10 * 1024 * 1024];
    let t0 = Instant::now();
    let mut f = File::create(out_path).unwrap();
    f.write_all(&data).unwrap();
    f.sync_all().unwrap();
    println!("Sequential write 10 MB + sync: {:?}", t0.elapsed());
    let _ = std::fs::remove_file(out_path);

    // Parallel pwrite 10 MB (12 slices)
    let f = File::create(out_path).unwrap();
    f.set_len(10 * 1024 * 1024).unwrap();
    let slice_sz = (10 * 1024 * 1024) / 12;
    let t0 = Instant::now();
    thread::scope(|s| {
        for i in 0..12 {
            let f_ref = &f;
            let slice = &data[i * slice_sz..(i + 1) * slice_sz];
            let off = (i * slice_sz) as u64;
            s.spawn(move || {
                f_ref.write_all_at(slice, off).unwrap();
            });
        }
    });
    f.sync_all().unwrap();
    println!("Parallel pwrite 10 MB (12 threads) + sync: {:?}", t0.elapsed());
    let _ = std::fs::remove_file(out_path);

    // 3. Parallel memory scan bandwidth (240 MB)
    let mem_240mb = vec![1u64; 30 * 1024 * 1024]; // 240 MB
    let slice_len = mem_240mb.len() / 12;
    let t0 = Instant::now();
    let mut sums = [0u64; 12];
    thread::scope(|s| {
        for (i, sum_out) in sums.iter_mut().enumerate() {
            let slice = &mem_240mb[i * slice_len..(i + 1) * slice_len];
            s.spawn(move || {
                let mut acc = 0u64;
                for &v in slice {
                    acc = acc.wrapping_add(v);
                }
                *sum_out = acc;
            });
        }
    });
    let dur = t0.elapsed();
    let total_sum: u64 = sums.iter().sum();
    let bw_gb_s = (240.0 / 1024.0) / dur.as_secs_f64();
    println!("Parallel memory scan 240 MB (12 threads, sum={}): {:?} ({:.2} GB/s)", total_sum, dur, bw_gb_s);

    // 4. pread from /tmp/sd-full-b/docs.txt (243 MB) vs memory
    if let Ok(docs_file) = File::open("/tmp/sd-full-b/docs.txt") {
        let meta = docs_file.metadata().unwrap();
        let flen = meta.len() as usize;
        let chunk_sz = flen / 12;
        let t0 = Instant::now();
        thread::scope(|s| {
            for i in 0..12 {
                let f_ref = &docs_file;
                let off = (i * chunk_sz) as u64;
                let to_read = if i == 11 { flen - i * chunk_sz } else { chunk_sz };
                s.spawn(move || {
                    let mut buf = vec![0u8; 1024 * 1024]; // 1MB buffer
                    let mut cur = 0;
                    while cur < to_read {
                        let step = buf.len().min(to_read - cur);
                        f_ref.read_exact_at(&mut buf[..step], off + cur as u64).unwrap();
                        cur += step;
                    }
                });
            }
        });
        let dur = t0.elapsed();
        let read_bw = (flen as f64 / 1024.0 / 1024.0 / 1024.0) / dur.as_secs_f64();
        println!("Parallel pread 243 MB (12 threads, 1MB buffer): {:?} ({:.2} GB/s)", dur, read_bw);

        // 5. Mmap 243 MB: plain faults vs MAP_POPULATE vs MADV_POPULATE_READ
        use std::os::fd::AsRawFd;
        let fd = docs_file.as_raw_fd();
        const PROT_READ: i32 = 1;
        const MAP_SHARED: i32 = 1;
        const MAP_POPULATE: i32 = 0x08000;
        const MADV_POPULATE_READ: i32 = 22;

        extern "C" {
            fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
            fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
            fn madvise(addr: *mut std::ffi::c_void, length: usize, advice: i32) -> i32;
        }

        // Test plain mmap + touch with 1, 6, 12 threads
        for &t_cnt in &[1, 6, 12] {
            unsafe {
                let ptr = mmap(std::ptr::null_mut(), flen, PROT_READ, MAP_SHARED, fd, 0);
                assert!(!ptr.is_null() && ptr != usize::MAX as *mut std::ffi::c_void);
                let slice = std::slice::from_raw_parts(ptr as *const u8, flen);
                let t0 = Instant::now();
                let chunk = flen / t_cnt;
                thread::scope(|s| {
                    for i in 0..t_cnt {
                        let sub = &slice[i * chunk..if i == t_cnt - 1 { flen } else { (i + 1) * chunk }];
                        s.spawn(move || {
                            let mut acc = 0u64;
                            for p in (0..sub.len()).step_by(4096) {
                                acc = acc.wrapping_add(sub[p] as u64);
                            }
                            std::hint::black_box(acc);
                        });
                    }
                });
                let dur = t0.elapsed();
                munmap(ptr, flen);
                println!("mmap plain fault touch ({} threads): {:?}", t_cnt, dur);
            }
        }

        // Test MAP_POPULATE
        unsafe {
            let t0 = Instant::now();
            let ptr = mmap(std::ptr::null_mut(), flen, PROT_READ, MAP_SHARED | MAP_POPULATE, fd, 0);
            assert!(!ptr.is_null() && ptr != usize::MAX as *mut std::ffi::c_void);
            let dur = t0.elapsed();
            munmap(ptr, flen);
            println!("mmap MAP_POPULATE: {:?}", dur);
        }

        // Test parallel MADV_POPULATE_READ (12 threads)
        unsafe {
            let ptr = mmap(std::ptr::null_mut(), flen, PROT_READ, MAP_SHARED, fd, 0);
            assert!(!ptr.is_null() && ptr != usize::MAX as *mut std::ffi::c_void);
            let t0 = Instant::now();
            let chunk = flen / 12;
            thread::scope(|s| {
                for i in 0..12 {
                    let sub_addr = (ptr as usize) + i * chunk;
                    let sub_len = if i == 11 { flen - i * chunk } else { chunk };
                    s.spawn(move || {
                        madvise(sub_addr as *mut std::ffi::c_void, sub_len, MADV_POPULATE_READ);
                    });
                }
            });
            let dur = t0.elapsed();
            munmap(ptr, flen);
            println!("mmap parallel MADV_POPULATE_READ (12 threads): {:?}", dur);
        }
    }
}
