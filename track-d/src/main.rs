use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::thread;

#[derive(Copy, Clone, Eq, PartialEq)]
struct State {
    cost: u64,
    node: u32,
}

impl Ord for State {
    #[inline(always)]
    fn cmp(&self, other: &Self) -> Ordering {
        other.cost.cmp(&self.cost)
    }
}

impl PartialOrd for State {
    #[inline(always)]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct Graph {
    offsets: Vec<u32>,
    targets: Vec<u32>,
    weights: Vec<u32>,
}

fn main() -> std::io::Result<()> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string());

    let graph_path = Path::new(&data_dir).join("graph.txt");
    let queries_path = Path::new(&data_dir).join("queries.txt");
    let out_path = Path::new(&out_dir).join("distances.txt");


    let bytes = fs::read(&graph_path)?;
    let b_len = bytes.len();

    // Parse header: n and m
    let mut p = 0;
    while p < b_len && (bytes[p] == b' ' || bytes[p] == b'\r' || bytes[p] == b'\n') {
        p += 1;
    }
    let mut n = 0usize;
    while p < b_len && bytes[p] >= b'0' && bytes[p] <= b'9' {
        n = n * 10 + (bytes[p] - b'0') as usize;
        p += 1;
    }
    while p < b_len && bytes[p] == b' ' {
        p += 1;
    }
    let mut m = 0usize;
    while p < b_len && bytes[p] >= b'0' && bytes[p] <= b'9' {
        m = m * 10 + (bytes[p] - b'0') as usize;
        p += 1;
    }
    while p < b_len && bytes[p] != b'\n' {
        p += 1;
    }
    if p < b_len {
        p += 1;
    }
    let body_start = p;

    // Parallel 2-Pass Graph Builder across 6 threads
    const NUM_PARSERS: usize = 6;
    let data_len = b_len - body_start;
    let approx_p = data_len / NUM_PARSERS;
    let mut chunk_bounds = Vec::with_capacity(NUM_PARSERS + 1);
    chunk_bounds.push(body_start);
    for i in 1..NUM_PARSERS {
        let mut pos = body_start + i * approx_p;
        while pos < b_len && bytes[pos] != b'\n' {
            pos += 1;
        }
        chunk_bounds.push((pos + 1).min(b_len));
    }
    chunk_bounds.push(b_len);

    let mut worker_f_degs: Vec<Vec<u32>> = (0..NUM_PARSERS).map(|_| vec![0u32; n]).collect();
    let mut worker_b_degs: Vec<Vec<u32>> = (0..NUM_PARSERS).map(|_| vec![0u32; n]).collect();

    thread::scope(|s| {
        let b_slice = &bytes;
        for (w_idx, (w_f, w_b)) in worker_f_degs.iter_mut().zip(worker_b_degs.iter_mut()).enumerate() {
            let start = chunk_bounds[w_idx];
            let end = chunk_bounds[w_idx + 1];
            s.spawn(move || {
                let slice = &b_slice[start..end];
                let len = slice.len();
                let mut p = 0;
                while p < len {
                    while p < len && (slice[p] == b' ' || slice[p] == b'\r' || slice[p] == b'\n') {
                        p += 1;
                    }
                    if p >= len {
                        break;
                    }
                    let mut u = 0usize;
                    while p < len && slice[p] >= b'0' && slice[p] <= b'9' {
                        u = u * 10 + (slice[p] - b'0') as usize;
                        p += 1;
                    }
                    while p < len && slice[p] == b' ' {
                        p += 1;
                    }
                    let mut v = 0usize;
                    while p < len && slice[p] >= b'0' && slice[p] <= b'9' {
                        v = v * 10 + (slice[p] - b'0') as usize;
                        p += 1;
                    }
                    w_f[u] += 1;
                    w_b[v] += 1;

                    while p < len && slice[p] != b'\n' {
                        p += 1;
                    }
                    if p < len {
                        p += 1;
                    }
                }
            });
        }
    });

    // Build CSR offsets and convert worker degrees into starting offsets in-place
    let mut f_offsets = vec![0u32; n + 1];
    let mut b_offsets = vec![0u32; n + 1];
    for u in 0..n {
        let mut cur_f = f_offsets[u];
        let mut cur_b = b_offsets[u];
        for w in 0..NUM_PARSERS {
            let deg_f = worker_f_degs[w][u];
            let deg_b = worker_b_degs[w][u];
            worker_f_degs[w][u] = cur_f;
            worker_b_degs[w][u] = cur_b;
            cur_f += deg_f;
            cur_b += deg_b;
        }
        f_offsets[u + 1] = cur_f;
        b_offsets[u + 1] = cur_b;
    }
    let worker_f_starts = worker_f_degs;
    let worker_b_starts = worker_b_degs;

    let mut f_targets = vec![0u32; m];
    let mut f_weights = vec![0u32; m];
    let mut b_targets = vec![0u32; m];
    let mut b_weights = vec![0u32; m];

    let f_targets_ptr = f_targets.as_mut_ptr() as usize;
    let f_weights_ptr = f_weights.as_mut_ptr() as usize;
    let b_targets_ptr = b_targets.as_mut_ptr() as usize;
    let b_weights_ptr = b_weights.as_mut_ptr() as usize;

    thread::scope(|s| {
        let b_slice = &bytes;
        for (w_idx, (mut w_f, mut w_b)) in worker_f_starts.into_iter().zip(worker_b_starts.into_iter()).enumerate() {
            let start = chunk_bounds[w_idx];
            let end = chunk_bounds[w_idx + 1];
            s.spawn(move || {
                let slice = &b_slice[start..end];
                let len = slice.len();
                let mut p = 0;
                while p < len {
                    while p < len && (slice[p] == b' ' || slice[p] == b'\r' || slice[p] == b'\n') {
                        p += 1;
                    }
                    if p >= len {
                        break;
                    }
                    let mut u = 0usize;
                    while p < len && slice[p] >= b'0' && slice[p] <= b'9' {
                        u = u * 10 + (slice[p] - b'0') as usize;
                        p += 1;
                    }
                    while p < len && slice[p] == b' ' {
                        p += 1;
                    }
                    let mut v = 0u32;
                    while p < len && slice[p] >= b'0' && slice[p] <= b'9' {
                        v = v * 10 + (slice[p] - b'0') as u32;
                        p += 1;
                    }
                    while p < len && slice[p] == b' ' {
                        p += 1;
                    }
                    let mut w = 0u32;
                    while p < len && slice[p] >= b'0' && slice[p] <= b'9' {
                        w = w * 10 + (slice[p] - b'0') as u32;
                        p += 1;
                    }
                    while p < len && slice[p] != b'\n' {
                        p += 1;
                    }
                    if p < len {
                        p += 1;
                    }

                    let f_dest = w_f[u] as usize;
                    w_f[u] += 1;
                    let b_dest = w_b[v as usize] as usize;
                    w_b[v as usize] += 1;

                    // Invariant: worker chunks partition each node's forward and backward degree strictly disjointly
                    unsafe {
                        *(f_targets_ptr as *mut u32).add(f_dest) = v;
                        *(f_weights_ptr as *mut u32).add(f_dest) = w;
                        *(b_targets_ptr as *mut u32).add(b_dest) = u as u32;
                        *(b_weights_ptr as *mut u32).add(b_dest) = w;
                    }
                }
            });
        }
    });

    drop(bytes);

    let f_graph = Graph {
        offsets: f_offsets,
        targets: f_targets,
        weights: f_weights,
    };
    let b_graph = Graph {
        offsets: b_offsets,
        targets: b_targets,
        weights: b_weights,
    };

    // Read all queries
    let q_bytes = fs::read(&queries_path)?;
    let q_len = q_bytes.len();
    let mut queries = Vec::with_capacity(2048);

    let mut qp = 0;
    while qp < q_len {
        while qp < q_len && (q_bytes[qp] == b' ' || q_bytes[qp] == b'\r' || q_bytes[qp] == b'\n') {
            qp += 1;
        }
        if qp >= q_len {
            break;
        }
        let mut s = 0u32;
        while qp < q_len && q_bytes[qp] >= b'0' && q_bytes[qp] <= b'9' {
            s = s * 10 + (q_bytes[qp] - b'0') as u32;
            qp += 1;
        }
        while qp < q_len && q_bytes[qp] == b' ' {
            qp += 1;
        }
        let mut t = 0u32;
        while qp < q_len && q_bytes[qp] >= b'0' && q_bytes[qp] <= b'9' {
            t = t * 10 + (q_bytes[qp] - b'0') as u32;
            qp += 1;
        }
        while qp < q_len && q_bytes[qp] != b'\n' {
            qp += 1;
        }
        if qp < q_len {
            qp += 1;
        }
        queries.push((s, t));
    }
    drop(q_bytes);

    let num_queries = queries.len();
    let mut results = vec![-1i64; num_queries];

    let num_threads = 6;
    let next_query = AtomicUsize::new(0);

    let mut thread_outputs: Vec<Vec<(usize, i64)>> = (0..num_threads).map(|_| Vec::with_capacity(num_queries / num_threads + 32)).collect();

    thread::scope(|s| {
        let fg = &f_graph;
        let bg = &b_graph;
        let q_ref = &queries;
        let nq_ref = &next_query;

        for out_vec in &mut thread_outputs {
            s.spawn(move || {
                let mut f_heap = BinaryHeap::new();
                let mut b_heap = BinaryHeap::new();
                let mut f_dist = vec![u64::MAX; n];
                let mut b_dist = vec![u64::MAX; n];
                let mut f_touched = Vec::with_capacity(1024);
                let mut b_touched = Vec::with_capacity(1024);

                loop {
                    let idx = nq_ref.fetch_add(1, AtomicOrdering::Relaxed);
                    if idx >= num_queries {
                        break;
                    }

                    let (src, tgt) = q_ref[idx];
                    if src == tgt {
                        out_vec.push((idx, 0));
                        continue;
                    }
                    let src_idx = src as usize;
                    let tgt_idx = tgt as usize;
                    if fg.offsets[src_idx] == fg.offsets[src_idx + 1] || bg.offsets[tgt_idx] == bg.offsets[tgt_idx + 1] {
                        out_vec.push((idx, -1));
                        continue;
                    }

                    f_heap.clear();
                    b_heap.clear();
                    f_touched.clear();
                    b_touched.clear();

                    f_dist[src_idx] = 0;
                    f_touched.push(src);
                    f_heap.push(State { cost: 0, node: src });

                    b_dist[tgt_idx] = 0;
                    b_touched.push(tgt);
                    b_heap.push(State { cost: 0, node: tgt });

                    let mut mu = u64::MAX;

                    while !f_heap.is_empty() && !b_heap.is_empty() {
                        let f_top = f_heap.peek().unwrap().cost;
                        let b_top = b_heap.peek().unwrap().cost;

                        if f_top.saturating_add(b_top) >= mu {
                            break;
                        }

                        if f_top <= b_top {
                            let State { cost, node } = f_heap.pop().unwrap();
                            let node_idx = node as usize;
                            if cost > f_dist[node_idx] {
                                continue;
                            }

                            let start = fg.offsets[node_idx] as usize;
                            let end = fg.offsets[node_idx + 1] as usize;
                            for e_idx in start..end {
                                let v = fg.targets[e_idx];
                                let w = fg.weights[e_idx] as u64;
                                let next_cost = cost + w;
                                let v_idx = v as usize;

                                if next_cost < f_dist[v_idx] && next_cost < mu {
                                    if f_dist[v_idx] == u64::MAX {
                                        f_touched.push(v);
                                    }
                                    f_dist[v_idx] = next_cost;
                                    f_heap.push(State { cost: next_cost, node: v });

                                    let b_cost = b_dist[v_idx];
                                    if b_cost != u64::MAX {
                                        let total = next_cost + b_cost;
                                        if total < mu {
                                            mu = total;
                                        }
                                    }
                                }
                            }
                        } else {
                            let State { cost, node } = b_heap.pop().unwrap();
                            let node_idx = node as usize;
                            if cost > b_dist[node_idx] {
                                continue;
                            }

                            let start = bg.offsets[node_idx] as usize;
                            let end = bg.offsets[node_idx + 1] as usize;
                            for e_idx in start..end {
                                let u = bg.targets[e_idx];
                                let w = bg.weights[e_idx] as u64;
                                let next_cost = cost + w;
                                let u_idx = u as usize;

                                if next_cost < b_dist[u_idx] && next_cost < mu {
                                    if b_dist[u_idx] == u64::MAX {
                                        b_touched.push(u);
                                    }
                                    b_dist[u_idx] = next_cost;
                                    b_heap.push(State { cost: next_cost, node: u });

                                    let f_cost = f_dist[u_idx];
                                    if f_cost != u64::MAX {
                                        let total = next_cost + f_cost;
                                        if total < mu {
                                            mu = total;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    for &v in &f_touched {
                        f_dist[v as usize] = u64::MAX;
                    }
                    for &v in &b_touched {
                        b_dist[v as usize] = u64::MAX;
                    }

                    let res = if mu == u64::MAX { -1 } else { mu as i64 };
                    out_vec.push((idx, res));
                }
            });
        }
    });

    for out_vec in thread_outputs {
        for (idx, res) in out_vec {
            results[idx] = res;
        }
    }

    // Write results sequentially
    fs::create_dir_all(&out_dir)?;
    let out_file = File::create(&out_path)?;
    let mut writer = BufWriter::with_capacity(512 * 1024, out_file);

    for &res in &results {
        writeln!(writer, "{}", res)?;
    }

    writer.flush()?;

    Ok(())
}
