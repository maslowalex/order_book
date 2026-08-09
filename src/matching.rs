use std::collections::{BTreeMap, HashMap};

use crate::orderbook::{OrderBook, OrderBookError};
use crate::types::{ClientId, ExchangeId, Order, OrderType, Price, PriceLevel, Side, TimeInForce};

/// A single executed fill between a resting maker and an incoming taker.
///
/// The trade always prints at the **maker's** price (price-time priority: the
/// resting order set the price, the taker accepted it). `quantity` is the filled
/// amount for this fill, not the size of either order.
#[derive(Debug, Clone, PartialEq)]
pub struct Trade {
    pub price: Price,
    pub quantity: u64,
    pub maker_order_id: ExchangeId,
    pub taker_order_id: ExchangeId,
    pub maker_client: ClientId,
    pub taker_client: ClientId,
    pub taker_side: Side,
    pub timestamp: u128,
}

/// What ultimately happened to the incoming (taker) order after `submit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Limit order didn't cross; the whole order now rests in the book.
    Rested,
    /// Limit order partially filled; the remainder rests in the book.
    PartiallyFilledAndRested,
    /// Order fully filled — nothing left to rest.
    Filled,
    /// Market order's unfilled remainder was discarded (IOC semantics).
    Killed,
}

/// The result of submitting one order: every fill it caused, what became of it,
/// and any resting orders we cancelled to prevent self-trading.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionReport {
    pub order_id: ExchangeId,
    pub trades: Vec<Trade>,
    pub outcome: SubmitOutcome,
    /// Resting orders cancelled because they belonged to the taker's own client
    /// (self-trade prevention).
    pub cancelled: Vec<ExchangeId>,
}

#[derive(Debug, Clone)]
struct MatchingResult {
    pub trades: Vec<Trade>,
    pub cancelled: Vec<ExchangeId>,
    pub outcome: SubmitOutcome,
}

impl OrderBook {
    /// Submit an order to be matched against the book, resting any remainder
    /// (limit) or discarding it (market). Contrast with `add_order`, which
    /// always rests without matching (used for seeding the book).
    pub fn submit(&mut self, mut order: Order) -> Result<ExecutionReport, OrderBookError> {
        // The exchange assigns the order's id on receipt — ignoring whatever id
        // the caller put on it — and advances the sequence for the next order.
        // Stamping it onto `order` before matching keeps the trades, any rested
        // remainder, and `order_id` all referring to the same assigned id.
        let order_id = ExchangeId::from_sequence(self.next_seq);
        self.next_seq += 1;
        order.exchange_id = order_id.clone();

        let matching_result = match order.order_type {
            OrderType::Limit { price, tif } => match tif {
                TimeInForce::Gtc => self.process_limit_order(order, price),
                // IOC/FOK arrive in the next commits
                TimeInForce::Ioc | TimeInForce::Fok => Err(OrderBookError::Unsupported),
            },
            OrderType::Market => self.process_market_order(order),
            OrderType::StopMarket { .. } | OrderType::StopLimit { .. } => {
                Err(OrderBookError::Unsupported)
            }
        }?;

        Ok(ExecutionReport {
            order_id,
            trades: matching_result.trades,
            outcome: matching_result.outcome,
            cancelled: matching_result.cancelled,
        })
    }

    fn process_limit_order(
        &mut self,
        mut order: Order,
        limit: Price,
    ) -> Result<MatchingResult, OrderBookError> {
        // A limit only sweeps the marketable prefix: fill against the opposite
        // side while it crosses the limit price, then rest whatever is left.
        let (trades, cancelled) = match order.side {
            Side::Bid => fill_against(&mut self.asks, &mut self.index, &mut order, Some(limit)),
            Side::Ask => fill_against(&mut self.bids, &mut self.index, &mut order, Some(limit)),
        };

        if order.remaining_quantity == 0 {
            return Ok(MatchingResult {
                trades,
                cancelled,
                outcome: SubmitOutcome::Filled,
            });
        }

        // Some quantity is left over — rest it in the book.
        let outcome = if trades.is_empty() {
            SubmitOutcome::Rested
        } else {
            SubmitOutcome::PartiallyFilledAndRested
        };
        self.add_order(order)?;

        Ok(MatchingResult {
            trades,
            cancelled,
            outcome,
        })
    }

