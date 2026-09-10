use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};

use crate::allocation::MatchingAlgorithm;
use crate::instrument::{InstrumentSpec, Qty, RejectReason, Ticks};
use crate::order_queue::{OrderArena, OrderKey};
use crate::storage::{BTreeStore, OrderBookStore, StoreError};
use crate::types::{ExchangeId, Order, OrderType, Price, PriceLevel, PriceLevelView, Side};

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

/// Physical address kept private so handles never escape their owning book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OrderAddress {
    Book {
        side: Side,
        price: Price,
        key: OrderKey,
    },
    StopBook {
        side: Side,
        trigger: Price,
    },
}

impl OrderAddress {
    fn location(self) -> OrderLocation {
        match self {
            Self::Book { side, price, .. } => OrderLocation::Book { side, price },
            Self::StopBook { side, trigger } => OrderLocation::StopBook { side, trigger },
        }
    }
}

/// The book, parameterised independently by allocation policy and active-book
/// storage.
///
/// `M` is the *allocation* axis — who at a price level gets filled, and by how
/// much. `S` is the *storage* axis — how active bid/ask levels are found and
/// held. Matching depends only on [`OrderBookStore`], while allocation sees
/// each level only through
/// [`PriceLevelView::makers`](crate::types::PriceLevelView::makers) — a matcher sees a
/// slice of sizes and arrivals, never an `Order` and never this struct.
///
/// Note the derives add `Clone`/`Debug` bounds to both parameters. Free for the
/// built-in policies and stores, but real bounds for third-party types.
#[derive(Debug, Clone)]
pub struct OrderBook<M: MatchingAlgorithm, S: OrderBookStore = BTreeStore> {
    /// Active bid/ask levels. The layout is an independent static-dispatch
    /// axis from `matcher`.
    pub(crate) store: S,
    /// All active order nodes, shared by both sides and every price level.
    pub(crate) nodes: OrderArena,
    /// Buy stops keyed by trigger, FIFO within one trigger price. A buy stop
    /// fires when the market trades UP to its trigger, so the LOWEST key is
    /// nearest to firing.
    pub stop_bids: BTreeMap<Price, Vec<Order>>,
    /// Sell stops keyed by trigger. Fires when the market trades DOWN to the
    /// trigger, so the HIGHEST key is nearest to firing.
    pub stop_asks: BTreeMap<Price, Vec<Order>>,
    /// Price of the most recent trade — the signal stop triggers compare to.
    pub last_trade_price: Option<Price>,
    pub(crate) index: HashMap<ExchangeId, OrderAddress>,
    pub next_seq: u64,
    /// The stamp the next order to come to rest will carry — the exchange's
    /// answer to "who was here first", handed out by `add_order`.
    ///
    /// Kept separate from `next_seq` on purpose, even though both are monotone
    /// counters the book hands out. `next_seq` is an *id generator*: every
    /// value it emits becomes an `ExchangeId` somebody can address. Bumping it
    /// here would punch holes in the id space — one hole per rest, which is
    /// most orders — and make the ids stop being a sequence. They also count
    /// different things: an order can be submitted and never rest (market,
    /// IOC, a killed FOK), and an order can rest twice (a stop that parks,
    /// triggers, and comes back as a limit).
    ///
    /// `u32` to match [`Order::arrival`], which is `u32` for a measured layout
    /// reason documented there. Saturating at ~4.29 billion rests; `add_order`
    /// panics rather than wrap.
    pub next_arrival: u32,
    /// The instrument's tick and lot grid.
    ///
    /// This field replaces a comment. The book used to carry a note saying that
    /// quantities were stored "at the lowest fraction for precision" with
    /// normalization done "on the higher levels" — a description of an
    /// invariant that nothing enforced and, with `Price` aliased to `Decimal`,
    /// nothing could. Every price and quantity in this book is now a point on
    /// this spec's lattice, and the type system knows it.
    spec: InstrumentSpec,
    /// How this book apportions a taker across one price level.
    ///
    /// `pub(crate)` rather than private because `matching.rs` needs to borrow
    /// it *as a field*, alongside `&mut self.asks` and `&mut self.index` — the
    /// same disjoint-borrow trick `fill_against`'s signature is built around.
    /// Going through `matcher()` would borrow all of `self` and lose that.
    pub(crate) matcher: M,
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
    Storage(StoreError),
}

