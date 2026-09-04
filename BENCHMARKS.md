# Benchmark Results

Three full runs of the Criterion suite in `benches/order_book.rs`. The first two are compared
side by side below; the third — the tick/lot migration — is its own A/B at the end of this file,
because it was measured against its own immediate parent rather than against `phase62`.

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
   **Partly superseded** — the 2026-08-14 run shrank `Order` by 16 bytes and this benchmark did
   not improve, so the +20% is probably not per-`Order` copy cost after all. See observation 6
   of the tick/lot section.
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

---

# Tick/lot migration (2026-08-14)

`Price` stopped being an alias for `rust_decimal::Decimal` and became a `u64` count of the
instrument's minor units; quantity became `Qty(u64)`; the book gained an `InstrumentSpec` and
admission checks on `submit`/`add_order`.

- **`pre-ticks`** — criterion baseline saved at `d39cac3` (`Price = Decimal`, `instrument`
  module present but unused by the engine). Its absolute numbers land within run-to-run
  variance of the "now" column above, so it stands in for the post-Phase-4 state.
- **`post`** — the migration, *including* the new ingress admission checks. Every figure below
  is therefore **net of work that was added**, not just work that was removed.

Same machine and toolchain as the runs above. Reproduce with
`cargo bench --bench order_book -- --baseline pre-ticks`. Note the `--bench order_book`: plain
`cargo bench` also runs the lib's default test harness, which rejects criterion's flags.

**Predictions were written down before the comparison run** (`scratchpad/predictions.md`), per
the Phase 6.2 methodology note. They are scored in observation 5.

## Core operations (per op)

| Benchmark | pre-ticks | post | Δ |
|---|---:|---:|---:|
| `add_order/tight/100` | 106 ns | 85 ns | **−18%** |
| `add_order/tight/1000` | 94 ns | 76 ns | **−19%** |
| `add_order/tight/10000` | 84 ns | 68 ns | **−20%** |
| `add_order/tight/100000` | 99 ns | 85 ns | **−14%** |
| `add_order/wide/100` | 94 ns | 71 ns | **−23%** |
| `add_order/wide/1000` | 114 ns | 78 ns | **−32%** |
| `add_order/wide/10000` | 152 ns | 97 ns | **−37%** |
| `add_order/wide/100000` | 366 ns | 263 ns | **−26%** |
| `cancel_order/tight/100` | 109 ns | 91 ns | **−18%** |
| `cancel_order/tight/1000` | 143 ns | 127 ns | **−11%** |
| `cancel_order/tight/10000` | 541 ns | 494 ns | **−9%** |
| `cancel_order/tight/100000` | 4.76 µs | 4.51 µs | **−5%** |
| `cancel_order/wide/100` | 163 ns | 119 ns | **−27%** |
| `cancel_order/wide/1000` | 194 ns | 120 ns | **−37%** |
| `cancel_order/wide/10000` | 224 ns | 157 ns | **−31%** |
| `cancel_order/wide/100000` | 401 ns | 370 ns | **−11%** |

## Top-of-book reads (single op)

| Benchmark | pre-ticks | post | Δ |
|---|---:|---:|---:|
| `best_bid` (tight/100) | 1.3268 ns | 1.3310 ns | — |
| `best_bid` (tight/100k) | 1.3304 ns | 1.3332 ns | — |
| `best_bid` (wide/100) | 1.3274 ns | 1.3295 ns | — |
| `best_bid` (wide/100k) | 2.6549 ns | 2.6591 ns | — |
| `spread` (tight/100k) | 2.6608 ns | 1.8582 ns | **−30%** |
| `best_bid_level_clone` (tight/100k) | 89.6 µs | 87.3 µs | −3% |
| `best_bid_level_clone` (wide/100k) | 190 ns | 193 ns | +3% |

## Matching path (`submit`, per op)

