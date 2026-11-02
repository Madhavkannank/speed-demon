# Track B: Index

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

Our baseline implementation is built in safe Rust with zero external runtime dependencies ($D = 1.0$):
1. **Corpus Ingestion & Tokenization**: Reads `/data/docs.txt` line by line, extracting tokens as maximal ASCII alphanumeric sequences (`[a-z0-9]+` lowercased).
2. **Inverted Index**: Builds an in-memory index mapping each unique term to its posting list of document IDs.
3. **Left-to-Right Boolean Evaluation**: Evaluates `BOOL` queries left-to-right (`AND`, `OR`, `NOT`) respecting the specification's lack of operator precedence.
4. **Phrase Verification**: Intersects posting lists for all phrase terms to form candidate sets, then scans token windows for exact match ordering.
5. **Exact Formatting**: Emits match count, sum of IDs, and up to 10 smallest doc IDs.

## Dependencies

None. Standard library only ($D = 1.0$).

## With another 24 hours

1. Compressed integer posting lists (Roaring / SIMD-bitsets).
2. Smallest-cardinality-first set intersections for multi-term ANDs.
3. Token-position indexing to eliminate phrase text rescans.
