# Speed Demon 2026 — Pre-Kickoff Prep & Tooling Sandbox

> **DISQUALIFICATION GUARDRAIL**:
> All code committed to the official Speed Demon submission repository prior to **October 30, 2026, 18:00 UTC** results in immediate disqualification.
> This repository is strictly for **pre-kickoff tooling, environment validation, Docker verification, and testing scripts**.
> The official submission repository will be created and committed to only after the official kickoff gun.

## Configuration & Verified Constants
- **Target CPU**: `x86-64-v3` (`RUSTFLAGS="-C target-cpu=x86-64-v3"`). Never `native`.
- **Cargo Release Profile**:
  - `codegen-units = 1`
  - `lto = "fat"`
  - `panic = "abort"`
  - `opt-level = 3`
- **Builder Image**: `rust:1-slim-trixie@sha256:1a939748a7cbab172de91a014e3804a97af1394a8c844faa67fce9401c4e40fb`
- **Runtime Image**: `debian:13-slim`
- **Memory Strategy**:
  - Phase 1 Baseline: Streaming I/O (never load full 239MB files into memory buffers).
  - Phase 2 Optimization: Zero-dependency POSIX `mmap` evaluated against baselines for Tracks A, B, and D.
- **PGO Strategy**: Optional check on Oct 28 when reference materials drop. Test whether `llvm-profdata` is available offline inside the base image; if not, skip.