| Benchmark | pre-ticks | post | Δ |
|---|---:|---:|---:|
| `rest_limit/tight/10000` | 344 ns | 296 ns | **−15%** |
| `rest_limit/wide/10000` | 217 ns | 177 ns | **−18%** |
| `cross_limit/levels/1` | 1.66 µs | 1.63 µs | −1% |
| `cross_limit/levels/5` | 7.44 µs | 7.27 µs | −3% |
| `cross_limit/levels/20` | 28.59 µs | 27.76 µs | −3% |
| `market/levels/1` | 1.66 µs | 1.63 µs | −1% |
| `market/levels/20` | 28.30 µs | 27.54 µs | −2% |

## Scenarios (per op)

| Benchmark | pre-ticks | post | Δ |
|---|---:|---:|---:|
| `burst_1000/tight` | 373 ns | 346 ns | **−7%** |
| `burst_1000/wide` | 311 ns | 285 ns | **−8%** |
| `cancel_storm/tight/10000` | 333 ns | 308 ns | **−8%** |
| `cancel_storm/tight/100000` | 2.60 µs | 2.36 µs | **−9%** |
| `cancel_storm/wide/10000` | 226 ns | 169 ns | **−25%** |
| `cancel_storm/wide/100000` | 365 ns | 268 ns | **−27%** |

## Observations

1. **Two changes landed together and cannot be separated by this measurement.** `Decimal`'s
   `Ord` goes through `cmp_impl`, which aligns two scales before it can answer; `u64`'s is one
   instruction, and every `BTreeMap` probe pays it at every level of the descent. But `Order`
   also shrank — measured, not assumed: `OrderType` **36 → 24 bytes**, `Order` **128 → 112**,
   because `StopLimit { trigger, price, tif }` stopped carrying two 16-byte `Decimal`s. Both
   effects push the same direction on the same benchmarks. Splitting them needs an artificial
   intermediate commit; nothing here should be attributed wholly to comparison cost.

2. **The distribution split predicted by "comparison-bound vs scan-bound" held, and it is the
   strongest result in the run.** `add_order` improves 14–20% tight but 23–37% wide;
   `cancel_order` 5–18% tight but 11–37% wide; `cancel_storm` 8–9% tight but 25–27% wide. Wide
   at N=100k spreads orders over ~20k levels, so the tree is deep and the work really is key
   comparison. Tight packs ~2.5k orders into ~40 levels, so time goes into the `Vec` scan inside
   a level — which no key type can help, and which is still the 6.2a Tier 2 motivation.

3. **`spread()` got 30% faster while doing strictly more work.** It went from one `Decimal`
   subtraction to an integer `abs_diff` **plus a division** (it now returns `Ticks`, so it
   converts a currency difference into a tick count). Finishing ahead anyway is the cleanest
   single illustration of what `Decimal` arithmetic was costing.

4. **The matching path is flat, again.** `cross_limit` and `market` move −1…−3%, which is where
   they sat when stops were added too. The fill loop's cost is `Vec` manipulation, `String`
   clones for trade ids, and quantity arithmetic — the price key never enters it. Two
   consecutive changes to the price representation have now both failed to move this group,
   which is a fact about where the time is, not about either change.

5. **Scoring the predictions.** Four of five held; one failed outright.
   - *Held:* wide beats tight on `add_order`/`cancel_order` (the headline call, and the stated
     falsification test — if wide had not improved, the whole `Decimal`-cost premise was wrong).
   - *Held:* `best_bid` unchanged at 1.33 ns; the fill loop flat; `cancel_storm/tight` barely
     moving relative to wide.
   - *Too pessimistic:* `submit/rest_limit` was predicted to be "a small win, possibly a wash on
     tight" because of the new admission checks. It came in at −15%/−18%, the same order as
     `add_order`. The per-order bounds check and the one `u128` multiply for min-notional are
     not measurable next to what the key change bought.
   - **Failed: `best_bid_level_clone` was predicted to be the cleanest exhibit of the `Order`
     shrink, and it is not.** Tight/100k moved −3%, wide/100k **+3%** — the wrong direction,
     against a struct that verifiably lost 16 bytes per element.

