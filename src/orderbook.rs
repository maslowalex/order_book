use std::cmp::Reverse;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};

use crate::instrument::{InstrumentSpec, Qty, RejectReason, Ticks};
use crate::types::{ExchangeId, Order, OrderType, Price, PriceLevel, Side};

/// Where a live order physically is — needed by `cancel_order`/`get_order`
/// to know which structure to search. A bare `(Side, Price, kind)` tuple
/// would invite reading a trigger price as a book price; the enum makes the
/// two residencies impossible to confuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderLocation {
    /// Resting in the book at this level price.
    Book { side: Side, price: Price },
    /// Parked in the stop book, waiting for the market to reach `trigger`.
    StopBook { side: Side, trigger: Price },
}

#[derive(Debug, Clone)]
pub struct OrderBook {
    pub bids: BTreeMap<Reverse<Price>, PriceLevel>, // descending: best (highest) bid first
    pub asks: BTreeMap<Price, PriceLevel>,          // ascending: best (lowest) ask first
    /// Buy stops keyed by trigger, FIFO within one trigger price. A buy stop
    /// fires when the market trades UP to its trigger, so the LOWEST key is
    /// nearest to firing.
    pub stop_bids: BTreeMap<Price, Vec<Order>>,
    /// Sell stops keyed by trigger. Fires when the market trades DOWN to the
    /// trigger, so the HIGHEST key is nearest to firing.
    pub stop_asks: BTreeMap<Price, Vec<Order>>,
    /// Price of the most recent trade — the signal stop triggers compare to.
    pub last_trade_price: Option<Price>,
    pub index: HashMap<ExchangeId, OrderLocation>,
    pub next_seq: u64,
    /// The instrument's tick and lot grid.
    ///
    /// This field replaces a comment. The book used to carry a note saying that
    /// quantities were stored "at the lowest fraction for precision" with
    /// normalization done "on the higher levels" — a description of an
    /// invariant that nothing enforced and, with `Price` aliased to `Decimal`,
    /// nothing could. Every price and quantity in this book is now a point on
    /// this spec's lattice, and the type system knows it.
    spec: InstrumentSpec,
}

#[derive(Debug, PartialEq)]
pub enum OrderBookError {
    Generic,
    ExchangeIdDuplicated,
    OrderNotFound,
    /// Order type the matching engine doesn't handle yet.
    Unsupported,
    /// Only limit orders can rest in the book — market orders execute or die,
    /// stops park in the stop book until triggered. Before `OrderType` carried
    /// its price, `add_order` would silently rest a market order at 0.00.
    NotRestable,
    /// Well-formed, on the lattice, and outside what this instrument accepts.
    /// Note what is NOT here: an off-tick price or an off-lot quantity, which
    /// no longer have a way to reach the engine at all.
    Rejected(RejectReason),
}

impl OrderBook {
    /// A book must be told what instrument it trades before it can hold a
    /// single order — there is no `Default`, deliberately. A *default* lattice
    /// would be the "someone upstream normalized this" assumption sneaking back
    /// in through a derive, which is the exact failure this spec exists to end.
    /// Callers should be able to point at the line where they chose a grid;
    /// [`InstrumentSpec::cents`] is the named one-cent-tick default.
    #[allow(clippy::new_without_default)]
    pub fn new(spec: InstrumentSpec) -> Self {
        OrderBook {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            stop_bids: BTreeMap::new(),
            stop_asks: BTreeMap::new(),
            last_trade_price: None,
            index: HashMap::new(),
            next_seq: 1,
            spec,
        }
    }

    pub fn spec(&self) -> InstrumentSpec {
        self.spec
    }

