# order_book

A limit order book and matching engine in Rust: prices and quantities on an integer
tick/lot lattice, price–time priority, stop orders, self-trade prevention, and
pluggable allocation policies behind one trait.

> **This is an educational project.** It was written to learn how a matching engine
> actually works and what its design choices cost, not to run one. It is
> single-threaded, in-memory, and has no networking, persistence, or risk controls.
> Do not point real money at it.

## What this document is

The code has no users, so an API reference would document the least interesting thing
about it. What the repo actually accumulated is an argument with itself: a decision,
a reason, a measurement, and occasionally a retraction. That is the part worth
keeping, so this README is the log of it.

Each entry states what was chosen and what it cost. Where a decision was settled by
measurement, the figure is quoted; the tables and methodology behind them are in
[BENCHMARKS.md](BENCHMARKS.md). Where an argument is easier in arithmetic than in
prose, the arithmetic is shown.

---

## Representation

### Prices are integers on a tick grid, not `Decimal`

`Price` is a `u64` count of the instrument's minor units — cents, satoshis, lamports
— which is what ITCH-style feeds put on the wire. `Decimal` survives in `instrument`
and nowhere else: it is a good type for parsing and formatting at the boundary and a
bad one behind it.

The speed argument is real — `Decimal`'s `Ord` has to align two scales before it can
answer, and a `BTreeMap` pays that at every level of every descent. Removing it made
`add_order` 14–20% faster on a tight book and 23–37% faster on a wide one, and
`spread()` 30% faster *while doing strictly more work* (it went from one subtraction
to an integer `abs_diff` **plus** a division, since it now returns `Ticks`).

But the speed was not the case. `100.0` and `100.00` are `Ord`-equal and *not*
identical `Decimal`s, so they collided onto one book level while the level kept
whichever scale happened to create it — making the scale a trade printed at depend on
arrival order. `10000` has no such freedom. Had the benchmarks come back flat, the
migration would still have been worth doing.

**Cost:** `Price` is unsigned, so negative prices are unrepresentable rather than
validated. Instruments do trade negative — WTI in April 2020, calendar spreads
routinely — and this book cannot model them. In exchange, every price in the engine
is known non-negative without a single runtime check.

### Only `InstrumentSpec` can mint a `Price` or a `Qty`

The newtypes' fields are private and `from_minor_unchecked` is `pub(crate)`, so an
off-tick price cannot be constructed through the public API at all. The bit pattern
still exists — nothing stops a `u64` from holding `100_003` — but no safe path
reaches it.

This is the guarantee `String` gives over `Vec<u8>`: invalid UTF-8 is representable in
the bytes and unconstructable through the type. It is what turns "we assume
normalization happened upstream" from a comment into an invariant.

### `OrderType` carries the data its variant needs

```rust
enum OrderType {
    Limit      { price: Price, tif: TimeInForce },
    Market,
    StopMarket { trigger: Price },
    StopLimit  { trigger: Price, price: Price, tif: TimeInForce },
}
```

A market order has no price to carry, a stop cannot exist without a trigger, and time
in force only means anything for a limit. Putting each variant's data inside it makes
a GTC market order or a triggerless stop *unrepresentable* rather than *validated* —
there is no constructor to reject, because there is no shape to reject.

**Cost, measured:** an enum pays for its widest variant, on every value. `OrderType`
was sized by `StopLimit`, so every plain limit resting in the book carried
stop-shaped padding it would never use — 36 bytes when the prices were `Decimal`s, 24
once they became integers. That lands on the hottest struct in the system.

### Two counters, not one

`next_seq` generates order ids; `next_arrival` records queue position. Merging them
looks tempting and is wrong twice over. `next_seq` is an *id generator* — every value
it emits becomes an `ExchangeId` somebody can address — so bumping it on every rest
would punch holes in the id space, one per rest, and the ids would stop being a
sequence.