impl<M: MatchingAlgorithm> OrderBook<M, BTreeStore> {
    /// A book must be told what instrument it trades AND what policy it
    /// allocates by before it can hold a single order.
    ///
    /// There is no `Default` and no default type parameter, deliberately, and
    /// for one reason twice over. A *default* lattice would be the "someone
    /// upstream normalized this" assumption sneaking back in through a derive,
    /// which is the exact failure the spec exists to end; a *default* matcher
    /// would be the same move on the other axis — FIFO quietly standing in for
    /// a decision nobody made. Callers should be able to point at the line
    /// where they chose each. [`InstrumentSpec::cents`] is the named
    /// one-cent-tick grid; [`FifoMatcher`](crate::allocation::FifoMatcher) is
    /// the named price-time policy.
    pub fn new(spec: InstrumentSpec, matcher: M) -> Self {
        Self::try_new(spec, matcher).expect("BTreeStore construction is infallible")
    }
}

impl<M: MatchingAlgorithm, S: OrderBookStore> OrderBook<M, S> {
    /// Build a book with an explicitly selected storage backend.
    pub fn try_new(spec: InstrumentSpec, matcher: M) -> Result<Self, StoreError> {
        // A matcher that floors to a lot must floor to THIS instrument's lot.
        // Get this wrong and the damage is not a bad print: off-lot fills are
        // subtracted into the makers, which stay resting, off-grid, and
        // matchable — the lattice leaks one partial fill at a time. See the
        // worked counterexample on `ProRataMatcher`.
        assert!(
            matcher.lot_size().is_none_or(|lot| lot == spec.lot_size()),
            "matcher floors to lot {:?}, but this instrument's lot is {}",
            matcher.lot_size(),
            spec.lot_size()
        );

        let store = S::try_new(spec)?;
        Ok(OrderBook {
            store,
            nodes: OrderArena::with_key(),
            stop_bids: BTreeMap::new(),
            stop_asks: BTreeMap::new(),
            last_trade_price: None,
            index: HashMap::new(),
            next_seq: 1,
            next_arrival: 1,
            spec,
            matcher,
        })
    }

    pub fn spec(&self) -> InstrumentSpec {
        self.spec
    }

    /// The instrument's lot, without copying the whole spec.
    ///
    /// `spec()` returns `InstrumentSpec` by value — it is `Copy`, and ~72 bytes
    /// of it. The match path reads the lot on every `submit`, including ones
    /// that never touch a level, so going through `spec()` there put a
    /// nine-field struct copy on the hot path to fetch one `u64`.
    pub(crate) fn lot_size(&self) -> u64 {
        self.spec.lot_size()
    }

    pub fn matcher(&self) -> &M {
        &self.matcher
    }

