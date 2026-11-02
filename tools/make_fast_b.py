with open('tools/prototype_b1_b2.rs') as f:
    code = f.read()

# Replace the single-threaded sampling pass with parallel sampling
single_sampling = """    // Fast sampling to detect dense terms
    let mut sampled_counts = vec![0u16; vocab_size];
    let mut sample_p = 0;
    while sample_p < file_len {
        let mut did = 0u32;
        while sample_p < file_len && mmap_slice[sample_p] >= b'0' && mmap_slice[sample_p] <= b'9' {
            did = did * 10 + (mmap_slice[sample_p] - b'0') as u32;
            sample_p += 1;
        }
        while sample_p < file_len && mmap_slice[sample_p] != b'\t' && mmap_slice[sample_p] != b'\n' {
            sample_p += 1;
        }
        if sample_p < file_len && mmap_slice[sample_p] == b'\t' {
            sample_p += 1;
        }
        if did % 32 == 0 {
            let mut last_seen = 0u32;
            while sample_p < file_len && mmap_slice[sample_p] != b'\n' {
                if mmap_slice[sample_p].is_ascii_alphanumeric() {
                    let start_t = sample_p;
                    while sample_p < file_len && mmap_slice[sample_p].is_ascii_alphanumeric() {
                        sample_p += 1;
                    }
                    let count = sample_p - start_t;
                    let (h, w0, w1, last_b) = hash_token(mmap_slice.as_ptr().wrapping_add(start_t), count);
                    let b_idx = (h as usize) & BLOOM_MASK;
                    if ((bloom[b_idx >> 6] >> (b_idx & 63)) & 1) != 0 {
                        let tid = table.lookup(h, w0, w1, count as u8, last_b);
                        if tid > 0 && tid != last_seen {
                            sampled_counts[tid as usize] = sampled_counts[tid as usize].saturating_add(1);
                            last_seen = tid;
                        }
                    }
                } else {
                    sample_p += 1;
                }
            }
        } else {
            while sample_p < file_len && mmap_slice[sample_p] != b'\n' {
                sample_p += 1;
            }
        }
        if sample_p < file_len && mmap_slice[sample_p] == b'\n' {
            sample_p += 1;
        }
    }"""

parallel_sampling = """    // Parallel sampling to detect dense terms (12 threads)
    let num_workers = 12;
    let approx_chunk = file_len / num_workers;
    let mut chunk_bounds = Vec::with_capacity(num_workers + 1);
    chunk_bounds.push(0);
    for i in 1..num_workers {
        let mut pos = (i * approx_chunk).min(file_len);
        while pos < file_len && mmap_slice[pos] != b'\n' {
            pos += 1;
        }
        if pos < file_len {
            pos += 1;
        }
        loop {
            if pos >= file_len { break; }
            let mut dp = pos;
            let mut did = 0u32;
            while dp < file_len && mmap_slice[dp] >= b'0' && mmap_slice[dp] <= b'9' {
                did = did * 10 + (mmap_slice[dp] - b'0') as u32;
                dp += 1;
            }
            if did % 64 == 0 {
                break;
            }
            while pos < file_len && mmap_slice[pos] != b'\n' {
                pos += 1;
            }
            if pos < file_len {
                pos += 1;
            }
        }
        chunk_bounds.push(pos.min(file_len));
    }
    chunk_bounds.push(file_len);

    let mut thread_samples = vec![vec![0u16; vocab_size]; num_workers];
    thread::scope(|s| {
        for (w, s_buf) in thread_samples.iter_mut().enumerate() {
            let (start, end) = (chunk_bounds[w], chunk_bounds[w + 1]);
            let slice = &mmap_slice[start..end];
            let table_ref = &table;
            let bloom_ref = &bloom;
            s.spawn(move || {
                let mut p = 0;
                let len = slice.len();
                while p < len {
                    let mut did = 0u32;
                    while p < len && slice[p] >= b'0' && slice[p] <= b'9' {
                        did = did * 10 + (slice[p] - b'0') as u32;
                        p += 1;
                    }
                    while p < len && slice[p] != b'\t' && slice[p] != b'\n' { p += 1; }
                    if p < len && slice[p] == b'\t' { p += 1; }
                    if did % 32 == 0 {
                        let mut last_seen = 0u32;
                        while p < len && slice[p] != b'\n' {
                            if slice[p].is_ascii_alphanumeric() {
                                let start_t = p;
                                while p < len && slice[p].is_ascii_alphanumeric() { p += 1; }
                                let count = p - start_t;
                                let (h, w0, w1, last_b) = hash_token(slice.as_ptr().wrapping_add(start_t), count);
                                let b_idx = (h as usize) & BLOOM_MASK;
                                if ((bloom_ref[b_idx >> 6] >> (b_idx & 63)) & 1) != 0 {
                                    let tid = table_ref.lookup(h, w0, w1, count as u8, last_b);
                                    if tid > 0 && tid != last_seen {
                                        s_buf[tid as usize] = s_buf[tid as usize].saturating_add(1);
                                        last_seen = tid;
                                    }
                                }
                            } else {
                                p += 1;
                            }
                        }
                    } else {
                        while p < len && slice[p] != b'\n' { p += 1; }
                    }
                    if p < len && slice[p] == b'\n' { p += 1; }
                }
            });
        }
    });

    let mut sampled_counts = vec![0u16; vocab_size];
    for ts in thread_samples {
        for t in 1..vocab_size {
            sampled_counts[t] = sampled_counts[t].saturating_add(ts[t]);
        }
    }"""

code = code.replace(single_sampling, parallel_sampling)

# Remove redundant second chunk_bounds calculation
redundant_chunk = """    let num_workers = 12;
    let approx_chunk = file_len / num_workers;
    let mut chunk_bounds = Vec::with_capacity(num_workers + 1);
    chunk_bounds.push(0);
    for i in 1..num_workers {
        let mut pos = (i * approx_chunk).min(file_len);
        while pos < file_len && mmap_slice[pos] != b'\n' {
            pos += 1;
        }
        if pos < file_len {
            pos += 1;
        }
        // align to doc_id % 64 == 0
        loop {
            if pos >= file_len { break; }
            let mut dp = pos;
            let mut did = 0u32;
            while dp < file_len && mmap_slice[dp] >= b'0' && mmap_slice[dp] <= b'9' {
                did = did * 10 + (mmap_slice[dp] - b'0') as u32;
                dp += 1;
            }
            if did % 64 == 0 {
                break;
            }
            while pos < file_len && mmap_slice[pos] != b'\n' {
                pos += 1;
            }
            if pos < file_len {
                pos += 1;
            }
        }
        chunk_bounds.push(pos.min(file_len));
    }
    chunk_bounds.push(file_len);"""

code = code.replace(redundant_chunk, "// chunk_bounds already computed in parallel sampling")

with open('tools/fast_b_proto.rs', 'w') as f:
    f.write(code)
print('Generated tools/fast_b_proto.rs')
