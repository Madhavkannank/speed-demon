# SPEED DEMON 2026 — HARDWARE & RUNTIME FLOORS

Measured on local test host (comparable x86-64 Linux environment):

| Benchmark Item | Measurement | Notes / Implications |
|---|---|---|
| **Empty Rust binary startup & exit** | < 1 ms (0.00s), RSS ~2.0 MB | Process startup floor is negligible. Idle RSS is ~2.0 MB. |
| **`std::thread::scope` spawn (6 threads)** | ~350 - 440 µs | Fast enough to spawn dynamically, but single pool per track saves 0.4 ms. |
| **`std::thread::scope` spawn (8 threads)** | ~160 - 180 µs | |
| **`std::thread::scope` spawn (12 threads)** | ~210 - 250 µs | Single thread pool with `Barrier` amortizes this to zero across passes. |
| **Writing 10 MB file to disk + sync** | Sequential: 2.9 - 3.6 ms<br>Parallel pwrite (12 threads): 5.0 ms | Sequential `write_all` is faster than parallel `pwrite` due to filesystem lock contention on sequential offsets. |
| **Parallel RAM scan (240 MB, 12 threads)** | ~9.0 ms (25.9 GB/s) | Memory bandwidth limit for reading 240 MB across 12 threads is ~9 ms. Any algorithm scanning 240 MB can never be faster than ~9 ms. |
| **Parallel `pread` (243 MB, 12 threads, 1MB buffers)** | ~18.0 ms (13.1 GB/s) | Synchronous pread syscalls add syscall & buffer copy overhead. |
| **mmap 243 MB plain fault touch (1 thread)** | 11.5 ms | Single core page faulting. |
| **mmap 243 MB plain fault touch (6 threads)** | 4.3 ms | 6 threads touching pages concurrently. |
| **mmap 243 MB plain fault touch (12 threads)** | 5.5 ms | Core contention on page table lock. |
| **mmap 243 MB MAP_POPULATE** | 8.8 ms | Kernel synchronous faulting in single thread. |
| **mmap 243 MB parallel `MADV_POPULATE_READ` (12 threads)** | **1.25 ms** | Super fast asynchronous page population across 12 threads! |

## Memory (RSS / cgroup) Floors
- Base idle binary RSS: **2.0 MB**
- Track A output buffer (10.1 MB): ~10 MB dirty page cache
- Track B output buffer (1.46 MB): ~1.5 MB dirty page cache
- Track C output buffer (17 KB): < 0.1 MB

## Track B Phase Timing Breakdown (Seed 1 Full: 254 MB docs, 50,000 queries)
Measured with 12 hardware threads on `taskset -c 0-11`:

| Phase | Time (ms) | % Total | Bottleneck / Notes |
|---|---|---|---|
| **1. Read queries** | **0.75 ms** | 0.1% | `fs::read` from page cache is essentially instant |
| **2. Build term table** | **46.54 ms** | 4.8% | Open addressing hash table + 256 KB Bloom filter for 75,746 query terms |
| **3. Scan (`docs.txt`)** | **294.68 ms** | 30.1% | 12 threads scanning 254 MB via `pread` into 512 KB chunk buffers |
| **4. Merge / Scatter** | **93.44 ms** | 9.5% | Writing postings into global arena |
| **5. Query: BOOL (31k)** | **42.23 ms/thread** | 4.3% | (506.77 ms CPU aggregate) Boolean intersection & unions with galloping search |
| **6. Query: PHRASE (19k)**| **436.46 ms/thread**| **44.6%**| **CRITICAL BOTTLENECK**: 214,226 synchronous `pread` syscalls verifying phrase candidates straight from disk (430 ms CPU time in kernel) |
| **7. Format + Write** | **0.18 ms** | <0.1% | Fast write buffer flush to disk |
| **TOTAL** | **~750 - 978 ms** | 100% | Eliminating 214k `pread`s via `mmap` candidate verification (Rule R5) will cut ~350-400 ms immediately |

