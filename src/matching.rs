use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};

use crate::instrument::Qty;
use crate::orderbook::{OrderBook, OrderBookError, OrderLocation};
use crate::types::{ClientId, ExchangeId, Order, OrderType, Price, PriceLevel, Side, TimeInForce};

/// A single executed fill between a resting maker and an incoming taker.
///
/// The trade always prints at the **maker's** price (price-time priority: the
/// resting order set the price, the taker accepted it). `quantity` is the filled
/// amount for this fill, not the size of either order.
#[derive(Debug, Clone, PartialEq)]
pub struct Trade {
    pub price: Price,
    pub quantity: Qty,
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
    /// The unfilled remainder was discarded (market/IOC), or a FOK couldn't
    /// fill completely and executed nothing.
    Killed,
    /// A stop order parked in the stop book, waiting for its trigger.
    StopPending,
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
    /// Stop orders this submit's trades set off, one report per activated
    /// stop, in trigger order. FLAT: a triggered stop's own trades may
    /// trigger further stops, but those land here too (their `triggered` is
    /// always empty), so consumers never recurse.
    pub triggered: Vec<ExecutionReport>,
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
    ///
    /// If the resulting trades move the last-trade price through pending stop
    /// triggers, those stops activate here, in a loop: each activation may
    /// trade, which may trigger further stops. The loop terminates because
    /// every iteration permanently removes one stop and activations (market/
    /// limit orders) can never add one. The cascade's reports are collected
    /// flat into the returned report's `triggered`.
    pub fn submit(&mut self, mut order: Order) -> Result<ExecutionReport, OrderBookError> {
        // The exchange assigns the order's id on receipt — ignoring whatever id
        // the caller put on it — and advances the sequence for the next order.
        // Stamping it onto `order` before matching keeps the trades, any rested
        // remainder, and `order_id` all referring to the same assigned id.
        let order_id = ExchangeId::from_sequence(self.next_seq);
        self.next_seq += 1;
        order.exchange_id = order_id;

        let mut report = self.execute(order)?;

        while let Some(stop) = self.pop_triggered_stop() {
            report.triggered.push(self.execute(activate(stop))?);
        }

        Ok(report)
    }

    /// Run ONE order through matching — no cascade, no id minting (the id is
    /// already stamped). Both `submit` and stop activation come through here.
    fn execute(&mut self, order: Order) -> Result<ExecutionReport, OrderBookError> {
        let order_id = order.exchange_id.clone();

        let matching_result = match order.order_type {
            OrderType::Limit { price, tif } => match tif {
                TimeInForce::GTC | TimeInForce::IOC => self.process_limit_order(order, price, tif),
                TimeInForce::FOK => self.process_fok_limit_order(order, price),
            },
            OrderType::Market => self.process_market_order(order),
            OrderType::StopMarket { trigger } | OrderType::StopLimit { trigger, .. } => {
                return self.park_or_activate_stop(order, trigger);
            }
        }?;

        // the trigger signal for stops: the price of the most recent trade
        if let Some(last) = matching_result.trades.last() {
            self.last_trade_price = Some(last.price);
        }

        Ok(ExecutionReport {
            order_id,
            trades: matching_result.trades,
            outcome: matching_result.outcome,
            cancelled: matching_result.cancelled,
            triggered: vec![],
        })
    }

    /// A stop whose trigger the market has already reached activates
    /// immediately; otherwise it parks in the stop book until a trade
    /// reaches its trigger.
    fn park_or_activate_stop(
        &mut self,
        order: Order,
        trigger: Price,
    ) -> Result<ExecutionReport, OrderBookError> {
        let already_triggered = match (self.last_trade_price, order.side) {
            (None, _) => false, // no trade has ever printed — nothing to compare
            (Some(last), Side::Bid) => last >= trigger, // buy stop: market rose to it
            (Some(last), Side::Ask) => last <= trigger, // sell stop: market fell to it
        };
        if already_triggered {
            // recursion depth is 1: `activate` yields Market/Limit, which
            // can never re-enter this function
            return self.execute(activate(order));
        }

        let order_id = order.exchange_id.clone();
        match self.index.entry(order_id.clone()) {
            Entry::Occupied(_) => return Err(OrderBookError::ExchangeIdDuplicated),
            Entry::Vacant(e) => e.insert(OrderLocation::StopBook {
                side: order.side,
                trigger,
            }),
        };
        let stops = match order.side {
            Side::Bid => &mut self.stop_bids,
            Side::Ask => &mut self.stop_asks,
        };
        stops.entry(trigger).or_default().push(order);

        Ok(ExecutionReport {
            order_id,
            trades: vec![],
            outcome: SubmitOutcome::StopPending,
            cancelled: vec![],
            triggered: vec![],
        })
    }

