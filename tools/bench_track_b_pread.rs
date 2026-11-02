use std::fs::File;
use std::os::unix::fs::FileExt;
use std::time::Instant;
use std::thread;

const NUM_WORKERS: usize = 12;
const CHUNK_SIZE: usize = 128 * 1024;

fn main() {
    let path = "/tmp/sd-full-b/docs.txt";
    let file = File::open(path).unwrap();
    let file_len = file.metadata().unwrap().len() as usize;

    let approx_chunk = file_len / NUM_WORKERS;
    let mut chunk_bounds = Vec::with_capacity(NUM_WORKERS + 1);
    chunk_bounds.push(0);
    let mut scan_buf = vec![0u8; 4096];
    for i in 1..NUM_WORKERS {
        let approx_pos = i * approx_chunk;
        file.read_exact_at(&mut scan_buf, approx_pos as u64).unwrap();
        let mut offset = 0;
        while offset < scan_buf.len() && scan_buf[offset] != b'\n' { offset += 1; }
        chunk_bounds.push(approx_pos + offset + 1);
    }
    chunk_bounds.push(file_len);

    let t0 = Instant::now();
    thread::scope(|s| {
        for w_idx in 0..NUM_WORKERS {
            let (start_pos, end_pos) = (chunk_bounds[w_idx], chunk_bounds[w_idx + 1]);
            let p = path.to_string();
            s.spawn(move || {
                let f = File::open(&p).unwrap();
                let mut buf = vec![0u8; CHUNK_SIZE * 2];
                let (mut cur_offset, mut rem_len) = (start_pos, 0);
                let mut line_count = 0;

                while cur_offset < end_pos || rem_len > 0 {
                    let to_read = (CHUNK_SIZE).min(end_pos.saturating_sub(cur_offset));
                    let read_bytes = if to_read > 0 {
                        f.read_exact_at(&mut buf[rem_len..rem_len + to_read], cur_offset as u64).unwrap();
                        cur_offset += to_read;
                        to_read
                    } else { 0 };
                    let total_available = rem_len + read_bytes;
                    if total_available == 0 { break; }
                    let mut last_nl = total_available;
                    while last_nl > 0 && buf[last_nl - 1] != b'\n' { last_nl -= 1; }
                    let slice = &buf[..last_nl];

                    for &b in slice {
                        if b == b'\n' { line_count += 1; }
                    }

                    rem_len = total_available - last_nl;
                    if rem_len > 0 { buf.copy_within(last_nl..total_available, 0); }
                }
            });
        }
    });
    let d = t0.elapsed();
    println!("Read and line count 254MB with pread across 12 workers: {:?}", d);
}