6. **Why that failure matters: observation 3 of the Phase 4 run needs revising.** That
   observation read `best_bid_level_clone/wide/100k` **+20%** as "per-`Order` copy cost almost
   neat" because the level holds only ~5 orders. Running the same benchmark against a 16-byte
   *reduction* produced no gain at all. Cloning 5 `Order`s means **10 heap allocations** —
   `ExchangeId` and `ClientId` are both `String` — and at 190 ns those allocations dominate a
   16-byte-per-element copy difference completely. The earlier +20% is more likely attributable
   to something else in Phase 4, or to noise at this scale, than to struct width. The lesson the
   old observation drew (same operation, opposite sensitivity depending on N) survives; the
   specific attribution does not. Deciding it properly is a Phase 6.3 profiling job.

7. **The `String` ids are now the visible ceiling.** They were always the plan's known cost, but
   with `Decimal` gone they are what is left holding up `best_bid_level_clone` (observation 6),
   `PriceLevel::remove_order`'s scan (String compares, `src/types.rs`), and the trade-id clones
   in the fill loop. 6.2a Tier 2's `u32` handles address all three at once.

8. **Ballpark throughput today**: ~2.9M mixed orders/sec tight, ~3.5M wide (burst scenario),
   single-threaded — up from ~2.7M/~3.3M, and now above the `phase62` figures the Phase 4 work
   had dipped below. Cancel at tight/100k remains the tail problem at 4.51 µs; it improved 5%,
   which is the smallest gain in the whole table and exactly where the linear scan lives.

9. **What this did not buy.** The migration's case was never only speed: `100.0` and `100.00`
   were `Ord`-equal but distinct `Decimal`s, so they collided onto one level while the level kept
   whichever scale created it first — making the scale a trade printed at depend on arrival
   order. That bug is gone by construction, as is any off-tick price or off-lot quantity. Had the
   wide-distribution numbers come back flat, that would still have been the result worth keeping.

---

# Phase 5.3 — allocation through the trait (2026-08-14)

`fill_against` stopped running a hardcoded FIFO loop and started calling
`MatchingAlgorithm::allocate` on every price level it touches. `OrderBook` became
`OrderBook<M: MatchingAlgorithm>`, `Order` gained an engine-stamped `arrival`, and self-trade
prevention became a pre-pass over the whole level.

- **`post-tick`** — the `post` column from the tick/lot section above. It is the real parent of
  this work, and the comparison that means anything. It was never saved as a criterion baseline,
  so the Δ column is computed against the figures recorded in this file.
- **`phase53`** — this work, saved as criterion baseline `phase53`. **Use this, not `phase62`,
  as the Tier 0 number for 6.2a**: `phase62` predates the tick/lot migration and conflates two
  changes.

Same machine and toolchain as the runs above. Reproduce with
`cargo bench --bench order_book -- --baseline phase53`.

**Two rows are rebased and are NOT comparable to any earlier column.** `best_bid` and `spread`
now `black_box` the *book*, not just the result — see observation 4. Their earlier figures were
measuring a hoisted constant.

**Thermal caveat, and it is not small.** These numbers were taken after roughly an hour of
back-to-back benching on a fanless M3. The `scenarios` group (`sample_size(10)`) drifted 4–10%
between consecutive identical runs, and `cancel_storm/wide/100000` carried a ±18% confidence
interval. Scenario figures below are the median of repeated runs, and any scenario Δ under ~15%
should be read as noise. The core and submit groups were stable to ~2%.

## Core operations (per op)

| Benchmark | post-tick | phase53 | Δ |
|---|---:|---:|---:|
| `add_order/tight/100` | 85 ns | 101 ns | +19% |
| `add_order/tight/1000` | 76 ns | 75.2 ns | −1% |
| `add_order/tight/10000` | 68 ns | 67.5 ns | −1% |
| `add_order/tight/100000` | 85 ns | 83.0 ns | −2% |
| `add_order/wide/100` | 71 ns | 74.1 ns | +4% |
| `add_order/wide/1000` | 78 ns | 79.8 ns | +2% |
| `add_order/wide/10000` | 97 ns | 99.4 ns | +2% |
| `add_order/wide/100000` | 263 ns | 297 ns | +13% |
| `cancel_order/tight/100` | 91 ns | 90.9 ns | 0% |
| `cancel_order/tight/1000` | 127 ns | 127 ns | 0% |
| `cancel_order/tight/10000` | 494 ns | 501 ns | +1% |
| `cancel_order/tight/100000` | 4.51 µs | 4.49 µs | 0% |
| `cancel_order/wide/100` | 119 ns | 117 ns | −2% |
| `cancel_order/wide/1000` | 120 ns | 128 ns | +6% |
| `cancel_order/wide/10000` | 157 ns | 171 ns | +9% |
| `cancel_order/wide/100000` | 370 ns | 344 ns | −7% |

