# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

Our baseline implementation is built in safe Rust with zero external runtime dependencies ($D = 1.0$):
1. **RFC 4180 Streaming State Machine**: Reads `/data/records.csv` through a state machine that robustly handles quoted commas, escaped double-quotes (`""`), embedded newlines (`\n` and `\r\n`), and chunk-boundary UTF-8 BOMs (`0xEF, 0xBB, 0xBF`).
2. **Lookup Table Ingestion**: Reads `/data/regions.csv` into an in-memory `HashMap<u32, String>` mapping `region_id` to country code.
3. **Exact Cents Parsing**: Parses `amount` strings directly to integer cents ($1420.00 \to 142000$, $-5.25 \to -525$) with zero floating-point imprecision.
4. **Relational Aggregation**: Groups by country (mapping missing IDs to `_UNMATCHED`), maintaining running counts, ID sums, min/max amounts, distinct name sets, and UTF-8 byte lengths.
5. **Exact Formatting**: Formats outputs in sorted country code order with exact 2-decimal cent strings.

## Dependencies

None. Standard library only ($D = 1.0$).

## With another 24 hours

1. Zero-allocation skipping of unneeded fields (`name` and `note` string allocations).
2. Direct linear-probe or flat array lookup for dense region IDs.
3. SIMD quote-masking for newline and delimiter detection.
