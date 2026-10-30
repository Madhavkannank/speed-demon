# Track D: Traverse

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

Our baseline implementation is built in safe Rust with zero external runtime dependencies ($D = 1.0$):
1. **Compressed Sparse Row (CSR) Graph**: Reads the graph using a 1 MB `BufReader` into a flat Compressed Sparse Row representation (`offsets`, `targets`, `weights`). This guarantees contiguous memory access and zero pointer chasing.
2. **Generational Dijkstra**: Answers shortest-path queries using standard `std::collections::BinaryHeap`. To avoid $O(V)$ re-initialization of distance/visited arrays per query, it uses a generational `visited_query: Vec<u32>` tag alongside `dist: Vec<u64>`.
3. **Early Termination**: Terminates the priority queue search as soon as the target node is popped from the min-heap.
4. **Buffered Streaming Output**: Formats outputs with a 512 KB `BufWriter` directly to `/out/distances.txt`.

## Dependencies

None. Standard library only ($D = 1.0$).

## With another 24 hours

1. Multi-threaded query answering across 8-12 worker threads with thread-local search scratchpads.
2. Bidirectional Dijkstra for early meeting.
3. Custom memory-mapped binary or integer parsing for graph ingestion.