    /// The next pending stop whose trigger the last trade price has reached,
    /// removed from the stop book and the index. Buy stops are checked first
    /// (lowest trigger — nearest to a rising market), then sell stops
    /// (highest trigger — nearest to a falling market); FIFO within one
    /// trigger price.
    fn pop_triggered_stop(&mut self) -> Option<Order> {
        let last = self.last_trade_price?;

        let queue_entry = match self.stop_bids.first_entry() {
            Some(entry) if *entry.key() <= last => Some(entry),
            _ => match self.stop_asks.last_entry() {
                Some(entry) if *entry.key() >= last => Some(entry),
                _ => None,
            },
        };

        let mut entry = queue_entry?;
        let order = entry.get_mut().remove(0);
        if entry.get().is_empty() {
            entry.remove();
        }
        self.index.remove(&order.exchange_id);
        Some(order)
    }

    fn process_limit_order(
        &mut self,
        mut order: Order,
        limit: Price,
        tif: TimeInForce,
    ) -> Result<MatchingResult, OrderBookError> {
        // A limit only sweeps the marketable prefix: fill against the opposite
        // side while it crosses the limit price, then TIF decides the
        // remainder's fate.
        let (trades, cancelled) = match order.side {
            Side::Bid => fill_against(&mut self.asks, &mut self.index, &mut order, Some(limit)),
            Side::Ask => fill_against(&mut self.bids, &mut self.index, &mut order, Some(limit)),
        };

        if order.remaining_quantity.is_zero() {
            return Ok(MatchingResult {
                trades,
                cancelled,
                outcome: SubmitOutcome::Filled,
            });
        }

        let outcome = match tif {
            // GTC: the remainder rests in the book.
            TimeInForce::GTC => {
                let outcome = if trades.is_empty() {
                    SubmitOutcome::Rested
                } else {
                    SubmitOutcome::PartiallyFilledAndRested
                };
                self.add_order(order)?;
                outcome
            }
            // IOC: the remainder is discarded — same fate as a market
            // order's remainder, just bounded by the limit price.
            TimeInForce::IOC => SubmitOutcome::Killed,
            // FOK never reaches here: fillability is decided before matching.
            TimeInForce::FOK => return Err(OrderBookError::Unsupported),
        };

        Ok(MatchingResult {
            trades,
            cancelled,
            outcome,
        })
    }

    /// FOK is "check before execute": decide fillability against a read-only
    /// view of the book, then either sweep normally (full fill guaranteed) or
    /// return `Killed` having touched **nothing** — no partial fills, and no
    /// self-trade cancellations either, because nothing executed.
    fn process_fok_limit_order(
        &mut self,
        mut order: Order,
        limit: Price,
    ) -> Result<MatchingResult, OrderBookError> {
        let available = match order.side {
            Side::Bid => fillable_quantity(&self.asks, &order, limit),
            Side::Ask => fillable_quantity(&self.bids, &order, limit),
        };

        if available < order.remaining_quantity {
            return Ok(MatchingResult {
                trades: vec![],
                cancelled: vec![],
                outcome: SubmitOutcome::Killed,
            });
        }

        let (trades, cancelled) = match order.side {
            Side::Bid => fill_against(&mut self.asks, &mut self.index, &mut order, Some(limit)),
            Side::Ask => fill_against(&mut self.bids, &mut self.index, &mut order, Some(limit)),
        };
        debug_assert!(
            order.remaining_quantity.is_zero(),
            "FOK dry-run promised a full fill"
        );

        Ok(MatchingResult {
            trades,
            cancelled,
            outcome: SubmitOutcome::Filled,
        })
    }