## Top-of-book reads (single op)

| Benchmark | post-tick | phase53 | Δ |
|---|---:|---:|---:|
| `best_bid` (tight/100) | 1.331 ns | 0.820 ns | *rebased* |
| `best_bid` (tight/100k) | 1.333 ns | 0.823 ns | *rebased* |
| `best_bid` (wide/100) | 1.329 ns | 0.827 ns | *rebased* |
| `best_bid` (wide/100k) | 2.659 ns | 2.145 ns | *rebased* |
| `spread` (tight/100k) | 1.858 ns | 2.159 ns | *rebased* |
| `best_bid_level_clone` (tight/100k) | 87.3 µs | 92.7 µs | +6% |
| `best_bid_level_clone` (wide/100k) | 193 ns | 199 ns | +3% |

## Matching path (`submit`, per op)

| Benchmark | post-tick | phase53 | Δ |
|---|---:|---:|---:|
| `rest_limit/tight/10000` | 296 ns | 295 ns | 0% |
| `rest_limit/wide/10000` | 177 ns | 189 ns | +7% |
| `cross_limit/levels/1` | 1.63 µs | 1.78 µs | +9% |
| `cross_limit/levels/5` | 7.27 µs | 7.48 µs | +3% |
| `cross_limit/levels/20` | 27.76 µs | 29.96 µs | +8% |
| `market/levels/1` | 1.63 µs | 1.73 µs | +6% |
| `market/levels/20` | 27.54 µs | 30.05 µs | +9% |

## Scenarios (per op, median of repeated runs)

| Benchmark | post-tick | phase53 | Δ |
|---|---:|---:|---:|
| `burst_1000/tight` | 346 ns | **440 ns** | **+27%** |
| `burst_1000/wide` | 285 ns | 337 ns | +18% |
| `cancel_storm/tight/10000` | 308 ns | 316 ns | +3% |
| `cancel_storm/tight/100000` | 2.36 µs | 2.55 µs | +8% |
| `cancel_storm/wide/10000` | 169 ns | 184 ns | +9% |
| `cancel_storm/wide/100000` | 268 ns | 357 ns | +33% (±18% CI — unreliable) |

## Allocation policy (new group, `matcher/*`)

The policy axis with the storage layout held fixed: same ladder, same takers, three matchers.

| Benchmark | FIFO | pro-rata | time-pro-rata |
|---|---:|---:|---:|
| `matcher/levels/1` (one partial level) | 962 ns | 1.459 µs **+52%** | 1.524 µs **+58%** |
| `matcher/levels/20` (19 full + 1 partial) | 28.35 µs | 29.35 µs +4% | 29.33 µs +3% |

## Observations

1. **The headline is a layout accident, and it cost 14% of `Order` for one field.** `arrival`
   was written as `u64` first, and `Order` went **112 → 128 bytes**. Not the 8 bytes the field
   holds: `timestamp` is a `u128`, so `Order` aligns to 16, and 106 bytes of content had exactly
   6 bytes of tail padding — enough for a `u32`, not enough for a `u64`, so the field pushed the
   struct over a boundary and took a whole 16-byte step. Measured, both ways, with
   `size_of::<Order>()`.

   The `u64` version cost **+24% on `add_order/tight/10000`, +16% on `cancel_order/tight/100000`,
   +27% on `rest_limit/tight`**. Narrowing to `u32` — free, it lands in the padding — brought all
   three back to **−1%, 0%, 0%**. Every one of those benchmarks is bound by walking or memmoving
   a level's `Vec`, so they are counting bytes per element and nothing else, which is why the
   tight distribution (deep levels) took it all and wide barely noticed.

   The price is a ~4.29-billion-rest ceiling per book, enforced with `checked_add` rather than
   left to wrap — a wrapped counter would make the newest order at a level read as the oldest and
   silently invert time priority, which is the one thing the field exists to establish.