    /// The instrument's admission policy, applied to one order.
    ///
    /// Both doors call this — `submit` and `add_order` — because `add_order`
    /// is public and rests orders without matching, so leaving it ungated
    /// would reopen the exact hole this work closed: a way into the book that
    /// skips the checks.
    ///
    /// Only bounds are checked. Tick and lot alignment cannot be violated by
    /// anything that reached this far, because `Price` and `Qty` cannot hold
    /// an off-grid value.
    pub(crate) fn admit(&self, order: &Order) -> Result<(), OrderBookError> {
        for price in order.order_type.prices().into_iter().flatten() {
            self.spec
                .admits_price(price)
                .map_err(OrderBookError::Rejected)?;
        }
        self.spec
            .admits_qty(order.original_quantity)
            .map_err(OrderBookError::Rejected)?;

        // Min-notional only applies where a price is known at ingress.
        // `Market` and `StopMarket` carry none — `limit_price()` returns
        // `None` for both — so their value is genuinely unknowable here.
        // Real venues substitute a reference price; this one skips, and says
        // so rather than pretending the rule was applied.
        //
        // Note also what is NOT enforced: a partial fill can leave a resting
        // remainder worth less than the floor. That dust is allowed to rest.
        // Cancelling it would make quantity vanish without a corresponding
        // fill, which is precisely what the conservation and depth-accounting
        // properties forbid — so it would be a much larger change than it
        // looks, and it belongs with a deliberate dust policy, not here.
        if let Some(price) = order.order_type.limit_price() {
            self.spec
                .admits_notional(price, order.original_quantity)
                .map_err(OrderBookError::Rejected)?;
        }

        Ok(())
    }

    pub fn add_order(&mut self, order: Order) -> Result<(), OrderBookError> {
        let OrderType::Limit { price, .. } = order.order_type else {
            return Err(OrderBookError::NotRestable);
        };
        self.admit(&order)?;
        let side = order.side;
        let exchange_id = order.exchange_id.clone();

        match self.index.entry(exchange_id) {
            Entry::Occupied(_) => return Err(OrderBookError::ExchangeIdDuplicated),
            Entry::Vacant(e) => e.insert(OrderLocation::Book { side, price }),
        };

        let price_level = match side {
            Side::Ask => self
                .asks
                .entry(price)
                .or_insert_with(|| PriceLevel::new(price, side)),
            Side::Bid => self
                .bids
                .entry(Reverse(price))
                .or_insert_with(|| PriceLevel::new(price, side)),
        };

        price_level
            .add_order(order)
            .map_err(|_| OrderBookError::Generic)?;

        Ok(())
    }

    pub fn cancel_order(&mut self, exchange_id: ExchangeId) -> Result<(), OrderBookError> {
        let location = self
            .index
            .remove(&exchange_id)
            .ok_or(OrderBookError::OrderNotFound)?;

        match location {
            OrderLocation::Book {
                side: Side::Ask,
                price,
            } => {
                if let Some(level) = self.asks.get_mut(&price) {
                    level.remove_order(&exchange_id);
                    if level.is_empty() {
                        self.asks.remove(&price);
                    }
                }
            }
            OrderLocation::Book {
                side: Side::Bid,
                price,
            } => {
                if let Some(level) = self.bids.get_mut(&Reverse(price)) {
                    level.remove_order(&exchange_id);
                    if level.is_empty() {
                        self.bids.remove(&Reverse(price));
                    }
                }
            }
            OrderLocation::StopBook { side, trigger } => {
                let stops = match side {
                    Side::Bid => &mut self.stop_bids,
                    Side::Ask => &mut self.stop_asks,
                };
                if let Some(queue) = stops.get_mut(&trigger) {
                    queue.retain(|o| o.exchange_id != exchange_id);
                    if queue.is_empty() {
                        stops.remove(&trigger);
                    }
                }
            }
        }

        Ok(())
    }

    pub fn best_bid(&self) -> Option<Price> {
        self.bids.iter().next().map(|(price, _)| price.0)
    }

    pub fn best_ask(&self) -> Option<Price> {
        self.asks.iter().next().map(|(price, _)| *price)
    }

