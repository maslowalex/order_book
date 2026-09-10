//! Criterion benchmark suite for the order book (LEARNING_PLAN.md Phase 6.2).
//!
//! # Methodology
//!
//! **Amortized K-op chunks.** Timing a single ~sub-µs `add_order` with a fresh
//! book clone per iteration would explode wall time: criterion picks iteration
//! counts from routine duration, so a ~500ns op demands millions of iterations —
//! each needing an untimed clone of a book that costs milliseconds to clone at
//! N=100k. Instead every mutating benchmark times a chunk of `K = max(10, N/10)`
//! operations against one cloned book and declares `Throughput::Elements(K)`,
//! so criterion still reports per-element time and ops/sec. The chunk bounds
//! book drift to ≤10% of N, keeping the measured op representative of a book
//! of size ~N, and the methodology is identical across N so scaling curves are
//! honest.
//!
//! **Percentile honesty.** Criterion reports the sampling distribution of the
//! *mean* iteration time (point estimate, 95% CI, median, slope) — and our
//! chunks average further. It cannot produce per-operation p99/p99.9 tail
//! latency; Phase 6.1's p50/p99/p99.9 ambition needs per-invocation timestamps
//! (e.g. `Instant` + `hdrhistogram`), a natural add-on during Phase 6.3
//! profiling. What we record now: mean ± CI and elements/sec. Treat criterion's
//! "N outliers among M measurements" lines as a qualitative tail smell only.
//!
//! **Setup stays untimed.** All order construction happens in generators or
//! `iter_batched` setup closures: `OrderBuilder::default()` calls
//! `SystemTime::now()` (a syscall), `cancel_order` takes `ExchangeId` by value
//! (a String clone), and book clones are expensive — none of that belongs in
//! the timed routine. Routines return the mutated book so its (large) drop also
//! lands outside the timed region; `ExecutionReport` construction/drop stays
//! timed on purpose — producing the report *is* part of `submit`'s real cost.
//!
//! Run: `cargo bench`, or filtered: `cargo bench -- 'add_order/tight/1000$'`.
//! HTML report: `target/criterion/report/index.html`.

use order_book::types::Side;
use std::hint::black_box;
use std::time::Duration;

use criterion::{
    BatchSize, BenchmarkId, Criterion, SamplingMode, Throughput, criterion_group, criterion_main,
};
use order_book::allocation::{FifoMatcher, MatchingAlgorithm, ProRataMatcher, TimeProRataMatcher};
use order_book::storage::{BTreeStore, HashMapStore, OrderBookStore, TickLadderStore};

/// Deterministic workload generation. Self-contained because
/// `src/test_helpers.rs` is `#[cfg(test)]`-private and invisible to bench
/// targets (benches link the lib as an external crate).
mod generators {
    use order_book::allocation::{FifoMatcher, MatchingAlgorithm};
    use order_book::instrument::{InstrumentSpec, Qty};
    use order_book::orderbook::OrderBook;
    use order_book::storage::OrderBookStore;
    use order_book::types::{ExchangeId, Order, OrderType, Price, Side};
    use rand::rngs::StdRng;
    use rand::seq::SliceRandom;
    use rand::{Rng, SeedableRng};

    pub const SEED: u64 = 0xB00C;

    /// A price distribution: which tick grid each side draws from.
    ///
    /// Tick 400 (= 100.00) is excluded from both sides in the tight grid so a
    /// seeded book never crosses itself; the wide grid leaves 100.00 out the
    /// same way. Distribution shape is the experiment: tight concentrates
    /// N=100k orders into ~40 levels (~2.5k orders/level → the queue traversal inside
    /// a level dominates), wide spreads them over ~20k levels (~5 orders/level
    /// → the BTreeMap walk dominates).
    #[derive(Clone, Copy)]
    pub struct Dist {
        pub name: &'static str,
        bid_ticks: (u64, u64), // inclusive
        ask_ticks: (u64, u64), // inclusive
        tick_cents: u64,       // minor units (cents) per tick
    }

