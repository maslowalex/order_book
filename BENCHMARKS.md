# Benchmark Results

Two full runs of the Criterion suite in `benches/order_book.rs`, side by side:

- **`phase62`** — 2026-08-05, the **Tier 0 baseline** the 6.2a storage bake-off is measured
  against (saved as criterion baseline `phase62`).
- **now** — 2026-08-10, after Phase 4: Shape B `OrderType` (price + TIF inside the variant),
  IOC/FOK, `depth()`/`get_order()`, and stop orders (`StopMarket`/`StopLimit` + trigger
  cascade + the `OrderLocation` index).

The 2026-08-09 addendum (spot-check: `cancel_order/tight/10000` **+6%**) is superseded by this
full run, which measures **+7%** on that benchmark and shows where else the cost landed.

- **Machine:** Apple M3, 16 GB, macOS (Darwin 25.5.0)
- **Toolchain:** rustc 1.92.0, criterion 0.8.2, `cargo bench` (release, default opts) — same
  for both runs
- **Reproduce:** `cargo bench --bench order_book -- --baseline phase62` (compare against the
  baseline) or `cargo bench` (fresh). Filter examples: `cargo bench -- 'add_order/tight/1000$'`,
  `cargo bench -- cancel_storm`. HTML report: `target/criterion/report/index.html` (not committed).

**Reading the numbers.** Mutating benchmarks time a chunk of `K = max(10, N/10)` ops per cloned
book (see the methodology comment in `benches/order_book.rs`); "per op" below is chunk time / K,
straight from criterion's `Throughput::Elements`. Criterion reports the sampling distribution of
the **mean** — it cannot give per-op p99/p99.9 tail latency. The p50/p99/p99.9 ambition from
Phase 6.1 needs per-invocation timing (`Instant` + `hdrhistogram`), a natural add-on in Phase 6.3.
Outlier counts in criterion output are a qualitative tail smell only.

The Δ column is computed from the `phase62` figures recorded in this file, not from criterion's
own change detection: only `add_order/tight/1000` carried a `change:` line this run (+10.0%,
p = 0.00 — "Performance has regressed"); criterion's rolling `base` slot held no comparable
data for the other 35. Everything within ±3% is noise-level; read only the bolded cells as signal.

Distributions: **tight** = 40 price levels around mid 100.00 (0.25 tick; at N=100k that's ~2.5k
orders/level), **wide** = ~20k levels over 1.00–200.00 (0.01 tick; ~5 orders/level at 100k).

## Core operations

| Benchmark | phase62 | now | Δ | throughput | 95% CI (chunk) |
|---|---:|---:|---:|---:|---|
| `add_order/tight/100` | 109 ns | 117 ns | +7% | 8.55 M/s | [1.12, 1.23] µs / 10 |
| `add_order/tight/1000` | 95 ns | 104 ns | +9% | 9.64 M/s | [9.93, 10.91] µs / 100 |
| `add_order/tight/10000` | 79 ns | 86 ns | +9% | 11.63 M/s | [84.7, 87.3] µs / 1k |
| `add_order/tight/100000` | 95 ns | 97 ns | +2% | 10.27 M/s | [964, 986] µs / 10k |
| `add_order/wide/100` | 96 ns | 103 ns | +7% | 9.72 M/s | [974 ns, 1.10 µs] / 10 |
| `add_order/wide/1000` | 116 ns | 121 ns | +4% | 8.23 M/s | [11.6, 12.7] µs / 100 |
| `add_order/wide/10000` | 149 ns | 150 ns | +1% | 6.65 M/s | [149, 151] µs / 1k |
| `add_order/wide/100000` | 348 ns | 344 ns | −1% | 2.90 M/s | [3.34, 3.56] ms / 10k |
| `cancel_order/tight/100` | 109 ns | 110 ns | +1% | 9.07 M/s | [1.095, 1.108] µs / 10 |
| `cancel_order/tight/1000` | 139 ns | 145 ns | +4% | 6.91 M/s | [14.32, 14.66] µs / 100 |
| `cancel_order/tight/10000` | 512 ns | 549 ns | **+7%** | 1.82 M/s | [533, 571] µs / 1k |
| `cancel_order/tight/100000` | 4.60 µs | **5.00 µs** | **+9%** | **0.20 M/s** | [49.4, 50.7] ms / 10k |
| `cancel_order/wide/100` | 163 ns | 159 ns | −2% | 6.27 M/s | [1.577, 1.612] µs / 10 |
| `cancel_order/wide/1000` | 193 ns | 196 ns | +2% | 5.10 M/s | [19.59, 19.67] µs / 100 |
| `cancel_order/wide/10000` | 223 ns | 224 ns | 0% | 4.46 M/s | [223.5, 224.8] µs / 1k |
| `cancel_order/wide/100000` | 414 ns | 410 ns | −1% | 2.44 M/s | [3.97, 4.23] ms / 10k |

