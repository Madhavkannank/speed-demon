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

    // Pass 1: Count forward and backward degrees
    let mut f_deg = vec![0u32; n];
    let mut b_deg = vec![0u32; n];
    while p < b_len {
        while p < b_len && (bytes[p] == b' ' || bytes[p] == b'\r' || bytes[p] == b'\n') {
            p += 1;
        }
        if p >= b_len {
            break;
        }
        let mut u = 0usize;
        while p < b_len && bytes[p] >= b'0' && bytes[p] <= b'9' {
            u = u * 10 + (bytes[p] - b'0') as usize;
            p += 1;
        }
        while p < b_len && bytes[p] == b' ' {
            p += 1;
        }
        let mut v = 0usize;
        while p < b_len && bytes[p] >= b'0' && bytes[p] <= b'9' {
            v = v * 10 + (bytes[p] - b'0') as usize;
            p += 1;
        }
        f_deg[u] += 1;
        b_deg[v] += 1;

        while p < b_len && bytes[p] != b'\n' {
            p += 1;
        }
        if p < b_len {
            p += 1;
        }
    }

    // Build CSR offsets
    let mut f_offsets = vec![0u32; n + 1];
    let mut b_offsets = vec![0u32; n + 1];
    for i in 0..n {
        f_offsets[i + 1] = f_offsets[i] + f_deg[i];
        b_offsets[i + 1] = b_offsets[i] + b_deg[i];
    }
    drop(f_deg);
    drop(b_deg);

    let mut f_targets = vec![0u32; m];
    let mut f_weights = vec![0u32; m];
    let mut b_targets = vec![0u32; m];
    let mut b_weights = vec![0u32; m];
    let mut f_cur = f_offsets.clone();
    let mut b_cur = b_offsets.clone();

    // Pass 2: Fill forward and backward CSR
    p = body_start;
    while p < b_len {
        while p < b_len && (bytes[p] == b' ' || bytes[p] == b'\r' || bytes[p] == b'\n') {
            p += 1;
        }
        if p >= b_len {
            break;
        }
        let mut u = 0usize;
        while p < b_len && bytes[p] >= b'0' && bytes[p] <= b'9' {
            u = u * 10 + (bytes[p] - b'0') as usize;
            p += 1;
        }
        while p < b_len && bytes[p] == b' ' {
            p += 1;
        }
        let mut v = 0u32;
        while p < b_len && bytes[p] >= b'0' && bytes[p] <= b'9' {
            v = v * 10 + (bytes[p] - b'0') as u32;
            p += 1;
        }
        while p < b_len && bytes[p] == b' ' {
            p += 1;
        }
        let mut w = 0u32;
        while p < b_len && bytes[p] >= b'0' && bytes[p] <= b'9' {
            w = w * 10 + (bytes[p] - b'0') as u32;
            p += 1;
        }
        while p < b_len && bytes[p] != b'\n' {
            p += 1;
        }
        if p < b_len {
            p += 1;
        }

        let f_dest = f_cur[u] as usize;
        f_cur[u] += 1;
        f_targets[f_dest] = v;
        f_weights[f_dest] = w;

        let b_dest = b_cur[v as usize] as usize;
        b_cur[v as usize] += 1;
        b_targets[b_dest] = u as u32;
        b_weights[b_dest] = w;
    }

    drop(bytes);
    drop(f_cur);
    drop(b_cur);

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