2. **The abstraction has a real cost, and it is the projection, not the dispatch.** `allocate`
   takes `&[Maker]`, so `fill_against` must materialise the *whole* level before it can allocate
   anything — `makers.extend(level.makers())` is O(level) no matter how little the taker
   consumes. The old FIFO loop was O(makers actually touched).

   The distribution split is the evidence. `burst_1000` is **+27% tight, +18% wide**, and tight
   at N=10k packs 10,000 orders into ~40 levels — **~250 makers per level**, against a taker
   drawing qty 1..=50 that needs one or two of them. Wide spreads the same orders over ~20k
   levels, so there is almost nothing to project and the regression is proportionally smaller.
   The `submit` group agrees more mildly (+3…+9%) because its ladder holds only 10 makers a level.

   This is not a bug in the implementation; it is what `allocate(&[Maker])` *means*. Pro-rata
   genuinely needs the level total before it can apportion. FIFO does not, and pays anyway.
   Per the project's own rule the fix waits for a flamegraph (6.3), but the shape is already
   visible: either a fast path for policies that don't need the whole level, or a projection the
   matcher pulls lazily. Worth deciding *before* 6.2a swaps the storage under it.

3. **Static dispatch cost nothing measurable, as predicted.** `allocate` is called once per price
   level and does O(level) work inside, so even a vtable would have amortised away — which is why
   5.2's dispatch question was never a performance question. The `matcher/*` group confirms the
   flip side: with dispatch free, the +52% at `levels/1` is *all* policy — pro-rata touching all
   ten makers on a partial level where FIFO touches five, at two `String` clones per `Trade`.
   At `levels/20` that shrinks to +3…4%, because nineteen of the twenty levels are consumed whole
   and every matcher takes its `available >= total` early return on those.

4. **A benchmark was measuring a hoisted constant, and the generic change exposed it.**
   `best_bid` reads a book that never changes with `black_box` on the *result* only, leaving the
   whole call loop-invariant. It reported **0.55 ns** — under 1.5 cycles, less than a `BTreeMap`
   first-key descent can possibly cost. Fencing the book with `black_box(&book)` gives **0.82 ns**.

   The weakness dates to the Phase 6.2 harness; making `OrderBook` generic just gave the
   optimizer enough to act on it. Two consequences worth carrying forward: the `best_bid`/`spread`
   rows are rebased and not comparable to `phase62` or `pre-ticks`, and **6.2a must audit every
   read-only benchmark the same way** before it starts comparing storage layouts, because that is
   precisely a group of benchmarks where the compiler can delete the work being measured.

5. **The first version of the `matcher/*` group measured nothing, and the reason generalises.**
   It reused `crossing_takers`, which sizes each taker to consume an exact number of whole levels
   — so at every level `available >= total`, every weighted matcher took its take-everything
   early return, and all three "policies" allocated byte-for-byte identically. The group showed
   FIFO 1.722 µs vs pro-rata 1.772 µs and looked like a legitimate 3% result.

   `partial_takers` (demand `levels_each · depth − depth/2`, so the last level is always partial)
   turned that into the +52% above. The lesson for 6.2a: a benchmark can exercise the code path
   under test and still route it entirely through a degenerate branch. Check that the thing you
   are comparing is actually doing different work before believing a small delta.

6. **`market/levels/20` printed +113% once and it was a phantom.** Re-measured immediately, the
   same benchmark came back at 30.05 µs (+9%), in line with `cross_limit/levels/20`'s +8%;
   criterion's own change detection called the stored value 48.9% worse than reality. Recorded
   here because an unreproduced 2× on a tracked artifact is how a phantom becomes folklore — and
   because it is the concrete argument for the thermal caveat at the top of this section.

