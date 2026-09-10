//! Phase 3.5/4/5 — property-based tests over random order streams.
//!
//! Each property drives a fresh book through a random stream of orders
//! (through the public `submit` API only) and asserts an invariant that must
//! hold for *every* stream, not just hand-picked examples:
//!
//! 1. Conservation: filled + rested + killed quantity == original quantity,
//!    for the submitted order AND every stop its trades triggered
//! 2. Depth accounting: the book's total depth changes by exactly
//!    rested − filled − self-trade-cancelled, across the whole cascade
//! 3. Price validity: every trade prints at the maker's resting price, never
//!    worse than the taker's limit, sweeping best-price-first — and never
//!    between two orders of the same client
//! 4. Time priority: same-price makers fill in arrival order. Split in two,
//!    because pro-rata abolishes half of it: `fifo_same_price_priority` is
//!    the strong FIFO-only form, `queue_order_within_one_execution` the part
//!    every policy still owes
//! 5. No crossing: after every submit, best_bid < best_ask
//! 6. Index consistency: `index`, the price levels, and the stop books agree
//!    exactly, and everything drains by cancelling every live id
//! 7. Stop discipline: no pending stop whose trigger the market has already
//!    reached survives a submit
//! 8. Lattice closure: every price and quantity the book holds is still on
//!    the instrument's tick/lot grid, checked where it is falsifiable — at
//!    the `Decimal` boundary, not in the integers
//! 9. Rejection is a no-op: an order outside the instrument's bounds leaves
//!    the book bit-identical, sequence number included
//!
//! # The matcher axis
//!
//! Every invariant above except the FIFO half of 4 runs against **every**
//! allocation policy and every active-book store, via
//! [`for_each_configuration`] — a type-level Cartesian product. That is why
//! the bodies live in generic `check_*` functions rather than inline in
//! `proptest!`.
//!
//! They are universal by construction, not by luck. Contract clause (4) —
//! `Σ fills == min(available, total)` — says every conforming matcher fills
//! the same *total* at a level; only the distribution among that level's
//! makers varies. So outcomes, filled quantities, depth deltas, the set of
//! levels touched and the last trade price are all matcher-invariant, which
//! `every_matcher_agrees_on_the_totals` asserts head-on.
//!
//! Prices and triggers are drawn from a tight tick grid (95.00–105.00, 0.25
//! steps) so that random streams actually cross and stops actually fire;
//! clients come from a pool of 4 so self-trade prevention gets exercised.

use std::collections::HashMap;

use order_book::allocation::{FifoMatcher, MatchingAlgorithm, ProRataMatcher, TimeProRataMatcher};
use order_book::instrument::{InstrumentSpec, Qty};
use order_book::matching::{ExecutionReport, SubmitOutcome};
use order_book::orderbook::{OrderBook, OrderBookError, OrderLocation};
use order_book::storage::{BTreeStore, HashMapStore, OrderBookStore, TickLadderStore};
use order_book::types::{ExchangeId, Order, OrderType, Price, Side, TimeInForce};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use rust_decimal::Decimal;

// --- matcher × storage axes --------------------------------------------

/// Run one invariant body against every allocation/storage pairing, on the
/// SAME drawn stream.
///
/// A loop, not a proptest input. Drawing the matcher with
/// `prop::sample::select` would shrink and print more prettily, but it would
/// also split the case budget across policies — a pro-rata-only bug would
/// then hide behind sampling luck instead of failing every run.
macro_rules! for_each_configuration {
    ($check:ident, $($arg:expr),* $(,)?) => {{
        $check::<_, BTreeStore>(FifoMatcher, $($arg),*)?;
        $check::<_, BTreeStore>(ProRataMatcher::new(lot()), $($arg),*)?;
        $check::<_, BTreeStore>(TimeProRataMatcher::new(lot()), $($arg),*)?;
        $check::<_, TickLadderStore>(FifoMatcher, $($arg),*)?;
        $check::<_, TickLadderStore>(ProRataMatcher::new(lot()), $($arg),*)?;
        $check::<_, TickLadderStore>(TimeProRataMatcher::new(lot()), $($arg),*)?;
        $check::<_, HashMapStore>(FifoMatcher, $($arg),*)?;
        $check::<_, HashMapStore>(ProRataMatcher::new(lot()), $($arg),*)?;
        $check::<_, HashMapStore>(TimeProRataMatcher::new(lot()), $($arg),*)?;
    }};
}

// --- strategies ---------------------------------------------------------

fn arb_side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Bid), Just(Side::Ask)]
}

/// The instrument every property runs against: cents, a 0.25 tick, unit lot.
///
/// The tick is deliberately NOT one minor unit. A one-cent tick would make
/// every cent-scale price legal, so the lattice would be vacuously satisfied
/// and no property could tell a working tick check from a missing one.
///
/// The *lot*, however, is one — so clause (5) is vacuous here in exactly the
/// way the tick is not. `the_lattice_holds_on_a_non_unit_lot` covers that gap
/// on its own spec rather than making all nine properties pay for it.
fn spec() -> InstrumentSpec {
    let spec = InstrumentSpec::new(2, 0, 25, 1).expect("2/0/25/1 is a valid spec");
    let min = spec.price_from_minor(9_500).unwrap();
    let max = spec.price_from_minor(10_500).unwrap();
    spec.with_price_range(min, Some(max)).unwrap()
}

/// The lot the weighted matchers must be built with to be attachable to a
/// book on [`spec`] — the book asserts on exactly this.
fn lot() -> u64 {
    spec().lot_size()
}

/// Base units as a `Qty`. The spec's lot is one, so base units and lots
/// coincide and every quantity literal below reads exactly as it always did.
fn qty(n: u64) -> Qty {
    spec()
        .qty_from_base(n)
        .expect("a unit lot accepts any count")
}

fn arb_limit_price() -> impl Strategy<Value = Price> {
    // 95.00 ..= 105.00 in 0.25 ticks
    (380u64..=420).prop_map(|ticks| {
        spec()
            .price_from_minor(ticks * 25)
            .expect("drawn on the tick grid")
    })
}

