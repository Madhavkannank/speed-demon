# Performance Engineering Log — Track B: Index (C++)

## Overview
Track B builds an in-memory inverted index over legal text documents (254 MB corpus) and evaluates 50,000 queries (62% boolean with strict left-to-right evaluation, 38% exact phrase search).

## Architecture & Optimizations
1. **Byte-Exact Tokenizer:** Implements exact ASCII lowercasing (bytes `0x41` to `0x5A` mapped to `0x61` to `0x7A`, with all bytes $\ge 0x80$ treated as non-token separators).
2. **Dense Inverted Postings:** Terms mapped to contiguous, ascending vectors of `uint32_t` document IDs.
3. **Linear Two-Pointer Set Operations:** Boolean expressions (`AND`, `OR`, `NOT`) evaluated strictly left-to-right via two-pointer merges on sorted integer arrays in $O(|A| + |B|)$ time.
4. **Fast Subsequence Phrase Verification:** Candidates pre-filtered by intersection of all term postings, followed by contiguous subsequence scans over document token arrays. Matches are naturally emitted in sorted document ID order, completely eliminating sorting overhead.

## Optimization Progression & Benchmark

| Version | Description | Wall Time (sample) | Peak Memory | Correctness Gate | Status |
|---|---|---:|---:|:---:|:---:|
| Reference | Python 3.13 inverted dictionary & set ops | ~63.2s (sample), ~56.4s (full) | ~180 MB | PASS | Baseline |
| V1 (C++) | C++20 custom inverted index + 2-pointer set merges | 1.25s | ~32 MB | PASS (`eeeec1...`) | ACCEPTED |

## Verification
- **Sample dataset hash:** `eeeec1e9319da98a6f3c664c78f93e7078fd603c25c2bb3027174e81e3638a8f` (100% MATCH)
- **Repeatability:** 5/5 identical runs.