    /// The quarter-tick grid the matching ladder is built on, shared by
    /// `matching_book` and every taker generator that has to land on it.
    /// Base units as a `Qty`. Every bench spec uses a unit lot, so any of
    /// them converts identically; named `base_qty` only to avoid shadowing the
    /// `qty` locals the generators already use.
    pub fn base_qty(n: u64) -> Qty {
        quarter_tick()
            .qty_from_base(n)
            .expect("unit lot accepts any count")
    }

    pub fn quarter_tick() -> InstrumentSpec {
        InstrumentSpec::new(2, 0, 25, 1).expect("2/0/25/1 is a valid spec")
    }

    /// Mirrors the proptest grid in tests/properties.rs: 95.00–99.75 bids,
    /// 100.25–105.00 asks, 0.25 tick.
    pub const TIGHT: Dist = Dist {
        name: "tight",
        bid_ticks: (380, 399),
        ask_ticks: (401, 420),
        tick_cents: 25,
    };

    /// Uniform over a wide range: 1.00–99.99 bids, 100.01–200.00 asks, 0.01 tick.
    pub const WIDE: Dist = Dist {
        name: "wide",
        bid_ticks: (100, 9_999),
        ask_ticks: (10_001, 20_000),
        tick_cents: 1,
    };

    impl Dist {
        /// Each distribution now carries the tick it was always drawing on —
        /// 0.25 for tight, 0.01 for wide — instead of implying it through a
        /// mantissa multiplier. Tick indices are dense as a result, which is
        /// what a future tick-indexed ladder wants.
        pub fn spec(&self) -> InstrumentSpec {
            InstrumentSpec::new(2, 0, self.tick_cents, 1).expect("dist ticks are non-zero")
        }

        /// The same lattice with a finite band spanning this workload. Storage
        /// comparisons require this because a dense ladder must know its full
        /// slot count before accepting an order.
        pub fn bounded_spec(&self) -> InstrumentSpec {
            let spec = self.spec();
            let min = spec
                .price_from_minor(self.bid_ticks.0 * self.tick_cents)
                .unwrap();
            let max = spec
                .price_from_minor(self.ask_ticks.1 * self.tick_cents)
                .unwrap();
            spec.with_price_range(min, Some(max)).unwrap()
        }

        fn price(&self, rng: &mut StdRng, side: Side) -> Price {
            let (lo, hi) = match side {
                Side::Bid => self.bid_ticks,
                Side::Ask => self.ask_ticks,
            };
            self.spec()
                .price_from_minor(rng.random_range(lo..=hi) * self.tick_cents)
                .expect("drawn on the tick grid")
        }
    }

    fn limit(id: String, client: String, side: Side, price: Price, qty: u64, ts: u128) -> Order {
        Order::builder()
            .exchange_id(id)
            .client_id(client)
            .order_type(OrderType::limit_gtc(price))
            .side(side)
            .quantity(base_qty(qty))
            .timestamp(ts)
            .build()
    }

    /// A book of `n` resting orders (50/50 sides, qty 1..=50), inserted via
    /// `add_order` (rest-only, never matches, never bumps `next_seq` — so
    /// clones behave identically). Returns the exchange ids pre-shuffled for
    /// cancel benchmarks. Client ids are all distinct (`maker-{i}`) so no
    /// submit benchmark can accidentally trip self-trade prevention.
    pub fn seeded_book(dist: Dist, n: usize) -> (OrderBook<FifoMatcher>, Vec<ExchangeId>) {
        let mut rng = StdRng::seed_from_u64(SEED);
        let mut book = OrderBook::new(dist.spec(), FifoMatcher);
        let mut ids = Vec::with_capacity(n);
        for i in 0..n {
            let side = if rng.random_range(0..2) == 0 {
                Side::Bid
            } else {
                Side::Ask
            };
            let price = dist.price(&mut rng, side);
            let qty = rng.random_range(1..=50);
            let id = format!("seed-{i}");
            ids.push(ExchangeId(id.clone()));
            let order = limit(id, format!("maker-{i}"), side, price, qty, i as u128);
            book.add_order(order).expect("seed ids are unique");
        }
        ids.shuffle(&mut rng); // cancel victims hit random levels/positions
        (book, ids)
    }