/// The same grid with size limits that bite: 5..=1000. `arb_order` draws
/// 1..=50, so a stream against this spec is a realistic mix of accepted and
/// rejected orders rather than a uniformly happy path.
fn bounded_spec() -> InstrumentSpec {
    spec()
        .with_qty_range(qty(5), Some(qty(1000)))
        .expect("5 and 1000 are whole lots")
}

fn arb_order_type() -> impl Strategy<Value = OrderType> {
    // Mostly GTC limits (they build the book); IOC/FOK limits and markets
    // sweep it; stops park and later fire off other orders' trades.
    prop_oneof![
        4 => arb_limit_price().prop_map(OrderType::limit_gtc),
        1 => arb_limit_price().prop_map(OrderType::limit_ioc),
        1 => arb_limit_price().prop_map(OrderType::limit_fok),
        1 => Just(OrderType::Market),
        1 => arb_limit_price().prop_map(OrderType::stop_market),
        1 => (arb_limit_price(), arb_limit_price())
            .prop_map(|(trigger, price)| OrderType::stop_limit(trigger, price)),
    ]
}

fn arb_order() -> impl Strategy<Value = Order> {
    (arb_side(), arb_order_type(), 1..=50u64, 0..4u8).prop_map(
        |(side, order_type, quantity, client)| {
            Order::builder()
                // submit() overrides this with an engine-assigned id
                .exchange_id("caller-provided")
                .client_id(format!("client-{client}"))
                .order_type(order_type)
                .side(side)
                .quantity(qty(quantity))
                .build()
        },
    )
}

fn arb_stream() -> impl Strategy<Value = Vec<Order>> {
    prop::collection::vec(arb_order(), 1..60)
}

/// A stream in which no two orders share a client.
///
/// Only `every_matcher_agrees_on_the_totals` uses this, and it must. With the
/// shared 4-client pool the books legitimately diverge in *composition* — FIFO
/// consumes alice's order whole where pro-rata leaves her a remainder — and a
/// later self-trade cancellation then removes different amounts of depth from
/// each. That is correct behaviour in both, so the differential property has
/// to remove self-trading from the picture to say anything at all.
fn arb_stream_unique_clients() -> impl Strategy<Value = Vec<Order>> {
    prop::collection::vec((arb_side(), arb_order_type(), 1..=50u64), 1..40).prop_map(|draws| {
        draws
            .into_iter()
            .enumerate()
            .map(|(i, (side, order_type, quantity))| {
                Order::builder()
                    .exchange_id("caller-provided")
                    .client_id(format!("client-{i}"))
                    .order_type(order_type)
                    .side(side)
                    .quantity(qty(quantity))
                    .build()
            })
            .collect()
    })
}

/// The same tick grid on a lot of ten.
fn lot_ten_spec() -> InstrumentSpec {
    let spec = InstrumentSpec::new(2, 0, 25, 10).expect("2/0/25/10 is a valid spec");
    let min = spec.price_from_minor(9_500).unwrap();
    let max = spec.price_from_minor(10_500).unwrap();
    spec.with_price_range(min, Some(max)).unwrap()
}

/// Orders whose quantities are whole lots of ten. Drawn in *lots* and
/// multiplied up, rather than drawn in base units and filtered: `qty_from_base`
/// rejects an off-lot count outright, so a filter would throw most of the
/// stream away.
fn arb_lot_ten_order() -> impl Strategy<Value = Order> {
    (arb_side(), arb_order_type(), 1..=20u64, 0..4u8).prop_map(
        |(side, order_type, lots, client)| {
            Order::builder()
                .exchange_id("caller-provided")
                .client_id(format!("client-{client}"))
                .order_type(order_type)
                .side(side)
                .quantity(
                    lot_ten_spec()
                        .qty_from_base(lots * 10)
                        .expect("drawn in whole lots"),
                )
                .build()
        },
    )
}

// --- helpers ------------------------------------------------------------

fn locations<M: MatchingAlgorithm, S: OrderBookStore>(
    book: &OrderBook<M, S>,
) -> HashMap<ExchangeId, OrderLocation> {
    book.order_ids()
        .map(|id| (id.clone(), book.order_location(id).unwrap()))
        .collect()
}

fn opposite(side: Side) -> Side {
    match side {
        Side::Bid => Side::Ask,
        Side::Ask => Side::Bid,
    }
}

/// Everything resting in the book right now: id → (side, price, remaining).
/// Parked stops are deliberately NOT part of this — they hold no depth.
fn resting_snapshot<M: MatchingAlgorithm, S: OrderBookStore>(
    book: &OrderBook<M, S>,
) -> HashMap<ExchangeId, (Side, Price, Qty)> {
    book.levels(Side::Bid)
        .chain(book.levels(Side::Ask))
        .flat_map(|level| {
            level.orders().map(move |o| {
                (
                    o.exchange_id.clone(),
                    (o.side, level.price, o.remaining_quantity),
                )
            })
        })
        .collect()
}

fn total_depth<M: MatchingAlgorithm, S: OrderBookStore>(book: &OrderBook<M, S>) -> Qty {
    book.levels(Side::Bid)
        .chain(book.levels(Side::Ask))
        .map(|level| level.total_quantity())
        .sum()
}

/// Every price that currently holds a level, bids then asks, in book order.
/// Every live level as (price, depth), bids then asks, in book order.
///
/// Depth per level, not just the price: contract (4) fixes how much each level
/// gives up, so two conforming matchers must agree level by level and not
/// merely in total. Comparing prices alone would let a policy that moved a fill
/// from one level to another through — `(100: 5, 101: 10)` and `(100: 6,
/// 101: 9)` have identical keys, identical totals, and identical touch prices.
fn level_depths<M: MatchingAlgorithm, S: OrderBookStore>(
    book: &OrderBook<M, S>,
) -> Vec<(Price, Qty)> {
    book.levels(Side::Bid)
        .chain(book.levels(Side::Ask))
        .map(|level| (level.price, level.total_quantity()))
        .collect()
}

/// Remaining quantity of `id` if it rests in the BOOK (not the stop book).
fn rested_remaining<M: MatchingAlgorithm, S: OrderBookStore>(
    book: &OrderBook<M, S>,
    id: &ExchangeId,
) -> Option<Qty> {
    match book.order_location(id)? {
        OrderLocation::Book { .. } => book.get_order(id).map(|o| o.remaining_quantity),
        OrderLocation::StopBook { .. } => None,
    }
}

