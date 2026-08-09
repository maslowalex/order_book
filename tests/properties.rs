//! Phase 3.5 — property-based tests over random order streams.
//!
//! Each property drives a fresh book through a random stream of limit/market
//! orders (through the public `submit` API only) and asserts an invariant that
//! must hold for *every* stream, not just hand-picked examples:
//!
//! 1. Conservation: filled + rested + killed quantity == original quantity
//! 2. Depth accounting: the book's total depth changes by exactly
//!    rested − filled − self-trade-cancelled
//! 3. Price validity: every trade prints at the maker's resting price, never
//!    worse than the taker's limit, sweeping best-price-first
//! 4. Time priority: same-price makers fill in arrival (sequence) order
//! 5. No crossing: after every submit, best_bid < best_ask
//! 6. Index consistency: `index` and the price levels agree exactly, and the
//!    book drains to empty by cancelling every resting id
//!
//! Prices are drawn from a tight tick grid (95.00–105.00, 0.25 steps) so that
//! random streams actually cross; clients come from a pool of 4 so self-trade
//! prevention gets exercised.

use std::cmp::Reverse;
use std::collections::HashMap;

use order_book::matching::SubmitOutcome;
use order_book::orderbook::OrderBook;
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
    // Mostly GTC limits (they build the book), some IOC/FOK limits and
    // markets (they sweep it without adding depth).
    prop_oneof![
        3 => arb_limit_price().prop_map(OrderType::limit_gtc),
        1 => arb_limit_price().prop_map(OrderType::limit_ioc),
        1 => arb_limit_price().prop_map(OrderType::limit_fok),
        1 => Just(OrderType::Market),
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

/// Remaining quantity of `id` if it rests in the book, found via the index.
fn rested_remaining(book: &OrderBook, id: &ExchangeId) -> Option<u64> {
    let (side, price) = book.index.get(id)?;
    let level = match side {
        Side::Bid => book.bids.get(&Reverse(*price))?,
        Side::Ask => book.asks.get(price)?,
    };
    level
        .orders
        .iter()
        .find(|o| &o.exchange_id == id)
        .map(|o| o.remaining_quantity)
}

/// Engine-assigned ids are "exchId-N"; N is the arrival sequence number.
fn seq_of(id: &ExchangeId) -> u64 {
    id.0.strip_prefix("exchId-")
        .and_then(|n| n.parse().ok())
        .expect("engine-assigned id of the form exchId-N")
}

// --- properties ---------------------------------------------------------

proptest! {
    /// Invariant 1 — conservation: for every submit, the original quantity is
    /// fully accounted for by fills + rested remainder + killed remainder,
    /// and the reported outcome agrees with where the quantity went.
    #[test]
    fn conservation_per_submit(stream in arb_stream()) {
        let mut book = OrderBook::new();

        for order in stream {
            let original = order.original_quantity;
            // order types whose unfilled remainder is discarded, not rested
            let can_kill = matches!(
                order.order_type,
                OrderType::Market
                    | OrderType::Limit {
                        tif: TimeInForce::Ioc | TimeInForce::Fok,
                        ..
                    }
            );
            let is_fok = matches!(
                order.order_type,
                OrderType::Limit {
                    tif: TimeInForce::Fok,
                    ..
                }
            );

            let report = book.submit(order).unwrap();
            let filled: u64 = report.trades.iter().map(|t| t.quantity).sum();
            for trade in &report.trades {
                prop_assert!(trade.quantity > 0, "zero-quantity trade");
            }

            let rested = rested_remaining(&book, &report.order_id);
            match report.outcome {
                SubmitOutcome::Filled => {
                    prop_assert_eq!(filled, original);
                    prop_assert_eq!(rested, None);
                }
                SubmitOutcome::Rested => {
                    prop_assert!(report.trades.is_empty());
                    prop_assert_eq!(rested, Some(original));
                }
                SubmitOutcome::PartiallyFilledAndRested => {
                    prop_assert!(filled > 0 && filled < original);
                    prop_assert_eq!(rested, Some(original - filled));
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
            }
        }
    }

    /// Invariant 2 — depth accounting: each submit changes total book depth by
    /// exactly (rested remainder) − (filled) − (self-trade cancelled).
    #[test]
    fn book_depth_accounting(stream in arb_stream()) {
        let mut book = OrderBook::new();

        for order in stream {
            let before = resting_snapshot(&book);
            let depth_before = total_depth(&book);

            let report = book.submit(order).unwrap();

            let filled: u64 = report.trades.iter().map(|t| t.quantity).sum();
            let mut cancelled_qty: u64 = 0;
            for id in &report.cancelled {
                let (_, _, remaining) = before
                    .get(id)
                    .expect("cancelled order must have been resting before submit");
                cancelled_qty += remaining;
            }
            let rested = rested_remaining(&book, &report.order_id).unwrap_or(0);

            prop_assert_eq!(
                i128::from(total_depth(&book)),
                i128::from(depth_before) + i128::from(rested)
                    - i128::from(filled)
                    - i128::from(cancelled_qty)
            );
        }
    }

    /// Invariant 3 — price validity: every trade prints at the maker's resting
    /// price, on the opposite side, never worse than a limit taker's price,
    /// and one submit sweeps prices best-first (monotonically).
    #[test]
    fn trades_at_maker_price_within_taker_limit(stream in arb_stream()) {
        let mut book = OrderBook::new();

        for order in stream {
            let taker_side = order.side;
            // Some(price) for limit takers, None for market takers
            let limit = order.order_type.limit_price();

            let before = resting_snapshot(&book);
            let report = book.submit(order).unwrap();

            let mut prev_price: Option<Price> = None;
            for trade in &report.trades {
                let (maker_side, maker_price, _) = before
                    .get(&trade.maker_order_id)
                    .expect("maker must have been resting before submit");

                prop_assert_eq!(trade.price, *maker_price, "trade must print at maker's price");
                prop_assert_eq!(*maker_side, opposite(taker_side));
                prop_assert_eq!(trade.taker_side, taker_side);
                prop_assert_eq!(&trade.taker_order_id, &report.order_id);

                if let Some(limit) = limit {
                    match taker_side {
                        Side::Bid => prop_assert!(trade.price <= limit),
                        Side::Ask => prop_assert!(trade.price >= limit),
                    }
                }

                // best price first: a bid sweeps asks upward, an ask sweeps bids downward
                if let Some(prev) = prev_price {
                    match taker_side {
                        Side::Bid => prop_assert!(trade.price >= prev),
                        Side::Ask => prop_assert!(trade.price <= prev),
                    }
                }
                prev_price = Some(trade.price);
            }
        }
    }

    /// Invariant 4 — time priority: at any given price (and maker side), makers
    /// are consumed in arrival order, so their sequence numbers never decrease
    /// across the whole run.
    #[test]
    fn same_price_fifo_priority(stream in arb_stream()) {
        let mut book = OrderBook::new();
        // (maker side is Bid?, price) → last maker sequence traded there
        let mut last_seq: HashMap<(bool, Price), u64> = HashMap::new();

        for order in stream {
            let maker_is_bid = opposite(order.side) == Side::Bid;
            let report = book.submit(order).unwrap();

            for trade in &report.trades {
                let seq = seq_of(&trade.maker_order_id);
                let key = (maker_is_bid, trade.price);
                if let Some(&prev) = last_seq.get(&key) {
                    prop_assert!(
                        seq >= prev,
                        "FIFO violated at {}: maker seq {} traded after {}",
                        trade.price, seq, prev
                    );
                }
                last_seq.insert(key, seq);
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

    /// Invariant 6 — index consistency: after any stream, `index` and the price
    /// levels describe exactly the same set of live orders, every level is
    /// non-empty and internally consistent, and cancelling every indexed id
    /// drains the book to empty.
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
                let expected = (Side::Ask, level.price);
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
                // everything resting must be a limit at its level's price
                prop_assert_eq!(o.order_type.limit_price(), Some(level.price));
                let expected = (Side::Bid, level.price);
                prop_assert_eq!(book.index.get(&o.exchange_id), Some(&expected));
            }
        }
        prop_assert_eq!(live_orders, book.index.len(), "index and levels disagree");

        let ids: Vec<ExchangeId> = book.index.keys().cloned().collect();
        for id in ids {
            prop_assert!(book.cancel_order(id).is_ok());
        }
        prop_assert!(book.bids.is_empty());
        prop_assert!(book.asks.is_empty());
        prop_assert!(book.index.is_empty());
    }
}