    pub fn store(&self) -> &S {
        &self.store
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

    #[inline]
    pub fn add_order(&mut self, mut order: Order) -> Result<(), OrderBookError> {
        let OrderType::Limit { price, .. } = order.order_type else {
            return Err(OrderBookError::NotRestable);
        };
        self.admit(&order)?;
        let side = order.side;
        let exchange_id = order.exchange_id.clone();

        let index_entry = match self.index.entry(exchange_id) {
            Entry::Occupied(_) => return Err(OrderBookError::ExchangeIdDuplicated),
            Entry::Vacant(entry) => entry,
        };
        // Compute exhaustion before mutating either the level store or the arena.
        let next_arrival = self
            .next_arrival
            .checked_add(1)
            .expect("arrival counter exhausted: this book has rested u32::MAX orders");
        let level = self
            .store
            .ensure_level(side, price)
            .map_err(OrderBookError::Storage)?;
        order.arrival = self.next_arrival;
        let key = level.queue.push_back(&mut self.nodes, order);
        index_entry.insert(OrderAddress::Book { side, price, key });
        self.next_arrival = next_arrival;

        Ok(())
    }

    #[inline]
    pub fn cancel_order(&mut self, exchange_id: ExchangeId) -> Result<(), OrderBookError> {
        let location = self
            .index
            .remove(&exchange_id)
            .ok_or(OrderBookError::OrderNotFound)?;

        match location {
            OrderAddress::Book { side, price, key } => {
                let level = self
                    .store
                    .level_mut(side, price)
                    .expect("indexed level must exist");
                level
                    .queue
                    .remove_key(&mut self.nodes, key)
                    .expect("indexed node must be live");
                if level.is_empty() {
                    self.store.remove_level(side, price);
                }
            }
            OrderAddress::StopBook { side, trigger } => {
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

    #[inline]
    pub fn best_bid(&self) -> Option<Price> {
        self.store.best_price(Side::Bid)
    }

    #[inline]
    pub fn best_ask(&self) -> Option<Price> {
        self.store.best_price(Side::Ask)
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
        self.best_level(Side::Bid).map(|level| level.to_owned())
    }

    pub fn best_ask_level(&self) -> Option<PriceLevel> {
        self.best_level(Side::Ask).map(|level| level.to_owned())
    }

    /// Aggregated market depth: the top `levels` price levels of `side`,
    /// best price first, as `(price, total resting quantity)`.
    pub fn depth(&self, side: Side, levels: usize) -> Vec<(Price, Qty)> {
        self.levels(side)
            .take(levels)
            .map(|level| (level.price, level.total_quantity()))
            .collect()
    }

    /// Active levels on one side, best price first, independent of backend.
    #[inline]
    pub fn levels(&self, side: Side) -> impl Iterator<Item = PriceLevelView<'_>> {
        self.store
            .levels(side)
            .map(|level| PriceLevelView::new(level, &self.nodes))
    }

    /// Borrow one active level without cloning its orders.
    pub fn level(&self, side: Side, price: Price) -> Option<PriceLevelView<'_>> {
        self.store
            .level(side, price)
            .map(|level| PriceLevelView::new(level, &self.nodes))
    }

    /// Borrow the best active level on one side.
    pub fn best_level(&self, side: Side) -> Option<PriceLevelView<'_>> {
        self.store
            .best_level(side)
            .map(|level| PriceLevelView::new(level, &self.nodes))
    }

    /// Logical residency, without exposing an arena handle.
    pub fn order_location(&self, id: &ExchangeId) -> Option<OrderLocation> {
        self.index.get(id).map(|address| address.location())
    }

    /// Number of live orders, including pending stops.
    pub fn order_count(&self) -> usize {
        self.index.len()
    }

    /// Live exchange IDs, including pending stops, in unspecified order.
    pub fn order_ids(&self) -> impl ExactSizeIterator<Item = &ExchangeId> {
        self.index.keys()
    }

    /// Look up a live order by its exchange id — resting in the book or
    /// parked in the stop book. Active orders use the book index followed by
    /// a direct arena lookup; pending stops still scan their trigger queue.
    pub fn get_order(&self, exchange_id: &ExchangeId) -> Option<&Order> {
        match self.index.get(exchange_id)? {
            OrderAddress::Book { key, .. } => self.nodes.get(*key).map(|node| &node.order),
            OrderAddress::StopBook {
                side: Side::Bid,
                trigger,
            } => self
                .stop_bids
                .get(trigger)?
                .iter()
                .find(|order| &order.exchange_id == exchange_id),
            OrderAddress::StopBook {
                side: Side::Ask,
                trigger,
            } => self
                .stop_asks
                .get(trigger)?
                .iter()
                .find(|order| &order.exchange_id == exchange_id),
        }
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
    use crate::allocation::FifoMatcher;
    use crate::instrument::{RejectReason, SpecError};
    use crate::test_helpers::{book, order, px, qty};
    use rust_decimal::Decimal;

    /// A non-crossed book: bids 99/98/97, asks 100/101/102.
    /// best_bid = 99, best_ask = 100, spread = 1.
    fn book_with_depth() -> OrderBook<FifoMatcher> {
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
        assert_eq!(orderbook.levels(Side::Bid).count(), 0);
        assert_eq!(orderbook.levels(Side::Ask).count(), 0);
    }

    #[test]
    fn add_order_adds_order_to_correct_side_ask() {
        let mut orderbook = book();

        assert!(
            orderbook
                .add_order(order(Side::Ask, 100, 10, None, "ex_1"))
                .is_ok()
        );
        assert_eq!(orderbook.levels(Side::Ask).count(), 1);
        assert_eq!(orderbook.levels(Side::Bid).count(), 0);
    }

    #[test]
    fn add_order_adds_order_to_correct_side_bid() {
        let mut orderbook = book();

        assert!(
            orderbook
                .add_order(order(Side::Bid, 100, 10, None, "ex_1"))
                .is_ok()
        );
        assert_eq!(orderbook.levels(Side::Ask).count(), 0);
        assert_eq!(orderbook.levels(Side::Bid).count(), 1);
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

        assert_eq!(orderbook.levels(Side::Bid).count(), 0);

        let level = orderbook.level(Side::Ask, px(100)).unwrap();
        assert_eq!(level.total_quantity(), qty(15)); // 10 + 5
    }

    #[test]
    fn maintains_an_index_of_all_orders() {
        let mut orderbook = book();

        orderbook
            .add_order(order(Side::Ask, 100, 10, None, "ex_1"))
            .unwrap();

        let location = orderbook
            .order_location(&ExchangeId("ex_1".to_owned()))
            .unwrap();
        assert_eq!(
            location,
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
        assert_eq!(orderbook.levels(Side::Bid).count(), 0);
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
        let level = orderbook.level(Side::Ask, px(100)).unwrap();
        assert_eq!(level.order_count(), 1);
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

        assert_eq!(orderbook.levels(Side::Ask).count(), 0);
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
        let mut ob = OrderBook::new(bounded_spec(), FifoMatcher);
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
        let mut ob = OrderBook::new(bounded_spec(), FifoMatcher);

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
        let mut ob = OrderBook::new(bounded_spec(), FifoMatcher);
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
        let mut ob = OrderBook::new(bounded_spec(), FifoMatcher);
        let result = ob.add_order(order(Side::Bid, 5000, 4, None, "small"));

        assert!(matches!(
            result,
            Err(OrderBookError::Rejected(
                RejectReason::QuantityBelowMinimum { .. }
            ))
        ));
        assert_eq!(ob.levels(Side::Bid).count(), 0);
        assert!(ob.index.is_empty());
    }

    /// A stop's trigger is a price on the same grid as any other, so the band
    /// applies to it — `limit_price()` alone would have missed this one, since
    /// a StopMarket has no limit price at all.
    #[test]
    fn a_stop_trigger_is_checked_against_the_band() {
        let mut ob = OrderBook::new(bounded_spec(), FifoMatcher);
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
        let mut ob = OrderBook::new(min_notional_spec(), FifoMatcher);

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
        let mut ob = OrderBook::new(min_notional_spec(), FifoMatcher);
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

    // ------------------------------------------------------ THE ARRIVAL CLOCK

    /// `arrival` of the order resting at `price` on `side`, at queue position
    /// `pos`.
    fn arrival_at<M: MatchingAlgorithm, S: OrderBookStore>(
        ob: &OrderBook<M, S>,
        side: Side,
        price: i64,
        pos: usize,
    ) -> u32 {
        let level = ob.level(side, px(price)).expect("level should exist");
        level
            .orders()
            .nth(pos)
            .expect("position must exist")
            .arrival
    }

    #[test]
    fn resting_hands_out_increasing_arrivals() {
        let mut ob = book();
        ob.add_order(order(Side::Bid, 99, 10, None, "first"))
            .unwrap();
        ob.add_order(order(Side::Bid, 99, 10, None, "second"))
            .unwrap();
        // a different level shares the one counter — "who was here first" is a
        // fact about the book, not about a price
        ob.add_order(order(Side::Ask, 101, 10, None, "third"))
            .unwrap();

        assert_eq!(arrival_at(&ob, Side::Bid, 99, 0), 1);
        assert_eq!(arrival_at(&ob, Side::Bid, 99, 1), 2);
        assert_eq!(arrival_at(&ob, Side::Ask, 101, 0), 3);
    }

    /// The counter is not `next_seq` under another name. Two submits that never
    /// rest burn two ids and no arrivals.
    #[test]
    fn arrivals_count_rests_not_submissions() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 10, None, "maker"))
            .unwrap();

        let market = |id: &str| {
            Order::builder()
                .side(Side::Bid)
                .quantity(qty(2))
                .client_id(id)
                .exchange_id(id)
                .order_type(OrderType::Market)
                .build()
        };
        ob.submit(market("t1")).unwrap();
        ob.submit(market("t2")).unwrap();

        assert_eq!(ob.next_arrival, 2, "only the maker ever rested");
        assert_eq!(ob.next_seq, 3, "but both takers were assigned ids");
    }

    #[test]
    fn a_rejected_add_burns_no_arrival() {
        let mut ob = book();
        let before = ob.next_arrival;

        // not restable
        let market = Order::builder()
            .side(Side::Bid)
            .quantity(qty(1))
            .client_id("c")
            .exchange_id("m")
            .order_type(OrderType::Market)
            .build();
        assert_eq!(ob.add_order(market), Err(OrderBookError::NotRestable));

        // duplicate id
        ob.add_order(order(Side::Bid, 99, 10, None, "dup")).unwrap();
        assert_eq!(
            ob.add_order(order(Side::Bid, 99, 10, None, "dup")),
            Err(OrderBookError::ExchangeIdDuplicated)
        );

        assert_eq!(ob.next_arrival, before + 1, "exactly one order got in");
    }

    /// A taker that partially fills is aged from when its remainder came to
    /// rest, not from when it was sent — it queued behind everything already
    /// standing at its level, and the stamp has to say so.
    #[test]
    fn a_partially_filled_taker_is_stamped_at_rest_time() {
        let mut ob = book();
        ob.add_order(order(Side::Ask, 100, 5, None, "maker"))
            .unwrap(); // arrival 1
        ob.add_order(order(Side::Bid, 100, 10, None, "resident"))
            .unwrap(); // arrival 2

        // crosses the ask for 5, rests the other 5 at 100 behind `resident`
        let taker = Order::builder()
            .side(Side::Bid)
            .quantity(qty(10))
            .client_id("t")
            .exchange_id("t")
            .order_type(OrderType::limit_gtc(px(100)))
            .build();
        ob.submit(taker).unwrap();

        assert_eq!(arrival_at(&ob, Side::Bid, 100, 0), 2, "resident was first");
        assert_eq!(arrival_at(&ob, Side::Bid, 100, 1), 3, "remainder is newer");
    }

    /// The case `Order.timestamp` gets wrong, and the reason this field exists.
    /// A stop submitted before everything else parks — it is not in a queue —
    /// and when it finally triggers it joins the back of the line, behind
    /// orders that were sent long after it.
    #[test]
    fn a_stop_is_aged_from_when_it_rested_not_when_it_parked() {
        let mut ob = book();

        // parked first: sell stop at 99, waiting for the market to fall
        let stop = Order::builder()
            .side(Side::Ask)
            .quantity(qty(10))
            .client_id("stopper")
            .exchange_id("stop")
            .order_type(OrderType::stop_limit(px(99), px(105)))
            .build();
        ob.submit(stop).unwrap();
        assert_eq!(ob.next_arrival, 1, "parking is not resting");

        // submitted later, rests immediately
        ob.add_order(order(Side::Ask, 105, 10, None, "latecomer"))
            .unwrap(); // arrival 1

        // print a trade at 99 to fire the stop
        ob.add_order(order(Side::Bid, 99, 1, None, "bid_99"))
            .unwrap(); // arrival 2
        let taker = Order::builder()
            .side(Side::Ask)
            .quantity(qty(1))
            .client_id("t")
            .exchange_id("t")
            .order_type(OrderType::limit_gtc(px(99)))
            .build();
        ob.submit(taker).unwrap();

        assert_eq!(
            arrival_at(&ob, Side::Ask, 105, 0),
            1,
            "the latecomer holds the front of the queue"
        );
        assert_eq!(
            arrival_at(&ob, Side::Ask, 105, 1),
            3,
            "the stop queues where it landed, not where it was sent from"
        );
    }
}

#[cfg(test)]
mod arena_tests {
    use super::*;
    use crate::allocation::{FifoMatcher, ProRataMatcher, TimeProRataMatcher};
    use crate::storage::{HashMapStore, TickLadderStore};
    use crate::test_helpers::{order, px, qty};
    use crate::types::TimeInForce;
    use proptest::prelude::*;
    use std::collections::HashSet;