    /// The touch, measured in ticks — which is how a spread is actually quoted
    /// ("it's two ticks wide"), and the only unit in which the number is
    /// comparable across instruments. A currency spread of `0.50` is tight on
    /// one book and wide on another; two ticks is two ticks.
    ///
    /// Returning [`Ticks`] rather than a `Price` is not decoration. A price
    /// difference is not a price — you cannot rest an order at a spread — and
    /// keeping the types apart makes `best_ask() + best_bid()` fail to compile
    /// instead of quietly type-checking.
    pub fn spread(&self) -> Option<Ticks> {
        match (self.best_ask(), self.best_bid()) {
            (Some(ask), Some(bid)) => Some(self.spec.ticks_between(ask, bid)),
            _ => None,
        }
    }

    pub fn best_bid_level(&self) -> Option<PriceLevel> {
        self.bids
            .iter()
            .next()
            .map(|(_price, price_level)| price_level.clone())
    }

    pub fn best_ask_level(&self) -> Option<PriceLevel> {
        self.asks
            .iter()
            .next()
            .map(|(_price, price_level)| price_level.clone())
    }

    /// Aggregated market depth: the top `levels` price levels of `side`,
    /// best price first, as `(price, total resting quantity)`.
    pub fn depth(&self, side: Side, levels: usize) -> Vec<(Price, Qty)> {
        let aggregate = |level: &PriceLevel| (level.price, level.total_quantity());
        match side {
            Side::Bid => self.bids.values().take(levels).map(aggregate).collect(),
            Side::Ask => self.asks.values().take(levels).map(aggregate).collect(),
        }
    }

    /// Look up a live order by its exchange id — resting in the book or
    /// parked in the stop book. Index hop to the level/queue, then a linear
    /// scan within it (same O(level) cost as cancel).
    pub fn get_order(&self, exchange_id: &ExchangeId) -> Option<&Order> {
        let orders = match self.index.get(exchange_id)? {
            OrderLocation::Book {
                side: Side::Bid,
                price,
            } => &self.bids.get(&Reverse(*price))?.orders,
            OrderLocation::Book {
                side: Side::Ask,
                price,
            } => &self.asks.get(price)?.orders,
            OrderLocation::StopBook {
                side: Side::Bid,
                trigger,
            } => self.stop_bids.get(trigger)?,
            OrderLocation::StopBook {
                side: Side::Ask,
                trigger,
            } => self.stop_asks.get(trigger)?,
        };
        orders.iter().find(|o| &o.exchange_id == exchange_id)
    }

    pub fn crosses(&self, side: Side, price: Price) -> bool {
        match side {
            Side::Ask => match self.best_bid() {
                None => false,
                Some(best_bid) => price <= best_bid,
            },
            Side::Bid => match self.best_ask() {
                None => false,
                Some(best_ask) => price >= best_ask,
            },
        }
    }
}
/*
Q: Why knowing the *spread* is important?

A: At any moment when a LIMIT order arrives, we must check if it crosses the spread.
   For example, best ask 100 and best bid 99 (spread = 1). A LIMIT buy at 101 crosses
   the spread because 101 >= best ask (100), so it executes immediately as a taker
   rather than resting in the book.
*/
#[cfg(test)]
mod test {
    use super::*;
    use crate::instrument::{RejectReason, SpecError};
    use crate::test_helpers::{book, order, px, qty};
    use rust_decimal::Decimal;

    /// A non-crossed book: bids 99/98/97, asks 100/101/102.
    /// best_bid = 99, best_ask = 100, spread = 1.
    fn book_with_depth() -> OrderBook {
        let mut ob = book();
        ob.add_order(order(Side::Bid, 99, 110, None, "bid_99"))
            .unwrap();
        ob.add_order(order(Side::Bid, 98, 500, None, "bid_98"))
            .unwrap();
        ob.add_order(order(Side::Bid, 97, 500, None, "bid_97"))
            .unwrap();
        ob.add_order(order(Side::Ask, 100, 100, None, "ask_100"))
            .unwrap();
        ob.add_order(order(Side::Ask, 101, 200, None, "ask_101"))
            .unwrap();
        ob.add_order(order(Side::Ask, 102, 500, None, "ask_102"))
            .unwrap();
        ob
    }