    fn process_market_order(&mut self, mut order: Order) -> Result<MatchingResult, OrderBookError> {
        // A market order accepts any price, so there is no limit bound.
        let (trades, cancelled) = match order.side {
            Side::Bid => fill_against(&mut self.asks, &mut self.index, &mut order, None),
            Side::Ask => fill_against(&mut self.bids, &mut self.index, &mut order, None),
        };

        // IOC: anything left unfilled is discarded, not rested.
        let outcome = if order.remaining_quantity == 0 {
            SubmitOutcome::Filled
        } else {
            SubmitOutcome::Killed
        };

        Ok(MatchingResult {
            trades,
            cancelled,
            outcome,
        })
    }
}

/// Walk the opposite side of the book and fill `taker` against it — best price
/// first, FIFO within a level. `limit` bounds the sweep: `None` takes any price
/// (market), `Some(p)` stops once the best resting price no longer crosses `p`
/// (limit). `taker.remaining_quantity` is decremented as it fills.
///
/// Self-trade prevention: a resting order from the taker's own client is
/// cancelled (removed from the book) instead of traded against; its id goes into
/// the returned `cancelled` list.
///
/// Generic over the key type so one body serves both `asks` (keyed by `Price`)
/// and `bids` (keyed by `Reverse<Price>`) — the level carries its own `price`,
/// so only the key's ordering differs. Taking the side map and index as separate
/// `&mut` args (rather than `&mut self`) is what keeps the borrows disjoint.
fn fill_against<K: Ord>(
    side: &mut BTreeMap<K, PriceLevel>,
    index: &mut HashMap<ExchangeId, (Side, Price)>,
    taker: &mut Order,
    limit: Option<Price>,
) -> (Vec<Trade>, Vec<ExchangeId>) {
    let mut trades: Vec<Trade> = vec![];
    let mut cancelled: Vec<ExchangeId> = vec![];

    while taker.remaining_quantity > 0 {
        // best opposing level, or stop — this side of the book is dry
        let Some(mut level_entry) = side.first_entry() else {
            break;
        };
        let level = level_entry.get_mut();

        // a limit order stops once the level no longer crosses its price
        if let Some(limit) = limit {
            let crosses = match taker.side {
                Side::Bid => level.price <= limit,
                Side::Ask => level.price >= limit,
            };
            if !crosses {
                break;
            }
        }

        // consume FIFO from the front of this level
        while taker.remaining_quantity > 0 {
            let Some(front) = level.orders.first_mut() else {
                break;
            };

            // self-trade: cancel the resting order rather than trade against it
            if front.client_id == taker.client_id {
                let self_order = level.orders.remove(0);
                index.remove(&self_order.exchange_id);
                cancelled.push(self_order.exchange_id);
                continue;
            }

            let fill = taker.remaining_quantity.min(front.remaining_quantity);
            trades.push(Trade {
                price: level.price, // maker's price == its level's price
                quantity: fill,
                maker_order_id: front.exchange_id.clone(),
                taker_order_id: taker.exchange_id.clone(),
                maker_client: front.client_id.clone(),
                taker_client: taker.client_id.clone(),
                taker_side: taker.side,
                timestamp: taker.timestamp,
            });

            front.remaining_quantity -= fill;
            taker.remaining_quantity -= fill;

            if front.remaining_quantity == 0 {
                let done = level.orders.remove(0);
                index.remove(&done.exchange_id);
            }
        }

        if level.orders.is_empty() {
            level_entry.remove(); // level drained → drop it, move to the next price
        } else {
            break; // level survived → the taker must be full
        }
    }

    (trades, cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{order, px};

    // The `order()` helper builds a resting GTC limit maker with
    // client_id == exchange_id == id; the incoming taker controls its own
    // type, so these build it explicitly.
    fn limit_order(side: Side, price: i64, qty: u64, id: &str) -> Order {
        Order::builder()
            .side(side)
            .quantity(qty)
            .client_id(id)
            .exchange_id(id)
            .order_type(OrderType::limit_gtc(px(price)))
            .build()
    }

    fn market_order(side: Side, qty: u64, id: &str) -> Order {
        Order::builder()
            .side(side)
            .quantity(qty)
            .client_id(id)
            .exchange_id(id)
            .order_type(OrderType::Market)
            .build()
    }

    fn id(s: &str) -> ExchangeId {
        ExchangeId(s.to_owned())
    }

    // ---- market BUY (incoming Bid) walks the asks, best (lowest) price first ----

    #[test]
    fn market_buy_fully_fills_single_resting_ask() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Ask, 100, 10, None, "a1")).unwrap();

        let report = ob.submit(market_order(Side::Bid, 10, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        assert_eq!(report.trades.len(), 1);

        let trade = &report.trades[0];
        assert_eq!(trade.price, px(100)); // maker's price
        assert_eq!(trade.quantity, 10);
        assert_eq!(trade.maker_order_id, id("a1"));
        assert_eq!(trade.taker_order_id, report.order_id); // the exchange-assigned id
        assert_eq!(trade.maker_client, ClientId("a1".to_owned()));
        assert_eq!(trade.taker_client, ClientId("t1".to_owned()));
        assert_eq!(trade.taker_side, Side::Bid);

        // the resting order is gone from both the book and the index
        assert_eq!(ob.best_ask(), None);
        assert!(!ob.index.contains_key(&id("a1")));
    }

    #[test]
    fn market_buy_partial_fill_reduces_resting_maker() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Ask, 100, 10, None, "a1")).unwrap();

        // taker smaller than the resting maker: taker fills, maker shrinks and stays
        let report = ob.submit(market_order(Side::Bid, 4, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.trades[0].quantity, 4);

        assert_eq!(ob.best_ask(), Some(px(100)));
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), 6); // 10 - 4
        assert!(ob.index.contains_key(&id("a1")));
    }

    #[test]
    fn market_buy_sweeps_levels_best_price_first() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Ask, 100, 5, None, "a1")).unwrap();
        ob.add_order(order(Side::Ask, 101, 5, None, "a2")).unwrap();
        ob.add_order(order(Side::Ask, 102, 5, None, "a3")).unwrap();

        let report = ob.submit(market_order(Side::Bid, 8, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        // fills 5 @ 100 then 3 @ 101 — ascending price order
        assert_eq!(report.trades.len(), 2);
        assert_eq!(
            (report.trades[0].price, report.trades[0].quantity),
            (px(100), 5)
        );
        assert_eq!(
            (report.trades[1].price, report.trades[1].quantity),
            (px(101), 3)
        );

        // a1 fully consumed, a2 left with 2, a3 untouched
        assert!(!ob.index.contains_key(&id("a1")));
        assert_eq!(ob.best_ask(), Some(px(101)));
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), 2);
        assert!(ob.asks.contains_key(&px(102)));
    }

    #[test]
    fn market_buy_is_fifo_within_a_level() {
        let mut ob = OrderBook::new();
        // same price — the one added first must fill first
        ob.add_order(order(Side::Ask, 100, 5, None, "a1")).unwrap();
        ob.add_order(order(Side::Ask, 100, 5, None, "a2")).unwrap();

        let report = ob.submit(market_order(Side::Bid, 5, "t1")).unwrap();

        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.trades[0].maker_order_id, id("a1")); // oldest first
        assert!(!ob.index.contains_key(&id("a1")));
        assert!(ob.index.contains_key(&id("a2")));
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), 5); // only a2 remains
    }

    #[test]
    fn market_buy_on_empty_book_is_killed() {
        let mut ob = OrderBook::new();

        let report = ob.submit(market_order(Side::Bid, 5, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Killed);
        assert!(report.trades.is_empty());
    }

    #[test]
    fn market_buy_insufficient_liquidity_kills_remainder() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Ask, 100, 3, None, "a1")).unwrap();

        let report = ob.submit(market_order(Side::Bid, 10, "t1")).unwrap();

        // takes all 3, discards the unfilled 7 (IOC)
        assert_eq!(report.outcome, SubmitOutcome::Killed);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.trades[0].quantity, 3);
        assert_eq!(ob.best_ask(), None);
        assert!(ob.index.is_empty());
    }

    // ---- market SELL (incoming Ask) walks the bids, best (highest) price first ----

    #[test]
    fn market_sell_fills_against_best_bids_first() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Bid, 99, 5, None, "b1")).unwrap();
        ob.add_order(order(Side::Bid, 98, 5, None, "b2")).unwrap();

        let report = ob.submit(market_order(Side::Ask, 8, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        assert_eq!(report.trades.len(), 2);
        // highest bid first: 5 @ 99 then 3 @ 98
        assert_eq!(
            (report.trades[0].price, report.trades[0].quantity),
            (px(99), 5)
        );
        assert_eq!(
            (report.trades[1].price, report.trades[1].quantity),
            (px(98), 3)
        );
        assert_eq!(report.trades[0].taker_side, Side::Ask);

        assert_eq!(ob.best_bid(), Some(px(98)));
        assert_eq!(ob.best_bid_level().unwrap().total_quantity(), 2);
    }

    // ---- limit orders: rest when they don't cross, match the marketable prefix when they do ----

    #[test]
    fn limit_buy_non_crossing_rests() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Ask, 100, 10, None, "a1")).unwrap();

        // buy at 99 < best ask 100 -> does not cross
        let report = ob.submit(limit_order(Side::Bid, 99, 5, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Rested);
        assert!(report.trades.is_empty());
        assert!(report.cancelled.is_empty());
        assert_eq!(ob.best_bid(), Some(px(99)));
        assert_eq!(ob.best_bid_level().unwrap().total_quantity(), 5);
        assert!(ob.index.contains_key(&report.order_id)); // rests under the assigned id
    }

    #[test]
    fn limit_buy_crossing_fully_fills() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Ask, 100, 10, None, "a1")).unwrap();

        let report = ob.submit(limit_order(Side::Bid, 100, 5, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.trades[0].quantity, 5);
        // nothing rests for the taker; maker shrinks to 5
        assert!(!ob.index.contains_key(&report.order_id));
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), 5);
    }

    #[test]
    fn limit_buy_partial_fill_rests_remainder() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Ask, 100, 5, None, "a1")).unwrap();

        // wants 8, only 5 available at a crossing price -> fill 5, rest 3
        let report = ob.submit(limit_order(Side::Bid, 100, 8, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::PartiallyFilledAndRested);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.trades[0].quantity, 5);

        assert_eq!(ob.best_ask(), None); // a1 fully consumed
        assert_eq!(ob.best_bid(), Some(px(100))); // remainder rests as a bid
        assert_eq!(ob.best_bid_level().unwrap().total_quantity(), 3);
        assert!(ob.index.contains_key(&report.order_id)); // rests under the assigned id
    }

    #[test]
    fn limit_buy_sweeps_only_marketable_prefix() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Ask, 100, 5, None, "a1")).unwrap();
        ob.add_order(order(Side::Ask, 102, 5, None, "a2")).unwrap();

        // buy at 100 crosses a1 (100 <= 100) but NOT a2 (102 > 100)
        let report = ob.submit(limit_order(Side::Bid, 100, 10, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::PartiallyFilledAndRested);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(
            (report.trades[0].price, report.trades[0].quantity),
            (px(100), 5)
        );

        assert!(ob.asks.contains_key(&px(102))); // a2 untouched
        assert_eq!(ob.best_bid(), Some(px(100))); // remainder 5 rests
        assert_eq!(ob.best_bid_level().unwrap().total_quantity(), 5);
    }

    #[test]
    fn limit_buy_prints_at_maker_price_not_taker_price() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Ask, 100, 10, None, "a1")).unwrap();

        // aggressive buy at 105 against an ask resting at 100
        let report = ob.submit(limit_order(Side::Bid, 105, 5, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        assert_eq!(report.trades[0].price, px(100)); // maker's 100, not taker's 105
    }

    #[test]
    fn limit_buy_crosses_multiple_levels_and_fully_fills() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Ask, 100, 5, None, "a1")).unwrap();
        ob.add_order(order(Side::Ask, 101, 5, None, "a2")).unwrap();

        // buy at 101 crosses both levels; wants 8 -> 5 @ 100 then 3 @ 101, fully filled
        let report = ob.submit(limit_order(Side::Bid, 101, 8, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        assert_eq!(report.trades.len(), 2);
        assert_eq!(
            (report.trades[0].price, report.trades[0].quantity),
            (px(100), 5)
        );
        assert_eq!(
            (report.trades[1].price, report.trades[1].quantity),
            (px(101), 3)
        );

        assert!(!ob.index.contains_key(&id("a1"))); // a1 fully consumed
        assert!(!ob.index.contains_key(&report.order_id)); // taker fully filled — nothing rests
        assert_eq!(ob.best_ask(), Some(px(101))); // a2 left with 2
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), 2);
    }

    #[test]
    fn limit_buy_crosses_multiple_levels_then_rests_remainder() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Ask, 100, 5, None, "a1")).unwrap();
        ob.add_order(order(Side::Ask, 101, 5, None, "a2")).unwrap();
        ob.add_order(order(Side::Ask, 103, 5, None, "a3")).unwrap();

        // buy at 101 crosses a1 (100) and a2 (101) but not a3 (103);
        // wants 12 -> fills 5 + 5 = 10, remainder 2 rests as a bid @ 101
        let report = ob.submit(limit_order(Side::Bid, 101, 12, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::PartiallyFilledAndRested);
        assert_eq!(report.trades.len(), 2);
        assert_eq!(
            (report.trades[0].price, report.trades[0].quantity),
            (px(100), 5)
        );
        assert_eq!(
            (report.trades[1].price, report.trades[1].quantity),
            (px(101), 5)
        );

        // both crossed makers gone, a3 untouched
        assert!(!ob.index.contains_key(&id("a1")));
        assert!(!ob.index.contains_key(&id("a2")));
        assert_eq!(ob.best_ask(), Some(px(103)));
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), 5);

        // remainder rests on the bid side
        assert_eq!(ob.best_bid(), Some(px(101)));
        assert_eq!(ob.best_bid_level().unwrap().total_quantity(), 2);
        assert!(ob.index.contains_key(&report.order_id)); // rests under the assigned id
    }

    // ---- limit SELL (incoming Ask): rests above the book, sweeps bids when it crosses ----

    #[test]
    fn limit_sell_non_crossing_rests() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Bid, 99, 10, None, "b1")).unwrap();

        // sell at 100 > best bid 99 -> does not cross
        let report = ob.submit(limit_order(Side::Ask, 100, 5, "s1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Rested);
        assert!(report.trades.is_empty());
        assert!(report.cancelled.is_empty());
        assert_eq!(ob.best_ask(), Some(px(100)));
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), 5);
        assert!(ob.index.contains_key(&report.order_id));
    }

    #[test]
    fn limit_sell_crossing_fully_fills() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Bid, 100, 10, None, "b1")).unwrap();

        let report = ob.submit(limit_order(Side::Ask, 100, 5, "s1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(
            (report.trades[0].price, report.trades[0].quantity),
            (px(100), 5)
        );
        assert_eq!(report.trades[0].taker_side, Side::Ask);
        // nothing rests; b1 shrinks to 5
        assert!(!ob.index.contains_key(&report.order_id));
        assert_eq!(ob.best_bid_level().unwrap().total_quantity(), 5);
    }

    #[test]
    fn limit_sell_partial_fill_rests_remainder() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Bid, 100, 5, None, "b1")).unwrap();

        // wants to sell 8, only 5 bid at a crossing price -> fill 5, rest 3
        let report = ob.submit(limit_order(Side::Ask, 100, 8, "s1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::PartiallyFilledAndRested);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.trades[0].quantity, 5);

        assert_eq!(ob.best_bid(), None); // b1 fully consumed
        assert_eq!(ob.best_ask(), Some(px(100))); // remainder rests as an ask
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), 3);
        assert!(ob.index.contains_key(&report.order_id));
    }

    #[test]
    fn limit_sell_sweeps_only_marketable_prefix() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Bid, 100, 5, None, "b1")).unwrap();
        ob.add_order(order(Side::Bid, 98, 5, None, "b2")).unwrap();

        // sell at 100 crosses b1 (100 >= 100) but NOT b2 (98 < 100)
        let report = ob.submit(limit_order(Side::Ask, 100, 10, "s1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::PartiallyFilledAndRested);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(
            (report.trades[0].price, report.trades[0].quantity),
            (px(100), 5)
        );

        assert_eq!(ob.best_bid(), Some(px(98))); // b2 untouched, now best bid
        assert_eq!(ob.best_ask(), Some(px(100))); // remainder 5 rests
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), 5);
    }

    #[test]
    fn limit_sell_crosses_multiple_levels_then_rests_remainder() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Bid, 100, 5, None, "b1")).unwrap();
        ob.add_order(order(Side::Bid, 99, 5, None, "b2")).unwrap();
        ob.add_order(order(Side::Bid, 97, 5, None, "b3")).unwrap();

        // sell at 99 crosses b1 (100) and b2 (99) but not b3 (97);
        // wants 12 -> fills 5 + 5 = 10, remainder 2 rests as an ask @ 99
        let report = ob.submit(limit_order(Side::Ask, 99, 12, "s1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::PartiallyFilledAndRested);
        assert_eq!(report.trades.len(), 2);
        // best (highest) bid first: 5 @ 100 then 5 @ 99
        assert_eq!(
            (report.trades[0].price, report.trades[0].quantity),
            (px(100), 5)
        );
        assert_eq!(
            (report.trades[1].price, report.trades[1].quantity),
            (px(99), 5)
        );

        assert_eq!(ob.best_bid(), Some(px(97))); // only b3 remains
        assert_eq!(ob.best_ask(), Some(px(99))); // remainder rests
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), 2);
    }

    // ---- report plumbing ----

    #[test]
    fn submit_assigns_exchange_id_and_ignores_caller_id() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Ask, 100, 10, None, "a1")).unwrap();

        // whatever id the caller stamped on the order is ignored — the exchange
        // mints its own from the sequence and reports that
        let report = ob
            .submit(market_order(Side::Bid, 5, "caller-supplied"))
            .unwrap();

        assert_eq!(report.order_id, id("exchId-1"));
        // and the trade references that same assigned id for the taker
        assert_eq!(report.trades[0].taker_order_id, report.order_id);
    }

    #[test]
    fn submit_assigns_sequential_ids() {
        let mut ob = OrderBook::new();
        ob.add_order(order(Side::Ask, 100, 100, None, "a1"))
            .unwrap();

        // each submit advances the sequence, so assigned ids never repeat
        let first = ob.submit(market_order(Side::Bid, 1, "ignored")).unwrap();
        let second = ob.submit(market_order(Side::Bid, 1, "ignored")).unwrap();

        assert_eq!(first.order_id, id("exchId-1"));
        assert_eq!(second.order_id, id("exchId-2"));
        assert_ne!(first.order_id, second.order_id);
    }

    // ---- self-trade prevention (the `cancelled` path) ----
    // NOTE: this locks in "cancel the same-client resting order and keep going".
    // Delete or adjust if you haven't wired self-trade prevention yet.
    #[test]
    fn self_trade_cancels_resting_order_instead_of_filling() {
        let mut ob = OrderBook::new();
        let maker = Order::builder()
            .side(Side::Ask)
            .quantity(10)
            .client_id("alice")
            .exchange_id("a1")
            .order_type(OrderType::limit_gtc(px(100)))
            .build();
        ob.add_order(maker).unwrap();

        let taker = Order::builder()
            .side(Side::Bid)
            .quantity(5)
            .client_id("alice") // same client as the resting order
            .exchange_id("t1")
            .order_type(OrderType::Market)
            .build();
        let report = ob.submit(taker).unwrap();

        assert!(report.trades.is_empty()); // no self-trade printed
        assert_eq!(report.cancelled, vec![id("a1")]);
        assert_eq!(report.outcome, SubmitOutcome::Killed); // nothing left to fill against
        assert!(ob.index.is_empty());
        assert_eq!(ob.best_ask(), None);
    }
}