    /// Check both directions of the ownership relation, including arena orphans.
    fn assert_ownership<M: MatchingAlgorithm, S: OrderBookStore>(book: &OrderBook<M, S>) {
        let mut keys = HashSet::new();
        let mut ids = HashSet::new();
        for side in [Side::Bid, Side::Ask] {
            for level in book.store.levels(side) {
                assert_eq!(level.side, side);
                assert!(!level.is_empty());
                assert!(level.queue.links_are_consistent(&book.nodes));
                for (key, order) in level.queue.iter_with_keys(&book.nodes) {
                    assert!(keys.insert(key), "a node belongs to more than one level");
                    assert!(ids.insert(order.exchange_id.clone()), "duplicate live ID");
                    assert_eq!(order.side, side);
                    assert_eq!(order.order_type.limit_price(), Some(level.price));
                    assert!(!order.remaining_quantity.is_zero());
                    assert_eq!(
                        book.index.get(&order.exchange_id),
                        Some(&OrderAddress::Book {
                            side,
                            price: level.price,
                            key,
                        })
                    );
                }
            }
        }
        assert_eq!(keys.len(), book.nodes.len(), "orphaned arena nodes");
        for (key, _) in &book.nodes {
            assert!(keys.contains(&key));
        }
        for (side, stops) in [(Side::Bid, &book.stop_bids), (Side::Ask, &book.stop_asks)] {
            for (trigger, orders) in stops {
                assert!(!orders.is_empty());
                for order in orders {
                    assert!(ids.insert(order.exchange_id.clone()));
                    assert_eq!(order.side, side);
                    assert_eq!(
                        book.index.get(&order.exchange_id),
                        Some(&OrderAddress::StopBook {
                            side,
                            trigger: *trigger
                        })
                    );
                }
            }
        }
        assert_eq!(ids.len(), book.index.len(), "orphaned index entries");
    }

