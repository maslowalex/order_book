# order_book

`order_book` is an educational limit order book and matching engine written in
Rust. It supports market, limit, stop-market, and stop-limit orders; GTC, IOC,
and FOK time-in-force policies; self-trade prevention; and multiple allocation
and storage strategies.

It is designed to make the important matching decisions explicit and
testable. It is not a production exchange and should not be used with real
money.

## Core module layout

Main entities have dedicated modules; their builders, errors, and small supporting
types stay beside them. Import types directly from their owning module:

| Module | Contents |
|---|---|
| `order` | `Order`, its builder and validation error, IDs, side, lifecycle |
| `order_input` | Typed raw `OrderInput`, its builder, and `OrderKind`; `into_order(&spec)` validates and constructs a fresh order |
| `order_type` | `OrderType`, `TimeInForce` |
| `price_level` | Owned price levels and borrowed views |
| `instrument` | `InstrumentSpec`, conversion errors, admission reasons, rounding |
| `instrument::{price, qty, ticks, notional}` | Instrument value types, each in its own module |
| `allocation` | `MatchingAlgorithm`, `Maker`, `Fill` |
| `allocation::{fifo, pro_rata, time_pro_rata}` | Individual matcher implementations |
| `orderbook` | `OrderBook`; matching operations live in a private child module |
| `trade`, `execution_report` | Trades and submission results |
| `storage::book_level` | Active-level metadata; storage backends remain under `storage` |

For example, use `order_book_core::order::Order` and
`order_book_core::allocation::fifo::FifoMatcher`. The former `types` and public
`matching` modules and the old value-type and matcher re-exports are removed.
Historical learning and benchmark notes retain source paths from their recorded
revisions.

## Hotpath profiling

Run the deterministic workload with opt-in function profiling:

```sh
cargo run --profile profiling --features hotpath --bin profile_order_book -- --depth 1000 --operations 1000
# Include allocation tracking:
cargo run --profile profiling --features 'hotpath,hotpath-alloc' --bin profile_order_book -- --depth 1000 --operations 1000
```

The report prints on exit and covers the workload, order insertion/cancellation,
submission, matching sweeps, FIFO allocation, self-trade removal, and maker
projection. Sections with data appear automatically. Set
`HOTPATH_OUTPUT_FORMAT=json` for machine-readable output.

For a live view, run `cargo install hotpath --features tui`, then `hotpath console`
while the workload runs (increase `--operations` for a longer session). The metrics
server uses port 6770 by default. The `hotpath-prometheus` feature also exposes
hotpath's optional Prometheus integration.

Profiling is disabled by default. Keeping hotpath as a regular dependency is safe:
without its profiling features, only the hotpath crates compile, with no additional
third-party dependencies; instrumentation macros are noops with no runtime overhead.
Use feature-off builds for ordinary Criterion measurements.

## Main assumptions

### One process owns one underlying

The intended deployment model is one engine process per underlying (and one
`OrderBook` per instrument specification). That process is the sole authority
for the book's mutable state.

Keeping ownership local gives every order, cancellation, fill, and stop
activation one unambiguous position in the event stream. It also avoids
cross-instrument locking and makes replay, recovery, and failure containment
independent for each underlying. A system running many underlyings is expected
to partition and supervise those processes outside this crate.

This is an architectural assumption, not something the Rust types enforce.

### Events are processed serially

The engine is single-threaded. The caller supplies an already ordered stream of
commands, and each command finishes before the next one begins. The crate does
not arbitrate concurrent writers or decide how events from different sources
should be sequenced.

Serial processing keeps price-time priority deterministic and allows the same
input stream to reproduce the same book state. Concurrency belongs around the
engine—for example in network ingestion or across independently owned
underlyings—not inside one book.

### The book is in memory

There is no write-ahead log, snapshotting, replication, or recovery protocol.
If durable operation is required, the surrounding application must persist the
ordered command stream and rebuild or restore the book after a restart.

Networking, authentication, account management, pre-trade risk checks,
clearing, market-data distribution, monitoring, and high availability are also
outside the scope of this crate.

### Every book has an explicit instrument specification

An `InstrumentSpec` defines the price tick, quantity lot, scales, and optional
admission bounds for a book. Prices and quantities are stored as integers on
that grid; decimal conversion happens at the boundary.

This avoids floating-point ambiguity and prevents off-tick prices or off-lot
quantities from entering through the public API. Prices are unsigned, so this
model cannot represent instruments that trade below zero.

### The engine owns identity and priority

On submission, the engine assigns the exchange order ID. A separate monotonic
arrival value is assigned when an order actually joins a price level and is
used for time priority.

The timestamp carried by an order is caller-provided metadata. It is not trusted
for queue priority, so a client cannot move ahead by backdating an order. A stop
order receives its priority when it triggers and rests, not when it was first
submitted.

## Matching rules that matter to callers

- Trades execute at the resting maker's price.
- Price levels are crossed best-price first; allocation within one level is
  chosen explicitly with `FifoMatcher`, `ProRataMatcher`, or
  `TimeProRataMatcher`.
- Self-trade prevention cancels the taker's own resting orders encountered at a
  level instead of matching or skipping them.
- An unfillable FOK order is killed without fills or self-trade cancellations.
- Stop orders trigger from the most recent trade price. Cascaded activations are
  returned as a flat list of execution reports.
- The active price-level store is selected independently from the allocation
  policy: `BTreeStore`, `HashMapStore`, and `TickLadderStore` are provided.

## Verification and further reading

Run the test suite:

```sh
cargo test
```

Run the Criterion benchmarks:

```sh
cargo bench --bench order_book
```

The benchmark methodology and historical measurements live in
[BENCHMARKS.md](BENCHMARKS.md). The source modules contain the detailed API and
invariant documentation.

## License

MIT. See [LICENSE](LICENSE).