7. **What this bought.** The book can be told how to allocate: `OrderBook::new(spec,
   ProRataMatcher::new(lot))` and the whole engine changes policy, with the same nine property
   invariants holding against all three matchers (plus a differential property asserting the
   three agree on every total). The measured price is +3…9% on the match path and +27% on the
   deep-level burst, all of it in observation 2's projection, all of it recoverable behind the
   same seam that makes 6.2a possible.

---

# Phase 6.2a — storage seam and dense tick ladder (2026-08-27)

The active book is now `OrderBook<M, S = BTreeStore>`, with the existing BTree maps and a
`TickLadderStore` implementing the same `OrderBookStore` trait. The storage group holds FIFO,
orders, random seed, finite instrument band, and timed operation constant. Tight spans 41 legal
ticks per side; wide spans 19,901. Setup and cloning remain outside the timed routine, and
read-only calls fence the book itself.

## Direct comparison

Medians from `cargo bench --bench order_book -- storage`:

| workload | BTreeStore | TickLadderStore | ladder Δ |
|---|---:|---:|---:|
| add / tight / 100 | 86.7 ns/op | 85.6 ns/op | −1% |
| add / tight / 10k | 67.9 ns/op | 64.3 ns/op | −5% |
| add / tight / 100k | 90.3 ns/op | 81.2 ns/op | −10% |
| cancel / tight / 100k | 4.66 µs/op | 4.65 µs/op | flat |
| burst 1000 / tight | 436 µs | 431 µs | −1% |
| add / wide / 100k | 310 ns/op | 259 ns/op | −17% |
| cancel / wide / 100k | 399 ns/op | 305 ns/op | −24% |
| best bid / wide / 100k | 1.60 ns | 0.85 ns | −47% |
| burst 1000 / wide | 339 µs | 313 µs | −8% |
| cross 20 levels × 50 | 1.45 ms | 1.42 ms | −2% (noise) |

Chunk rows above are divided by Criterion's `Throughput::Elements`; the displayed group time is
the whole chunk. At small N the cached ladder touch is slower than BTree's already-hot first node
(about 0.84 ns vs 0.72 ns on tight), so “O(1)” is not shorthand for “always faster.”

## The benchmark caught a broken first attempt

The initial ladder repaired a removed best by scanning from the array boundary. On the wide
burst it took **1.84 ms against BTree's 346 µs** — 5.3× slower — and a sequential ask sweep
revisited a growing empty prefix after every level. Repair now searches only beyond the removed
touch. The wide burst dropped to **313 µs** and the 20-level crossing workload to **1.42 ms**.
This is the distribution lesson in executable form: price-as-index is cheap; repeatedly walking
empty price space is not.

## BTree seam regression against `phase53`

Representative unbounded BTree reruns used the unchanged legacy benchmark names and
`--baseline phase53`. Mixed bursts remained within noise and the 20-level sweep improved about
2%. After explicit inlining, `best_bid` improved 17% tight / 26% wide. Cancel at 100k was +2.7%
tight and statistically unchanged wide. `add_order/tight/100k` retained a measured ~7%
regression, while wide was noisy across runs; this is the known price of keeping rollback-safe
fallible store insertion behind the public trait rather than assuming every backend accepts an
order after the id index has been mutated.

## Memory conclusion is deliberately limited

The ladder reserves 2 × 41 slots for tight and 2 × 19,901 for wide — **485× more slots** for the
wide distribution before order queues are counted. This run measured CPU latency, not resident
heap size, so it does not claim the ladder is the overall winner. Phase 6.3 memory profiling is
the gate for that conclusion.

---

# Full-suite refresh — BTree, HashMap, and tick ladder (2026-09-03)

This is a fresh `cargo bench --bench order_book` run after adding `HashMapStore` coverage. It is
an absolute snapshot, not a Criterion baseline comparison. Figures below use Criterion medians;
mutating rows are per operation (the timed chunk is divided by its declared
`Throughput::Elements`). The storage comparisons use the same bounded instrument band and seeded
workload for every backend. `Gnuplot` was absent, so Criterion used its Plotters backend; that
changes report rendering, not the measurements.