    /// Backend-generic equivalent of `seeded_book`, using a finite price band
    /// shared by both contenders.
    pub fn seeded_book_with<S: OrderBookStore>(
        dist: Dist,
        n: usize,
    ) -> (OrderBook<FifoMatcher, S>, Vec<ExchangeId>) {
        let mut rng = StdRng::seed_from_u64(SEED);
        let mut book = OrderBook::<FifoMatcher, S>::try_new(dist.bounded_spec(), FifoMatcher)
            .expect("benchmark price span fits the selected store");
        let mut ids = Vec::with_capacity(n);
        for i in 0..n {
            let side = if rng.random_range(0..2) == 0 {
                Side::Bid
            } else {
                Side::Ask
            };
            let price = dist.price(&mut rng, side);
            let qty = rng.random_range(1..=50);
            let id = format!("seed-{i}");
            ids.push(ExchangeId(id.clone()));
            let order = limit(id, format!("maker-{i}"), side, price, qty, i as u128);
            book.add_order(order).expect("seed ids are unique");
        }
        ids.shuffle(&mut rng);
        (book, ids)
    }

    /// Non-crossing limit orders (each side stays in its own half of the grid)
    /// for add/rest benchmarks. Ids are `add-{i}` so they never collide with
    /// `seed-{i}`; `submit` overwrites the id anyway.
    pub fn resting_orders(dist: Dist, count: usize) -> Vec<Order> {
        let mut rng = StdRng::seed_from_u64(SEED ^ 0xADD);
        (0..count)
            .map(|i| {
                let side = if rng.random_range(0..2) == 0 {
                    Side::Bid
                } else {
                    Side::Ask
                };
                let price = dist.price(&mut rng, side);
                let qty = rng.random_range(1..=50);
                limit(
                    format!("add-{i}"),
                    format!("fresh-{i}"),
                    side,
                    price,
                    qty,
                    1_000_000 + i as u128,
                )
            })
            .collect()
    }

    /// A realistic mixed burst: 80% limit / 20% market, prices drawn across the
    /// FULL grid (both halves) so roughly half the limits cross and match.
    /// Every order gets a unique client id — self-trade prevention never fires,
    /// so we measure matching, not cancellation.
    pub fn burst(dist: Dist, count: usize) -> Vec<Order> {
        let mut rng = StdRng::seed_from_u64(SEED ^ 0xB025);
        let full = (dist.bid_ticks.0, dist.ask_ticks.1);
        (0..count)
            .map(|i| {
                let side = if rng.random_range(0..2) == 0 {
                    Side::Bid
                } else {
                    Side::Ask
                };
                let qty = rng.random_range(1..=50);
                let is_market = rng.random_range(0..5) == 0;
                let order_type = if is_market {
                    OrderType::Market
                } else {
                    let tick = rng.random_range(full.0..=full.1);
                    OrderType::limit_gtc(
                        dist.spec()
                            .price_from_minor(tick * dist.tick_cents)
                            .expect("drawn on the tick grid"),
                    )
                };
                Order::builder()
                    .exchange_id(format!("burst-{i}"))
                    .client_id(format!("burst-{i}"))
                    .order_type(order_type)
                    .side(side)
                    .quantity(base_qty(qty))
                    .timestamp(2_000_000 + i as u128)
                    .build()
            })
            .collect()
    }

    /// A deterministic ask ladder for match-path benchmarks: `levels` price
    /// levels from 100.25 upward in 0.25 steps, each holding `per_level` makers
    /// of `qty` — so level depth is exactly `per_level * qty` and a taker can
    /// be sized to consume an exact number of levels by construction.
    /// The allocation policy is a parameter because the `matcher/*` group
    /// benches the *same* ladder under each one — the whole point being to
    /// isolate the cost of the policy from the cost of the layout.
    pub fn matching_book<M: MatchingAlgorithm>(
        matcher: M,
        levels: usize,
        per_level: usize,
        qty: u64,
    ) -> OrderBook<M> {
        let spec = quarter_tick();
        let mut book = OrderBook::new(spec, matcher);
        for j in 0..levels {
            let price = spec
                .price_from_minor(10_025 + (j as u64) * 25)
                .expect("ladder steps one tick at a time");
            for m in 0..per_level {
                let order = limit(
                    format!("mm-{j}-{m}"),
                    format!("mm-{j}-{m}"),
                    Side::Ask,
                    price,
                    qty,
                    (j * per_level + m) as u128,
                );
                book.add_order(order).expect("ladder ids are unique");
            }
        }
        book
    }