/// Remaining quantity of `id` if it's parked in the stop book.
fn parked_remaining<M: MatchingAlgorithm, S: OrderBookStore>(
    book: &OrderBook<M, S>,
    id: &ExchangeId,
) -> Option<Qty> {
    match book.order_location(id)? {
        OrderLocation::StopBook { .. } => book.get_order(id).map(|o| o.remaining_quantity),
        OrderLocation::Book { .. } => None,
    }
}

/// The submitted order's report plus every triggered-stop report, in
/// execution order.
fn all_reports(report: &ExecutionReport) -> impl Iterator<Item = &ExecutionReport> {
    std::iter::once(report).chain(report.triggered.iter())
}

/// Total traded quantity per maker id across the whole cascade — needed
/// because an order can rest and then be (partially) consumed or cancelled
/// within the SAME submit once stops chain.
fn fills_by_maker(report: &ExecutionReport) -> HashMap<ExchangeId, Qty> {
    let mut fills: HashMap<ExchangeId, Qty> = HashMap::new();
    for r in all_reports(report) {
        for t in &r.trades {
            *fills.entry(t.maker_order_id.clone()).or_default() += t.quantity;
        }
    }
    fills
}

/// Everything this submit traded, cascade included.
fn cascade_filled(report: &ExecutionReport) -> Qty {
    all_reports(report)
        .flat_map(|r| &r.trades)
        .map(|t| t.quantity)
        .sum()
}

/// Everything two books running different policies must still agree on.
///
/// Generic over both policies because the books are different *types* — the
/// one place in the suite where static dispatch costs something.
fn same_public_state<A: MatchingAlgorithm, B: MatchingAlgorithm, S: OrderBookStore>(
    name: &str,
    other: &OrderBook<A, S>,
    base: &OrderBook<B, S>,
) -> Result<(), TestCaseError> {
    prop_assert_eq!(total_depth(other), total_depth(base), "{} depth", name);
    prop_assert_eq!(other.best_bid(), base.best_bid(), "{} best bid", name);
    prop_assert_eq!(other.best_ask(), base.best_ask(), "{} best ask", name);
    prop_assert_eq!(
        other.last_trade_price,
        base.last_trade_price,
        "{} last trade price",
        name
    );
    // `level_depths` subsumes the total above, but the total is kept: on a
    // twenty-level book it fails with one number instead of a twenty-pair diff,
    // which is the difference between reading a failure and decoding one.
    prop_assert_eq!(level_depths(other), level_depths(base), "{} levels", name);
    Ok(())
}

/// Differential oracle for the storage axis: with the matcher held fixed, a
/// backend is not allowed to change even maker identity or report ordering.
fn check_storage_agreement<M: MatchingAlgorithm + Clone>(
    matcher: M,
    stream: &[Order],
) -> Result<(), TestCaseError> {
    let mut tree = OrderBook::<M, BTreeStore>::try_new(spec(), matcher.clone()).unwrap();
    let mut ladder = OrderBook::<M, TickLadderStore>::try_new(spec(), matcher.clone()).unwrap();
    let mut hash_map = OrderBook::<M, HashMapStore>::try_new(spec(), matcher).unwrap();

    for order in stream.iter().cloned() {
        let tree_report = tree.submit(order.clone()).unwrap();
        let ladder_report = ladder.submit(order.clone()).unwrap();
        let hash_map_report = hash_map.submit(order).unwrap();
        prop_assert_eq!(&ladder_report, &tree_report, "execution reports diverged");
        prop_assert_eq!(&hash_map_report, &tree_report, "execution reports diverged");
        prop_assert_eq!(resting_snapshot(&ladder), resting_snapshot(&tree));
        prop_assert_eq!(resting_snapshot(&hash_map), resting_snapshot(&tree));
        prop_assert_eq!(level_depths(&ladder), level_depths(&tree));
        prop_assert_eq!(level_depths(&hash_map), level_depths(&tree));
        prop_assert_eq!(ladder.best_bid(), tree.best_bid());
        prop_assert_eq!(hash_map.best_bid(), tree.best_bid());
        prop_assert_eq!(ladder.best_ask(), tree.best_ask());
        prop_assert_eq!(hash_map.best_ask(), tree.best_ask());
        prop_assert_eq!(ladder.last_trade_price, tree.last_trade_price);
        prop_assert_eq!(hash_map.last_trade_price, tree.last_trade_price);
        prop_assert_eq!(locations(&ladder), locations(&tree));
        prop_assert_eq!(locations(&hash_map), locations(&tree));
        prop_assert_eq!(&ladder.stop_bids, &tree.stop_bids);
        prop_assert_eq!(&hash_map.stop_bids, &tree.stop_bids);
        prop_assert_eq!(&ladder.stop_asks, &tree.stop_asks);
        prop_assert_eq!(&hash_map.stop_asks, &tree.stop_asks);
        prop_assert_eq!(ladder.next_seq, tree.next_seq);
        prop_assert_eq!(hash_map.next_seq, tree.next_seq);
        prop_assert_eq!(ladder.next_arrival, tree.next_arrival);
        prop_assert_eq!(hash_map.next_arrival, tree.next_arrival);
    }
    Ok(())
}

// --- invariant bodies ---------------------------------------------------
//
// One generic function per invariant, taking the policy as a value. They are
// plain functions rather than `proptest!` bodies so that a single drawn stream
// can be pushed through several of them; `prop_assert!` expands to a `return
// Err(TestCaseError::fail(..))`, so it works unchanged in anything returning
// this type.

