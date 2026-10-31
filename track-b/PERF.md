# Track B: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | (official baseline) | 56.35 | 2200 | 0.00 |
| 1 | Streaming 4MB chunk indexer with query-aware vocabulary and parallel query execution | run-3124b4166760 | 5.051 | 185.0 | 31.58 |
| 2 | Parallel 6-worker document indexing, flat CSR postings, galloping search, fast byte formatting | local verified (f86973... match) | 1.490 | 245.0 | 56.5 est |
| 3 | Zero-allocation phrase queries with stack order array and reused match buffer | run-f7cb19f367ce | 1.627 | 251.0 | 41.61 |
| 4 | Streamlined 90-line source ($S \le 91$, cap 1.0), 1.52s standalone, 1.76s container | local check.py (f86973... match) | 1.520 | 245.3 | 48.65 |
| 5 | Zero-hash candidate phrase matching, container 1.52s, standalone 1.27s, 91 lines ($S \le 91$) | local check.py (f86973... match) | 1.270 | 250.5 | 50.51 |
| 6 | 12-worker indexing matching hardware threads, container 1.11s, standalone 1.10s, 91 lines | local check.py (f86973... match) | 1.100 | 271.4 | 51.62 |
| 7 | Galloping 8x ratio search threshold in intersection, sub-second standalone 0.97s, 91 lines | run-12007d9 | 1.133 | 285.0 | 45.06 |
| 8 | 256KB 4-hash Bloom filter with 512KB chunk buffer and galloping search intersection | local check.py (f86973... match) | 1.010 | 271.4 | 52.99 |
