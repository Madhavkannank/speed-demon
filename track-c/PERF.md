# Performance Engineering Log — Track C: Join (C++)

## Overview
Track C joins hostile `records.csv` (~200 MB) against `regions.csv` (200k regions, 240 countries) and aggregates metrics into `/out/aggregates.txt`.

## Architecture & Optimizations
1. **Hostile CSV State Machine:** Custom streaming byte parser with support for mid-stream BOMs (`0xEF, 0xBB, 0xBF`), quoted comma sequences, escaped double-quotes (`""`), multiline notes with embedded LFs, mixed CRLF/LF line endings, and empty fields.
2. **Exact Integer Arithmetic:** Monetary values are parsed and accumulated as 64-bit integer cents. No floating-point rounding errors or precision drift.
3. **Optimized Lookup:** Pre-allocated lookup structure mapping `region_id` to country code, defaulting to `_UNMATCHED` for unknown region IDs.
4. **Buffered Output Generation:** Custom integer-to-string decimal formatting directly into memory buffer written in 512 KB chunks.

## Optimization Progression & Benchmark

| Version | Description | Wall Time (sample) | Peak Memory | Correctness Gate | Status |
|---|---|---:|---:|:---:|:---:|
| Reference | Python 3.13 stdlib csv reader | ~6.0s (full), ~0.95s (sample) | ~95 MB | PASS | Baseline |
| V1 (C++) | C++20 custom streaming state machine | 0.40s | ~19 MB | PASS (`9035fed...`) | ACCEPTED |

## Verification
- **Sample dataset hash:** `9035fed36823232dd41ae2d1b9d6d21aa7c73f49d8e95647e953a89d5ebe9a66` (100% MATCH)
- **Repeatability:** 5/5 identical runs.
