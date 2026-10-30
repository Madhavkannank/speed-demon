# Track B: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | (official baseline) | 56.35 | 2,200 | 0 |
| 1 | Safe Rust baseline (in-memory inverted index, candidate-set phrase verification, zero crates) | (local 1.47s sample vs 1.97s ref) | pending | pending | pending |
| 2 | Linear two-pointer sorted set operations, integer token interning, rarest-first phrase candidate filter | (local 0.86s sample vs 1.97s ref) | pending | pending | pending |
| 3 | Query-first vocabulary filtering, flat array inverted index, O(1) deduplication, multi-threaded execution | (local 0.64s sample vs 1.97s ref) | pending | pending | pending |
