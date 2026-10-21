# Order Book Mastery: A Learning Journey

## Your Profile
- **Rust**: Intermediate (comfortable with ownership, borrowing, small projects)
- **Trading**: Basic understanding (knows orders, general matching concept)
- **Starting Point**: Theory first, then design

## Philosophy
This is a **guided exploration**, not a tutorial to copy-paste. Each phase includes:
- **Discuss**: We talk through concepts, you ask questions
- **Think**: Space for you to design solutions on paper/mentally
- **Implement**: You write the code, I guide when stuck
- **Reflect**: Verify understanding through tests and edge cases

---

## Phase 1: Deep Dive into Order Book Theory

### 1.1 The Order Book Mental Model
Discussion topics:
- **Structure**: The book as two sorted lists (bids descending, asks ascending)
- **The Spread**: Gap between best bid and best ask - why it matters
- **Depth**: How liquidity is distributed across price levels
- **Level 1 vs Level 2 vs Level 3 data**: What information is visible?

### 1.2 Anatomy of an Order
What makes up an order? Let's discuss:
- **Identifiers**: Order ID, Client Order ID - why both?
- **Core fields**: Price, Quantity, Side
- **Timing**: Timestamp precision matters (nanoseconds in real systems)
- **Lifecycle**: New → Partial Fill → Filled / Cancelled

### 1.3 The Matching Process
Walk through scenarios:
- Limit order arrives, no match → rests in book
- Limit order crosses spread → immediate match
- Multiple orders at same price → who gets filled first?
- Partial fills and order state management

### 1.4 Why FIFO (Price-Time Priority)?
- Fairness: First come, first served at each price level
- Incentivizes speed (the latency race)
- Alternatives and when they're used (Pro-Rata, etc.)

**Checkpoint questions** (for you to think about):
1. If best bid is 100.00 and best ask is 100.05, what's the spread?
2. A sell order arrives at 99.95 - what happens?
3. Two buy orders at 100.00: A (100 shares, 9:00am), B (50 shares, 9:01am). A sell of 120 shares arrives. Describe the fills.

---

## Phase 2: Data Structure Design

### 2.1 Design Questions (You Think First)
Before any code, answer these on paper:
- What fields does an Order need? (think about everything from 1.2)
- How do you store multiple orders at the same price?
- What data structure keeps prices sorted efficiently?
- What's the time complexity for: add order, cancel order, get best price?

### 2.2 Rust Design Decisions
Discussion points:
- `Decimal` vs `f64` vs integer cents for price - trade-offs?
- `u64` for quantity - why not `i64`?
- Newtype pattern: `OrderId(u64)` vs raw `u64`
- `BTreeMap` vs `HashMap` - when to use which?

### 2.3 Implementation (You Code)
Build incrementally, one struct at a time:
1. `Side` enum
2. `Order` struct
3. `PriceLevel` - collection of orders at one price
4. `OrderBook` - bids + asks + lookup structures

**Goal**: Store and retrieve orders. No matching yet.

### 2.4 Testing Checkpoint
Write tests that verify:
- Add order → find it by ID
- Cancel order → it's gone
- Best bid/ask returns correct price
- Empty book edge cases

---

## Phase 3: Matching Engine (FIFO)

### 3.0 Pre-work: Fix the quantity/lifecycle dual-accounting footgun

> **Finding (2026-06-12, code review of the Phase 2 implementation).** The current types hold
> *two* candidate sources of truth for "how much of this order is left": `Order.quantity`
> (never mutated) and `OrderLifecycle::PartiallyFilled(u64)` ("how much is filled"). Nothing
> fills yet, so nothing breaks — but the moment the matching engine exists, any reader that
> consults the wrong one silently diverges. Concretely: `PriceLevel::total_quantity()` sums
> `order.quantity`, so it would **over-report book depth** for partially filled orders.
>
> **Fix before writing any matching code** (in `order_book/src/types.rs`):
> 1. `quantity` always means **remaining** quantity — the matching loop decrements it.
> 2. `OrderLifecycle` becomes pure state with unit variants: `New | PartiallyFilled | Filled | Cancelled`
>    (add `Cancelled` now — later phases need it).
> 3. If "original size" is needed for display, add an immutable `original_quantity: u64` set at
>    build time — one write, no dual mutation.
>
> Why remaining-semantics and not a filled-counter: depth (`total_quantity()`) and the matching
> loop both want *remaining*; with remaining-semantics they stay correct with zero changes. With
> a filled-counter, every depth/match read needs `quantity - filled` arithmetic — more places to
> get wrong. Lesson: **one source of truth per fact**; an enum payload that duplicates a struct
> field is a design smell.