/// Invariant 1 — conservation: for every execution (the submitted order
/// and each triggered stop), the original quantity is fully accounted for
/// by fills + rested remainder + killed remainder, and the reported
/// outcome agrees with where the quantity went. "Rested" is checked
/// net of any quantity that later executions in the same cascade already
/// consumed from it.
fn check_conservation<M: MatchingAlgorithm, S: OrderBookStore>(
    matcher: M,
    stream: &[Order],
) -> Result<(), TestCaseError> {
    let mut book = OrderBook::<M, S>::try_new(spec(), matcher).unwrap();
    // original quantity of every currently-parked stop, by assigned id
    let mut parked: HashMap<ExchangeId, Qty> = HashMap::new();

    for order in stream.iter().cloned() {
        let original = order.original_quantity;
        let order_type = order.order_type;
        // order types whose unfilled remainder is discarded, not rested
        let can_kill = matches!(
            order.order_type,
            OrderType::Market
                | OrderType::Limit { tif: TimeInForce::IOC | TimeInForce::FOK, .. }
                // an already-triggered stop activates immediately as market/limit
                | OrderType::StopMarket { .. }
                | OrderType::StopLimit { tif: TimeInForce::IOC | TimeInForce::FOK, .. }
        );
        let is_fok = matches!(
            order.order_type,
            OrderType::Limit {
                tif: TimeInForce::FOK,
                ..
            }
        );

        let report = book.submit(order).unwrap();
        let consumed_later = fills_by_maker(&report);

        for trade in all_reports(&report).flat_map(|r| &r.trades) {
            prop_assert!(!trade.quantity.is_zero(), "zero-quantity trade");
        }

        // an order that rested during this submit can afterwards be
        // self-trade-CANCELLED (not traded) by a same-client triggered
        // stop; its remainder then vanishes without a fill to count
        let stp_cancelled =
            |id: &ExchangeId| all_reports(&report).any(|r| r.cancelled.contains(id));

        // -- the submitted order itself --
        let filled: Qty = report.trades.iter().map(|t| t.quantity).sum();
        let rested = rested_remaining(&book, &report.order_id);
        let eaten = consumed_later
            .get(&report.order_id)
            .copied()
            .unwrap_or(Qty::ZERO);
        let cancelled_later = stp_cancelled(&report.order_id);
        match report.outcome {
            SubmitOutcome::Filled => {
                prop_assert_eq!(filled, original);
                prop_assert_eq!(rested, None);
            }
            SubmitOutcome::Rested => {
                prop_assert!(report.trades.is_empty());
                if cancelled_later {
                    prop_assert_eq!(rested, None);
                } else {
                    prop_assert_eq!(rested.unwrap_or(Qty::ZERO) + eaten, original);
                }
            }
            SubmitOutcome::PartiallyFilledAndRested => {
                prop_assert!(!filled.is_zero() && filled < original);
                if cancelled_later {
                    prop_assert_eq!(rested, None);
                } else {
                    prop_assert_eq!(rested.unwrap_or(Qty::ZERO) + eaten, original - filled);
                }
            }
            SubmitOutcome::Killed => {
                prop_assert!(can_kill, "only market/IOC/FOK remainders are killed");
                prop_assert!(filled < original);
                if is_fok {
                    // all-or-nothing: a killed FOK executed NOTHING —
                    // no fills and no self-trade cancellations
                    prop_assert_eq!(filled, Qty::ZERO, "FOK partially filled");
                    prop_assert!(report.cancelled.is_empty());
                }
                prop_assert_eq!(rested, None);
            }
            SubmitOutcome::StopPending => {
                let was_stop = matches!(
                    order_type,
                    OrderType::StopMarket { .. } | OrderType::StopLimit { .. }
                );
                prop_assert!(was_stop, "only stops can park");
                prop_assert!(report.trades.is_empty());
                prop_assert_eq!(parked_remaining(&book, &report.order_id), Some(original));
                parked.insert(report.order_id.clone(), original);
            }
        }

        // -- every stop this submit triggered --
        for r in &report.triggered {
            let parked_original = parked
                .remove(&r.order_id)
                .expect("triggered stop must have been parked earlier");
            prop_assert!(r.triggered.is_empty(), "cascade reports must be flat");

            let filled: Qty = r.trades.iter().map(|t| t.quantity).sum();
            let rested = rested_remaining(&book, &r.order_id);
            let eaten = consumed_later
                .get(&r.order_id)
                .copied()
                .unwrap_or(Qty::ZERO);
            let cancelled_later = stp_cancelled(&r.order_id);
            match r.outcome {
                SubmitOutcome::Filled => {
                    prop_assert_eq!(filled, parked_original);
                    prop_assert_eq!(rested, None);
                }
                SubmitOutcome::Rested => {
                    prop_assert!(r.trades.is_empty());
                    if cancelled_later {
                        prop_assert_eq!(rested, None);
                    } else {
                        prop_assert_eq!(rested.unwrap_or(Qty::ZERO) + eaten, parked_original);
                    }
                }
                SubmitOutcome::PartiallyFilledAndRested => {
                    prop_assert!(!filled.is_zero() && filled < parked_original);
                    if cancelled_later {
                        prop_assert_eq!(rested, None);
                    } else {
                        prop_assert_eq!(
                            rested.unwrap_or(Qty::ZERO) + eaten,
                            parked_original - filled
                        );
                    }
                }
                SubmitOutcome::Killed => {
                    prop_assert!(filled < parked_original);
                    prop_assert_eq!(rested, None);
                }
                SubmitOutcome::StopPending => {
                    prop_assert!(false, "an activated stop can never re-park");
                }
            }
        }
    }
    Ok(())
}