    fn bounded_spec() -> InstrumentSpec {
        InstrumentSpec::cents()
            .with_price_range(px(90), Some(px(110)))
            .unwrap()
    }

    type Operation = (u8, bool, i64, u64, u8);

    fn exercise<M: MatchingAlgorithm + Clone, S: OrderBookStore>(matcher: M, ops: &[Operation]) {
        let mut book = OrderBook::<M, S>::try_new(bounded_spec(), matcher).unwrap();
        let mut submitted: Vec<ExchangeId> = Vec::new();
        for (step, &(kind, bid, price, size, client)) in ops.iter().enumerate() {
            if kind == 0 && !submitted.is_empty() {
                // Includes already filled/cancelled IDs and newly reused slots.
                let id = submitted[step % submitted.len()].clone();
                let expected = book.get_order(&id).is_some();
                assert_eq!(book.cancel_order(id).is_ok(), expected);
            } else {
                let order_type = match kind {
                    1 => OrderType::Market,
                    2 => OrderType::limit_ioc(px(price)),
                    3 => OrderType::limit_fok(px(price)),
                    4 => OrderType::stop_market(px(price)),
                    5 => OrderType::StopLimit {
                        trigger: px(price),
                        price: px(price),
                        tif: TimeInForce::GTC,
                    },
                    _ => OrderType::limit_gtc(px(price)),
                };
                let incoming = Order::builder()
                    .side(if bid { Side::Bid } else { Side::Ask })
                    .order_type(order_type)
                    .quantity(qty(size))
                    .client_id(client.to_string())
                    .timestamp(step as u128)
                    .exchange_id("assigned-by-submit")
                    .build();
                submitted.push(book.submit(incoming).unwrap().order_id);
            }
            assert_ownership(&book);
            // Cloning must preserve every handle-to-node relation.
            if step % 13 == 0 {
                assert_ownership(&book.clone());
            }
        }
        let mut clone = book.clone();
        let before: Vec<_> = book
            .levels(Side::Bid)
            .chain(book.levels(Side::Ask))
            .map(|level| level.to_owned())
            .collect();
        let stops_before = (book.stop_bids.clone(), book.stop_asks.clone());
        let ids: Vec<_> = clone.order_ids().cloned().collect();
        for id in ids {
            clone.cancel_order(id).unwrap();
            assert_ownership(&clone);
        }
        assert!(clone.nodes.is_empty());
        assert_eq!(clone.order_count(), 0);
        let after: Vec<_> = book
            .levels(Side::Bid)
            .chain(book.levels(Side::Ask))
            .map(|level| level.to_owned())
            .collect();
        assert_eq!(before, after);
        assert_eq!(
            stops_before,
            (book.stop_bids.clone(), book.stop_asks.clone())
        );
        assert_ownership(&book);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn mixed_operations_preserve_shared_arena_ownership(
            ops in prop::collection::vec((0u8..9, any::<bool>(), 95i64..106, 1u64..21, 0u8..4), 1..80)
        ) {
            macro_rules! stores {
                ($matcher:expr) => {{
                    exercise::<_, BTreeStore>($matcher, &ops);
                    exercise::<_, TickLadderStore>($matcher, &ops);
                    exercise::<_, HashMapStore>($matcher, &ops);
                }};
            }
            stores!(FifoMatcher);
            stores!(ProRataMatcher::new(1));
            stores!(TimeProRataMatcher::new(1));
        }
    }