    pub fn matching_book_with<M: MatchingAlgorithm, S: OrderBookStore>(
        matcher: M,
        levels: usize,
        per_level: usize,
        qty: u64,
    ) -> OrderBook<M, S> {
        let raw = quarter_tick();
        let min = raw.price_from_minor(25).unwrap();
        let max = raw
            .price_from_minor(10_025 + (levels.saturating_sub(1) as u64) * 25)
            .unwrap();
        let spec = raw.with_price_range(min, Some(max)).unwrap();
        let mut book = OrderBook::<M, S>::try_new(spec, matcher)
            .expect("matching ladder span fits the selected store");
        for j in 0..levels {
            let price = spec
                .price_from_minor(10_025 + (j as u64) * 25)
                .expect("ladder steps one tick at a time");
            for m in 0..per_level {
                let order = limit(
                    format!("mm-{j}-{m}"),
                    format!("mm-{j}-{m}"),
                    Side::Ask,
                    price,
                    qty,
                    (j * per_level + m) as u128,
                );
                book.add_order(order).expect("ladder ids are unique");
            }
        }
        book
    }

    /// `count` limit-bid takers, each sized and priced to consume EXACTLY
    /// `levels_each` levels of the `matching_book` ladder: taker `j` eats
    /// levels `j*k .. (j+1)*k` (depth per level = `level_depth`), its limit
    /// price set to the last level it consumes. Requires
    /// `count * levels_each <= levels` in the ladder.
    pub fn crossing_takers(count: usize, levels_each: usize, level_depth: u64) -> Vec<Order> {
        (0..count)
            .map(|j| {
                let last_level = ((j + 1) * levels_each - 1) as u64;
                limit(
                    format!("taker-{j}"),
                    format!("taker-{j}"),
                    Side::Bid,
                    quarter_tick()
                        .price_from_minor(10_025 + last_level * 25)
                        .expect("takers land on ladder levels"),
                    levels_each as u64 * level_depth,
                    3_000_000 + j as u128,
                )
            })
            .collect()
    }

    /// Takers that leave **half a level** standing, for the `matcher/*` group.
    ///
    /// `crossing_takers` cannot measure an allocation policy, and the reason is
    /// worth writing down: it sizes each taker to consume an exact number of
    /// whole levels, so at every level `available >= total` and every weighted
    /// matcher takes its "the taker swallows the level whole" early return —
    /// allocating byte-for-byte what FIFO would. The policies only diverge on a
    /// level the taker *cannot* finish, which is exactly what this generator
    /// guarantees: demand is `levels_each * level_depth − level_depth/2`, so
    /// the last level touched is always partial.
    ///
    /// Regions stay disjoint. Taker `j`'s limit price sits at the last level of
    /// its own block, so it can never reach into taker `j+1`'s; and its demand
    /// is under what its block plus the previous taker's leftover holds, so it
    /// always fills completely and no `rest_limit` cost leaks into the numbers.
    pub fn partial_takers(count: usize, levels_each: usize, level_depth: u64) -> Vec<Order> {
        (0..count)
            .map(|j| {
                let last_level = ((j + 1) * levels_each - 1) as u64;
                limit(
                    format!("taker-{j}"),
                    format!("taker-{j}"),
                    Side::Bid,
                    quarter_tick()
                        .price_from_minor(10_025 + last_level * 25)
                        .expect("takers land on ladder levels"),
                    levels_each as u64 * level_depth - level_depth / 2,
                    3_000_000 + j as u128,
                )
            })
            .collect()
    }

    /// Same consumption pattern as `crossing_takers` but as market orders —
    /// no limit price, IOC semantics (nothing rests).
    pub fn market_takers(count: usize, levels_each: usize, level_depth: u64) -> Vec<Order> {
        (0..count)
            .map(|j| {
                Order::builder()
                    .exchange_id(format!("taker-{j}"))
                    .client_id(format!("taker-{j}"))
                    .order_type(OrderType::Market)
                    .side(Side::Bid)
                    .quantity(base_qty(levels_each as u64 * level_depth))
                    .timestamp(3_000_000 + j as u128)
                    .build()
            })
            .collect()
    }
}