/// Invariant 2 — depth accounting: each submit (cascade included) changes
/// total book depth by exactly (quantity that came to rest) − (quantity
/// filled) − (quantity self-trade-cancelled). Rest-time quantities are
/// used, so intra-cascade consumption of freshly rested orders balances.
///
/// This is the invariant that catches an apply pass which removes an order
/// from a level without recording it — the STP pre-pass most of all.
fn check_depth_accounting<M: MatchingAlgorithm, S: OrderBookStore>(
    matcher: M,
    stream: &[Order],
) -> Result<(), TestCaseError> {
    let mut book = OrderBook::<M, S>::try_new(spec(), matcher).unwrap();
    let mut parked: HashMap<ExchangeId, Qty> = HashMap::new();

    for order in stream.iter().cloned() {
        let original = order.original_quantity;
        let before = resting_snapshot(&book);
        let depth_before = total_depth(&book);

        let report = book.submit(order).unwrap();
        let fills = fills_by_maker(&report);

        let mut filled_total = Qty::ZERO;
        let mut rested_total = Qty::ZERO; // at rest time
        let mut cancelled_total = Qty::ZERO; // remaining at cancel time
        // rest-time quantity per order that rested during THIS submit
        let mut rest_qty: HashMap<ExchangeId, Qty> = HashMap::new();

        for (i, r) in all_reports(&report).enumerate() {
            let exec_original = if i == 0 {
                original
            } else {
                *parked.get(&r.order_id).expect("triggered stop was parked")
            };
            let filled: Qty = r.trades.iter().map(|t| t.quantity).sum();
            filled_total += filled;

            if matches!(
                r.outcome,
                SubmitOutcome::Rested | SubmitOutcome::PartiallyFilledAndRested
            ) {
                let at_rest = exec_original - filled;
                rested_total += at_rest;
                rest_qty.insert(r.order_id.clone(), at_rest);
            }

            for id in &r.cancelled {
                // remaining at cancel = what it had when it (last) rested
                // minus everything traded against it this submit
                let start = before
                    .get(id)
                    .map(|(_, _, remaining)| *remaining)
                    .or_else(|| rest_qty.get(id).copied())
                    .expect("cancelled order rested before or during this submit");
                cancelled_total += start - fills.get(id).copied().unwrap_or(Qty::ZERO);
            }
        }
        for r in &report.triggered {
            parked.remove(&r.order_id);
        }
        if report.outcome == SubmitOutcome::StopPending {
            parked.insert(report.order_id.clone(), original);
        }

        prop_assert_eq!(
            i128::from(total_depth(&book).base()),
            i128::from(depth_before.base()) + i128::from(rested_total.base())
                - i128::from(filled_total.base())
                - i128::from(cancelled_total.base())
        );
    }
    Ok(())
}

/// Invariant 3 — price validity: every trade prints at the maker's resting
/// price, on the opposite side, never worse than a limit taker's price,
/// and each execution sweeps prices best-first (monotonically).
///
/// Also the home of the flat assertion that self-trade prevention works at
/// all: no trade ever has the same client on both sides. Nothing else in the
/// suite said so, and the STP rule changed in this phase.
fn check_price_validity<M: MatchingAlgorithm, S: OrderBookStore>(
    matcher: M,
    stream: &[Order],
) -> Result<(), TestCaseError> {
    let mut book = OrderBook::<M, S>::try_new(spec(), matcher).unwrap();

    for order in stream.iter().cloned() {
        let taker_side = order.side;
        // Some(price) for limit takers, None for market takers; a stop's
        // limit applies to the order it becomes, checked via `triggered`
        let limit = match order.order_type {
            OrderType::Limit { price, .. } => Some(price),
            _ => None,
        };

        let before = resting_snapshot(&book);
        let report = book.submit(order).unwrap();

        for (i, r) in all_reports(&report).enumerate() {
            let mut prev_price: Option<Price> = None;
            for trade in &r.trades {
                prop_assert_ne!(
                    &trade.maker_client,
                    &trade.taker_client,
                    "a client traded with itself"
                );

                // makers that rested before this submit must print at
                // their snapshot price; makers that rested mid-cascade
                // are covered by invariant 6's level==order price check
                if let Some((maker_side, maker_price, _)) = before.get(&trade.maker_order_id) {
                    prop_assert_eq!(
                        trade.price,
                        *maker_price,
                        "trade must print at maker's price"
                    );
                    prop_assert_eq!(*maker_side, opposite(trade.taker_side));
                }
                prop_assert_eq!(&trade.taker_order_id, &r.order_id);

                if i == 0 {
                    prop_assert_eq!(trade.taker_side, taker_side);
                    if let Some(limit) = limit {
                        match taker_side {
                            Side::Bid => prop_assert!(trade.price <= limit),
                            Side::Ask => prop_assert!(trade.price >= limit),
                        }
                    }
                }

                // best price first within one execution's sweep
                if let Some(prev) = prev_price {
                    match trade.taker_side {
                        Side::Bid => prop_assert!(trade.price >= prev),
                        Side::Ask => prop_assert!(trade.price <= prev),
                    }
                }
                prev_price = Some(trade.price);
            }
        }
    }
    Ok(())
}

/// Invariant 4, universal half — queue order within one execution.
///
/// At one price, in one execution, makers are traded oldest-first. This is
/// what survives of time priority when the policy stops being FIFO: contract
/// (1) makes the fills index-ascending, and the projection handed to the
/// matcher is the queue in arrival order, so the trades come back in queue
/// order whoever allocated them. A projection or apply pass that reordered a
/// level fails here.
///
/// Per *execution*, not per cascade: each triggered stop is its own sweep and
/// may legitimately revisit a price the first sweep already traded at.
fn check_queue_order_within_execution<M: MatchingAlgorithm, S: OrderBookStore>(
    matcher: M,
    stream: &[Order],
) -> Result<(), TestCaseError> {
    let mut book = OrderBook::<M, S>::try_new(spec(), matcher).unwrap();
    let mut next_stamp: u64 = 0;
    let mut rest_stamp: HashMap<ExchangeId, u64> = HashMap::new();

    for order in stream.iter().cloned() {
        let report = book.submit(order).unwrap();

        for r in all_reports(&report) {
            let mut last_traded: HashMap<(bool, Price), u64> = HashMap::new();
            for trade in &r.trades {
                let maker_is_bid = opposite(trade.taker_side) == Side::Bid;
                let stamp = *rest_stamp
                    .get(&trade.maker_order_id)
                    .expect("maker must have rested before trading");
                let key = (maker_is_bid, trade.price);
                if let Some(&prev) = last_traded.get(&key) {
                    prop_assert!(
                        stamp >= prev,
                        "queue order violated at {:?}: maker stamped {} traded after {}",
                        trade.price,
                        stamp,
                        prev
                    );
                }
                last_traded.insert(key, stamp);
            }
            if matches!(
                r.outcome,
                SubmitOutcome::Rested | SubmitOutcome::PartiallyFilledAndRested
            ) {
                rest_stamp.insert(r.order_id.clone(), next_stamp);
                next_stamp += 1;
            }
        }
    }
    Ok(())
}