    #[test]
    fn snapshots_and_clones_survive_partial_fills_and_slot_reuse() {
        let mut book = OrderBook::new(bounded_spec(), FifoMatcher);
        book.add_order(order(Side::Ask, 100, 10, None, "a"))
            .unwrap();
        book.add_order(order(Side::Ask, 100, 20, None, "b"))
            .unwrap();
        let snapshot = book.best_ask_level().unwrap();
        let view = book.best_level(Side::Ask).unwrap();
        assert_eq!(view.order_count(), view.orders().len());
        assert_eq!(
            view.makers().collect::<Vec<_>>(),
            snapshot.makers().collect::<Vec<_>>()
        );
        let mut clone = book.clone();
        clone
            .submit(order(Side::Bid, 100, 15, None, "taker"))
            .unwrap();
        assert_eq!(
            clone
                .get_order(&ExchangeId("b".into()))
                .unwrap()
                .remaining_quantity,
            qty(15)
        );
        clone
            .add_order(order(Side::Ask, 101, 3, None, "c"))
            .unwrap();
        assert!(book.get_order(&ExchangeId("c".into())).is_none());
        assert_eq!(book.best_ask_level(), Some(snapshot.clone()));
        book.cancel_order(ExchangeId("a".into())).unwrap();
        assert_eq!(snapshot.order_count(), 2);
        assert_eq!(snapshot.total_quantity(), qty(30));
        assert_ownership(&clone);
        assert_ownership(&book);
        drop(book);
        assert_eq!(
            snapshot
                .orders()
                .map(|order| order.exchange_id.0.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[test]
    fn duplicate_and_storage_rejections_leave_no_arena_or_index_entry() {
        let mut book =
            OrderBook::<_, TickLadderStore>::try_new(bounded_spec(), FifoMatcher).unwrap();
        book.add_order(order(Side::Ask, 100, 1, None, "a")).unwrap();
        let before = format!("{book:?}");
        assert_eq!(
            book.add_order(order(Side::Ask, 101, 2, None, "a")),
            Err(OrderBookError::ExchangeIdDuplicated)
        );
        assert_eq!(format!("{book:?}"), before);
        // A deliberately narrower backend rejects after book-level admission.
        // Start with an empty book so replacing its backend cannot orphan nodes.
        let mut book =
            OrderBook::<_, TickLadderStore>::try_new(bounded_spec(), FifoMatcher).unwrap();
        book.store = TickLadderStore::try_new(
            InstrumentSpec::cents()
                .with_price_range(px(100), Some(px(101)))
                .unwrap(),
        )
        .unwrap();
        let before = format!("{book:?}");
        assert_eq!(
            book.add_order(order(Side::Ask, 105, 1, None, "outside")),
            Err(OrderBookError::Storage(StoreError::PriceOutsideRange))
        );
        assert_eq!(format!("{book:?}"), before);
        assert_ownership(&book);
    }
}
