# Track D: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | (official baseline) | 72.69 | 893 | 0.00 |
| 1 | Bidirectional Dijkstra with atomic work-stealing across hardware threads | run-8dd0785ae185 | 1.752 | 288.3 | 38.54 |
| 2 | Parallel 2-pass chunked CSR graph builder with in-place degree reuse | local verified (a23dbd... match) | 1.680 | 271.0 | 58.0 est |