/// Invariant 5 — no crossing: after every submit the book is uncrossed.
///
/// Quietly load-bearing for the STP design: this is the property that rules
/// out "skip the self order but leave it resting". A taker's remainder would
/// come to rest through its own untouched order on the other side and the
/// book would sit crossed — which is why the engine cancels rather than skips.
fn check_never_crossed<M: MatchingAlgorithm, S: OrderBookStore>(
    matcher: M,
    stream: &[Order],
) -> Result<(), TestCaseError> {
    let mut book = OrderBook::<M, S>::try_new(spec(), matcher).unwrap();

    for order in stream.iter().cloned() {
        book.submit(order).unwrap();
        if let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) {
            prop_assert!(bid < ask, "book crossed: bid {bid:?} >= ask {ask:?}");
        }
    }
    Ok(())
}

/// Invariant 6 — index consistency: after any stream, `index`, the price
/// levels, AND the stop books describe exactly the same set of live
/// orders; every level/queue is non-empty and internally consistent; and
/// cancelling every indexed id drains everything to empty.
///
/// The `!remaining_quantity.is_zero()` checks are the ones an apply pass that
/// forgets to drop an exhausted maker fails.
fn check_index_consistency<M: MatchingAlgorithm, S: OrderBookStore>(
    matcher: M,
    stream: &[Order],
) -> Result<(), TestCaseError> {
    let mut book = OrderBook::<M, S>::try_new(spec(), matcher).unwrap();
    for order in stream.iter().cloned() {
        book.submit(order).unwrap();
    }

    let mut live_orders = 0usize;
    for level in book.levels(Side::Ask) {
        prop_assert!(!level.is_empty(), "empty level left in asks");
        prop_assert_eq!(level.side, Side::Ask);
        for o in level.orders() {
            live_orders += 1;
            prop_assert!(!o.remaining_quantity.is_zero());
            // everything resting must be a limit at its level's price
            prop_assert_eq!(o.order_type.limit_price(), Some(level.price));
            let expected = OrderLocation::Book {
                side: Side::Ask,
                price: level.price,
            };
            prop_assert_eq!(book.order_location(&o.exchange_id), Some(expected));
        }
    }
    for level in book.levels(Side::Bid) {
        prop_assert!(!level.is_empty(), "empty level left in bids");
        prop_assert_eq!(level.side, Side::Bid);
        for o in level.orders() {
            live_orders += 1;
            prop_assert!(!o.remaining_quantity.is_zero());
            prop_assert_eq!(o.order_type.limit_price(), Some(level.price));
            let expected = OrderLocation::Book {
                side: Side::Bid,
                price: level.price,
            };
            prop_assert_eq!(book.order_location(&o.exchange_id), Some(expected));
        }
    }
    for (side, stops) in [(Side::Bid, &book.stop_bids), (Side::Ask, &book.stop_asks)] {
        for (trigger, queue) in stops {
            prop_assert!(!queue.is_empty(), "empty stop queue left behind");
            for o in queue {
                live_orders += 1;
                prop_assert_eq!(o.side, side);
                prop_assert!(!o.remaining_quantity.is_zero());
                let is_stop = matches!(
                    o.order_type,
                    OrderType::StopMarket { .. } | OrderType::StopLimit { .. }
                );
                prop_assert!(is_stop, "non-stop order parked in the stop book");
                let expected = OrderLocation::StopBook {
                    side,
                    trigger: *trigger,
                };
                prop_assert_eq!(book.order_location(&o.exchange_id), Some(expected));
            }
        }
    }
    prop_assert_eq!(
        live_orders,
        book.order_count(),
        "index and structures disagree"
    );

    let ids: Vec<ExchangeId> = book.order_ids().cloned().collect();
    for id in ids {
        prop_assert!(book.cancel_order(id).is_ok());
    }
    prop_assert_eq!(book.levels(Side::Bid).count(), 0);
    prop_assert_eq!(book.levels(Side::Ask).count(), 0);
    prop_assert!(book.stop_bids.is_empty());
    prop_assert!(book.stop_asks.is_empty());
    prop_assert!(book.order_count() == 0);
    Ok(())
}

/// Invariant 7 — stop discipline: after every submit, every still-pending
/// stop's trigger is strictly beyond the last trade price. If it weren't,
/// the cascade failed to fire it.
fn check_stop_discipline<M: MatchingAlgorithm, S: OrderBookStore>(
    matcher: M,
    stream: &[Order],
) -> Result<(), TestCaseError> {
    let mut book = OrderBook::<M, S>::try_new(spec(), matcher).unwrap();

    for order in stream.iter().cloned() {
        book.submit(order).unwrap();
        if let Some(last) = book.last_trade_price {
            for trigger in book.stop_bids.keys() {
                prop_assert!(
                    *trigger > last,
                    "pending buy stop at {:?} but market already traded {:?}",
                    trigger,
                    last
                );
            }
            for trigger in book.stop_asks.keys() {
                prop_assert!(
                    *trigger < last,
                    "pending sell stop at {:?} but market already traded {:?}",
                    trigger,
                    last
                );
            }
        }
    }
    Ok(())
}