    #[test]
    fn order_book_new_returns_empty_orderbook() {
        let orderbook = book();
        let empty_bids: BTreeMap<Reverse<Price>, PriceLevel> = BTreeMap::new();
        let empty_asks: BTreeMap<Price, PriceLevel> = BTreeMap::new();
        assert_eq!(orderbook.bids, empty_bids);
        assert_eq!(orderbook.asks, empty_asks);
    }

    #[test]
    fn add_order_adds_order_to_correct_side_ask() {
        let mut orderbook = book();

        assert!(
            orderbook
                .add_order(order(Side::Ask, 100, 10, None, "ex_1"))
                .is_ok()
        );
        assert_eq!(orderbook.asks.len(), 1);
        assert_eq!(orderbook.bids.len(), 0);
    }

    #[test]
    fn add_order_adds_order_to_correct_side_bid() {
        let mut orderbook = book();

        assert!(
            orderbook
                .add_order(order(Side::Bid, 100, 10, None, "ex_1"))
                .is_ok()
        );
        assert_eq!(orderbook.asks.len(), 0);
        assert_eq!(orderbook.bids.len(), 1);
    }

    #[test]
    fn add_order_adds_multiple_orders() {
        let mut orderbook = book();

        assert!(
            orderbook
                .add_order(order(Side::Ask, 100, 10, None, "ex_1"))
                .is_ok()
        );
        assert!(
            orderbook
                .add_order(order(Side::Ask, 100, 5, None, "ex_2"))
                .is_ok()
        );

        assert_eq!(orderbook.bids.len(), 0);

        let level = orderbook.asks.get(&px(100)).unwrap();
        assert_eq!(level.total_quantity(), qty(15)); // 10 + 5
    }

    #[test]
    fn maintains_an_index_of_all_orders() {
        let mut orderbook = book();

        orderbook
            .add_order(order(Side::Ask, 100, 10, None, "ex_1"))
            .unwrap();

        let location = orderbook.index.get(&ExchangeId("ex_1".to_owned())).unwrap();
        assert_eq!(
            *location,
            OrderLocation::Book {
                side: Side::Ask,
                price: px(100)
            }
        );
    }

    #[test]
    fn add_order_rejects_duplicate_exchange_id() {
        let mut orderbook = book();

        assert!(
            orderbook
                .add_order(order(Side::Ask, 100, 10, None, "same_id"))
                .is_ok()
        );
        assert!(
            orderbook
                .add_order(order(Side::Bid, 99, 5, None, "same_id"))
                .is_err_and(|e| e == OrderBookError::ExchangeIdDuplicated)
        );
    }

    #[test]
    fn cancel_order_cancels_existing_order_by_id() {
        let mut orderbook = book();

        assert!(
            orderbook
                .add_order(order(Side::Ask, 100, 10, None, "ex_1"))
                .is_ok()
        );
        assert!(
            orderbook
                .cancel_order(ExchangeId("ex_1".to_owned()))
                .is_ok()
        );
    }

    #[test]
    fn cancel_order_on_bid_side() {
        let mut orderbook = book();

        orderbook
            .add_order(order(Side::Bid, 99, 10, None, "bid_order"))
            .unwrap();

        assert!(
            orderbook
                .cancel_order(ExchangeId("bid_order".to_owned()))
                .is_ok()
        );
        assert_eq!(orderbook.bids.len(), 0);
        assert!(orderbook.index.is_empty());
    }