## Core path

| Workload | tight / 100 | tight / 10k | tight / 100k | wide / 100 | wide / 10k | wide / 100k |
|---|---:|---:|---:|---:|---:|---:|
| add | 83.5 ns | 66.2 ns | 79.9 ns | 75.0 ns | 95.0 ns | 258 ns |
| cancel | 97.8 ns | 500 ns | 4.37 µs | 130 ns | 154 ns | 320 ns |

The distribution remains the dominant factor for cancellation: at 100k, a tight book costs
**4.37 µs/op** versus **320 ns/op** wide (13.7×). The tight book packs many orders into each
level, so the linear `Vec` scan/removal remains the limiting work. Add is comparatively stable
through 10k, while wide/100k rises to 258 ns/op as the sparse price map grows.

## Reads, matching, and scenarios

| Workload | Result |
|---|---:|
| best bid, tight / 100 and 100k | 0.732 ns and 0.734 ns |
| best bid, wide / 100 and 100k | 0.733 ns and 1.593 ns |
| spread, tight / 100k | 1.378 ns |
| best-bid level clone, tight / wide 100k | 87.7 µs / 186 ns |
| rest limit, tight / wide 10k | 291 ns / 169 ns per order |
| cross limit, 1 / 5 / 20 levels | 1.679 / 7.291 / 27.95 µs per order |
| market, 1 / 20 levels | 1.646 / 27.99 µs per order |
| burst 1000, tight / wide | 436 / 332 ns per order |
| cancel storm, tight 10k / 100k | 309 ns / 2.329 µs per cancel |
| cancel storm, wide 10k / 100k | 155 ns / 259 ns per cancel |

Wide mixed bursts are **31% faster** than tight (3.015 vs 2.296 M orders/s). In contrast, a
tight 100k cancel storm is ~9.0× slower than its wide equivalent; it still beats isolated tight
100k cancels because each successful cancel shortens the levels that remain. Matching is nearly
linear in fills: 1.679 µs for the one-level crossing case and 27.95 µs for 20 levels. Market and
crossing-limit orders are effectively tied at 20 levels (27.99 vs 27.95 µs).

## Storage backend comparison

All figures are medians. BTree remains the production default; the table shows the trade-offs
under the finite-band benchmark, not a memory verdict.

| Workload | BTreeStore | HashMapStore | TickLadderStore | Fastest |
|---|---:|---:|---:|---|
| add, tight / 100k | 83.2 ns | 81.9 ns | 78.9 ns | ladder |
| cancel, tight / 100k | 4.308 µs | 4.312 µs | 4.329 µs | BTree (tie) |
| best bid, tight / 100k | 0.716 ns | 6.432 ns | 0.861 ns | BTree |
| burst 1000, tight | 433 ns | 442 ns | 421 ns | ladder |
| add, wide / 100k | 264 ns | 245 ns | 227 ns | ladder |
| cancel, wide / 100k | 290 ns | 329 ns | 222 ns | ladder |
| best bid, wide / 100k | 1.594 ns | 6.433 ns | 0.832 ns | ladder |
| burst 1000, wide | 328 ns | 1.844 µs | 308 ns | ladder |
| cross limit, 20 levels | 1.379 ms | 1.831 ms | 1.376 ms | ladder (near tie) |

The dense tick ladder is the clear wide-book winner: versus BTree at 100k it improves add by
**14%**, cancel by **24%**, best-bid by **48%**, and burst throughput by **6%**. It also leads
tight add/burst modestly, but tight cancellation is unchanged because order removal within a
level, not price lookup, dominates. Its cross-limit result is indistinguishable from BTree
(1.376 vs 1.379 ms).

`HashMapStore` makes a different trade: it can insert wide orders 7% faster than BTree, but has
no ordered-key first-price operation. Its `best_bid` is ~4× slower on tight books and ~4× slower
on wide books; the penalty compounds into a **5.6×** slower wide burst and **33%** slower
20-level crossing workload. It is therefore not a suitable general order-book backend without a
separate ordered-price index.