/// Invariant 8 — lattice closure.
///
/// Note where this is asserted: on the `Decimal` the spec renders, not
/// on the integer it stores. Asking whether `price.minor()` is a tick
/// multiple would be unfalsifiable — `Price` cannot hold anything else,
/// which is the entire point of the type — so the interesting question
/// is whether the *conversion* still agrees. A `to_decimal` that
/// dropped a digit, or a scale that drifted, shows up here and nowhere
/// else.
///
/// Matching is what makes this worth checking rather than obvious:
/// fills subtract from resting quantities on every trade, so a level
/// that started on the grid has been arithmetic'd many times by the
/// end of a 60-order stream. Under a weighted policy it is worse than that —
/// pro-rata *divides* quantities, and a floor pass that forgot the lot would
/// leave every maker it touched off the grid.
fn check_lattice_closure<M: MatchingAlgorithm, S: OrderBookStore>(
    matcher: M,
    spec: InstrumentSpec,
    stream: &[Order],
) -> Result<(), TestCaseError> {
    let tick = spec.tick_decimal();
    let lot = spec.lot_decimal();
    let mut book = OrderBook::<M, S>::try_new(spec, matcher).unwrap();

    for order in stream.iter().cloned() {
        let report = book.submit(order).unwrap();

        for trade in all_reports(&report).flat_map(|r| &r.trades) {
            let q = spec.qty_to_decimal(trade.quantity);
            prop_assert_eq!(q % lot, Decimal::ZERO, "trade quantity off the lot grid");
            let p = spec.to_decimal(trade.price);
            prop_assert_eq!(p % tick, Decimal::ZERO, "trade price off the tick grid");
        }
    }

    let levels = book.levels(Side::Bid).chain(book.levels(Side::Ask));
    for level in levels {
        let p = spec.to_decimal(level.price);
        prop_assert_eq!(p % tick, Decimal::ZERO, "level price off the tick grid");
        prop_assert_eq!(
            spec.price(p).unwrap(),
            level.price,
            "price lost in conversion"
        );

        for o in level.orders() {
            let q = spec.qty_to_decimal(o.remaining_quantity);
            prop_assert_eq!(q % lot, Decimal::ZERO, "resting quantity off the lot grid");
            prop_assert_eq!(
                spec.qty(q).unwrap(),
                o.remaining_quantity,
                "quantity lost in conversion"
            );
        }
    }

    let parked = book.stop_bids.iter().chain(book.stop_asks.iter());
    for (trigger, queue) in parked {
        let t = spec.to_decimal(*trigger);
        prop_assert_eq!(t % tick, Decimal::ZERO, "stop trigger off the tick grid");
        for o in queue {
            let q = spec.qty_to_decimal(o.remaining_quantity);
            prop_assert_eq!(q % lot, Decimal::ZERO, "parked quantity off the lot grid");
        }
    }
    Ok(())
}

/// Invariant 9 — a rejected order is not an order.
///
/// Admission runs before the exchange id is minted, so a reject must
/// leave *everything* as it was, `next_seq` included. Burning a
/// sequence number on something that never became an order would punch
/// a hole in the id space; asserting on `next_seq` is what makes that
/// choice a checked property rather than a comment.
fn check_reject_is_a_no_op<M: MatchingAlgorithm, S: OrderBookStore>(
    matcher: M,
    stream: &[Order],
    too_small: u64,
    side: Side,
    price: Price,
) -> Result<(), TestCaseError> {
    let mut book = OrderBook::<M, S>::try_new(bounded_spec(), matcher).unwrap();
    for order in stream.iter().cloned() {
        // some of these are themselves rejected — that is the point
        let _ = book.submit(order);
    }

    let resting_before = resting_snapshot(&book);
    let depth_before = total_depth(&book);
    let seq_before = book.next_seq;
    let arrival_before = book.next_arrival;
    let stops_before = book.stop_bids.len() + book.stop_asks.len();

    let bad = Order::builder()
        .side(side)
        .order_type(OrderType::limit_gtc(price))
        .quantity(qty(too_small))
        .client_id("rejected")
        .exchange_id("rejected")
        .build();

    let err = book.submit(bad).expect_err("below the size minimum");
    prop_assert!(matches!(err, OrderBookError::Rejected(_)), "got {err:?}");

    prop_assert_eq!(book.next_seq, seq_before, "a reject must not burn an id");
    prop_assert_eq!(
        book.next_arrival,
        arrival_before,
        "a reject must not take a place in line"
    );
    prop_assert_eq!(total_depth(&book), depth_before);
    prop_assert_eq!(resting_snapshot(&book), resting_before);
    prop_assert_eq!(book.stop_bids.len() + book.stop_asks.len(), stops_before);
    Ok(())
}

// --- properties ---------------------------------------------------------