    fn process_market_order(&mut self, mut order: Order) -> Result<MatchingResult, OrderBookError> {
        // A market order accepts any price, so there is no limit bound.
        let (trades, cancelled) = match order.side {
            Side::Bid => fill_against(&mut self.asks, &mut self.index, &mut order, None),
            Side::Ask => fill_against(&mut self.bids, &mut self.index, &mut order, None),
        };

        // IOC: anything left unfilled is discarded, not rested.
        let outcome = if order.remaining_quantity.is_zero() {
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

/// What a triggered stop becomes: `StopMarket` → `Market`, `StopLimit` →
/// `Limit` at its stored price/TIF. Id, side, quantities, and timestamp are
/// untouched — the exchange id assigned at submission follows the order
/// through activation.
fn activate(mut order: Order) -> Order {
    order.order_type = match order.order_type {
        OrderType::StopMarket { .. } => OrderType::Market,
        OrderType::StopLimit { price, tif, .. } => OrderType::Limit { price, tif },
        other => other,
    };
    order
}

/// How much of `taker` the book could fill right now within `limit`, without
/// touching anything. Mirrors `fill_against`'s walk (best price first, stop
/// once a level no longer crosses) but read-only. Resting orders owned by the
/// taker's client are EXCLUDED: self-trade prevention cancels them instead of
/// trading, so counting them would overpromise and let a "fill or kill"
/// partially fill. Returns early once `taker.remaining_quantity` is reachable.
fn fillable_quantity<K: Ord>(side: &BTreeMap<K, PriceLevel>, taker: &Order, limit: Price) -> Qty {
    let needed = taker.remaining_quantity;
    let mut available: Qty = Qty::ZERO;

    for level in side.values() {
        let crosses = match taker.side {
            Side::Bid => level.price <= limit,
            Side::Ask => level.price >= limit,
        };
        if !crosses {
            break;
        }
        for order in &level.orders {
            if order.client_id == taker.client_id {
                continue; // would be STP-cancelled, not traded
            }
            available += order.remaining_quantity;
            if available >= needed {
                return available;
            }
        }
    }

    available
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
    index: &mut HashMap<ExchangeId, OrderLocation>,
    taker: &mut Order,
    limit: Option<Price>,
) -> (Vec<Trade>, Vec<ExchangeId>) {
    let mut trades: Vec<Trade> = vec![];
    let mut cancelled: Vec<ExchangeId> = vec![];

    while !taker.remaining_quantity.is_zero() {
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
        while !taker.remaining_quantity.is_zero() {
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

            if front.remaining_quantity.is_zero() {
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
    use crate::test_helpers::{book, order, px, qty};

    // The `order()` helper builds a resting GTC limit maker with
    // client_id == exchange_id == id; the incoming taker controls its own
    // type, so these build it explicitly.
    fn limit_order(side: Side, price: i64, quantity: u64, id: &str) -> Order {
        Order::builder()
            .side(side)
            .quantity(qty(quantity))
            .client_id(id)
            .exchange_id(id)
            .order_type(OrderType::limit_gtc(px(price)))
            .build()
    }

    fn market_order(side: Side, quantity: u64, id: &str) -> Order {
        Order::builder()
            .side(side)
            .quantity(qty(quantity))
            .client_id(id)
            .exchange_id(id)
            .order_type(OrderType::Market)
            .build()
    }

    fn ioc_order(side: Side, price: i64, quantity: u64, id: &str) -> Order {
        Order::builder()
            .side(side)
            .quantity(qty(quantity))
            .client_id(id)
            .exchange_id(id)
            .order_type(OrderType::limit_ioc(px(price)))
            .build()
    }

    fn fok_order(side: Side, price: i64, quantity: u64, id: &str) -> Order {
        Order::builder()
            .side(side)
            .quantity(qty(quantity))
            .client_id(id)
            .exchange_id(id)
            .order_type(OrderType::limit_fok(px(price)))
            .build()
    }

    fn stop_market(side: Side, trigger: i64, quantity: u64, id: &str) -> Order {
        Order::builder()
            .side(side)
            .quantity(qty(quantity))
            .client_id(id)
            .exchange_id(id)
            .order_type(OrderType::stop_market(px(trigger)))
            .build()
    }

    fn stop_limit(side: Side, trigger: i64, price: i64, quantity: u64, id: &str) -> Order {
        Order::builder()
            .side(side)
            .quantity(qty(quantity))
            .client_id(id)
            .exchange_id(id)
            .order_type(OrderType::stop_limit(px(trigger), px(price)))
            .build()
    }

    fn id(s: &str) -> ExchangeId {
        ExchangeId(s.to_owned())
    }

    // ---- market BUY (incoming Bid) walks the asks, best (lowest) price first ----

    #[test]
    fn market_buy_fully_fills_single_resting_ask() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 10, None, "a1")).unwrap();

        let report = ob.submit(market_order(Side::Bid, 10, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        assert_eq!(report.trades.len(), 1);

        let trade = &report.trades[0];
        assert_eq!(trade.price, px(100)); // maker's price
        assert_eq!(trade.quantity, qty(10));
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
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 10, None, "a1")).unwrap();

        // taker smaller than the resting maker: taker fills, maker shrinks and stays
        let report = ob.submit(market_order(Side::Bid, 4, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.trades[0].quantity, qty(4));

        assert_eq!(ob.best_ask(), Some(px(100)));
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), qty(6)); // 10 - 4
        assert!(ob.index.contains_key(&id("a1")));
    }

    #[test]
    fn market_buy_sweeps_levels_best_price_first() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 5, None, "a1")).unwrap();
        ob.add_order(order(Side::Ask, 101, 5, None, "a2")).unwrap();
        ob.add_order(order(Side::Ask, 102, 5, None, "a3")).unwrap();