## Top-of-book reads (single op)

| Benchmark | phase62 | now | Δ |
|---|---:|---:|---:|
| `best_bid` (tight/100) | 1.33 ns | 1.35 ns | — |
| `best_bid` (tight/100k) | 1.33 ns | 1.33 ns | — |
| `best_bid` (wide/100) | 1.33 ns | 1.33 ns | — |
| `best_bid` (wide/100k) | 2.67 ns | 2.66 ns | — |
| `spread` (tight/100k) | 2.66 ns | 2.71 ns | — |
| `best_bid_level_clone` (wide/100k) | 192 ns | 230 ns | **+20%** |
| `best_bid_level_clone` (tight/100k) | 88.6 µs | **88.5 µs** | 0% |

## Matching path (`submit`)

| Benchmark | phase62 | now | Δ | throughput |
|---|---:|---:|---:|---:|
| `rest_limit/tight/10000` (non-crossing) | 297 ns | 348 ns | **+17%** | 2.88 M/s |
| `rest_limit/wide/10000` (non-crossing) | 207 ns | 214 ns | +3% | 4.68 M/s |
| `cross_limit/levels/1` (10 fills/order) | 1.63 µs | 1.65 µs | +1% | 607 K/s |
| `cross_limit/levels/5` (50 fills/order) | 7.28 µs | 7.30 µs | 0% | 137 K/s |
| `cross_limit/levels/20` (200 fills/order) | 28.3 µs | 28.2 µs | 0% | 35.5 K/s |
| `market/levels/1` | 1.62 µs | 1.66 µs | +3% | 602 K/s |
| `market/levels/20` | 28.1 µs | 28.6 µs | +2% | 35.0 K/s |

## Scenarios

| Benchmark | phase62 | now | Δ | throughput |
|---|---:|---:|---:|---:|
| `burst_1000/tight` (80% limit / 20% market into 10k book) | 350 ns | 366 ns | +5% | 2.73 M/s |
| `burst_1000/wide` | 303 ns | 303 ns | 0% | 3.30 M/s |
| `cancel_storm/tight/10000` (cancel all N) | 321 ns | 331 ns | +3% | 3.02 M/s |
| `cancel_storm/tight/100000` | 2.44 µs | 2.58 µs | +6% | 387 K/s |
| `cancel_storm/wide/10000` | 232 ns | 226 ns | −3% | 4.42 M/s |
| `cancel_storm/wide/100000` | 346 ns | 392 ns | +13%† | 2.55 M/s |

† Noisiest cell in the suite: the CI spans [36.7, 41.6] ms → 367–416 ns/op. Read it as "flat to
mildly worse", not as a measured 13%.

## Observations

1. **Distribution still flips the winner, harder than before: cancel is now 12× worse tight
   than wide at 100k** (5.00 µs vs 410 ns; was 11×). Unchanged cause —
   `PriceLevel::remove_order` (`src/types.rs:252`) linearly scans the level's `Vec` with String
   compares, then `Vec::remove` shifts the tail. Still the strongest motivation for 6.2a Tier 2
   (slab + intrusive list → O(1) cancel by handle).