They also count different things. An order can be submitted and never rest (market,
IOC, a killed FOK), and an order can rest *twice* — a stop that parks, triggers, and
comes back as a limit.

### `arrival` is `u32`, and the 16 bytes that bought

The one entry in this log where a variable's *width* was the whole story.

`arrival` was written as `u64` first. `Order` has a `u128` timestamp, so it aligns to
16, and at 106 bytes of content it had exactly 6 bytes of tail padding going spare. A
`u32` lands in that padding and `Order` stays **112 bytes**; a `u64` does not fit and
pushes it to **128**.

That 14% is paid on every byte the engine memmoves compacting a level and every byte
it walks scanning one:

| benchmark | `u64` | `u32` |
|---|---:|---:|
| `add_order/tight/10000` | +24% | −1% |
| `cancel_order/tight/100000` | +16% | 0% |
| `submit/rest_limit/tight` | +27% | 0% |

Every one of those is bound by walking or memmoving a level's `Vec`, so they are
counting bytes per element and nothing else — which is why the tight distribution
(~2,500 orders per level) took all of it and the wide one barely noticed. For a field
only `TimeProRataMatcher` ever reads.

**Cost:** a ceiling of ~4.29 billion rests per book, enforced with `checked_add`
rather than left to wrap. A wrapped counter would make the newest order at a level
read as the oldest and silently invert time priority — the one thing the field exists
to establish, corrupted with no symptom until someone audits a fill. A real venue
would either spend the 16 bytes or renumber at a session boundary; this book is
explicit about which it chose.

---

## Matching

### Trades print at the maker's price

The resting order set the price; the taker accepted it. This is why `Trade.price` is
read off the level rather than off either order, and why a marketable limit at 101
against an ask at 100 prints at 100.

### Self-trade prevention removes; it doesn't skip

Resting orders belonging to the taker's own client are cancelled — removed from the
book — before the level is handed to the matcher, and their ids come back in
`ExecutionReport::cancelled`.

Neither alternative works. Leaving a self order in the slice has the matcher fill it,
printing exactly the self-trade this exists to prevent. Skipping it without removing
it leaves it resting and crossable: the taker's remainder would come to rest through
its own untouched order on the other side, the book would sit crossed, and the
un-drained level would be re-selected forever.

**What changed with the allocation abstraction was reach.** The old FIFO loop
cancelled only the self orders the taker physically walked past, because it stopped
the moment the taker filled up — anything deeper survived. Under a policy that
apportions across the whole level there is no "walked past" to speak of, so the rule
became *your own orders at a level you trade through are gone*. That is more
aggressive than most venues' per-match STP. It also made the FOK dry run and the real
sweep agree on the cancellation set as well as on the quantities, which they had not
before.

### FOK checks fillability before it touches anything

`process_fok_limit_order` computes fillability against a read-only view first, then
either sweeps normally with a full fill guaranteed, or returns `Killed` having
touched **nothing** — no partial fills, and no self-trade cancellations either,
because nothing executed. The dry run excludes the taker's own resting orders, since
counting them would overpromise and let a "fill or kill" partially fill.

### The stop cascade is flat

A triggered stop can trade, and its trades can trigger further stops. Every resulting
report lands in one `ExecutionReport::triggered` vec — nested reports always have an
empty `triggered` — so consumers never recurse.

The loop terminates because every iteration permanently removes one stop, and
activations (market and limit orders) can never add one.

### The exchange assigns the id, and admission runs first

`submit` ignores whatever `exchange_id` the caller put on the order and stamps
`ExchangeId::from_sequence(next_seq)`. Admission runs *before* the id is minted, so a
rejected order consumes no sequence number and leaves the book bit-identical — an
order that was never accepted never became an order. There is a property test that
asserts exactly this.

Real venues often burn an id instead, so a reject is addressable by id. That trade is
worth knowing about; this choice buys a testable "nothing moved".

---

## Allocation

### Allocation is a trait, and `makers()` is the seam