## Reliability notes

Criterion could not fit the requested samples into two seconds for the 100k wide add/cancel
groups (and their storage equivalents). It still completed the configured samples, but a future
run should increase `measurement_time`, enable flat sampling, or reduce sample count for these
groups. Several slow 100k rows also have 10–40% outlier rates, especially HashMap wide cancel;
use their broad conclusion rather than treating single-digit deltas as precision claims.

---

# Phase 6.2a — within-level queue: Vec, slotmap, and custom arena (2026-09-04)

The baseline `PriceLevel` stored FIFO orders in a `Vec<Order>`. The first replacement used
`slotmap::SlotMap`; the second kept the same intrusive doubly-linked queue but replaced slotmap
with this crate's generational `Arena<T>`. In both contenders, a level maps exchange ids to
stable handles, and exhausted makers are unlinked by handle instead of rescanning and compacting
the level. The saved Criterion baselines are `vec_queue` and `slotmap_queue`.

Representative medians from the unchanged BTree workload follow. Chunked measurements are
divided by their declared element count; `cross 20` remains the full batch of 50 takers. The
arena column is from the direct slotmap-baseline pass; a second Vec-baseline pass showed the same
large conclusions but some thermal variation, most visibly 326–357 ns/op for tight/100k cancel.

| workload | Vec queue | slotmap queue | custom arena |
|---|---:|---:|---:|
| add, tight / 100k | 88.2 ns/op | 160 ns/op | 170 ns/op |
| cancel, tight / 100 | 79.7 ns/op | 158 ns/op | 155–157 ns/op |
| cancel, tight / 10k | 492 ns/op | 164 ns/op | 166–173 ns/op |
| cancel, tight / 100k | 4.59 µs/op | 468 ns/op | 318–320 ns/op |
| add, wide / 100k | 296 ns/op | 541 ns/op | 480 ns/op |
| cancel, wide / 100k | 366 ns/op | 659 ns/op | 593–599 ns/op |
| burst 1000, tight | 426 ns/op | 998 ns/op | 1.02 µs/op |
| burst 1000, wide | 333 ns/op | 444 ns/op | 411 ns/op |
| cross 20 levels × 50 | 1.42 ms | 1.95 ms | 2.13 ms |

## What the comparison says

1. **The asymptotic win is real and narrow.** At tight/100k, direct unlinking cuts cancellation
   by about 92% versus Vec. The crossover is already visible at tight/10k. At tight/100, Vec's
   tiny linear scan is cheaper than hashing plus three arena lookups.
2. **A handle is not free.** Every insert now performs an arena insertion and per-level hash-map
   insertion, then repairs links. Vec's amortized `push` is substantially cheaper, so arena add
   is about 2× slower on the large tight workload.
3. **Wide books are the wrong shape for this per-level design.** They hold only a few orders per
   level, so the old scan was short while every level now owns an arena and hash map. The custom
   arena improves most large wide operations by roughly 9–19% over slotmap, but remains slower
   than Vec.
4. **The custom arena is not a blanket slotmap win.** It improves large tight cancellation by
   about 32% and the large wide add/cancel cases by 9–19%, but regresses the 20-level crossing batch by
   9%. Slotmap remains the safer default when those deltas do not justify maintaining unsafe
   storage code.
5. **The next architectural experiment is global ownership.** One book-wide arena plus
   `OrderLocation::{price, handle}` would remove the hash map and allocation duplicated in every
   price level. That is a materially different layout and needs its own A/B; these results do not
   assume it will win.

## Unsafe validation

The arena's invariant is: a slot's `MaybeUninit<T>` contains one live `T` exactly when
`occupied` is true. Every unsafe operation has a local safety argument, stale handles are
rejected by generation, removal changes generation before reuse, and `Drop` visits only occupied
slots. Eleven deterministic arena tests and four intrusive-queue integration tests pass under
Miri. The randomized operation sequence runs
natively against slotmap as a differential oracle; its default Proptest workload is deliberately
not interpreted under Miri because generation dominates runtime rather than exercising a new
unsafe transition.
