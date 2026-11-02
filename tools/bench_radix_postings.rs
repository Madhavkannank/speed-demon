use std::time::Instant;

fn main() {
    let n = 1_850_000usize;
    let mut data: Vec<u32> = Vec::with_capacity(n);
    // Simulate 1.85M (term_id, local_doc_id)
    for i in 0..n {
        let term_id = (i % 53444) as u32;
        let local_doc = (i % 20833) as u32;
        data.push((term_id << 16) | local_doc);
    }

    let t0 = Instant::now();
    // Radix sort by top 16 bits (term_id)
    let mut counts0 = [0u32; 256];
    let mut counts1 = [0u32; 256];
    for &x in &data {
        let t = (x >> 16) as usize;
        counts0[t & 0xFF] += 1;
        counts1[(t >> 8) & 0xFF] += 1;
    }

    let mut offsets0 = [0u32; 256];
    let mut offsets1 = [0u32; 256];
    for i in 1..256 {
        offsets0[i] = offsets0[i - 1] + counts0[i - 1];
        offsets1[i] = offsets1[i - 1] + counts1[i - 1];
    }

    let mut temp = vec![0u32; n];
    for &x in &data {
        let b = ((x >> 16) & 0xFF) as usize;
        temp[offsets0[b] as usize] = x;
        offsets0[b] += 1;
    }

    for &x in &temp {
        let b = ((x >> 24) & 0xFF) as usize;
        data[offsets1[b] as usize] = x;
        offsets1[b] += 1;
    }
    let sort_ms = t0.elapsed().as_secs_f64() * 1000.0;

    println!("Radix sort 1.85M pairs by term_id: {:.2} ms", sort_ms);

    // Verify sorted by term_id
    let mut sorted = true;
    for i in 1..n {
        if (data[i] >> 16) < (data[i - 1] >> 16) {
            sorted = false;
            break;
        }
    }
    println!("Sorted check: {}", if sorted { "PASS" } else { "FAIL" });

    // Delta-encode into byte buffer
    let t1 = Instant::now();
    let mut comp: Vec<u8> = Vec::with_capacity(n * 2);
    let mut cur_term = u32::MAX;
    let mut last_doc = 0u32;
    for &x in &data {
        let term = x >> 16;
        let doc = x & 0xFFFF;
        if term != cur_term {
            cur_term = term;
            last_doc = doc;
            // Write first doc as varint
            let mut v = doc;
            while v >= 128 {
                comp.push((v & 0x7F) as u8 | 0x80);
                v >>= 7;
            }
            comp.push(v as u8);
        } else {
            let delta = doc - last_doc;
            last_doc = doc;
            let mut v = delta;
            while v >= 128 {
                comp.push((v & 0x7F) as u8 | 0x80);
                v >>= 7;
            }
            comp.push(v as u8);
        }
    }
    let comp_ms = t1.elapsed().as_secs_f64() * 1000.0;
    println!("Delta-varint compression: {:.2} ms (size: {:.2} MB, bytes/posting: {:.2})",
        comp_ms, comp.len() as f64 / 1e6, comp.len() as f64 / n as f64);
}
