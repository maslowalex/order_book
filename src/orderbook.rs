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
    use rust_decimal::Decimal;

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
        let order = Order::builder()
            .side(Side::Ask)
            .price(Decimal::new(100, 2))
            .client_id("client_id")
            .exchange_id("exchange_id")
            .quantity(10)
            .build();

        let result = orderbook.add_order(order);
        assert!(result.is_ok());
        assert_eq!(orderbook.asks.len(), 1);
        assert_eq!(orderbook.bids.len(), 0);
    }

    #[test]
    fn add_order_adds_order_to_correct_side_bid() {
        let mut orderbook = OrderBook::new();
        let order = Order::builder()
            .side(Side::Bid)
            .price(Decimal::new(100, 2))
            .client_id("client_id")
            .exchange_id("exchange_id")
            .quantity(10)
            .build();

        let result = orderbook.add_order(order);
        assert!(result.is_ok());
        assert_eq!(orderbook.asks.len(), 0);
        assert_eq!(orderbook.bids.len(), 1);
    }

    #[test]
    fn add_order_adds_multiple_orders() {
        let mut orderbook = OrderBook::new();
        let order1 = Order::builder()
            .side(Side::Ask)
            .price(Decimal::new(100, 2))
            .client_id("client_id")
            .exchange_id("exchange_id")
            .quantity(10)
            .build();
        let order2 = Order::builder()
            .side(Side::Ask)
            .price(Decimal::new(100, 2))
            .client_id("client_id_1")
            .exchange_id("exchange_id_1")
            .quantity(5)
            .build();

        assert!(orderbook.add_order(order1).is_ok());
        assert!(orderbook.add_order(order2).is_ok());

        assert_eq!(orderbook.bids.len(), 0);

        let level = orderbook.asks.get(&Decimal::new(100, 2)).unwrap();
        assert_eq!(level.total_quantity(), 15); // 10 + 5
    }

    #[test]
    fn maintains_an_index_of_all_orders() {
        let mut orderbook = OrderBook::new();
        let order = Order::builder()
            .side(Side::Ask)
            .price(Decimal::new(100, 2))
            .client_id("client_id")
            .exchange_id("ex_1")
            .quantity(10)
            .build();

        orderbook.add_order(order).unwrap();

        let (side, price) = orderbook.index.get(&ExchangeId("ex_1".to_owned())).unwrap();
        assert_eq!(*side, Side::Ask);
        assert_eq!(*price, Decimal::new(100, 2));
    }

    #[test]
    fn add_order_rejects_duplicate_exchange_id() {
        let mut orderbook = OrderBook::new();
        let order1 = Order::builder()
            .side(Side::Ask)
            .price(Decimal::new(100, 2))
            .client_id("client_1")
            .exchange_id("same_id")
            .quantity(10)
            .build();
        let order2 = Order::builder()
            .side(Side::Bid)
            .price(Decimal::new(99, 2))
            .client_id("client_2")
            .exchange_id("same_id")
            .quantity(5)
            .build();

        assert!(orderbook.add_order(order1).is_ok());
        assert!(
            orderbook
                .add_order(order2)
                .is_err_and(|e| e == OrderBookError::ExchangeIdDuplicated)
        );
    }

    #[test]
    fn cancel_order_cancels_existing_order_by_id() {
        let mut orderbook = OrderBook::new();
        let order = Order::builder()
            .side(Side::Ask)
            .price(Decimal::new(100, 2))
            .client_id("order_to_cancel_id")
            .exchange_id("exchange_id")
            .quantity(10)
            .build();

        assert!(orderbook.add_order(order).is_ok());
        assert!(
            orderbook
                .cancel_order(ExchangeId("exchange_id".to_owned()))
                .is_ok()
        );
    }

    #[test]
    fn cancel_order_on_bid_side() {
        let mut orderbook = OrderBook::new();
        let order = Order::builder()
            .side(Side::Bid)
            .price(Decimal::new(99, 2))
            .client_id("client_id")
            .exchange_id("bid_order")
            .quantity(10)
            .build();

        orderbook.add_order(order).unwrap();
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
        let order1 = Order::builder()
            .side(Side::Ask)
            .price(Decimal::new(100, 2))
            .client_id("client_1")
            .exchange_id("ex_1")
            .quantity(10)
            .build();
        let order2 = Order::builder()
            .side(Side::Ask)
            .price(Decimal::new(100, 2))
            .client_id("client_2")
            .exchange_id("ex_2")
            .quantity(5)
            .build();

        orderbook.add_order(order1).unwrap();
        orderbook.add_order(order2).unwrap();

        orderbook
            .cancel_order(ExchangeId("ex_1".to_owned()))
            .unwrap();

        // price level still exists with remaining order
        let level = orderbook.asks.get(&Decimal::new(100, 2)).unwrap();
        assert_eq!(level.orders.len(), 1);
        assert_eq!(level.total_quantity(), 5);

        // index only has the remaining order
        assert_eq!(orderbook.index.len(), 1);
        assert!(orderbook.index.contains_key(&ExchangeId("ex_2".to_owned())));
    }

    #[test]
    fn cancel_order_cleans_up_empty_price_level() {
        let mut orderbook = OrderBook::new();
        let order = Order::builder()
            .side(Side::Ask)
            .price(Decimal::new(100, 2))
            .client_id("client_id")
            .exchange_id("ex_1")
            .quantity(10)
            .build();

        orderbook.add_order(order).unwrap();
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
}
