use std::cmp::Reverse;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};

use crate::types::{ExchangeId, Order, Price, PriceLevel, Side};

/*
If we were to store the quantities in plain u64 we might need to normalize/denormalize the quantity.
For example, 1 USD is 100 cents, and 1 BTC is 100_000_000 satoshis.
For OrderBook operations we store at the lowest fraction for precision,
and do the normalization on the higher levels.
*/
#[derive(Debug)]
pub struct OrderBook {
    pub bids: BTreeMap<Reverse<Price>, PriceLevel>, // descending: best (highest) bid first
    pub asks: BTreeMap<Price, PriceLevel>,          // ascending: best (lowest) ask first
    pub index: HashMap<ExchangeId, (Side, Price)>,
}

#[derive(Debug, PartialEq)]
pub enum OrderBookError {
    Generic,
    ExchangeIdDuplicated,
    OrderNotFound,
}

impl OrderBook {
    pub fn new() -> Self {
        OrderBook {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            index: HashMap::new(),
        }
    }

    pub fn add_order(&mut self, order: Order) -> Result<(), OrderBookError> {
        let price = order.price;
        let side = order.side;
        let exchange_id = order.exchange_id.clone();

        match self.index.entry(exchange_id) {
            Entry::Occupied(_) => return Err(OrderBookError::ExchangeIdDuplicated),
            Entry::Vacant(e) => e.insert((side, price)),
        };

        let price_level = match side {
            Side::Ask => self
                .asks
                .entry(order.price)
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
        let (side, price) = self
            .index
            .remove(&exchange_id)
            .ok_or(OrderBookError::OrderNotFound)?;

        match side {
            Side::Ask => {
                if let Some(level) = self.asks.get_mut(&price) {
                    level.remove_order(&exchange_id);
                    if level.is_empty() {
                        self.asks.remove(&price);
                    }
                }
            }
            Side::Bid => {
                if let Some(level) = self.bids.get_mut(&Reverse(price)) {
                    level.remove_order(&exchange_id);
                    if level.is_empty() {
                        self.bids.remove(&Reverse(price));
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

    pub fn spread(&self) -> Option<Price> {
        match (self.best_ask(), self.best_bid()) {
            (Some(ask), Some(bid)) => Some(ask - bid),
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
    use crate::test_helpers::{order, px};

    /// A non-crossed book: bids 99/98/97, asks 100/101/102.
    /// best_bid = 99, best_ask = 100, spread = 1.
    fn book_with_depth() -> OrderBook {
        let mut ob = OrderBook::new();
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
        let orderbook = OrderBook::new();
        let empty_bids: BTreeMap<Reverse<Price>, PriceLevel> = BTreeMap::new();
        let empty_asks: BTreeMap<Price, PriceLevel> = BTreeMap::new();
        assert_eq!(orderbook.bids, empty_bids);
        assert_eq!(orderbook.asks, empty_asks);
    }

    #[test]
    fn add_order_adds_order_to_correct_side_ask() {
        let mut orderbook = OrderBook::new();

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
        let mut orderbook = OrderBook::new();

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
        let mut orderbook = OrderBook::new();

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
        assert_eq!(level.total_quantity(), 15); // 10 + 5
    }

    #[test]
    fn maintains_an_index_of_all_orders() {
        let mut orderbook = OrderBook::new();

        orderbook
            .add_order(order(Side::Ask, 100, 10, None, "ex_1"))
            .unwrap();

        let (side, price) = orderbook.index.get(&ExchangeId("ex_1".to_owned())).unwrap();
        assert_eq!(*side, Side::Ask);
        assert_eq!(*price, px(100));
    }

    #[test]
    fn add_order_rejects_duplicate_exchange_id() {
        let mut orderbook = OrderBook::new();

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
        let mut orderbook = OrderBook::new();

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
        let mut orderbook = OrderBook::new();

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
        let mut orderbook = OrderBook::new();

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
        assert_eq!(level.total_quantity(), 5);

        // index only has the remaining order
        assert_eq!(orderbook.index.len(), 1);
        assert!(orderbook.index.contains_key(&ExchangeId("ex_2".to_owned())));
    }

    #[test]
    fn cancel_order_cleans_up_empty_price_level() {
        let mut orderbook = OrderBook::new();

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
        let mut orderbook = OrderBook::new();
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
        let orderbook = OrderBook::new();

        assert_eq!(orderbook.best_bid(), None);
        assert_eq!(orderbook.best_ask(), None);
    }

    #[test]
    fn spread_is_difference_between_best_ask_and_best_bid() {
        let orderbook = book_with_depth();

        assert_eq!(orderbook.spread(), Some(px(1))); // 100 - 99
    }

    #[test]
    fn spread_is_none_when_either_side_empty() {
        let mut orderbook = OrderBook::new();
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
        assert_eq!(bid_level.total_quantity(), 110);

        let ask_level = orderbook.best_ask_level().unwrap();
        assert_eq!(ask_level.price, px(100));
        assert_eq!(ask_level.total_quantity(), 100);
    }

    #[test]
    fn best_level_is_none_on_empty_book() {
        let orderbook = OrderBook::new();

        assert!(orderbook.best_bid_level().is_none());
        assert!(orderbook.best_ask_level().is_none());
    }

    #[test]
    fn crosses_returns_false_on_empty_book() {
        let orderbook = OrderBook::new();

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
}