        let report = ob.submit(market_order(Side::Bid, 8, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        // fills 5 @ 100 then 3 @ 101 — ascending price order
        assert_eq!(report.trades.len(), 2);
        assert_eq!(
            (report.trades[0].price, report.trades[0].quantity),
            (px(100), qty(5))
        );
        assert_eq!(
            (report.trades[1].price, report.trades[1].quantity),
            (px(101), qty(3))
        );

        // a1 fully consumed, a2 left with 2, a3 untouched
        assert!(!ob.index.contains_key(&id("a1")));
        assert_eq!(ob.best_ask(), Some(px(101)));
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), qty(2));
        assert!(ob.asks.contains_key(&px(102)));
    }

    #[test]
    fn market_buy_is_fifo_within_a_level() {
        let mut ob = book();
        // same price — the one added first must fill first
        ob.add_order(order(Side::Ask, 100, 5, None, "a1")).unwrap();
        ob.add_order(order(Side::Ask, 100, 5, None, "a2")).unwrap();

        let report = ob.submit(market_order(Side::Bid, 5, "t1")).unwrap();

        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.trades[0].maker_order_id, id("a1")); // oldest first
        assert!(!ob.index.contains_key(&id("a1")));
        assert!(ob.index.contains_key(&id("a2")));
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), qty(5)); // only a2 remains
    }

    #[test]
    fn market_buy_on_empty_book_is_killed() {
        let mut ob = book();

        let report = ob.submit(market_order(Side::Bid, 5, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Killed);
        assert!(report.trades.is_empty());
    }

    #[test]
    fn market_buy_insufficient_liquidity_kills_remainder() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 3, None, "a1")).unwrap();

        let report = ob.submit(market_order(Side::Bid, 10, "t1")).unwrap();

        // takes all 3, discards the unfilled 7 (IOC)
        assert_eq!(report.outcome, SubmitOutcome::Killed);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.trades[0].quantity, qty(3));
        assert_eq!(ob.best_ask(), None);
        assert!(ob.index.is_empty());
    }

    // ---- market SELL (incoming Ask) walks the bids, best (highest) price first ----

    #[test]
    fn market_sell_fills_against_best_bids_first() {
        let mut ob = book();
        ob.add_order(order(Side::Bid, 99, 5, None, "b1")).unwrap();
        ob.add_order(order(Side::Bid, 98, 5, None, "b2")).unwrap();

        let report = ob.submit(market_order(Side::Ask, 8, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        assert_eq!(report.trades.len(), 2);
        // highest bid first: 5 @ 99 then 3 @ 98
        assert_eq!(
            (report.trades[0].price, report.trades[0].quantity),
            (px(99), qty(5))
        );
        assert_eq!(
            (report.trades[1].price, report.trades[1].quantity),
            (px(98), qty(3))
        );
        assert_eq!(report.trades[0].taker_side, Side::Ask);

        assert_eq!(ob.best_bid(), Some(px(98)));
        assert_eq!(ob.best_bid_level().unwrap().total_quantity(), qty(2));
    }

    // ---- limit orders: rest when they don't cross, match the marketable prefix when they do ----

    #[test]
    fn limit_buy_non_crossing_rests() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 10, None, "a1")).unwrap();

        // buy at 99 < best ask 100 -> does not cross
        let report = ob.submit(limit_order(Side::Bid, 99, 5, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Rested);
        assert!(report.trades.is_empty());
        assert!(report.cancelled.is_empty());
        assert_eq!(ob.best_bid(), Some(px(99)));
        assert_eq!(ob.best_bid_level().unwrap().total_quantity(), qty(5));
        assert!(ob.index.contains_key(&report.order_id)); // rests under the assigned id
    }

    #[test]
    fn limit_buy_crossing_fully_fills() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 10, None, "a1")).unwrap();

        let report = ob.submit(limit_order(Side::Bid, 100, 5, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.trades[0].quantity, qty(5));
        // nothing rests for the taker; maker shrinks to 5
        assert!(!ob.index.contains_key(&report.order_id));
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), qty(5));
    }

    #[test]
    fn limit_buy_partial_fill_rests_remainder() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 5, None, "a1")).unwrap();

        // wants 8, only 5 available at a crossing price -> fill 5, rest 3
        let report = ob.submit(limit_order(Side::Bid, 100, 8, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::PartiallyFilledAndRested);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.trades[0].quantity, qty(5));

        assert_eq!(ob.best_ask(), None); // a1 fully consumed
        assert_eq!(ob.best_bid(), Some(px(100))); // remainder rests as a bid
        assert_eq!(ob.best_bid_level().unwrap().total_quantity(), qty(3));
        assert!(ob.index.contains_key(&report.order_id)); // rests under the assigned id
    }

    #[test]
    fn limit_buy_sweeps_only_marketable_prefix() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 5, None, "a1")).unwrap();
        ob.add_order(order(Side::Ask, 102, 5, None, "a2")).unwrap();

        // buy at 100 crosses a1 (100 <= 100) but NOT a2 (102 > 100)
        let report = ob.submit(limit_order(Side::Bid, 100, 10, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::PartiallyFilledAndRested);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(
            (report.trades[0].price, report.trades[0].quantity),
            (px(100), qty(5))
        );

        assert!(ob.asks.contains_key(&px(102))); // a2 untouched
        assert_eq!(ob.best_bid(), Some(px(100))); // remainder 5 rests
        assert_eq!(ob.best_bid_level().unwrap().total_quantity(), qty(5));
    }

    #[test]
    fn limit_buy_prints_at_maker_price_not_taker_price() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 10, None, "a1")).unwrap();

        // aggressive buy at 105 against an ask resting at 100
        let report = ob.submit(limit_order(Side::Bid, 105, 5, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        assert_eq!(report.trades[0].price, px(100)); // maker's 100, not taker's 105
    }

    #[test]
    fn limit_buy_crosses_multiple_levels_and_fully_fills() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 5, None, "a1")).unwrap();
        ob.add_order(order(Side::Ask, 101, 5, None, "a2")).unwrap();

        // buy at 101 crosses both levels; wants 8 -> 5 @ 100 then 3 @ 101, fully filled
        let report = ob.submit(limit_order(Side::Bid, 101, 8, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        assert_eq!(report.trades.len(), 2);
        assert_eq!(
            (report.trades[0].price, report.trades[0].quantity),
            (px(100), qty(5))
        );
        assert_eq!(
            (report.trades[1].price, report.trades[1].quantity),
            (px(101), qty(3))
        );

        assert!(!ob.index.contains_key(&id("a1"))); // a1 fully consumed
        assert!(!ob.index.contains_key(&report.order_id)); // taker fully filled — nothing rests
        assert_eq!(ob.best_ask(), Some(px(101))); // a2 left with 2
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), qty(2));
    }

    #[test]
    fn limit_buy_crosses_multiple_levels_then_rests_remainder() {
        let mut ob = book();
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
            (px(100), qty(5))
        );
        assert_eq!(
            (report.trades[1].price, report.trades[1].quantity),
            (px(101), qty(5))
        );

        // both crossed makers gone, a3 untouched
        assert!(!ob.index.contains_key(&id("a1")));
        assert!(!ob.index.contains_key(&id("a2")));
        assert_eq!(ob.best_ask(), Some(px(103)));
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), qty(5));

        // remainder rests on the bid side
        assert_eq!(ob.best_bid(), Some(px(101)));
        assert_eq!(ob.best_bid_level().unwrap().total_quantity(), qty(2));
        assert!(ob.index.contains_key(&report.order_id)); // rests under the assigned id
    }

    // ---- limit SELL (incoming Ask): rests above the book, sweeps bids when it crosses ----

    #[test]
    fn limit_sell_non_crossing_rests() {
        let mut ob = book();
        ob.add_order(order(Side::Bid, 99, 10, None, "b1")).unwrap();

        // sell at 100 > best bid 99 -> does not cross
        let report = ob.submit(limit_order(Side::Ask, 100, 5, "s1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Rested);
        assert!(report.trades.is_empty());
        assert!(report.cancelled.is_empty());
        assert_eq!(ob.best_ask(), Some(px(100)));
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), qty(5));
        assert!(ob.index.contains_key(&report.order_id));
    }

    #[test]
    fn limit_sell_crossing_fully_fills() {
        let mut ob = book();
        ob.add_order(order(Side::Bid, 100, 10, None, "b1")).unwrap();

        let report = ob.submit(limit_order(Side::Ask, 100, 5, "s1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(
            (report.trades[0].price, report.trades[0].quantity),
            (px(100), qty(5))
        );
        assert_eq!(report.trades[0].taker_side, Side::Ask);
        // nothing rests; b1 shrinks to 5
        assert!(!ob.index.contains_key(&report.order_id));
        assert_eq!(ob.best_bid_level().unwrap().total_quantity(), qty(5));
    }

    #[test]
    fn limit_sell_partial_fill_rests_remainder() {
        let mut ob = book();
        ob.add_order(order(Side::Bid, 100, 5, None, "b1")).unwrap();

        // wants to sell 8, only 5 bid at a crossing price -> fill 5, rest 3
        let report = ob.submit(limit_order(Side::Ask, 100, 8, "s1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::PartiallyFilledAndRested);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.trades[0].quantity, qty(5));

        assert_eq!(ob.best_bid(), None); // b1 fully consumed
        assert_eq!(ob.best_ask(), Some(px(100))); // remainder rests as an ask
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), qty(3));
        assert!(ob.index.contains_key(&report.order_id));
    }

    #[test]
    fn limit_sell_sweeps_only_marketable_prefix() {
        let mut ob = book();
        ob.add_order(order(Side::Bid, 100, 5, None, "b1")).unwrap();
        ob.add_order(order(Side::Bid, 98, 5, None, "b2")).unwrap();

        // sell at 100 crosses b1 (100 >= 100) but NOT b2 (98 < 100)
        let report = ob.submit(limit_order(Side::Ask, 100, 10, "s1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::PartiallyFilledAndRested);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(
            (report.trades[0].price, report.trades[0].quantity),
            (px(100), qty(5))
        );

        assert_eq!(ob.best_bid(), Some(px(98))); // b2 untouched, now best bid
        assert_eq!(ob.best_ask(), Some(px(100))); // remainder 5 rests
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), qty(5));
    }

    #[test]
    fn limit_sell_crosses_multiple_levels_then_rests_remainder() {
        let mut ob = book();
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
            (px(100), qty(5))
        );
        assert_eq!(
            (report.trades[1].price, report.trades[1].quantity),
            (px(99), qty(5))
        );

        assert_eq!(ob.best_bid(), Some(px(97))); // only b3 remains
        assert_eq!(ob.best_ask(), Some(px(99))); // remainder rests
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), qty(2));
    }

    // ---- IOC: fill what crosses now, never rest ----

    #[test]
    fn ioc_full_fill_behaves_like_gtc() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 10, None, "a1")).unwrap();

        let report = ob.submit(ioc_order(Side::Bid, 100, 10, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.trades[0].quantity, qty(10));
        assert_eq!(ob.best_ask(), None);
    }

    #[test]
    fn ioc_partial_fill_kills_remainder_instead_of_resting() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 10, None, "a1")).unwrap();

        // wants 15, only 10 crosses — GTC would rest the 5, IOC discards it
        let report = ob.submit(ioc_order(Side::Bid, 100, 15, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Killed);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.trades[0].quantity, qty(10));

        // nothing rested: no bid side, taker not in the index
        assert_eq!(ob.best_bid(), None);
        assert!(!ob.index.contains_key(&report.order_id));
    }

    #[test]
    fn ioc_that_does_not_cross_is_killed_with_no_trades() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 10, None, "a1")).unwrap();

        // bid at 99 doesn't reach the 100 ask — GTC would rest, IOC dies
        let report = ob.submit(ioc_order(Side::Bid, 99, 5, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Killed);
        assert!(report.trades.is_empty());

        // book completely untouched
        assert_eq!(ob.best_ask(), Some(px(100)));
        assert_eq!(ob.best_bid(), None);
        assert_eq!(ob.index.len(), 1);
    }

    #[test]
    fn ioc_respects_its_limit_price_while_sweeping() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 5, None, "a1")).unwrap();
        ob.add_order(order(Side::Ask, 101, 5, None, "a2")).unwrap();

        // wants 8 but only the 100 level crosses its limit
        let report = ob.submit(ioc_order(Side::Bid, 100, 8, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Killed);
        assert_eq!(report.trades.len(), 1);
        assert_eq!(
            (report.trades[0].price, report.trades[0].quantity),
            (px(100), qty(5))
        );
        // the 101 level is untouched, and nothing rested
        assert_eq!(ob.best_ask(), Some(px(101)));
        assert_eq!(ob.best_bid(), None);
    }

    // ---- FOK: fill completely right now, or touch nothing ----

    #[test]
    fn fok_fills_completely_when_depth_suffices() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 5, None, "a1")).unwrap();
        ob.add_order(order(Side::Ask, 101, 5, None, "a2")).unwrap();

        let report = ob.submit(fok_order(Side::Bid, 101, 10, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        assert_eq!(report.trades.len(), 2);
        let filled: Qty = report.trades.iter().map(|t| t.quantity).sum();
        assert_eq!(filled, qty(10));
        assert_eq!(ob.best_ask(), None);
    }

    #[test]
    fn fok_kills_without_touching_the_book_when_depth_insufficient() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 5, None, "a1")).unwrap();

        // wants 10, only 5 exists — nothing may execute, not even the 5
        let report = ob.submit(fok_order(Side::Bid, 100, 10, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Killed);
        assert!(report.trades.is_empty());
        assert!(report.cancelled.is_empty());

        // a1 still resting, untouched
        assert_eq!(ob.best_ask(), Some(px(100)));
        assert_eq!(ob.best_ask_level().unwrap().total_quantity(), qty(5));
        assert!(ob.index.contains_key(&id("a1")));
    }

    #[test]
    fn fok_only_counts_depth_within_its_limit_price() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 5, None, "a1")).unwrap();
        ob.add_order(order(Side::Ask, 102, 20, None, "a2")).unwrap();

        // 25 exists in total, but only 5 within the 101 limit
        let report = ob.submit(fok_order(Side::Bid, 101, 10, "t1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Killed);
        assert!(report.trades.is_empty());
        assert_eq!(ob.index.len(), 2); // both makers untouched
    }

    #[test]
    fn fok_excludes_own_resting_orders_from_fillability() {
        let mut ob = book();
        // alice's own ask can't fill alice — STP would cancel it, not trade it
        ob.add_order(limit_order(Side::Ask, 100, 5, "alice"))
            .unwrap();
        ob.add_order(limit_order(Side::Ask, 100, 5, "bob")).unwrap();

        // 10 rests at 100, but only bob's 5 is really fillable for alice
        let report = ob.submit(fok_order(Side::Bid, 100, 10, "alice")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Killed);
        assert!(report.trades.is_empty());
        // crucially: alice's resting order was NOT self-trade-cancelled,
        // because nothing executed
        assert!(report.cancelled.is_empty());
        assert_eq!(ob.index.len(), 2);
    }

    #[test]
    fn fok_executes_through_own_order_cancelling_it() {
        let mut ob = book();
        // alice's order is first in FIFO, bob's behind it has enough depth
        ob.add_order(limit_order(Side::Ask, 100, 5, "alice"))
            .unwrap();
        ob.add_order(limit_order(Side::Ask, 100, 10, "bob"))
            .unwrap();

        // dry-run: bob's 10 ≥ 10 → execute; sweep STP-cancels alice's on the way
        let report = ob.submit(fok_order(Side::Bid, 100, 10, "alice")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        let filled: Qty = report.trades.iter().map(|t| t.quantity).sum();
        assert_eq!(filled, qty(10));
        assert_eq!(report.cancelled, vec![id("alice")]);
        assert_eq!(ob.best_ask(), None); // level fully drained
    }

    // ---- stops: park, trigger off trades, cascade ----

    #[test]
    fn stop_parks_when_no_trade_has_printed() {
        let mut ob = book();

        let report = ob.submit(stop_market(Side::Bid, 101, 10, "s1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::StopPending);
        assert!(report.trades.is_empty());
        assert!(report.triggered.is_empty());

        // parked: findable and cancellable, but holds no book depth
        assert!(ob.get_order(&report.order_id).is_some());
        assert_eq!(ob.best_bid(), None);
        assert!(ob.depth(Side::Bid, 10).is_empty());
        assert_eq!(ob.stop_bids.len(), 1);
    }

    #[test]
    fn buy_stop_triggers_when_market_trades_up_to_trigger() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 101, 10, None, "a1")).unwrap();
        ob.add_order(order(Side::Ask, 102, 10, None, "a2")).unwrap();

        // parks: no trade has printed yet
        let parked = ob.submit(stop_market(Side::Bid, 101, 10, "s1")).unwrap();
        assert_eq!(parked.outcome, SubmitOutcome::StopPending);

        // this trade prints at 101 >= trigger → stop fires as a market buy
        let report = ob.submit(market_order(Side::Bid, 10, "t1")).unwrap();

        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.trades[0].price, px(101));
        assert_eq!(report.triggered.len(), 1);

        let stop_report = &report.triggered[0];
        assert_eq!(stop_report.order_id, parked.order_id); // id survives activation
        assert_eq!(stop_report.outcome, SubmitOutcome::Filled);
        assert_eq!(stop_report.trades[0].price, px(102)); // swept the next level

        // stop book is drained, nothing pending
        assert!(ob.stop_bids.is_empty());
        assert!(ob.get_order(&parked.order_id).is_none());
    }

    #[test]
    fn sell_stop_triggers_when_market_trades_down_to_trigger() {
        let mut ob = book();
        ob.add_order(order(Side::Bid, 99, 10, None, "b1")).unwrap();
        ob.add_order(order(Side::Bid, 98, 10, None, "b2")).unwrap();

        let parked = ob.submit(stop_market(Side::Ask, 99, 10, "s1")).unwrap();
        assert_eq!(parked.outcome, SubmitOutcome::StopPending);

        // sell 5 @ 99 → last trade 99 <= trigger 99 → stop fires
        let report = ob.submit(market_order(Side::Ask, 5, "t1")).unwrap();

        assert_eq!(report.triggered.len(), 1);
        let stop_report = &report.triggered[0];
        assert_eq!(stop_report.outcome, SubmitOutcome::Filled);
        // fills the rest of 99 (5 left), then 5 more at 98
        let filled: Qty = stop_report.trades.iter().map(|t| t.quantity).sum();
        assert_eq!(filled, qty(10));
        assert_eq!(stop_report.trades.last().unwrap().price, px(98));
    }

    #[test]
    fn stop_cascade_chains_and_stays_flat() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 101, 10, None, "a1")).unwrap();
        ob.add_order(order(Side::Ask, 102, 10, None, "a2")).unwrap();
        ob.add_order(order(Side::Ask, 103, 10, None, "a3")).unwrap();

        // stop A fires at 101; its fill at 102 fires stop B
        let a = ob.submit(stop_market(Side::Bid, 101, 10, "sA")).unwrap();
        let b = ob.submit(stop_market(Side::Bid, 102, 10, "sB")).unwrap();

        let report = ob.submit(market_order(Side::Bid, 10, "t1")).unwrap();

        // one flat list, in trigger order, no nesting
        assert_eq!(report.triggered.len(), 2);
        assert_eq!(report.triggered[0].order_id, a.order_id);
        assert_eq!(report.triggered[1].order_id, b.order_id);
        assert!(report.triggered.iter().all(|r| r.triggered.is_empty()));

        assert_eq!(report.triggered[0].trades[0].price, px(102));
        assert_eq!(report.triggered[1].trades[0].price, px(103));
        assert!(ob.stop_bids.is_empty());
    }

    #[test]
    fn stop_already_triggered_on_arrival_executes_immediately() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 10, None, "a1")).unwrap();

        // print a trade at 100
        ob.submit(market_order(Side::Bid, 5, "t1")).unwrap();

        // trigger 100 <= last trade 100 → activates NOW, never parks
        let report = ob.submit(stop_market(Side::Bid, 100, 5, "s1")).unwrap();

        assert_eq!(report.outcome, SubmitOutcome::Filled);
        assert_eq!(report.trades.len(), 1);
        assert!(ob.stop_bids.is_empty());
    }

    #[test]
    fn stop_limit_becomes_limit_and_rests_its_remainder() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 101, 10, None, "a1")).unwrap();
        ob.add_order(order(Side::Ask, 102, 20, None, "a2")).unwrap();

        // trigger at 101, then buy up to 30 at limit 102
        let parked = ob
            .submit(stop_limit(Side::Bid, 101, 102, 30, "s1"))
            .unwrap();
        assert_eq!(parked.outcome, SubmitOutcome::StopPending);

        let report = ob.submit(market_order(Side::Bid, 10, "t1")).unwrap();

        let stop_report = &report.triggered[0];
        assert_eq!(stop_report.outcome, SubmitOutcome::PartiallyFilledAndRested);
        let filled: Qty = stop_report.trades.iter().map(|t| t.quantity).sum();
        assert_eq!(filled, qty(20)); // all of a2

        // the unfilled 10 rests as a normal limit bid at 102
        assert_eq!(ob.best_bid(), Some(px(102)));
        let rested = ob.get_order(&stop_report.order_id).unwrap();
        assert_eq!(rested.remaining_quantity, qty(10));
        assert_eq!(rested.order_type.limit_price(), Some(px(102)));
    }

    #[test]
    fn pending_stop_can_be_cancelled() {
        let mut ob = book();

        let parked = ob.submit(stop_market(Side::Ask, 95, 10, "s1")).unwrap();
        assert_eq!(parked.outcome, SubmitOutcome::StopPending);

        assert!(ob.cancel_order(parked.order_id.clone()).is_ok());
        assert!(ob.stop_asks.is_empty());
        assert!(ob.index.is_empty());
        assert!(
            ob.cancel_order(parked.order_id)
                .is_err_and(|e| e == OrderBookError::OrderNotFound)
        );
    }

    #[test]
    fn stops_fifo_within_same_trigger_price() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 5, None, "a1")).unwrap();
        ob.add_order(order(Side::Ask, 101, 20, None, "a2")).unwrap();

        let first = ob.submit(stop_market(Side::Bid, 100, 5, "s1")).unwrap();
        let second = ob.submit(stop_market(Side::Bid, 100, 5, "s2")).unwrap();

        let report = ob.submit(market_order(Side::Bid, 5, "t1")).unwrap();

        assert_eq!(report.triggered.len(), 2);
        assert_eq!(report.triggered[0].order_id, first.order_id); // parked first, fires first
        assert_eq!(report.triggered[1].order_id, second.order_id);
    }

    // ---- report plumbing ----

    #[test]
    fn submit_assigns_exchange_id_and_ignores_caller_id() {
        let mut ob = book();
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
        let mut ob = book();
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
        let mut ob = book();
        let maker = Order::builder()
            .side(Side::Ask)
            .quantity(qty(10))
            .client_id("alice")
            .exchange_id("a1")
            .order_type(OrderType::limit_gtc(px(100)))
            .build();
        ob.add_order(maker).unwrap();

        let taker = Order::builder()
            .side(Side::Bid)
            .quantity(qty(5))
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
