use std::fs::File;
use std::os::unix::fs::FileExt;
use std::time::Instant;

fn main() {
    let path = "/tmp/sd-full-a/requests.log";
    let file = File::open(path).unwrap();
    let total_len = 17 * 1024 * 1024;

    for chunk_kb in [64, 128, 256, 512, 1024, 2048, 4096, 17408] {
        let chunk_size = chunk_kb * 1024;
        let mut buf = vec![0u8; chunk_size];
        let mut offset = 0;
        let t0 = Instant::now();
        while offset < total_len {
            let to_read = (chunk_size).min(total_len - offset);
            file.read_exact_at(&mut buf[..to_read], offset as u64).unwrap();
            offset += to_read;
        }
        let d = t0.elapsed();
        println!("Chunk size: {:5} KB -> time: {:?}", chunk_kb, d);
    }
}
