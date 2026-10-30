# Track B: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | (official baseline) | 56.35 | 2200 | 0.00 |
| 1 | Streaming 4MB chunk indexer with query-aware vocabulary and parallel query execution | run-3124b4166760 | 5.051 | 185.0 | 31.58 |
| 2 | Parallel 6-worker document indexing, flat CSR postings, galloping search, fast byte formatting | local verified (f86973... match) | 1.490 | 245.0 | 56.5 est |
