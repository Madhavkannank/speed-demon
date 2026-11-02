use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::thread;
use std::time::Instant;

fn main() {
    let path = "/tmp/sd-full-a/requests.log";
    let file = File::open(path).unwrap();
    let file_len = file.metadata().unwrap().len() as usize;

    for num_workers in [4, 6, 8, 10, 12] {
        let approx_chunk = file_len / num_workers;
        let mut chunk_bounds = Vec::with_capacity(num_workers + 1);
        chunk_bounds.push(0);
        let mut scan_buf = vec![0u8; 1024];
        for i in 1..num_workers {
            let approx_pos = i * approx_chunk;
            file.read_exact_at(&mut scan_buf, approx_pos as u64).unwrap();
            let mut offset = 0;
            while offset < scan_buf.len() && scan_buf[offset] != b'\n' { offset += 1; }
            chunk_bounds.push(approx_pos + offset + 1);
        }
        chunk_bounds.push(file_len);

        let t0 = Instant::now();
        thread::scope(|s| {
            for i in 0..num_workers {
                let start_pos = chunk_bounds[i];
                let end_pos = chunk_bounds[i + 1];
                let p = path.to_string();
                s.spawn(move || {
                    let f = File::open(&p).unwrap();
                    let mut buf = vec![0u8; 128 * 1024];
                    let mut offset = start_pos;
                    while offset < end_pos {
                        let to_read = (128 * 1024).min(end_pos - offset);
                        f.read_exact_at(&mut buf[..to_read], offset as u64).unwrap();
                        offset += to_read;
                    }
                });
            }
        });
        let d = t0.elapsed();
        println!("Workers: {:2} -> Read whole file time: {:?}", num_workers, d);
    }
}
