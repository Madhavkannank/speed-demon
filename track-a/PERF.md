# Performance Engineering Log — Track A: Ingest (C++)

## Overview
Track A processes 3,100,000 structured HTTP log lines across 72 hours and produces hourly endpoint aggregates, percentiles (p50, p95, p99), and error rates.

## Architecture & Optimizations
1. **Zero-Copy Chunk Parsing:** Direct pointer scanning on 4 MB buffer chunks. Timestamp truncation to the hour (`YYYY-MM-DDTHH:00:00Z`) done by direct 13-byte slice without date/time library overhead.
2. **Compact Group Representation:** Hash map keyed by `(std::string hour, std::string endpoint)` with custom 64-bit combiner hash.
3. **Optimized Latency Percentiles:** Fast `std::sort` per group and nearest-rank integer calculation: `rank = (p * n + 99) / 100`.
4. **Buffered Fast I/O:** `std::to_chars` decimal formatting with 2 MB batch write buffer directly to `/out/summary.txt`.

## Optimization Experiments & Progression

| Version | Description | Wall Time (sample) | Peak Memory | Correctness Gate | Status |
|---|---|---:|---:|:---:|:---:|
| Reference | Naive Python 3.13 | 1.34s | 237 MB | PASS | Baseline |
| V1 (C++) | C++20 chunk parser, std::unordered_map, buffered output | 0.20s | ~18 MB | PASS (`ca6311...`) | ACCEPTED |

## Verification
- **Tiny dataset:** `c24fa507c963eaeac9224fd8435019b4f4e8c002ece7e32f316681a3957eb92f` (100% MATCH)
- **Sample dataset:** `ca6311bbeb536fa55d85265bf6fd3d24cc9586f13de1ac73bd2a23ac3893aac3` (100% MATCH)
- **Repeatability:** 5/5 identical runs.