const SIZES: [usize; 4] = [100, 1_000, 10_000, 100_000];
const DISTS: [generators::Dist; 2] = [generators::TIGHT, generators::WIDE];

/// K-op chunk size: bounds book drift to ≤10% of N (see file-top comment).
fn chunk(n: usize) -> usize {
    (n / 10).max(10)
}

/// BatchSize per book size. `SmallInput` buffers ~10k setup outputs — never
/// viable with a whole book as input. `LargeInput` buffers ~10, fine up to
/// N=10k (~10 × ~3MB). A 100k-order book clone is tens of MB, so 10 buffered
/// clones would be hundreds of MB resident — use `PerIteration` there; its
/// per-call overhead is amortized because the routine is a µs–ms K-op chunk.
fn batch_for(n: usize) -> BatchSize {
    if n >= 100_000 {
        BatchSize::PerIteration
    } else {
        BatchSize::LargeInput
    }
}

/// `add_order` (rest-only, no matching): BTreeMap entry + arena insertion + HashMap
/// index insert. Expect O(log levels); tight should beat wide at large N since
/// 40 hot levels stay in cache while wide walks a ~20k-node tree.
fn bench_add_order(c: &mut Criterion) {
    let mut group = c.benchmark_group("add_order");
    group
        .sample_size(20)
        .measurement_time(Duration::from_secs(2))
        .warm_up_time(Duration::from_millis(500));
    for dist in DISTS {
        for n in SIZES {
            let k = chunk(n);
            let (book, _) = generators::seeded_book(dist, n);
            let fresh = generators::resting_orders(dist, k);
            group.throughput(Throughput::Elements(k as u64));
            group.bench_with_input(BenchmarkId::new(dist.name, n), &n, |b, _| {
                b.iter_batched(
                    || (book.clone(), fresh.clone()),
                    |(mut book, orders)| {
                        for order in orders {
                            black_box(book.add_order(order)).unwrap();
                        }
                        book // dropped untimed
                    },
                    batch_for(n),
                )
            });
        }
    }
    group.finish();
}

/// Active cancellation: one ID-index removal, price-level lookup, and arena
/// unlinking. Tight and wide books exercise different level-map and node-access
/// patterns; neither performs the old per-level linear ID scan.
fn bench_cancel_order(c: &mut Criterion) {
    let mut group = c.benchmark_group("cancel_order");
    group
        .sample_size(20)
        .measurement_time(Duration::from_secs(2))
        .warm_up_time(Duration::from_millis(500));
    for dist in DISTS {
        for n in SIZES {
            let k = chunk(n);
            let (book, ids) = generators::seeded_book(dist, n);
            group.throughput(Throughput::Elements(k as u64));
            group.bench_with_input(BenchmarkId::new(dist.name, n), &n, |b, _| {
                // Victims come from a pre-shuffled id list via a wrapping
                // cursor; every iteration gets a fresh book clone, so each
                // victim window is always fully cancellable. The String clone
                // (`cancel_order` takes ExchangeId by value) is hoisted into
                // untimed setup on purpose.
                let mut cursor = 0usize;
                b.iter_batched(
                    || {
                        let victims = ids[cursor..cursor + k].to_vec();
                        cursor = (cursor + k) % ids.len();
                        (book.clone(), victims)
                    },
                    |(mut book, victims)| {
                        for id in victims {
                            black_box(book.cancel_order(id)).unwrap();
                        }
                        book
                    },
                    batch_for(n),
                )
            });
        }
    }
    group.finish();
}