```rust
struct Maker { remaining_quantity: Qty, arrival: u128 }

trait MatchingAlgorithm {
    fn allocate(&self, available: Qty, makers: &[Maker], now: u128) -> Vec<Fill>;
    fn lot_size(&self) -> Option<u64> { None }
}
```

Everything a policy learns about a price level passes through those two fields. No
prices, no ids, no order types — deliberately not an `Order`. An allocation policy has
no business reading any of that, and a storage layout holding nothing resembling an
`Order` can still hand out this view.

`Fill` is index-based for the same reason: `ExchangeId` is a `String`, so an
id-carrying fill would heap-allocate on the hot path *and* force a second linear scan
to find the maker again. An index is `Copy`, free, and carries zero information about
how orders are stored.

The book is `OrderBook<M: MatchingAlgorithm>`, so policy and storage layout are two
independent axes. Only the first is a type parameter today.

### Static dispatch — and the dispatch was never the cost

`allocate` is called once per price level and does O(level) work inside, so even a
vtable would have amortised away. Dispatch was never a performance question, and the
measurements agree: it cost nothing detectable.

**The real cost is the projection.** `allocate(&[Maker])` forces `fill_against` to
materialise the *whole* level before it can allocate anything, because pro-rata needs
the level's total before it can apportion. The old FIFO loop was O(makers actually
touched); this is O(level) regardless.

The distribution split is the evidence: `burst_1000` regressed **+27% tight, +18%
wide**, and tight packs ~250 makers per level against takers drawing 1–50 units. The
`submit` group agrees more mildly at +3…+9%, because its ladder holds only ten makers
a level.

This is not a bug in the implementation. It is what `allocate(&[Maker])` *means*.
Pro-rata genuinely needs the level total; FIFO does not, and pays anyway.

With dispatch free, the policy group measures policy alone:

| | FIFO | pro-rata | time-pro-rata |
|---|---:|---:|---:|
| `matcher/levels/1` (one partial level) | 962 ns | 1.459 µs (+52%) | 1.524 µs (+58%) |
| `matcher/levels/20` (19 full, 1 partial) | 28.35 µs | 29.35 µs (+4%) | 29.33 µs (+3%) |

The +52% is pro-rata touching all ten makers on a partial level where FIFO touches
five, at two `String` clones per trade. It collapses to +4% at twenty levels because
nineteen of them are consumed whole and every matcher takes its `available >= total`
early return.

### Pro-rata must be told the lot size; FIFO must not

Floor division does not preserve divisibility, and the consequence is not a rounding
nicety — it is state corruption. Lot size 10, makers holding `[10, 20]`, taker
bringing 10:

```text
f₁ = ⌊10·10/30⌋ = 3     f₂ = ⌊10·20/30⌋ = 6     Σ = 9
remainder 1 → front by time priority → fills [4, 6]
```

Neither 4 nor 6 is a whole lot. Worse than a bad print: the engine subtracts those
fills from the makers, which are left resting at 6 and 14 — permanently off-lot,
sitting in the book, visible in `depth()`, and available to be matched again. The
lattice would leak, one partial fill at a time, and no care in the remainder pass
repairs a floor pass that already left the grid.

FIFO takes `min(left, remaining)`, and the minimum of two lot multiples is a lot
multiple. It is closed under the lattice for free and needs to know nothing about
lots. Pro-rata is the only allocator here that can leave the grid, so it is the only
one that has to be told where the grid is — which is what `lot_size()` on the trait
is for, and why `OrderBook::new` asserts the matcher's lot against the instrument's.

### Time-weighted pro-rata has to water-fill

Pro-rata's weight *is* its size, so below the take-everything branch every share lands
strictly under the maker's own remaining and a floor pass can never over-allocate
anyone. The moment the weight stops being the size, that guarantee dies. Makers
holding `[1, 100]`, aged `[10, 1]`, taker bringing 50:

