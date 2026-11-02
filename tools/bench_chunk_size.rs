use std::fs::File;
use std::os::unix::fs::FileExt;
use std::thread;
use std::time::Instant;

fn main() {
    let file = File::open("/tmp/sd-full-c/records.csv").unwrap();
    let flen = file.metadata().unwrap().len() as usize;

    for &chunk_kb in &[128, 256, 512, 1024, 2048, 4096] {
        let chunk_size = chunk_kb * 1024;
        let t0 = Instant::now();
        const NUM_WORKERS: usize = 12;
        let approx_chunk = flen / NUM_WORKERS;

        thread::scope(|s| {
            for w in 0..NUM_WORKERS {
                let start = w * approx_chunk;
                let end = ((w + 1) * approx_chunk).min(flen);
                let f = &file;
                s.spawn(move || {
                    let mut buf = vec![0u8; chunk_size];
                    let mut cur = start;
                    let mut bytes = 0;
                    while cur < end {
                        let to_read = (chunk_size).min(end - cur);
                        f.read_exact_at(&mut buf[..to_read], cur as u64).unwrap();
                        bytes += buf[0] as usize;
                        cur += to_read;
                    }
                    bytes
                });
            }
        });
        let dur = t0.elapsed();
        println!("Chunk size {:4} KB read entire 201 MB file in: {:?}", chunk_kb, dur);
    }
}