/// Read-only top-of-book queries on a shared (uncloned) book. `best_bid` and
/// `spread` should be near-constant few-ns lookups regardless of N.
/// `best_bid_level_clone` measures an owned FIFO snapshot: at tight/100k that's
/// ~2.5k orders with two heap Strings each. `best_bid_level_view` separately
/// measures borrowing the level, with no cloning.
/// Top-of-book reads.
///
/// **The book itself is `black_box`ed, not just the result**, and that is not
/// decoration. These routines read a book that never changes, so the whole call
/// is loop-invariant: `black_box` on the *return value* alone still lets the
/// compiler hoist the read out of the iteration loop and time an already-known
/// answer. It measurably did — `best_bid/tight` reported 0.55 ns unguarded
/// (≈1.5 cycles, less than a `BTreeMap` first-key descent can possibly cost)
/// against 0.82 ns with the book fenced.
///
/// The weakness predates the matcher work; making `OrderBook` generic just gave
/// the optimizer more to work with and pushed the artifact into the open. It
/// means the `best_bid` figures here are NOT comparable with the `phase62` and
/// `pre-ticks` columns in BENCHMARKS.md, which were taken with the old, leakier
/// routine. Consider that row rebased.
fn bench_best_price(c: &mut Criterion) {
    let mut group = c.benchmark_group("best_price");
    for dist in DISTS {
        for n in [100usize, 100_000] {
            let (book, _) = generators::seeded_book(dist, n);
            group.bench_function(
                BenchmarkId::new(format!("best_bid/{}", dist.name), n),
                |b| b.iter(|| black_box(black_box(&book).best_bid())),
            );
            group.bench_function(
                BenchmarkId::new(format!("best_bid_level_view/{}", dist.name), n),
                |b| b.iter(|| black_box(black_box(&book).best_level(Side::Bid))),
            );
            if n == 100_000 {
                group.bench_function(
                    BenchmarkId::new(format!("best_bid_level_clone/{}", dist.name), n),
                    |b| b.iter(|| black_box(book.best_bid_level())),
                );
            }
        }
    }
    let (book, _) = generators::seeded_book(generators::TIGHT, 100_000);
    group.bench_function("spread/tight/100000", |b| {
        b.iter(|| black_box(black_box(&book).spread()))
    });
    group.finish();
}

/// The matching hot path, `submit`:
/// - `rest_limit`: non-crossing limits on the same 10k book as `add_order`'s
///   10k config — the delta over `add_order` isolates submit's overhead (the
///   `ExchangeId::from_sequence` format! allocation + the crossing check).
/// - `cross_limit`/`market`: takers each consuming exactly `levels` price
///   levels of a 1000-level ladder (10 makers × qty 10 per level → depth 100).
///   These include `Trade` construction (4 String clones per fill) and
///   `ExecutionReport` allocation — deliberately timed, that IS the product's
///   real per-order cost.
fn bench_submit(c: &mut Criterion) {
    let mut group = c.benchmark_group("submit");
    group
        .sample_size(30)
        .measurement_time(Duration::from_secs(2))
        .warm_up_time(Duration::from_secs(1));

    for dist in DISTS {
        let (book, _) = generators::seeded_book(dist, 10_000);
        let orders = generators::resting_orders(dist, 100);
        group.throughput(Throughput::Elements(orders.len() as u64));
        group.bench_with_input(
            BenchmarkId::new(format!("rest_limit/{}", dist.name), 10_000),
            &(),
            |b, _| {
                b.iter_batched(
                    || (book.clone(), orders.clone()),
                    |(mut book, orders)| {
                        for order in orders {
                            black_box(book.submit(order)).unwrap();
                        }
                        book
                    },
                    BatchSize::LargeInput,
                )
            },
        );
    }

    // 1000 levels × (10 makers × qty 10) = 10k orders, level depth exactly 100.
    let ladder = generators::matching_book(FifoMatcher, 1_000, 10, 10);
    const TAKERS: usize = 50;

    for levels_each in [1usize, 5, 20] {
        let takers = generators::crossing_takers(TAKERS, levels_each, 100);
        group.throughput(Throughput::Elements(TAKERS as u64));
        group.bench_with_input(
            BenchmarkId::new("cross_limit/levels", levels_each),
            &(),
            |b, _| {
                b.iter_batched(
                    || (ladder.clone(), takers.clone()),
                    |(mut book, takers)| {
                        for taker in takers {
                            black_box(book.submit(taker)).unwrap();
                        }
                        book
                    },
                    BatchSize::LargeInput,
                )
            },
        );
    }

    for levels_each in [1usize, 20] {
        let takers = generators::market_takers(TAKERS, levels_each, 100);
        group.throughput(Throughput::Elements(TAKERS as u64));
        group.bench_with_input(
            BenchmarkId::new("market/levels", levels_each),
            &(),
            |b, _| {
                b.iter_batched(
                    || (ladder.clone(), takers.clone()),
                    |(mut book, takers)| {
                        for taker in takers {
                            black_box(book.submit(taker)).unwrap();
                        }
                        book
                    },
                    BatchSize::LargeInput,
                )
            },
        );
    }

    group.finish();
}