```text
w = [1·10, 100·1] = [10, 100]     Σw = 110
f₁ = ⌊50·10/110⌋ = 4              — against a maker holding 1
```

Handing out 4 where 1 exists breaks the contract, so the excess goes back in the pot
and is re-apportioned among whoever can still take it. That water-filling loop is the
only structural difference between the two weighted matchers.

One pass may cap several makers at once, and that is provable rather than hopeful:
removing capped makers only ever *raises* the survivors' shares, and flooring is
monotone, so anyone over their cap after a redistribution was already over it before.
The loop therefore runs at most once per maker.

### Age is `max(1, elapsed/tick)`, not `1 + elapsed/tick`

Both keep the weight positive; only one leaves real dwell ratios undistorted. With
arrivals 0/1/2 read at `now = 3`, `max(1, …)` gives ages 3/2/1 — the truth — where
`1 + …` gives 4/3/2 and flattens the very difference the algorithm exists to express.

Relatedly, `arrival` is engine-assigned and `timestamp` is not. `timestamp` is
whatever the client put on the message and is never re-stamped on receipt, so an
allocator weighting by it would let a sender backdate its way to the front of the
queue. `arrival` is stamped at the instant an order actually comes to rest — which is
also why it is not submission order: a stop submitted early can trigger late, and it
queues where it landed, not where it was sent from.

### `allocate` never reads a clock

`now` is a parameter. An allocation is a pure function of its arguments, so it
replays identically off a log. The engine's `now` is a count of orders that have
rested, not a wall clock — unforgeable by a client, monotone, and replayable for the
same reason.

---

## Verification and measurement

### No `Default` where it would hide a decision

- No `Default` for `InstrumentSpec`. `::cents()` is named instead, so callers can
  point at the line where they chose a grid; a *default* lattice is the "someone
  upstream handled it" assumption sneaking back in through a derive.
- No `Default` for `ProRataMatcher` or `TimeProRataMatcher`. A matcher that guessed
  lot 1 would look right in every unit-lot test and quietly produce off-lot fills
  everywhere else.
- No default `OrderType` in `OrderBuilder`. `Market` silently standing in for a
  forgotten `.order_type(…)` hid intent; it is now a loud panic.
- No `Nearest` in `Rounding` — rounding a price toward the wrong side of the spread
  is a decision, so the variants are `Down`, `Up`, and `TowardPassive(Side)`.

### Every property runs against every matcher — as a loop, not a proptest input

`tests/properties.rs` states nine invariants across twelve properties, and every one
of them except the strong FIFO-only half of time priority runs against all three
allocation policies through a `for_each_matcher!` macro. Drawing the matcher with
`prop::sample::select` would shrink and print more prettily, but it would also split
the case budget across policies — and a pro-rata-only bug would then hide behind
sampling luck instead of failing every run.

The invariants are universal by construction, not by luck: the contract says every
conforming matcher fills the same *total* at a level and only the distribution
varies, so outcomes, filled quantities, depth deltas and last trade price are all
matcher-invariant. `every_matcher_agrees_on_the_totals` asserts that head-on.

### Debug assertions live in one body, not a `#[cfg]`-split pair

`debug_assert_fills` checks the allocation contract against every allocation the
engine ever makes — including the clause the unit tests structurally cannot, that
every fill is a whole lot, since only the engine knows the instrument's lot.

It is one body under `cfg!(debug_assertions)` rather than a `#[cfg]`-split pair,
because the release twin of such a pair is never type-checked, so it rots. The
optimizer deletes it entirely when assertions are off.

### Predictions get written down before the comparison run

For the tick/lot migration, five predictions were recorded before the benchmark
comparison, including a stated falsification test. Four held. One failed outright:
`best_bid_level_clone` was predicted to be the clean exhibit of a 16-byte `Order`
shrink and moved −3% / **+3%** — the wrong direction. Cloning five orders means ten
heap allocations, because `ExchangeId` and `ClientId` are both `String`, and at
190 ns those dominate a per-element width difference completely.

