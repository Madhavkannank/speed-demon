use std::time::Instant;

fn main() {
    let n = 260_000;
    let mut rng = 123456789u64;
    let mut next_rand = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    // Approach 1: Giant 260k u64 sort
    let mut records: Vec<u64> = Vec::with_capacity(n);
    for _ in 0..n {
        let g_id = (next_rand() % 10000) as u32;
        let lat = (next_rand() % 1_000_000) as u32;
        records.push(((g_id as u64) << 32) | (lat as u64));
    }

    let t1 = Instant::now();
    records.sort_unstable();
    let d1 = t1.elapsed();
    println!("Approach 1 (Giant 260k u64 sort): {:?}", d1);

    // Approach 2: Grouped u32 slices
    let mut counts = vec![0u32; 10001];
    for r in &records {
        let g = (r >> 32) as usize;
        counts[g + 1] += 1;
    }
    for i in 1..=10000 {
        counts[i] += counts[i - 1];
    }
    let mut latencies = vec![0u32; n];
    let mut cur = counts.clone();
    for r in &records {
        let g = (r >> 32) as usize;
        let lat = *r as u32;
        latencies[cur[g] as usize] = lat;
        cur[g] += 1;
    }

    let t2 = Instant::now();
    for i in 0..10000 {
        let start = counts[i] as usize;
        let end = counts[i + 1] as usize;
        if end > start + 1 {
            latencies[start..end].sort_unstable();
        }
    }
    let d2 = t2.elapsed();
    println!("Approach 2 (Per-group slice sort in L1): {:?}", d2);
}