/// Realistic scenarios, throughput-oriented (orders/sec via Elements):
/// - `burst_1000`: 1000 mixed orders (80% limit across the full grid so about
///   half cross, 20% market) into a pre-seeded 10k book.
/// - `cancel_storm`: cancel EVERY resting order in shuffled sequence — the
///   worst realistic cancel pattern (e.g. a market-maker pulling all quotes).
fn bench_scenarios(c: &mut Criterion) {
    let mut group = c.benchmark_group("scenarios");
    group
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .sampling_mode(SamplingMode::Flat);

    group.measurement_time(Duration::from_secs(5));
    for dist in DISTS {
        let (book, _) = generators::seeded_book(dist, 10_000);
        let orders = generators::burst(dist, 1_000);
        group.throughput(Throughput::Elements(orders.len() as u64));
        group.bench_with_input(BenchmarkId::new("burst_1000", dist.name), &(), |b, _| {
            b.iter_batched(
                || (book.clone(), orders.clone()),
                |(mut book, orders)| {
                    for order in orders {
                        black_box(book.submit(order)).unwrap();
                    }
                    book
                },
                BatchSize::LargeInput,
            )
        });
    }

    group.measurement_time(Duration::from_secs(10));
    for dist in DISTS {
        for n in [10_000usize, 100_000] {
            let (book, ids) = generators::seeded_book(dist, n);
            group.throughput(Throughput::Elements(n as u64));
            group.bench_with_input(
                BenchmarkId::new(format!("cancel_storm/{}", dist.name), n),
                &(),
                |b, _| {
                    b.iter_batched(
                        || (book.clone(), ids.clone()),
                        |(mut book, ids)| {
                            for id in ids {
                                black_box(book.cancel_order(id)).unwrap();
                            }
                            book // empty now, but drop of the shell stays untimed
                        },
                        batch_for(n),
                    )
                },
            );
        }
    }

    group.finish();
}

/// Storage-layout bake-off with allocation policy, workload, and instrument
/// held fixed. Unlike the legacy groups above, every spec here has a finite
/// price band so the BTree and dense ladder receive identical inputs.
fn bench_storage(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(2))
        .warm_up_time(Duration::from_millis(500));

    fn run<S: OrderBookStore + 'static>(
        group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
        backend: &str,
    ) {
        for dist in DISTS {
            for n in [100usize, 10_000, 100_000] {
                let k = chunk(n);
                let (book, ids) = generators::seeded_book_with::<S>(dist, n);
                let fresh = generators::resting_orders(dist, k);

                group.throughput(Throughput::Elements(k as u64));
                group.bench_with_input(
                    BenchmarkId::new(format!("{backend}/add/{}", dist.name), n),
                    &(),
                    |b, _| {
                        b.iter_batched(
                            || (book.clone(), fresh.clone()),
                            |(mut book, orders)| {
                                for order in orders {
                                    black_box(book.add_order(order)).unwrap();
                                }
                                book
                            },
                            batch_for(n),
                        )
                    },
                );

                group.bench_with_input(
                    BenchmarkId::new(format!("{backend}/cancel/{}", dist.name), n),
                    &(),
                    |b, _| {
                        b.iter_batched(
                            || (book.clone(), ids[..k].to_vec()),
                            |(mut book, victims)| {
                                for id in victims {
                                    black_box(book.cancel_order(id)).unwrap();
                                }
                                book
                            },
                            batch_for(n),
                        )
                    },
                );

                if n == 100 || n == 100_000 {
                    group.throughput(Throughput::Elements(1));
                    group.bench_with_input(
                        BenchmarkId::new(format!("{backend}/best_bid/{}", dist.name), n),
                        &(),
                        |b, _| b.iter(|| black_box(black_box(&book).best_bid())),
                    );
                }
            }

            let (book, _) = generators::seeded_book_with::<S>(dist, 10_000);
            let orders = generators::burst(dist, 1_000);
            group.throughput(Throughput::Elements(orders.len() as u64));
            group.bench_with_input(
                BenchmarkId::new(format!("{backend}/burst_1000"), dist.name),
                &(),
                |b, _| {
                    b.iter_batched(
                        || (book.clone(), orders.clone()),
                        |(mut book, orders)| {
                            for order in orders {
                                black_box(book.submit(order)).unwrap();
                            }
                            book
                        },
                        BatchSize::LargeInput,
                    )
                },
            );
        }

        const TAKERS: usize = 50;
        let ladder = generators::matching_book_with::<FifoMatcher, S>(FifoMatcher, 1_000, 10, 10);
        let takers = generators::crossing_takers(TAKERS, 20, 100);
        group.throughput(Throughput::Elements(TAKERS as u64));
        group.bench_function(format!("{backend}/cross_limit/20_levels"), |b| {
            b.iter_batched(
                || (ladder.clone(), takers.clone()),
                |(mut book, takers)| {
                    for taker in takers {
                        black_box(book.submit(taker)).unwrap();
                    }
                    book
                },
                BatchSize::LargeInput,
            )
        });
    }

    run::<BTreeStore>(&mut group, "btree");
    run::<HashMapStore>(&mut group, "hash_map");
    run::<TickLadderStore>(&mut group, "tick_ladder");
    group.finish();
}

