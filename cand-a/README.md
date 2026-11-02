# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

Our baseline implementation is built in safe Rust with zero external runtime dependencies ($D = 1.0$):
1. **Streaming Input**: Reads `/data/requests.log` using an 512 KB `BufReader`, streaming line-by-line to prevent high memory usage.
2. **Fixed-Offset Hour Slicing**: Truncates the RFC 3339 timestamp at byte 13 (`YYYY-MM-DDTHH`) and appends `:00:00Z`, bypassing heavyweight ISO date parsing.
3. **Contiguous Aggregation**: Groups by `(hour, endpoint)` in a flat hash map, recording latencies in contiguous `Vec<u32>` and counting 5xx status codes ($500 \le \text{status} \le 599$).
4. **Exact Percentiles & Output**: Sorts groups in total chronological and byte order, sorts each group's latencies, and evaluates p50/p95/p99 using integer nearest-rank arithmetic `(p * n + 99) / 100`.

## Dependencies

None. Standard library only ($D = 1.0$).

## With another 24 hours

1. Zero-dependency POSIX `mmap` with `MADV_SEQUENTIAL`.
2. Custom SIMD/SWAR whitespace scanning.
3. Fixed-bucket histograms to compute nearest-rank percentiles without sorting latencies.