proptest! {

#[test]
fn conservation_per_submit(stream in arb_stream()) {
    for_each_configuration!(check_conservation, &stream);
}

#[test]
fn book_depth_accounting(stream in arb_stream()) {
    for_each_configuration!(check_depth_accounting, &stream);
}

#[test]
fn trades_at_maker_price_within_taker_limit(stream in arb_stream()) {
    for_each_configuration!(check_price_validity, &stream);
}

#[test]
fn queue_order_within_one_execution(stream in arb_stream()) {
    for_each_configuration!(check_queue_order_within_execution, &stream);
}

#[test]
fn book_never_crossed_after_submit(stream in arb_stream()) {
    for_each_configuration!(check_never_crossed, &stream);
}

#[test]
fn index_matches_levels_and_book_drains(stream in arb_stream()) {
    for_each_configuration!(check_index_consistency, &stream);
}

#[test]
fn no_satisfied_stop_left_pending(stream in arb_stream()) {
    for_each_configuration!(check_stop_discipline, &stream);
}

#[test]
fn everything_the_book_holds_stays_on_the_lattice(
    stream in prop::collection::vec(arb_order(), 1..60)
) {
    for_each_configuration!(check_lattice_closure, spec(), &stream);
}

#[test]
fn a_rejected_order_leaves_the_book_untouched(
    stream in prop::collection::vec(arb_order(), 0..30),
    too_small in 1u64..5,
    side in arb_side(),
    price in arb_limit_price(),
) {
    for_each_configuration!(check_reject_is_a_no_op, &stream, too_small, side, price);
}

#[test]
fn storage_backends_are_observationally_identical(stream in arb_stream()) {
    check_storage_agreement(FifoMatcher, &stream)?;
    check_storage_agreement(ProRataMatcher::new(lot()), &stream)?;
    check_storage_agreement(TimeProRataMatcher::new(lot()), &stream)?;
}

/// Invariant 4, FIFO half — the strong form of time priority: at any given
/// price (and maker side), makers are consumed in the order they RESTED
/// there, across the whole stream. Submission sequence is not the right
/// proxy once stops exist — a stop submitted early (low sequence) can
/// trigger and rest late, and correctly queues behind orders already at its
/// level. So the test stamps every order at the moment it rests and checks
/// stamps never decrease per level.
///
/// **This one cannot be universal, and the counterexample is short.** Makers
/// A(10) and B(10) rest at 100, in that order. A taker for 4 arrives: pro-rata
/// fills A=2 and B=2, so the last maker traded at 100 is B. A second taker for
/// 4 arrives and touches A again — stamp(A) < stamp(B), and the assert below
/// fires. FIFO's "one maker at a time, front first" is exactly what a
/// proportional policy abolishes; what survives is
/// `queue_order_within_one_execution`.
#[test]
fn fifo_same_price_priority(stream in arb_stream()) {
    let mut book = OrderBook::new(spec(), FifoMatcher);
    let mut next_stamp: u64 = 0;
    // order id → when it (last) came to rest in the book
    let mut rest_stamp: HashMap<ExchangeId, u64> = HashMap::new();
    // (maker side is Bid?, price) → stamp of the last maker traded there
    let mut last_traded: HashMap<(bool, Price), u64> = HashMap::new();

    for order in stream {
        let report = book.submit(order).unwrap();

        // walk executions in order: an execution can only consume orders
        // that rested strictly before it, so stamps are always assigned
        // before they're needed
        for r in all_reports(&report) {
            for trade in &r.trades {
                let maker_is_bid = opposite(trade.taker_side) == Side::Bid;
                let stamp = *rest_stamp
                    .get(&trade.maker_order_id)
                    .expect("maker must have rested before trading");
                let key = (maker_is_bid, trade.price);
                if let Some(&prev) = last_traded.get(&key) {
                    prop_assert!(
                        stamp >= prev,
                        "FIFO violated at {:?}: maker stamped {} traded after {}",
                        trade.price, stamp, prev
                    );
                }
                last_traded.insert(key, stamp);
            }
            if matches!(
                r.outcome,
                SubmitOutcome::Rested | SubmitOutcome::PartiallyFilledAndRested
            ) {
                rest_stamp.insert(r.order_id.clone(), next_stamp);
                next_stamp += 1;
            }
        }
    }
}

/// The abstraction's central claim, asserted head-on: the policy decides
/// **who** fills, never **how much** the book fills.
///
/// Contract clause (4) fixes the total allocated at every level, so three
/// books fed the same stream must agree, submit by submit, on the outcome,
/// the traded quantity, the cascade length, total depth, both touch prices,
/// the set of live levels, and the last trade price. Everything they are
/// allowed to disagree about is *inside* a level.
///
/// Distinct clients throughout — see `arb_stream_unique_clients`.
#[test]
fn every_matcher_agrees_on_the_totals(stream in arb_stream_unique_clients()) {
    let mut fifo = OrderBook::new(spec(), FifoMatcher);
    let mut pro_rata = OrderBook::new(spec(), ProRataMatcher::new(lot()));
    let mut timed = OrderBook::new(spec(), TimeProRataMatcher::new(lot()));

    for order in stream {
        let base = fifo.submit(order.clone()).unwrap();
        let others = [
            ("pro-rata", pro_rata.submit(order.clone()).unwrap()),
            ("time-pro-rata", timed.submit(order).unwrap()),
        ];

        // No execution anywhere may cancel: every client in this stream owns
        // exactly one order, so self-trade prevention has nothing to fire on.
        // Checked on the base too, and through the whole cascade — a wrong
        // cancellation that FIFO also makes, or that only an activated stop
        // makes, is invisible to a purely differential comparison.
        for (name, report) in [("fifo", &base), ("pro-rata", &others[0].1), ("time-pro-rata", &others[1].1)] {
            for (i, r) in all_reports(report).enumerate() {
                prop_assert!(
                    r.cancelled.is_empty(),
                    "{} cancelled {:?} in execution {} — no client owns two orders here",
                    name, r.cancelled, i
                );
            }
        }

        for (name, report) in &others {
            prop_assert_eq!(report.outcome, base.outcome, "{} disagreed on the outcome", name);
            prop_assert_eq!(
                cascade_filled(report), cascade_filled(&base),
                "{} disagreed on how much filled", name
            );
            prop_assert_eq!(
                report.triggered.len(), base.triggered.len(),
                "{} disagreed on the cascade", name
            );
        }

        // two calls rather than a loop: the books are different *types*, so
        // there is no array to iterate — the price of static dispatch, paid
        // here and nowhere else in the suite
        same_public_state("pro-rata", &pro_rata, &fifo)?;
        same_public_state("time-pro-rata", &timed, &fifo)?;
    }
}

/// The gap the main suite structurally cannot see.
///
/// `spec()` has a lot of **one**, so "every fill is a whole lot" is vacuously
/// true there — the suite could not tell a lot-aware engine from a lot-blind
/// one. On a lot of ten it can: a pro-rata floor pass that divided without
/// flooring to the lot leaves makers resting off the grid, and the conversion
/// check below rejects them.
///
/// One targeted property rather than re-parameterising all nine on lot: this
/// is the only clause a non-unit lot makes falsifiable.
#[test]
fn the_lattice_holds_on_a_non_unit_lot(
    stream in prop::collection::vec(arb_lot_ten_order(), 1..60)
) {
    let lot = lot_ten_spec().lot_size();
    check_lattice_closure::<_, BTreeStore>(FifoMatcher, lot_ten_spec(), &stream)?;
    check_lattice_closure::<_, BTreeStore>(ProRataMatcher::new(lot), lot_ten_spec(), &stream)?;
    check_lattice_closure::<_, BTreeStore>(TimeProRataMatcher::new(lot), lot_ten_spec(), &stream)?;
    check_lattice_closure::<_, TickLadderStore>(FifoMatcher, lot_ten_spec(), &stream)?;
    check_lattice_closure::<_, TickLadderStore>(ProRataMatcher::new(lot), lot_ten_spec(), &stream)?;
    check_lattice_closure::<_, TickLadderStore>(TimeProRataMatcher::new(lot), lot_ten_spec(), &stream)?;
    check_lattice_closure::<_, HashMapStore>(FifoMatcher, lot_ten_spec(), &stream)?;
    check_lattice_closure::<_, HashMapStore>(ProRataMatcher::new(lot), lot_ten_spec(), &stream)?;
    check_lattice_closure::<_, HashMapStore>(TimeProRataMatcher::new(lot), lot_ten_spec(), &stream)?;
}

}