That failure retroactively overturned an earlier conclusion in this repo's own
benchmark write-up, which is the reason the practice is worth the friction. Two
neighbouring lessons, both recorded rather than quietly fixed:

- **A benchmark can measure a hoisted constant.** `best_bid` reported **0.55 ns** —
  under 1.5 cycles, less than a `BTreeMap` descent can possibly cost — because
  `black_box` fenced the result and not the book. Fencing the book gives 0.82 ns.
- **A benchmark can exercise the code path under test and still route it through a
  degenerate branch.** The first `matcher/*` group sized every taker to consume whole
  levels, so all three policies took the same early return and allocated
  byte-for-byte identically. It reported a 3% spread and looked entirely legitimate.
  Forcing a partial level turned that into +52%.

---

## Open questions

- **The projection cost.** `allocate(&[Maker])` materialises whole levels for policies
  that don't need them. Either a fast path for those, or a projection the matcher
  pulls lazily — and it wants deciding *before* the storage layout changes underneath
  it.
- **Cancel is the tail.** ~4.5 µs at 100k orders on a tight book, against ~344 ns on a
  wide one, because `PriceLevel::remove_order` linearly scans a `Vec` with `String`
  compares and then shifts. Compaction after a sweep has the same shape: O(level)
  however few makers were exhausted, because a `Vec` level has no handles. A slab plus
  an intrusive list makes both O(1) per order, and nothing in the matching path has to
  know.
- **`String` ids are the visible ceiling** now that `Decimal` is gone — they hold up
  the level scan, the trade-id clones in the fill loop, and `best_bid_level()`.
- **`best_bid_level()` deep-clones the level** — 92.7 µs at tight/100k, against 0.82 ns
  for `best_bid`. `depth()` already exists and is the right replacement for most
  callers.
- **Tail latency is unmeasured.** Criterion reports the sampling distribution of the
  mean; per-op p99/p99.9 needs per-invocation timing.

---

## Reference

| Module | Role |
|---|---|
| `instrument` | `Price`, `Qty`, `Ticks`, `Notional`, and the `InstrumentSpec` lattice + admission checks |
| `types` | `Order`, `OrderBuilder`, `OrderType`, `Side`, `TimeInForce`, `PriceLevel` |
| `orderbook` | `OrderBook<M>` — both sides, stop books, id index, resting/cancel/read paths |
| `matching` | `submit`, `Trade`, `ExecutionReport`, the stop cascade, self-trade prevention |
| `allocation` | `MatchingAlgorithm`, `Maker`/`Fill`, and the three matchers |

The crate has no root re-exports; paths are module-qualified
(`order_book::orderbook::OrderBook`).

```
cargo test
cargo bench --bench order_book
```

Note the `--bench order_book`: plain `cargo bench` also runs the library's default
test harness, which rejects criterion's flags. Numbers, methodology and the full
run-by-run history are in [BENCHMARKS.md](BENCHMARKS.md); for orientation, the most
recent burst scenario runs ~2.3M mixed orders/sec on a tight book and ~3.0M on a wide
one, single-threaded on an Apple M3 — down from ~2.9M/~3.5M before the allocation
trait, for the reason under "Static dispatch" above.

**Read that as a pre-optimization number.** No optimization pass has happened yet, and
nothing has been profiled — every attribution in `BENCHMARKS.md` is explicitly a
hypothesis awaiting a flamegraph. The two speedups on record were side effects rather
than the goal: the tick/lot migration was a correctness change that happened to be
faster, and narrowing `arrival` to `u32` declined a regression rather than winning
anything. The known costs are all still in place — four `String` clones per trade, a
linear scan per cancel, whole levels projected for policies that don't need them. See
[Open questions](#open-questions).

Edition 2024. Developed against rustc 1.92.0.

## License

MIT — see [LICENSE](LICENSE). It is a learning project; take the ideas, and check the
numbers on your own hardware before trusting any of them.