### 3.1 Matching Theory Deep Dive
Scenarios to understand:
- **Aggressive order**: crosses the spread, wants immediate execution
- **Passive order**: rests in book, provides liquidity
- **Partial fill**: order partially matched, remainder handled based on type
- **Self-trade prevention**: what if buyer and seller are same entity?

### 3.2 Algorithm Design (You Think)
Sketch on paper before coding:
```
Input:  Incoming order
Output: Vec<Trade>, Option<Order> (remainder)

Steps:
1. ???
2. ???
...
```

Questions to answer:
- Which side of the book do you match against?
- When do you stop matching?
- How do you update the book after each fill?

### 3.3 The Trade Struct
What information does a trade contain?
- Buyer order ID, Seller order ID
- Price (which price? aggressive or passive?)
- Quantity
- Timestamp

### 3.4 Implementation (You Code)
Implement `fn match_order(&mut self, order: Order) -> MatchResult`

### 3.5 Property-Based Testing
Introduction to `proptest`:
- Generate random orders
- Assert invariants that must ALWAYS hold

**Core invariants**:
1. Conservation: `sum(fills) + remainder = original_quantity`
2. Price validity: trades happen at valid prices only
3. Priority: earlier orders at same price fill first
4. No crossing: after matching, best_bid < best_ask

---

## Phase 4: Order Types

### 4.1 Order Types Taxonomy
**By execution style**:
- **Limit**: execute at this price or better
- **Market**: execute now at best available price

**By time-in-force (TIF)**:
- **GTC** (Good Till Cancel): stays until filled or cancelled
- **IOC** (Immediate or Cancel): fill what you can NOW, cancel rest
- **FOK** (Fill or Kill): all or nothing, immediately
- **Day**: cancel at end of trading day

**Conditional orders**:
- **Stop**: becomes market order when trigger price hit
- **Stop-Limit**: becomes limit order when trigger price hit

### 4.2 Rust Modeling (You Think)
Design questions:
- Is `Market` a separate type or a `Limit` with special price?
- Is TIF part of the order or separate?
- How to represent Stop orders (they're not in the book until triggered)?

Options to consider:
```rust
// Option A: Flat enum
enum OrderType { Market, Limit { price: Price }, Stop { trigger: Price }, ... }

// Option B: Composition
struct Order { core: OrderCore, tif: TimeInForce, ... }

// Option C: Trait
trait OrderBehavior { fn should_match(&self, book: &Book) -> bool; }
```

### 4.3 Incremental Implementation
Add one type at a time, with tests:
1. Market orders (simplest aggressive)
2. IOC (adds TIF concept)
3. FOK (adds "check before execute" logic)
4. Stop orders (adds triggered order concept)

---

## Phase 5: Pluggable Matching Algorithms

### 5.1 Algorithm Theory
**FIFO (Price-Time Priority)** - what you built:
- First order at price level gets filled first
- Rewards speed

**Pro-Rata**:
- Allocate proportionally to size
- Example: 3 orders of 100, 200, 300 shares. Incoming 300 fills 50, 100, 150.
- Used in some futures markets

**LMM (Lead Market Maker)**:
- Designated market makers get priority percentage
- Then FIFO for the rest

**Size Priority** (rare):
- Larger orders filled first at same price

### 5.2 Abstraction Design (You Think)
Core question: **What varies between algorithms?**

Answer: How to select which order(s) to fill at a price level.

```rust
// The abstraction boundary
trait MatchingAlgorithm {
    fn allocate(&self, available: Quantity, orders: &[Order]) -> Vec<Fill>;
}
```

Design decisions:
- Static dispatch (`OrderBook<M: MatchingAlgorithm>`) vs dynamic (`Box<dyn>`)?
- How does the algorithm access order metadata (timestamp, size)?

> **Note — two orthogonal axes.** This trait abstracts the *allocation policy* (who fills, by
> how much, at one price level). The *storage layout* (how price levels and resting orders are
> physically held) is a **separate axis**. Keep `allocate(&[Order]) -> Vec<Fill>` purely a
> function of a slice, and storage swaps stay invisible to matching. We compare storage layouts
> head-to-head later in **6.2a — Storage Structure Bake-off**.

### 5.3 Implementation
1. Extract FIFO logic into `FifoMatcher`
2. Implement `ProRataMatcher`
3. Verify same property tests pass for both

---

## Phase 6: Benchmarking & Optimization

### 6.1 What to Measure
Key metrics for order books:
- **Latency**: Time to process single order (p50, p99, p99.9)
- **Throughput**: Orders per second
- **Memory**: Per-order overhead, total book size

### 6.2 Benchmarking with Criterion
- Setup benchmark harness
- Micro-benchmarks: add, cancel, match operations
- Realistic scenarios: burst of orders, cancel storms

### 6.2a Storage Structure Bake-off

The goal here is not to benchmark *the* book — it's to put **several storage layouts on the same
scales** and let the numbers decide. The safe contenders establish the harness and abstraction
seam. Unsafe Rust is a separate guided workshop: landing a generated implementation and benchmark
does not count as learning or completing that workshop.

**Discuss — the storage axis is its own thing.**
Callback to 5.2: `MatchingAlgorithm` abstracts *allocation policy*; **storage layout is
orthogonal**. As long as the matcher only ever sees `&[Order]` → `Vec<Fill>`, you can rip out the
entire backing structure without touching a line of matching logic. That decoupling is what makes
this bake-off possible.

**Think — the common seam (`trait OrderBookStore`).**
Before writing any contender, design the *minimal* interface every backend must satisfy:
```rust
trait OrderBookStore {
    fn add(&mut self, order: Order) -> Result<(), StoreError>;
    fn cancel(&mut self, id: &ExchangeId) -> Option<Order>;
    fn best_bid(&self) -> Option<Price>;
    fn best_ask(&self) -> Option<Price>;
    fn level_depth(&self, side: Side, levels: usize) -> impl Iterator<Item = (Price, u64)>;
}
```
Design questions to answer on paper first:
- What's the *smallest* set of operations the matching engine actually needs? (Don't leak
  `BTreeMap` assumptions into the trait — e.g. don't promise sorted iteration if a backend can't.)