2. **What Phase 4 cost, measured.** The regression is confined to the tight distribution's write
   path (add +2…9%, cancel +1…9%, burst +5%, cancel_storm +3…6%) while wide is flat to −1%
   across the board. `Order` got fatter, so every path whose cost is *moving order bytes* pays,
   and every path whose cost is *walking the tree and missing cache* does not. Tight packs ~2.5k
   orders/level → long `Vec` scans, element shifting, regrowth memcpy; wide holds ~5
   orders/level → the `BTreeMap` walk dominates and the struct's width disappears into it. The
   08-09 addendum guessed this from two benchmarks; the full suite draws the line exactly.
3. **The cleanest single exhibit is `best_bid_level_clone`.** Wide/100k **+20%** (192 → 230 ns):
   it deep-clones ~5 `Order`s, so it measures per-`Order` copy cost almost neat. Tight/100k is
   *unchanged* at 88.5 µs, because there the cost is ~2.5k orders × 2 heap String allocations,
   and a wider struct body is invisible next to that. Same operation, opposite sensitivity to
   the same change — which of the two you benchmark decides what you conclude.
4. **An enum pays for its widest variant, on every value.** `OrderType` (`src/types.rs:40`) is
   sized by `StopLimit { trigger, price, tif }` — two `Decimal`s — so every *plain limit* resting
   in the book carries stop-shaped padding it will never use. That is the running cost of making
   invalid combinations unrepresentable, and it lands on the hottest struct in the system.
   Options for 6.2a: box the stop payload, or keep a narrow resting-order record in the level and
   the rich `Order` only at the API boundary.
5. **`submit/rest_limit/tight` is the largest single regression (+17%, 297 → 348 ns)** while its
   wide twin moved +3%. Same prime suspect as the baseline's observation 5 (the first `Vec::push`
   into a just-cloned 2.5k-order level triggers regrowth) — that memcpy now moves more bytes per
   order. Secondary contributors are cheap but real: `execute` dispatches over four `OrderType`
   variants (`src/matching.rs:98`), `ExecutionReport` gained a `triggered: Vec` field, and
   `submit` runs the stop-cascade loop after every order (`src/matching.rs:86` — it early-returns
   on `last_trade_price == None`, so a rest-only order pays one `Option` check). Attribution is a
   Phase 6.3 profiling job, not a guess.
6. **Stop orders cost the non-stop path ~nothing.** The matching path is flat within noise
   (`cross_limit` −0…+1%, `market` +2–3%), even though every `submit` now runs the cascade loop
   and every `cancel_order` branches on `OrderLocation`. Adding a feature to the control flow was
   nearly free; adding bytes to the hot struct was not.
7. **Cancel-storm per-op still beats steady-state cancel at the same N** (tight/100k: 2.58 vs
   5.00 µs) because levels shrink as the storm drains the book — the linear scan gets cheaper
   with every cancellation. Amortized ≠ steady-state.
8. **`best_bid` is still a 1.3 ns non-problem; `best_bid_level` is still an 88.5 µs API bug at
   tight/100k** (~66,000× `best_bid`): it deep-clones the whole `PriceLevel`. Fix in 6.4: return
   `Option<&PriceLevel>` or a lightweight view — `depth()` (`src/orderbook.rs:169`) already exists
   and is the natural replacement for most callers.
9. **Matching still scales linearly in fills, not levels**: ~165 ns/fill at 1 level, ~141 ns/fill
   at 20 levels. Market ≈ crossing limit (the limit-price check is free); a taker eating 20 levels
   costs ~28 µs — same as at baseline.
10. **Ballpark throughput today**: ~2.7M mixed orders/sec tight, ~3.3M wide (burst scenario),
    single-threaded, with String ids and `Decimal` prices — down ~5% on tight, unchanged on wide
    since `phase62`. Still ample for the TUI capstone; the tail (cancel) remains the interesting
    work — see observation 1.

Two criterion warnings ("Unable to complete 20 samples in 2.0s") fired again on the slowest 100k
configs (`add_order/wide/100000`, `cancel_order/wide/100000`); sample counts were still ≥10, so
estimates stand. Raise that group's `measurement_time` if re-runs look noisy.
