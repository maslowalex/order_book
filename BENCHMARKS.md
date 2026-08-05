# Benchmark Results — Phase 6.2 baseline

First full run of the Criterion suite in `benches/order_book.rs`. These numbers are the
**Tier 0 baseline** the 6.2a storage bake-off will be measured against (saved as criterion
baseline `phase62`).

- **Date:** 2026-08-05
- **Machine:** Apple M3, 16 GB, macOS (Darwin 25.5.0)
- **Toolchain:** rustc 1.92.0, criterion 0.8.2, `cargo bench` (release, default opts)
- **Reproduce:** `cargo bench --bench order_book -- --baseline phase62` (compare against this run) or `cargo bench` (fresh). Filter examples: `cargo bench -- 'add_order/tight/1000$'`, `cargo bench -- cancel_storm`. HTML report: `target/criterion/report/index.html` (not committed).

**Reading the numbers.** Mutating benchmarks time a chunk of `K = max(10, N/10)` ops per cloned
book (see the methodology comment in `benches/order_book.rs`); "per op" below is chunk time / K,
straight from criterion's `Throughput::Elements`. Criterion reports the sampling distribution of
the **mean** — it cannot give per-op p99/p99.9 tail latency. The p50/p99/p99.9 ambition from
Phase 6.1 needs per-invocation timing (`Instant` + `hdrhistogram`), a natural add-on in Phase 6.3.
Outlier counts in criterion output are a qualitative tail smell only.

Distributions: **tight** = 40 price levels around mid 100.00 (0.25 tick; at N=100k that's ~2.5k
orders/level), **wide** = ~20k levels over 1.00–200.00 (0.01 tick; ~5 orders/level at 100k).

## Core operations

| Benchmark | per op (mean) | throughput | 95% CI (chunk) |
|---|---:|---:|---|
| `add_order/tight/100` | 109 ns | 9.14 M/s | [1.06, 1.13] µs / 10 |
| `add_order/tight/1000` | 95 ns | 10.53 M/s | [9.23, 9.80] µs / 100 |
| `add_order/tight/10000` | 79 ns | 12.65 M/s | [78.3, 79.8] µs / 1k |
| `add_order/tight/100000` | 95 ns | 10.48 M/s | [936, 976] µs / 10k |
| `add_order/wide/100` | 96 ns | 10.37 M/s | [945, 989] ns / 10 |
| `add_order/wide/1000` | 116 ns | 8.59 M/s | [11.4, 12.0] µs / 100 |
| `add_order/wide/10000` | 149 ns | 6.70 M/s | [148, 150] µs / 1k |
| `add_order/wide/100000` | 348 ns | 2.87 M/s | [3.43, 3.53] ms / 10k |
| `cancel_order/tight/100` | 109 ns | 9.18 M/s | [1.081, 1.099] µs / 10 |
| `cancel_order/tight/1000` | 139 ns | 7.19 M/s | [13.86, 13.97] µs / 100 |
| `cancel_order/tight/10000` | 512 ns | 1.95 M/s | [510, 514] µs / 1k |
| `cancel_order/tight/100000` | **4.60 µs** | **0.22 M/s** | [45.1, 47.1] ms / 10k |
| `cancel_order/wide/100` | 163 ns | 6.15 M/s | [1.61, 1.64] µs / 10 |
| `cancel_order/wide/1000` | 193 ns | 5.18 M/s | [19.26, 19.34] µs / 100 |
| `cancel_order/wide/10000` | 223 ns | 4.49 M/s | [222, 223] µs / 1k |
| `cancel_order/wide/100000` | 414 ns | 2.42 M/s | [4.00, 4.28] ms / 10k |

## Top-of-book reads (single op)

| Benchmark | time |
|---|---:|
| `best_bid` (tight, any N; wide/100) | 1.33 ns |
| `best_bid` (wide/100k) | 2.67 ns |
| `spread` (tight/100k) | 2.66 ns |
| `best_bid_level_clone` (wide/100k) | 192 ns |
| `best_bid_level_clone` (tight/100k) | **88.6 µs** |

