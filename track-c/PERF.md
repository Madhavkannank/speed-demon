# Track C: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | (official baseline) | 14.44 | 367 | 0.00 |
| 1 | Safe Rust streaming CSV state machine with exact integer cents | run-012e7bd1d7e0 | 1.533 | 38.8 | 30.49 |
| 2 | Parallel multi-worker CSV boundary chunking with zero-copy slice parsing and direct region array | local verified (d8088b... match) | 0.190 | 64.9 | 72.3 est |
| 3 | Zero-alloc vector deduplication with parallel country sort-and-dedup | local verified (d8088b... match) | 0.120 | 42.6 | 75.9 est |
| 4 | Scale CSV streaming parser and country dedup to 12 hardware workers | local verified (d8088b... match) | 0.080 | 48.5 | 77.5 est |
