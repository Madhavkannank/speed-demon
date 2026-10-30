use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

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

    let mut line = String::new();
    while graph_reader.read_line(&mut line)? > 0 {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            let mut parts = trimmed.split_whitespace();
            let u: usize = parts.next().unwrap().parse().unwrap();
            let v: usize = parts.next().unwrap().parse().unwrap();
            let w: u64 = parts.next().unwrap().parse().unwrap();
            degrees[u] += 1;
            raw_edges.push((u, v, w));
        }
        line.clear();
    }

    // Build CSR
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

    // Read and answer queries
    let queries_file = File::open(&queries_path)?;
    let queries_reader = BufReader::with_capacity(512 * 1024, queries_file);

    fs::create_dir_all(&out_dir)?;
    let out_file = File::create(&out_path)?;
    let mut writer = BufWriter::with_capacity(512 * 1024, out_file);

    let mut heap = BinaryHeap::new();
    let mut dist = vec![u64::MAX; n];
    let mut visited_query = vec![0u32; n];
    let mut current_query = 0u32;

    for line_res in queries_reader.lines() {
        let q_line = line_res?;
        let trimmed = q_line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut parts = trimmed.split_whitespace();
        let s: usize = parts.next().unwrap().parse().unwrap();
        let t: usize = parts.next().unwrap().parse().unwrap();

        current_query += 1;
        heap.clear();

        dist[s] = 0;
        visited_query[s] = current_query;
        heap.push(State { cost: 0, node: s });

        let mut found = None;

        while let Some(State { cost, node }) = heap.pop() {
            if cost > dist[node] {
                continue;
            }
            if node == t {
                found = Some(cost);
                break;
            }

            let start = graph.offsets[node];
            let end = graph.offsets[node + 1];

            for edge_idx in start..end {
                let v = graph.targets[edge_idx];
                let w = graph.weights[edge_idx];
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

        match found {
            Some(d) => writeln!(writer, "{}", d)?,
            None => writeln!(writer, "-1")?,
        }
    }

    writer.flush()?;
    Ok(())
}
