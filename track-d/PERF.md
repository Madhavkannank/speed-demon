# Performance Engineering Log — Track D: Traverse (C++)

## Overview
Track D loads a directed graph (1,740,512 nodes, 5,000,000 edges) and answers 2,000 shortest-path queries.

## Architecture & Optimizations
1. **CSR (Compressed Sparse Row) Representation:** Eliminates 1.7 million small heap allocations. All graph edges are packed into contiguous `edges` and `weights` arrays indexed by a flat `head` array.
2. **Epoch / Generation Stamped Dijkstra:** Avoids clearing 1.7 million distance entries on every query. Distance initialization is $O(1)$ by tagging node visits with the monotonic `current_epoch`.
3. **64-bit Distance Accumulator:** Prevents integer overflow when chains of weights approach $2^{32}-1$.
4. **Early Exit Optimization:** Terminates search immediately when target $t$ is popped from the priority queue. Immediate $0$ return if $s == t$.

## Optimization Progression & Benchmark

| Version | Description | Wall Time (sample) | Peak Memory | Correctness Gate | Status |
|---|---|---:|---:|:---:|:---:|
| Reference | Python 3.13 heapq on adjacency lists | ~7.2s (sample), ~72.7s (full) | ~140 MB | PASS | Baseline |
| V1 (C++) | C++20 CSR + Epoch Dijkstra | 0.55s | ~24 MB | PASS (`a4c76e...`) | ACCEPTED |

## Verification
- **Sample dataset hash:** `a4c76ed030bc7e365db33191f6cf74708fba7f25eb48d64c8c4279f2e57046a0` (100% MATCH)
- **Repeatability:** 5/5 identical runs.
