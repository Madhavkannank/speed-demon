use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::thread;

#[derive(Copy, Clone, Eq, PartialEq)]
struct State {
    cost: u64,
    node: usize,
}

impl Ord for State {
    fn cmp(&self, other: &Self) -> Ordering {
        other.cost.cmp(&self.cost)
    }
}

impl PartialOrd for State {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct Graph {
    offsets: Vec<usize>,
    targets: Vec<usize>,
    weights: Vec<u64>,
}

#[inline(always)]
fn parse_usize(bytes: &[u8]) -> usize {
    let mut val = 0usize;
    for &b in bytes {
        if b.is_ascii_digit() {
            val = val * 10 + (b - b'0') as usize;
        }
    }
    val
}

#[inline(always)]
fn parse_u64(bytes: &[u8]) -> u64 {
    let mut val = 0u64;
    for &b in bytes {
        if b.is_ascii_digit() {
            val = val * 10 + (b - b'0') as u64;
        }
    }
    val
}

fn main() -> std::io::Result<()> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string());

    let graph_path = Path::new(&data_dir).join("graph.txt");
    let queries_path = Path::new(&data_dir).join("queries.txt");
    let out_path = Path::new(&out_dir).join("distances.txt");

    let graph_file = File::open(&graph_path)?;
    let mut graph_reader = BufReader::with_capacity(1024 * 1024, graph_file);

    let mut first_line = String::new();
    graph_reader.read_line(&mut first_line)?;
    let mut header_parts = first_line.split_whitespace();
    let n: usize = header_parts.next().unwrap().parse().unwrap();
    let m: usize = header_parts.next().unwrap().parse().unwrap();

    // Read edges
    let mut raw_edges = Vec::with_capacity(m);
    let mut degrees = vec![0usize; n];

    let mut line = Vec::with_capacity(128);
    while graph_reader.read_until(b'\n', &mut line)? > 0 {
        let len = line.len();
        if len > 0 && line[0].is_ascii_digit() {
            let mut p1 = 0;
            while p1 < len && line[p1] != b' ' {
                p1 += 1;
            }
            let mut p2 = p1 + 1;
            while p2 < len && line[p2] != b' ' {
                p2 += 1;
            }
            let mut p3 = len;
            while p3 > p2 && (line[p3 - 1] == b'\n' || line[p3 - 1] == b'\r') {
                p3 -= 1;
            }

            let u = parse_usize(&line[..p1]);
            let v = parse_usize(&line[p1 + 1..p2]);
            let w = parse_u64(&line[p2 + 1..p3]);

            degrees[u] += 1;
            raw_edges.push((u, v, w));
        }
        line.clear();
    }

    // Build CSR representation
    let mut offsets = vec![0usize; n + 1];
    for i in 0..n {
        offsets[i + 1] = offsets[i] + degrees[i];
    }

    let mut targets = vec![0usize; m];
    let mut weights = vec![0u64; m];
    let mut cur_offsets = offsets.clone();

    for (u, v, w) in raw_edges {
        let dest = cur_offsets[u];
        cur_offsets[u] += 1;
        targets[dest] = v;
        weights[dest] = w;
    }

    let graph = Graph {
        offsets,
        targets,
        weights,
    };

    // Read all queries
    let queries_file = File::open(&queries_path)?;
    let mut queries_reader = BufReader::with_capacity(512 * 1024, queries_file);

    let mut queries = Vec::new();
    while queries_reader.read_until(b'\n', &mut line)? > 0 {
        let len = line.len();
        if len > 0 && line[0].is_ascii_digit() {
            let mut p = 0;
            while p < len && line[p] != b' ' {
                p += 1;
            }
            let mut end = len;
            while end > p && (line[end - 1] == b'\n' || line[end - 1] == b'\r') {
                end -= 1;
            }
            let s = parse_usize(&line[..p]);
            let t = parse_usize(&line[p + 1..end]);
            queries.push((s, t));
        }
        line.clear();
    }

    let num_queries = queries.len();
    let mut results = vec![-1i64; num_queries];

    // Determine thread count (cap at 8 threads to respect container core allocation)
    let num_threads = thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(8)
        .min(8)
        .max(1);

    let chunk_size = (num_queries + num_threads - 1) / num_threads;

    thread::scope(|s| {
        let graph_ref = &graph;
        for (thread_idx, (chunk, res_chunk)) in queries
            .chunks(chunk_size)
            .zip(results.chunks_mut(chunk_size))
            .enumerate()
        {
            let _ = thread_idx;
            s.spawn(move || {
                let mut heap = BinaryHeap::new();
                let mut dist = vec![u64::MAX; n];
                let mut visited_query = vec![0u32; n];
                let mut current_query = 0u32;

                for (idx, &(src, tgt)) in chunk.iter().enumerate() {
                    current_query += 1;
                    heap.clear();

                    dist[src] = 0;
                    visited_query[src] = current_query;
                    heap.push(State { cost: 0, node: src });

                    let mut found = None;

                    while let Some(State { cost, node }) = heap.pop() {
                        if cost > dist[node] {
                            continue;
                        }
                        if node == tgt {
                            found = Some(cost);
                            break;
                        }

                        let start = graph_ref.offsets[node];
                        let end = graph_ref.offsets[node + 1];

                        for edge_idx in start..end {
                            let v = graph_ref.targets[edge_idx];
                            let w = graph_ref.weights[edge_idx];
                            let next_cost = cost + w;

                            let v_dist = if visited_query[v] == current_query {
                                dist[v]
                            } else {
                                visited_query[v] = current_query;
                                dist[v] = u64::MAX;
                                u64::MAX
                            };

                            if next_cost < v_dist {
                                dist[v] = next_cost;
                                heap.push(State {
                                    cost: next_cost,
                                    node: v,
                                });
                            }
                        }
                    }

                    res_chunk[idx] = match found {
                        Some(d) => d as i64,
                        None => -1,
                    };
                }
            });
        }
    });

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