    #[test]
    fn cancel_order_removes_only_target_order_from_level() {
        let mut orderbook = book();

        orderbook
            .add_order(order(Side::Ask, 100, 10, None, "ex_1"))
            .unwrap();
        orderbook
            .add_order(order(Side::Ask, 100, 5, None, "ex_2"))
            .unwrap();

        orderbook
            .cancel_order(ExchangeId("ex_1".to_owned()))
            .unwrap();

        // price level still exists with remaining order
        let level = orderbook.asks.get(&px(100)).unwrap();
        assert_eq!(level.orders.len(), 1);
        assert_eq!(level.total_quantity(), qty(5));

        // index only has the remaining order
        assert_eq!(orderbook.index.len(), 1);
        assert!(orderbook.index.contains_key(&ExchangeId("ex_2".to_owned())));
    }

    #[test]
    fn cancel_order_cleans_up_empty_price_level() {
        let mut orderbook = book();

        orderbook
            .add_order(order(Side::Ask, 100, 10, None, "ex_1"))
            .unwrap();
        orderbook
            .cancel_order(ExchangeId("ex_1".to_owned()))
            .unwrap();

        assert_eq!(orderbook.asks.len(), 0);
        assert!(orderbook.index.is_empty());
    }

    #[test]
    fn cancel_order_with_non_existing_order_returns_error() {
        let mut orderbook = book();
        let result = orderbook.cancel_order(ExchangeId("nonexisting_order".to_owned()));

        assert!(result.is_err_and(|e| e == OrderBookError::OrderNotFound));
    }

    #[test]
    fn best_bid_and_best_ask_return_top_of_book() {
        let orderbook = book_with_depth();

        assert_eq!(orderbook.best_bid(), Some(px(99)));
        assert_eq!(orderbook.best_ask(), Some(px(100)));
    }

    #[test]
    fn best_bid_and_ask_are_none_on_empty_book() {
        let orderbook = book();

        assert_eq!(orderbook.best_bid(), None);
        assert_eq!(orderbook.best_ask(), None);
    }

    #[test]
    fn spread_is_difference_between_best_ask_and_best_bid() {
        let orderbook = book_with_depth();

        // 100.00 − 99.99, which on a one-cent tick is one tick wide. The
        // number is unchanged; the unit is now stated rather than assumed.
        assert_eq!(orderbook.spread(), Some(Ticks::from_count(1)));
    }

    #[test]
    fn spread_is_none_when_either_side_empty() {
        let mut orderbook = book();
        assert_eq!(orderbook.spread(), None);

        orderbook
            .add_order(order(Side::Bid, 99, 10, None, "bid"))
            .unwrap();
        assert_eq!(orderbook.spread(), None); // only one side present
    }

    #[test]
    fn best_level_returns_top_of_book_with_its_orders() {
        let orderbook = book_with_depth();

        let bid_level = orderbook.best_bid_level().unwrap();
        assert_eq!(bid_level.price, px(99));
        assert_eq!(bid_level.total_quantity(), qty(110));

        let ask_level = orderbook.best_ask_level().unwrap();
        assert_eq!(ask_level.price, px(100));
        assert_eq!(ask_level.total_quantity(), qty(100));
    }

    #[test]
    fn best_level_is_none_on_empty_book() {
        let orderbook = book();

        assert!(orderbook.best_bid_level().is_none());
        assert!(orderbook.best_ask_level().is_none());
    }

    #[test]
    fn depth_returns_best_levels_first_aggregated() {
        // bids 99×110, 98×500, 97×500; asks 100×100, 101×200, 102×500
        let orderbook = book_with_depth();

        assert_eq!(
            orderbook.depth(Side::Bid, 2),
            vec![(px(99), qty(110)), (px(98), qty(500))]
        );
        assert_eq!(
            orderbook.depth(Side::Ask, 2),
            vec![(px(100), qty(100)), (px(101), qty(200))]
        );
    }

    #[test]
    fn depth_is_capped_by_available_levels() {
        let orderbook = book_with_depth();

        assert_eq!(orderbook.depth(Side::Ask, 10).len(), 3);
        assert_eq!(book().depth(Side::Bid, 5), vec![]);
    }

