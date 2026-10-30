# Track A: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | (official baseline) | 32.05 | 228 | 0.00 |
| 1 | Multi-worker streaming parser with integer hour indexing | run-629d3a4dee93 | 0.101 | 71.3 | 60.96 |
| 2 | Cacheline-aligned unrolled chunk nodes (LatChunk), branchless field offsets, zero heap churn | local verified (495ca0... match) | 0.160 | 29.3 | 74.5 est |
| 3 | 64-bit word fast-hash, 8192-slot table, parallel 6-thread chunk sort and direct byte formatting | local verified (495ca0... match) | 0.093 | 38.0 | 78.0 est |