- Static (`Book<S: OrderBookStore>`) vs dynamic (`Box<dyn OrderBookStore>`) dispatch? For a
  bake-off, generics keep the benched code identical and let the optimizer inline each backend.

**The payoff of the seam:** one Criterion suite **and** one property-test suite run *unchanged*
against every backend. Same correctness bar, same workload — the only variable is the layout.
That is the whole point: apples-to-apples.

**Implement — tiered contenders.** (Exact list is a research step when you get here; start with
Tier 0, add Tier 1 to validate the harness, then spend your time in Tier 2.)

- **Tier 0 — baseline:** the current `BTreeMap<Reverse<Price>, PriceLevel>` book. Establishes the
  number everything else is measured against.
- **Tier 1 — safe alternatives (warm-up):** for each, predict Big-O for `add` / `cancel` /
  `best` / `depth` *before* measuring, then check yourself:
  - sorted `Vec<(Price, PriceLevel)>` — O(n) insert/cancel, but contiguous and cache-friendly;
    `best` is free (it's an end).
  - flat **tick-indexed array** ("price ladder") — price-as-index, O(1) everything; loves a tight
    price range, wastes memory on a wide one (foreshadows the distribution lesson below).
  - `HashMap<Price, PriceLevel>` + a cached/heap best-price — O(1) level access, but you must
    *maintain* the best price separately (teaches why a hash map alone isn't enough).
  - within-level axis: `Vec` vs `VecDeque` for the FIFO queue (cancel is O(level) today because
    of the linear `position` scan in `PriceLevel::remove_order`).
- **Tier 2 — push to the edge (the focus):**
  - **arena / slab allocation** with `u32` index "pointers" instead of `Box`/pointers — kills
    per-order allocator traffic and shrinks nodes.
  - **intrusive linked lists** for the per-level FIFO queue — O(1) cancel given a handle, no
    per-node heap allocation.
  - **cache-line-aware layout** — struct-of-arrays, hot/cold field splitting so a price-level
    scan touches only the bytes it needs.
  - **branchless best-price** and eliding bounds checks on the hot path.
  - **`unsafe` Rust** — raw pointers, `UnsafeCell`, `get_unchecked`. Discipline is mandatory:
    justify every `unsafe` block in a comment, wrap it *behind* the safe `OrderBookStore` API,
    and **prove the win with a flamegraph (6.3) before keeping it.** No flamegraph, no merge.

**Methodology — the part that quietly lies to you.**
- **Workload generators & price distribution.** Generate realistic order streams and bench *both*
  a **tight** distribution (clustered around mid) and a **wide/uniform** one. Distribution flips
  the winner — the tick-array is unbeatable when tight and falls apart when wide. A benchmark on
  one distribution is a half-truth.
- **Don't time setup.** Use Criterion's `iter_batched` / `BatchSize` so book construction and
  cloning live in the (untimed) setup closure, not the measured routine.
- **Defeat the optimizer.** `black_box` your inputs and returned values, or the compiler will
  delete the work you're trying to measure.
- **Parameterize by book size N** (e.g. 100 / 1k / 10k / 100k) to *see* the scaling curve — this
  is where O(log n) vs O(1) vs O(n) stops being theory.
- **What's measurable today:** everything — the Phase 3 matching engine exists, so the 6.2
  harness already covers `add` / `cancel` / `best`-price *and* the match path (`submit`).

**Correctness under `unsafe` (a hard gate).**
Because every backend implements the same trait, point the **same proptest suite** at all of them
— a layout that's fast but wrong fails immediately. For Tier 2, add **Miri** and the
**Address/UB sanitizers** to CI and treat them as a gate: a perf number from an `unsafe` backend
doesn't count until Miri + sanitizers are clean.

**Reflect.**
- Read the distribution as p50 **and** p99 / p99.9 — tail latency is what a real venue lives or
  dies by, and a layout can win the median while losing the tail.
- Explain a result where O(log n) `BTreeMap` *beats* a theoretically-O(1) structure. (Constants,
  cache locality, allocator behavior — Big-O is not wall-clock.)
- Tie back to 6.4: you now have measurements, so optimization is allowed to begin.

### 6.2c Guided Unsafe Arena Workshop — Current Lesson

**Learning goal:** be able to design, implement, audit, and explain a generational arena—not
merely use one. Commit `f9c5afd` is a completed reference implementation and benchmark artifact.
It does **not** mark this lesson complete. Keep it on a reference branch while the learner works
from a starter branch without copying the implementation.

**Workshop contract.**

- Work on exactly one chunk at a time. The learner writes the first implementation; the assistant
  explains the model, supplies focused tests, reviews the code, and asks for the invariant in the
  learner's own words.
- Do not reveal or transplant code from the reference solution unless the learner explicitly asks
  for a hint or solution.
- Start each chunk with a prediction and end it with tests plus a short explanation of why every
  unsafe operation is sound. Passing tests alone is insufficient.
- Keep unsafe code inside the arena. The queue and matching engine consume a safe handle-based API.
- Run ordinary unit/property tests frequently; run Miri whenever a new unsafe transition appears.

**Chunks.**

1. [ ] **State machine on paper:** draw `vacant → occupied → vacant`, define the initialized-memory
   invariant, and explain why an index alone becomes an ABA/stale-handle bug.
2. [ ] **Safe oracle:** explore slotmap keys and slot reuse; predict the behavior of stale keys and
   record the Vec and slotmap benchmark expectations.
3. [ ] **Handle and vacant slot:** define `Handle { index, generation }` and `Slot<T>` storage, but
   implement only construction and state inspection.
4. [ ] **Insert and shared access:** implement `insert` and `get`; introduce `MaybeUninit::write`
   and the first tightly scoped `assume_init_ref`, with a local `SAFETY` proof.
5. [ ] **Exclusive access:** implement `get_mut` and explain why the safe API cannot manufacture
   two live mutable references.
6. [ ] **Remove and Drop:** move a value out exactly once, invalidate the old generation, and prove
   removal followed by arena destruction cannot double-drop.
7. [ ] **Arena and free list:** reuse vacant slots, reject stale handles, handle generation/index
   exhaustion deliberately, and maintain panic-safe bookkeeping.
8. [ ] **Differential testing:** generate random insert/get/remove histories and compare observable
   behavior with slotmap; run the deterministic unsafe transitions under Miri.
9. [ ] **Intrusive FIFO:** link arena nodes with generational handles, then implement head, tail,
   middle, and final-node removal while checking link invariants.
10. [ ] **Order-book integration:** replace the level Vec behind a safe iterator API; carry stable
    maker handles through allocation so exhausted makers need no rescan or memmove.
11. [ ] **Benchmark and explain:** compare Vec, slotmap, and arena across shallow/deep and
    tight/wide books. Explain the crossover and why O(1) cancellation can still lose.
12. [ ] **Design verdict:** choose among slotmap, the per-level custom arena, or a book-wide arena.
    Keeping custom unsafe code requires a measured benefit on the intended workload plus a clean
    audit, Miri run, and property suite.

**Completion evidence:** the learner can state the slot invariant, identify each unsafe
precondition, explain generation-based stale-handle rejection, and interpret the benchmark's
mixed result without referring to the solution commit.

### 6.3 Profiling
Tools:
- `perf` / `flamegraph` for CPU profiling
- `heaptrack` for memory profiling
- `cargo-instruments` on macOS

### 6.4 Optimization Techniques (After Measuring!)
Data structures:
- Arena allocation, intrusive linked lists, cache-friendly layout, and `unsafe` hot paths are the
  **Tier 2** of 6.2a — apply them there, behind the `OrderBookStore` seam, validated by the same
  proptests + Miri. (Listed once, in 6.2a, to avoid duplication.)

Advanced Rust:
- Avoid unnecessary allocations
- SmallVec for small collections
- Consider `unsafe` for hot paths (with justification — flamegraph first, see 6.2a)

---

## Phase 7: Advanced Topics (Choose Your Adventure)

Pick what interests you:
- **Persistence**: Snapshots, recovery, event sourcing
- **Market Data**: L2/L3 feeds, incremental updates
- **Multi-Symbol**: Multiple books, symbol routing
- **Concurrency**: Lock-free structures, sharding
- **Networking**: FIX protocol, binary formats

---

## How We Work Together

```
┌─────────────────────────────────────────────────┐
│  1. DISCUSS    - I explain concepts, you ask    │
│  2. THINK      - You design on paper first      │
│  3. IMPLEMENT  - You write code                 │
│  4. REVIEW     - We discuss your solution       │
│  5. TEST       - Property-based verification    │
│  6. ITERATE    - Refine based on learnings      │
└─────────────────────────────────────────────────┘
```

**My role**: Guide, explain, review, challenge assumptions
**Your role**: Think, design, implement, test

For curriculum work, “done” means the learner can explain and reproduce the idea. The assistant
must distinguish three modes explicitly:

- **Teach:** explain one chunk, then wait for the learner's attempt.
- **Review:** inspect the learner's code and diagnose it without silently completing the next
  chunk.
- **Implement:** write the solution only when the learner explicitly requests implementation.

The default for a new curriculum concept is **Teach**. Benchmark plumbing or repetitive edits may
be delegated only after saying so; conceptual core code remains learner-written. A finished
reference commit is evidence for comparison, not evidence of learner completion.

---

## Progress Tracker

- [x] Phase 1: Order Book Theory
  - [x] 1.1 Mental Model
  - [x] 1.2 Anatomy of an Order
  - [x] 1.3 Matching Process
  - [x] 1.4 FIFO Priority
- [x] Phase 2: Data Structures
- [x] Phase 3: FIFO Matching Engine
  - [x] 3.0 Dual-accounting refactor (quantity = remaining; lifecycle → unit variants)
  - [x] 3.5 Property-based testing (proptest, 2026-08-05: 6 invariant properties in `order_book/tests/properties.rs`)
- [x] Phase 4: Order Types (2026-08-09: Shape B refactor — price/TIF/trigger live in `OrderType` variants, `Market` carries no price; IOC, FOK w/ STP-aware dry-run, StopMarket + StopLimit w/ trigger cascade + flat `triggered` reports, `depth()`/`get_order()`; 79 tests incl. 7 proptest invariants. Day TIF consciously skipped — no session/clock concept yet)
- [x] **Tick/lot lattice** (2026-08-14, unplanned — driven by 6.2 observation 4 and the
  `Decimal`-in-hot-path finding): `order_book/src/instrument.rs`. `Price` became a `u64` count of
  minor units on the instrument's tick grid, quantity became `Qty(u64)` in base units, and
  `InstrumentSpec` (tick size, lot size, price/qty bands, min notional) became the only
  constructor of either — so an off-tick price or off-lot quantity is unrepresentable rather than
  validated. Admission checks gate both `submit` and `add_order`. Bought 5–37% across the board
  (see the A/B in `BENCHMARKS.md`) and killed the `100.0` vs `100.00` level-collision bug by
  construction. No phase in this plan covers it; it belongs between 4 and 5.
- [x] Phase 5: Pluggable Algorithms (2026-08-14)
  - [x] 5.2 Abstraction: `trait MatchingAlgorithm::allocate(available, &[Maker], now) -> Vec<Fill>`
    in `order_book/src/allocation.rs`, with a 5-clause contract. Index-based `Fill` and a
    `Maker { remaining_quantity, arrival }` view — neither carries any hint of how orders are
    stored, which is what keeps 6.2a's storage axis orthogonal. **Dispatch: static,
    `OrderBook<M: MatchingAlgorithm>`, no default type parameter and no FIFO-pinned constructor**
    — `new(spec, matcher)` makes every call site name its policy, the same argument the
    instrument spec already carries.
  - [x] 5.3 `FifoMatcher`, `ProRataMatcher` (lot-aware floor pass + time-priority remainder
    sweep), `TimeProRataMatcher` (size × age, water-filling cap loop), all three wired into
    `fill_against` and all three run against the whole property suite. `debug_assert_fills`
    polices the contract on every allocation the engine makes, including the whole-lot clause the
    unit tests structurally cannot check.
  - [x] Engine-stamped `Order.arrival` (`OrderBook::next_arrival`, separate from `next_seq`,
    assigned at rest time): the client's `timestamp` is unforgeable-by-nobody, so weighting by it
    would let a sender backdate to the front of the queue.
  - [x] **STP semantics widened, forced by pro-rata**: self-orders are now cancelled across a
    whole level the taker trades through, not just the prefix a FIFO walk reached. "The prefix I
    walked" is undefined once allocation is proportional; skipping instead of cancelling leaves
    the book crossed. This also makes `fill_against` agree with `fillable_quantity`, which always
    excluded every self order at every crossing level.
- [ ] Phase 6: Benchmarking
  - [x] 6.2 Criterion harness (2026-08-05: 29 benchmarks in `order_book/benches/order_book.rs` — add/cancel/best-price/submit + burst & cancel-storm scenarios, tight vs wide distributions, N = 100..100k; results in `order_book/BENCHMARKS.md`, baseline `phase62`)
  - [ ] 6.2a Storage Structure Bake-off — **in progress**. First milestone completed 2026-08-27:
    `OrderBook<M, S = BTreeStore>` and `OrderBookStore` now separate active-book layout from
    allocation policy; the BTree baseline and bounded dense `TickLadderStore` run through the
    same engine and Criterion workloads. The ladder requires a finite maximum price, caches the
    touch, and repairs it starting beyond a removed best level. HashMap/cached-best is complete;
    sorted Vec was deliberately skipped rather than adding another toy backend. The generated
    Vec → slotmap → custom-arena experiment and its benchmarks exist in reference commit
    `f9c5afd`, but do not complete the learner's unsafe-Rust milestone. Cache-aware layouts and
    the rest of Tier 2 remain. **Measure BTree seam regressions against
    `phase53`, not `phase62`** — `phase62` predates the tick/lot migration.
    Three findings from 5.3 that land squarely on this phase (all written up in `BENCHMARKS.md`):
    - **Audit every read-only benchmark for hoisting first.** `best_bid` was reporting 0.55 ns —
      a hoisted constant — because it `black_box`ed only its result, not the book. A bake-off of
      read paths is exactly where the compiler can delete the work being measured.
    - **Check that contenders actually do different work.** The first `matcher/*` group compared
      three policies that all took the same take-everything early return, and looked like a
      legitimate 3% result. Same trap awaits a storage comparison on a workload that never
      stresses the layout.
    - **`allocate(&[Maker])` costs an O(level) projection whatever the taker consumes** (+27% on
      the deep-level burst). The first bake-off deliberately kept that shape constant; any lazy
      projection or FIFO fast path gets a separate A/B later.
  - [x] 6.2b Property suite over storage backends (2026-08-27) — the invariant bodies run as a
    type-level Cartesian product across three matchers × two stores, plus a differential property
    requiring identical reports and public state from BTree and tick-ladder books.
  - [ ] 6.2c Guided Unsafe Arena Workshop — **current**. Reference solution: `f9c5afd`; learner
    completion: 0/12 chunks. Next: chunk 1, write the slot state machine and initialized-memory
    invariant before touching code.
  - [ ] 6.3 CPU profiling — deterministic steady-state maker-projection runner implemented
    2026-09-10 with a symbolized `profiling` Cargo profile, CLI/invariant tests, and reproduction
    commands in `BENCHMARKS.md`. Capture is pending because this Linux host has
    `kernel.perf_event_paranoid=4` and changing it requires an interactive sudo password. Do not
    mark complete or optimize until the flamegraph is interpreted and the learner can explain
    whether full-level projection is actually dominant.
- [ ] Phase 7: Advanced Topics