    #[test]
    fn depth_sums_all_orders_at_a_level() {
        let mut orderbook = book();
        orderbook
            .add_order(order(Side::Ask, 100, 10, None, "ex_1"))
            .unwrap();
        orderbook
            .add_order(order(Side::Ask, 100, 5, None, "ex_2"))
            .unwrap();

        assert_eq!(orderbook.depth(Side::Ask, 1), vec![(px(100), qty(15))]);
    }

    #[test]
    fn get_order_finds_a_resting_order() {
        let orderbook = book_with_depth();

        let found = orderbook
            .get_order(&ExchangeId("ask_101".to_owned()))
            .unwrap();
        assert_eq!(found.side, Side::Ask);
        assert_eq!(found.remaining_quantity, qty(200));
    }

    #[test]
    fn get_order_returns_none_for_unknown_id() {
        let orderbook = book_with_depth();

        assert!(
            orderbook
                .get_order(&ExchangeId("nope".to_owned()))
                .is_none()
        );
    }

    #[test]
    fn get_order_returns_none_after_cancel() {
        let mut orderbook = book_with_depth();
        orderbook
            .cancel_order(ExchangeId("bid_99".to_owned()))
            .unwrap();

        assert!(
            orderbook
                .get_order(&ExchangeId("bid_99".to_owned()))
                .is_none()
        );
    }

    #[test]
    fn crosses_returns_false_on_empty_book() {
        let orderbook = book();

        assert!(!orderbook.crosses(Side::Bid, px(100)));
        assert!(!orderbook.crosses(Side::Ask, px(100)));
    }

    #[test]
    fn crosses_detects_marketable_orders() {
        // book_with_depth: best_bid = 99, best_ask = 100
        let orderbook = book_with_depth();

        // (incoming side, limit price in cents, should it cross?)
        let cases = [
            // a BUY crosses when it meets or beats the best ask (100)
            (Side::Bid, 101, true),
            (Side::Bid, 100, true),
            (Side::Bid, 99, false),
            // a SELL crosses when it meets or undercuts the best bid (99)
            (Side::Ask, 98, true),
            (Side::Ask, 99, true),
            (Side::Ask, 100, false),
        ];

        for (side, price, expected) in cases {
            assert_eq!(
                orderbook.crosses(side, px(price)),
                expected,
                "crosses({side:?}, {price}) should be {expected}",
            );
        }
    }

    // ---------------------------------------------------------- admission

    /// Cents, a one-cent tick, and bounds that actually bite: prices in
    /// 1.00..=200.00, sizes in 5..=1000.
    fn bounded_spec() -> InstrumentSpec {
        InstrumentSpec::cents()
            .with_price_range(
                Price::from_minor_unchecked(100),
                Some(Price::from_minor_unchecked(20_000)),
            )
            .unwrap()
            .with_qty_range(qty(5), Some(qty(1000)))
            .unwrap()
    }

    #[test]
    fn submit_rejects_a_quantity_below_the_minimum() {
        let mut ob = OrderBook::new(bounded_spec());
        let result = ob.submit(order(Side::Bid, 5000, 4, None, "small"));

        assert_eq!(
            result,
            Err(OrderBookError::Rejected(
                RejectReason::QuantityBelowMinimum {
                    qty: qty(4),
                    min: qty(5),
                }
            ))
        );
    }

    #[test]
    fn submit_rejects_a_price_outside_the_band() {
        let mut ob = OrderBook::new(bounded_spec());

        assert!(matches!(
            ob.submit(order(Side::Bid, 50, 10, None, "cheap")),
            Err(OrderBookError::Rejected(
                RejectReason::PriceBelowMinimum { .. }
            ))
        ));
        assert!(matches!(
            ob.submit(order(Side::Ask, 30_000, 10, None, "dear")),
            Err(OrderBookError::Rejected(
                RejectReason::PriceAboveMaximum { .. }
            ))
        ));
    }