## Matching path (`submit`)

| Benchmark | per op (mean) | throughput |
|---|---:|---:|
| `rest_limit/tight/10000` (non-crossing) | 297 ns | 3.37 M/s |
| `rest_limit/wide/10000` (non-crossing) | 207 ns | 4.84 M/s |
| `cross_limit/levels/1` (10 fills/order) | 1.63 µs | 615 K/s |
| `cross_limit/levels/5` (50 fills/order) | 7.28 µs | 137 K/s |
| `cross_limit/levels/20` (200 fills/order) | 28.3 µs | 35 K/s |
| `market/levels/1` | 1.62 µs | 616 K/s |
| `market/levels/20` | 28.1 µs | 36 K/s |

## Scenarios

| Benchmark | per op (mean) | throughput |
|---|---:|---:|
| `burst_1000/tight` (80% limit / 20% market into 10k book) | 350 ns | 2.86 M/s |
| `burst_1000/wide` | 303 ns | 3.30 M/s |
| `cancel_storm/tight/10000` (cancel all N) | 321 ns | 3.12 M/s |
| `cancel_storm/tight/100000` | 2.44 µs | 410 K/s |
| `cancel_storm/wide/10000` | 232 ns | 4.31 M/s |
| `cancel_storm/wide/100000` | 346 ns | 2.89 M/s |

## Observations

1. **Distribution flips the winner — cancel is 11× worse tight than wide at 100k** (4.60 µs vs
   414 ns). `PriceLevel::remove_order` linearly scans the level's `Vec` with String compares;
   tight packs ~2.5k orders per level, wide ~5. This is the single strongest motivation for
   6.2a Tier 2 (slab + intrusive list → O(1) cancel by handle) and it showed up before any
   bake-off machinery exists.
2. **`add_order` flips the other way.** Tight stays flat at ~80–110 ns from N=100 to 100k
   (40 hot `BTreeMap` nodes stay cache-resident; `Vec::push` is amortized O(1)). Wide degrades
   96 → 348 ns as the tree grows to ~20k nodes — O(log n) is visible, but as cache misses, not
   comparisons. Big-O is not wall-clock, in both directions at once.
3. **Cancel-storm per-op beats steady-state cancel at the same N** (tight/100k: 2.44 µs vs
   4.60 µs) because levels shrink as the storm drains the book — the linear scan gets cheaper
   with every cancellation. Amortized ≠ steady-state.
4. **`best_bid` is a 1.3 ns non-problem. `best_bid_level` is a 88.6 µs API bug at tight/100k**
   (~66,000× `best_bid`): it deep-clones the whole `PriceLevel` — ~2.5k `Order`s, two heap
   Strings each. Fix in 6.4: return `Option<&PriceLevel>` (or a lightweight depth view).
5. **`submit` costs ~1.4–3.8× `add_order` for the same rest-only outcome** (tight: 297 vs
   79 ns; wide: 207 vs 149 ns). Known contributors: the `ExchangeId::from_sequence` `format!`
   allocation per order, the crossing check, and `ExecutionReport` construction. Why the
   overhead is *larger* on the tight book is not obvious (suspect: first `Vec::push` into a
   just-cloned 2.5k-order level triggers regrowth) — a question for Phase 6.3 profiling.
6. **Matching scales linearly in fills, not levels**: ~163 ns/fill at 1 level, ~141 ns/fill at
   20 levels — level-boundary overhead is small next to per-fill work (`Trade` with 2 String
   clones + FIFO `Vec::remove(0)` front-shifts). Market ≈ crossing limit (the limit-price check
   is free); a taker eating 20 levels costs ~28 µs.
7. **Ballpark throughput today**: ~3M mixed orders/sec (burst scenario), single-threaded, with
   String ids and `Decimal` prices. Plenty for the TUI capstone; the interesting work is the
   tail (cancel) — see observation 1.

Two criterion warnings ("Unable to complete 20 samples in 2.0s") fired on the slowest 100k
configs; sample counts were still ≥10, so estimates stand. Raise that group's
`measurement_time` if re-runs look noisy.
