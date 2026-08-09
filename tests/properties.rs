//! Phase 3.5/4 — property-based tests over random order streams.
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
//!    worse than the taker's limit, sweeping best-price-first
//! 4. Time priority: same-price makers fill in arrival (sequence) order
//! 5. No crossing: after every submit, best_bid < best_ask
//! 6. Index consistency: `index`, the price levels, and the stop books agree
//!    exactly, and everything drains by cancelling every live id
//! 7. Stop discipline: no pending stop whose trigger the market has already
//!    reached survives a submit
//!
//! Prices and triggers are drawn from a tight tick grid (95.00–105.00, 0.25
//! steps) so that random streams actually cross and stops actually fire;
//! clients come from a pool of 4 so self-trade prevention gets exercised.

use std::collections::HashMap;

use order_book::matching::{ExecutionReport, SubmitOutcome};
use order_book::orderbook::{OrderBook, OrderLocation};
use order_book::types::{ExchangeId, Order, OrderType, Price, Side, TimeInForce};
use proptest::prelude::*;
use rust_decimal::Decimal;

// --- strategies ---------------------------------------------------------

fn arb_side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Bid), Just(Side::Ask)]
}

fn arb_limit_price() -> impl Strategy<Value = Price> {
    // 95.00 ..= 105.00 in 0.25 ticks
    (380u32..=420).prop_map(|ticks| Decimal::new(i64::from(ticks) * 25, 2))
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
                .quantity(quantity)
                .build()
        },
    )
}

fn arb_stream() -> impl Strategy<Value = Vec<Order>> {
    prop::collection::vec(arb_order(), 1..60)
}

// --- helpers ------------------------------------------------------------

fn opposite(side: Side) -> Side {
    match side {
        Side::Bid => Side::Ask,
        Side::Ask => Side::Bid,
    }
}

/// Everything resting in the book right now: id → (side, price, remaining).
/// Parked stops are deliberately NOT part of this — they hold no depth.
fn resting_snapshot(book: &OrderBook) -> HashMap<ExchangeId, (Side, Price, u64)> {
    book.bids
        .values()
        .chain(book.asks.values())
        .flat_map(|level| {
            level.orders.iter().map(|o| {
                (
                    o.exchange_id.clone(),
                    (o.side, level.price, o.remaining_quantity),
                )
            })
        })
        .collect()
}

fn total_depth(book: &OrderBook) -> u64 {
    book.bids
        .values()
        .chain(book.asks.values())
        .map(|level| level.total_quantity())
        .sum()
}

/// Remaining quantity of `id` if it rests in the BOOK (not the stop book).
fn rested_remaining(book: &OrderBook, id: &ExchangeId) -> Option<u64> {
    match book.index.get(id)? {
        OrderLocation::Book { .. } => book.get_order(id).map(|o| o.remaining_quantity),
        OrderLocation::StopBook { .. } => None,
    }
}