/// What the allocation POLICY costs, with the storage layout held fixed.
///
/// The same ladder, the same takers, three matchers — so the only variable is
/// how a level is apportioned. This is the axis Phase 5 introduced; 6.2a adds
/// the orthogonal one (layout), and keeping them in separate groups is what
/// lets either be read without the other confounding it.
///
/// Takers come from `partial_takers`, NOT `crossing_takers`, and that choice is
/// the whole validity of this group: a taker sized to whole levels hits every
/// weighted matcher's `available >= total` early return and allocates exactly
/// what FIFO would, so the benchmark would have compared three spellings of the
/// same work. Leaving half a level standing is what makes the policies do
/// different things.
///
/// On that half-full level the ladder's 10 makers per level is what the
/// divergence rides on: FIFO fills 5 makers and stops, a weighted policy gives
/// all 10 a share — twice the `Trade` values, each carrying four `String` clones.
/// `levels_each ∈ {1, 20}` separates "one partial level, allocation dominates"
/// from "nineteen full levels plus one partial, the walk dominates".
fn bench_matcher(c: &mut Criterion) {
    let mut group = c.benchmark_group("matcher");
    group
        .sampling_mode(SamplingMode::Flat)
        .measurement_time(Duration::from_secs(10));

    const TAKERS: usize = 50;
    let lot = generators::quarter_tick().lot_size();

    // One generic body, three instantiations — the benched code is identical
    // across policies, which is the only way the comparison means anything.
    fn run<M: MatchingAlgorithm + Clone>(
        group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
        name: &str,
        matcher: M,
        levels_each: usize,
        takers: &[order_book::types::Order],
    ) {
        let ladder = generators::matching_book(matcher, 1_000, 10, 10);
        group.throughput(Throughput::Elements(TAKERS as u64));
        group.bench_with_input(
            BenchmarkId::new(format!("{name}/levels"), levels_each),
            &(),
            |b, _| {
                b.iter_batched(
                    || (ladder.clone(), takers.to_vec()),
                    |(mut book, takers)| {
                        for taker in takers {
                            black_box(book.submit(taker)).unwrap();
                        }
                        book
                    },
                    BatchSize::LargeInput,
                )
            },
        );
    }

    for levels_each in [1usize, 20] {
        let takers = generators::partial_takers(TAKERS, levels_each, 100);
        run(&mut group, "fifo", FifoMatcher, levels_each, &takers);
        run(
            &mut group,
            "pro_rata",
            ProRataMatcher::new(lot),
            levels_each,
            &takers,
        );
        run(
            &mut group,
            "time_pro_rata",
            TimeProRataMatcher::new(lot),
            levels_each,
            &takers,
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_add_order,
    bench_cancel_order,
    bench_best_price,
    bench_submit,
    bench_scenarios,
    bench_storage,
    bench_matcher
);
criterion_main!(benches);