    /// A rejected order is not an order: it consumes no sequence number, rests
    /// nothing, and indexes nothing. `next_seq` is an id generator, so burning
    /// one on something that never became an order would leave a hole.
    #[test]
    fn a_rejected_submit_leaves_the_book_bit_identical() {
        let mut ob = OrderBook::new(bounded_spec());
        ob.submit(order(Side::Bid, 5000, 10, None, "good"))
            .expect("within bounds");

        let seq_before = ob.next_seq;
        let depth_before = ob.depth(Side::Bid, 10);
        let index_before = ob.index.len();

        assert!(ob.submit(order(Side::Bid, 5000, 4, None, "small")).is_err());

        assert_eq!(ob.next_seq, seq_before, "a reject must not burn an id");
        assert_eq!(ob.depth(Side::Bid, 10), depth_before);
        assert_eq!(ob.index.len(), index_before);
    }

    /// `add_order` is public and rests without matching, so it is a second
    /// door into the book and gets the same lock.
    #[test]
    fn add_order_is_gated_too() {
        let mut ob = OrderBook::new(bounded_spec());
        let result = ob.add_order(order(Side::Bid, 5000, 4, None, "small"));

        assert!(matches!(
            result,
            Err(OrderBookError::Rejected(
                RejectReason::QuantityBelowMinimum { .. }
            ))
        ));
        assert!(ob.bids.is_empty());
        assert!(ob.index.is_empty());
    }

    /// A stop's trigger is a price on the same grid as any other, so the band
    /// applies to it — `limit_price()` alone would have missed this one, since
    /// a StopMarket has no limit price at all.
    #[test]
    fn a_stop_trigger_is_checked_against_the_band() {
        let mut ob = OrderBook::new(bounded_spec());
        let stop = Order::builder()
            .side(Side::Bid)
            .quantity(qty(10))
            .client_id("s")
            .exchange_id("s")
            .order_type(OrderType::stop_market(px(50)))
            .build();

        assert!(matches!(
            ob.submit(stop),
            Err(OrderBookError::Rejected(
                RejectReason::PriceBelowMinimum { .. }
            ))
        ));
    }

    /// $10 floor, cents, unit lot — so the notional lattice is hundredths and
    /// the threshold is 1000 raw units.
    fn min_notional_spec() -> InstrumentSpec {
        InstrumentSpec::cents()
            .with_min_notional(Decimal::new(10, 0))
            .unwrap()
    }

    #[test]
    fn submit_rejects_an_order_worth_less_than_the_floor() {
        let mut ob = OrderBook::new(min_notional_spec());

        // $0.10 x 99 == $9.90
        assert!(matches!(
            ob.submit(order(Side::Bid, 10, 99, None, "dust")),
            Err(OrderBookError::Rejected(
                RejectReason::NotionalBelowMinimum { .. }
            ))
        ));

        // $0.10 x 100 == $10.00 exactly, which clears the floor
        assert!(ob.submit(order(Side::Bid, 10, 100, None, "exact")).is_ok());
    }

    /// A market order has no price at ingress, so its notional is unknowable
    /// and the rule cannot apply. It must pass, not fail closed — the book
    /// says so out loud rather than pretending the check ran.
    #[test]
    fn a_market_order_skips_the_notional_floor() {
        let mut ob = OrderBook::new(min_notional_spec());
        ob.add_order(order(Side::Ask, 10, 100, None, "maker"))
            .expect("clears the floor");

        let taker = Order::builder()
            .side(Side::Bid)
            .quantity(qty(1))
            .client_id("t")
            .exchange_id("t")
            .order_type(OrderType::Market)
            .build();

        assert!(ob.submit(taker).is_ok());
    }

    /// The other half of the story: an off-tick price never reaches admission,
    /// because it cannot be built. This is the difference between a rule the
    /// book enforces and a rule the type system discharges.
    #[test]
    fn an_off_tick_price_cannot_even_be_constructed() {
        let spec = InstrumentSpec::new(2, 0, 25, 1).unwrap();

        assert!(matches!(
            spec.price_from_minor(10_003),
            Err(SpecError::PriceOffTick { .. })
        ));
    }
}