/// Remaining quantity of `id` if it's parked in the stop book.
fn parked_remaining(book: &OrderBook, id: &ExchangeId) -> Option<u64> {
    match book.index.get(id)? {
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
fn fills_by_maker(report: &ExecutionReport) -> HashMap<ExchangeId, u64> {
    let mut fills: HashMap<ExchangeId, u64> = HashMap::new();
    for r in all_reports(report) {
        for t in &r.trades {
            *fills.entry(t.maker_order_id.clone()).or_default() += t.quantity;
        }
    }
    fills
}

// --- properties ---------------------------------------------------------

proptest! {
    /// Invariant 1 — conservation: for every execution (the submitted order
    /// and each triggered stop), the original quantity is fully accounted for
    /// by fills + rested remainder + killed remainder, and the reported
    /// outcome agrees with where the quantity went. "Rested" is checked
    /// net of any quantity that later executions in the same cascade already
    /// consumed from it.
    #[test]
    fn conservation_per_submit(stream in arb_stream()) {
        let mut book = OrderBook::new();
        // original quantity of every currently-parked stop, by assigned id
        let mut parked: HashMap<ExchangeId, u64> = HashMap::new();

        for order in stream {
            let original = order.original_quantity;
            let order_type = order.order_type;
            // order types whose unfilled remainder is discarded, not rested
            let can_kill = matches!(
                order.order_type,
                OrderType::Market
                    | OrderType::Limit { tif: TimeInForce::Ioc | TimeInForce::Fok, .. }
                    // an already-triggered stop activates immediately as market/limit
                    | OrderType::StopMarket { .. }
                    | OrderType::StopLimit { tif: TimeInForce::Ioc | TimeInForce::Fok, .. }
            );
            let is_fok = matches!(
                order.order_type,
                OrderType::Limit { tif: TimeInForce::Fok, .. }
            );

            let report = book.submit(order).unwrap();
            let consumed_later = fills_by_maker(&report);

            for trade in all_reports(&report).flat_map(|r| &r.trades) {
                prop_assert!(trade.quantity > 0, "zero-quantity trade");
            }

            // an order that rested during this submit can afterwards be
            // self-trade-CANCELLED (not traded) by a same-client triggered
            // stop; its remainder then vanishes without a fill to count
            let stp_cancelled = |id: &ExchangeId| {
                all_reports(&report).any(|r| r.cancelled.contains(id))
            };

            // -- the submitted order itself --
            let filled: u64 = report.trades.iter().map(|t| t.quantity).sum();
            let rested = rested_remaining(&book, &report.order_id);
            let eaten = consumed_later.get(&report.order_id).copied().unwrap_or(0);
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
                        prop_assert_eq!(rested.unwrap_or(0) + eaten, original);
                    }
                }
                SubmitOutcome::PartiallyFilledAndRested => {
                    prop_assert!(filled > 0 && filled < original);
                    if cancelled_later {
                        prop_assert_eq!(rested, None);
                    } else {
                        prop_assert_eq!(rested.unwrap_or(0) + eaten, original - filled);
                    }
                }
                SubmitOutcome::Killed => {
                    prop_assert!(can_kill, "only market/IOC/FOK remainders are killed");
                    prop_assert!(filled < original);
                    if is_fok {
                        // all-or-nothing: a killed FOK executed NOTHING —
                        // no fills and no self-trade cancellations
                        prop_assert_eq!(filled, 0, "FOK partially filled");
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
                let qty = parked
                    .remove(&r.order_id)
                    .expect("triggered stop must have been parked earlier");
                prop_assert!(r.triggered.is_empty(), "cascade reports must be flat");

                let filled: u64 = r.trades.iter().map(|t| t.quantity).sum();
                let rested = rested_remaining(&book, &r.order_id);
                let eaten = consumed_later.get(&r.order_id).copied().unwrap_or(0);
                let cancelled_later = stp_cancelled(&r.order_id);
                match r.outcome {
                    SubmitOutcome::Filled => {
                        prop_assert_eq!(filled, qty);
                        prop_assert_eq!(rested, None);
                    }
                    SubmitOutcome::Rested => {
                        prop_assert!(r.trades.is_empty());
                        if cancelled_later {
                            prop_assert_eq!(rested, None);
                        } else {
                            prop_assert_eq!(rested.unwrap_or(0) + eaten, qty);
                        }
                    }
                    SubmitOutcome::PartiallyFilledAndRested => {
                        prop_assert!(filled > 0 && filled < qty);
                        if cancelled_later {
                            prop_assert_eq!(rested, None);
                        } else {
                            prop_assert_eq!(rested.unwrap_or(0) + eaten, qty - filled);
                        }
                    }
                    SubmitOutcome::Killed => {
                        prop_assert!(filled < qty);
                        prop_assert_eq!(rested, None);
                    }
                    SubmitOutcome::StopPending => {
                        prop_assert!(false, "an activated stop can never re-park");
                    }
                }
            }
        }
    }

    /// Invariant 2 — depth accounting: each submit (cascade included) changes
    /// total book depth by exactly (quantity that came to rest) − (quantity
    /// filled) − (quantity self-trade-cancelled). Rest-time quantities are
    /// used, so intra-cascade consumption of freshly rested orders balances.
    #[test]
    fn book_depth_accounting(stream in arb_stream()) {
        let mut book = OrderBook::new();
        let mut parked: HashMap<ExchangeId, u64> = HashMap::new();

        for order in stream {
            let original = order.original_quantity;
            let before = resting_snapshot(&book);
            let depth_before = total_depth(&book);

            let report = book.submit(order).unwrap();
            let fills = fills_by_maker(&report);

            let mut filled_total: u64 = 0;
            let mut rested_total: u64 = 0; // at rest time
            let mut cancelled_total: u64 = 0; // remaining at cancel time
            // rest-time quantity per order that rested during THIS submit
            let mut rest_qty: HashMap<ExchangeId, u64> = HashMap::new();

            for (i, r) in all_reports(&report).enumerate() {
                let exec_original = if i == 0 {
                    original
                } else {
                    *parked.get(&r.order_id).expect("triggered stop was parked")
                };
                let filled: u64 = r.trades.iter().map(|t| t.quantity).sum();
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
                    cancelled_total += start - fills.get(id).copied().unwrap_or(0);
                }
            }
            for r in &report.triggered {
                parked.remove(&r.order_id);
            }
            if report.outcome == SubmitOutcome::StopPending {
                parked.insert(report.order_id.clone(), original);
            }

            prop_assert_eq!(
                i128::from(total_depth(&book)),
                i128::from(depth_before) + i128::from(rested_total)
                    - i128::from(filled_total)
                    - i128::from(cancelled_total)
            );
        }
    }

    /// Invariant 3 — price validity: every trade prints at the maker's resting
    /// price, on the opposite side, never worse than a limit taker's price,
    /// and each execution sweeps prices best-first (monotonically).
    #[test]
    fn trades_at_maker_price_within_taker_limit(stream in arb_stream()) {
        let mut book = OrderBook::new();

        for order in stream {
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
                    // makers that rested before this submit must print at
                    // their snapshot price; makers that rested mid-cascade
                    // are covered by invariant 6's level==order price check
                    if let Some((maker_side, maker_price, _)) = before.get(&trade.maker_order_id) {
                        prop_assert_eq!(trade.price, *maker_price, "trade must print at maker's price");
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
    }

    /// Invariant 4 — time priority: at any given price (and maker side),
    /// makers are consumed in the order they RESTED there. Submission
    /// sequence is not the right proxy once stops exist — a stop submitted
    /// early (low sequence) can trigger and rest late, and correctly queues
    /// behind orders already at its level. So the test stamps every order at
    /// the moment it rests and checks stamps never decrease per level.
    #[test]
    fn same_price_fifo_priority(stream in arb_stream()) {
        let mut book = OrderBook::new();
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
                            "FIFO violated at {}: maker stamped {} traded after {}",
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

    /// Invariant 5 — no crossing: after every submit the book is uncrossed.
    #[test]
    fn book_never_crossed_after_submit(stream in arb_stream()) {
        let mut book = OrderBook::new();

        for order in stream {
            book.submit(order).unwrap();
            if let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) {
                prop_assert!(bid < ask, "book crossed: bid {} >= ask {}", bid, ask);
            }
        }
    }

    /// Invariant 6 — index consistency: after any stream, `index`, the price
    /// levels, AND the stop books describe exactly the same set of live
    /// orders; every level/queue is non-empty and internally consistent; and
    /// cancelling every indexed id drains everything to empty.
    #[test]
    fn index_matches_levels_and_book_drains(stream in arb_stream()) {
        let mut book = OrderBook::new();
        for order in stream {
            book.submit(order).unwrap();
        }

        let mut live_orders = 0usize;
        for (key, level) in &book.asks {
            prop_assert!(!level.orders.is_empty(), "empty level left in asks");
            prop_assert_eq!(level.side, Side::Ask);
            prop_assert_eq!(&level.price, key);
            for o in &level.orders {
                live_orders += 1;
                prop_assert!(o.remaining_quantity > 0);
                // everything resting must be a limit at its level's price
                prop_assert_eq!(o.order_type.limit_price(), Some(level.price));
                let expected = OrderLocation::Book { side: Side::Ask, price: level.price };
                prop_assert_eq!(book.index.get(&o.exchange_id), Some(&expected));
            }
        }
        for (key, level) in &book.bids {
            prop_assert!(!level.orders.is_empty(), "empty level left in bids");
            prop_assert_eq!(level.side, Side::Bid);
            prop_assert_eq!(level.price, key.0);
            for o in &level.orders {
                live_orders += 1;
                prop_assert!(o.remaining_quantity > 0);
                prop_assert_eq!(o.order_type.limit_price(), Some(level.price));
                let expected = OrderLocation::Book { side: Side::Bid, price: level.price };
                prop_assert_eq!(book.index.get(&o.exchange_id), Some(&expected));
            }
        }
        for (side, stops) in [(Side::Bid, &book.stop_bids), (Side::Ask, &book.stop_asks)] {
            for (trigger, queue) in stops {
                prop_assert!(!queue.is_empty(), "empty stop queue left behind");
                for o in queue {
                    live_orders += 1;
                    prop_assert_eq!(o.side, side);
                    prop_assert!(o.remaining_quantity > 0);
                    let is_stop = matches!(
                        o.order_type,
                        OrderType::StopMarket { .. } | OrderType::StopLimit { .. }
                    );
                    prop_assert!(is_stop, "non-stop order parked in the stop book");
                    let expected = OrderLocation::StopBook { side, trigger: *trigger };
                    prop_assert_eq!(book.index.get(&o.exchange_id), Some(&expected));
                }
            }
        }
        prop_assert_eq!(live_orders, book.index.len(), "index and structures disagree");

        let ids: Vec<ExchangeId> = book.index.keys().cloned().collect();
        for id in ids {
            prop_assert!(book.cancel_order(id).is_ok());
        }
        prop_assert!(book.bids.is_empty());
        prop_assert!(book.asks.is_empty());
        prop_assert!(book.stop_bids.is_empty());
        prop_assert!(book.stop_asks.is_empty());
        prop_assert!(book.index.is_empty());
    }

    /// Invariant 7 — stop discipline: after every submit, every still-pending
    /// stop's trigger is strictly beyond the last trade price. If it weren't,
    /// the cascade failed to fire it.
    #[test]
    fn no_satisfied_stop_left_pending(stream in arb_stream()) {
        let mut book = OrderBook::new();

        for order in stream {
            book.submit(order).unwrap();
            if let Some(last) = book.last_trade_price {
                for trigger in book.stop_bids.keys() {
                    prop_assert!(
                        *trigger > last,
                        "pending buy stop at {} but market already traded {}",
                        trigger, last
                    );
                }
                for trigger in book.stop_asks.keys() {
                    prop_assert!(
                        *trigger < last,
                        "pending sell stop at {} but market already traded {}",
                        trigger, last
                    );
                }
            }
        }
    }
}
