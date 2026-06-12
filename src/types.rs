use rust_decimal::Decimal;
use std::time::{SystemTime, UNIX_EPOCH};

pub type Price = Decimal;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExchangeId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Ask,
    Bid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType {
    Limit,
    Market,
    StopMarket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderLifecycle {
    New,
    PartiallyFilled(u64), // represents how much is filled
    Filled,
}

impl Default for OrderLifecycle {
    fn default() -> OrderLifecycle {
        OrderLifecycle::New
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Order {
    pub exchange_id: ExchangeId,
    pub client_id: ClientId,
    pub order_type: OrderType,
    pub price: Price,
    pub side: Side,
    pub quantity: u64,
    pub timestamp: u128, // matters, because of the FIFO processing we need to know the time of the order
    pub lifecycle: OrderLifecycle,
}

#[derive(Debug, Clone)]
pub struct OrderBuilder {
    exchange_id: Option<ExchangeId>,
    client_id: Option<ClientId>,
    order_type: Option<OrderType>,
    price: Option<Price>,
    side: Option<Side>,
    quantity: Option<u64>,
    timestamp: Option<u128>,
    lifecycle: Option<OrderLifecycle>,
}

impl Default for OrderBuilder {
    fn default() -> Self {
        let timestamp_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(); // u128

        OrderBuilder {
            lifecycle: Some(OrderLifecycle::New),
            timestamp: Some(timestamp_ns),
            order_type: Some(OrderType::Market),
            exchange_id: None,
            client_id: None,
            price: None,
            quantity: None,
            side: None,
        }
    }
}

impl OrderBuilder {
    pub fn build(self) -> Order {
        Order {
            exchange_id: self.exchange_id.expect("exchange_id should be present"),
            client_id: self.client_id.expect("client_id should be present"),
            lifecycle: self
                .lifecycle
                .expect("somehow lifecycle is not provided and default doesn't applied"),
            timestamp: self
                .timestamp
                .expect("somehow timetamp is not provided and default doesn't applied"),
            side: self.side.expect("side doesn't provided"),
            quantity: self.quantity.expect("quantity doesn't provided"),
            order_type: self.order_type.expect("order_type doesn't provided"),
            price: self.price.expect("price doesn't provided"),
        }
    }

    pub fn timestamp(mut self, timestamp: u128) -> Self {
        self.timestamp = Some(timestamp);
        self
    }

    pub fn exchange_id(mut self, exchange_id: impl Into<String>) -> Self {
        self.exchange_id = Some(ExchangeId(exchange_id.into()));
        self
    }

    pub fn client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = Some(ClientId(client_id.into()));
        self
    }

    pub fn order_type(mut self, order_type: OrderType) -> Self {
        self.order_type = Some(order_type);
        self
    }

    pub fn price(mut self, price: Price) -> Self {
        self.price = Some(price);
        self
    }

    pub fn side(mut self, side: Side) -> Self {
        self.side = Some(side);
        self
    }

    pub fn quantity(mut self, quantity: u64) -> Self {
        self.quantity = Some(quantity);
        self
    }

    pub fn lifecycle(mut self, lifecycle: OrderLifecycle) -> Self {
        self.lifecycle = Some(lifecycle);
        self
    }
}

impl Order {
    pub fn builder() -> OrderBuilder {
        OrderBuilder::default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PriceLevel {
    pub price: Price,
    pub side: Side,
    pub orders: Vec<Order>,
}

#[derive(Debug, PartialEq)]
pub enum OrderError {
    InvalidSide,
}

impl PriceLevel {
    pub fn new(price: Price, side: Side) -> Self {
        Self {
            price: price,
            side: side,
            orders: vec![],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }

    pub fn add_order(&mut self, order: Order) -> Result<&mut Self, OrderError> {
        if order.side != self.side {
            return Err(OrderError::InvalidSide);
        }

        self.orders.push(order);

        Ok(self)
    }

    pub fn remove_order(&mut self, order_exchange_id: &ExchangeId) -> Option<Order> {
        let idx = self
            .orders
            .iter()
            .position(|order| &order.exchange_id == order_exchange_id)?;

        Some(self.orders.remove(idx))
    }

    pub fn total_quantity(&self) -> u64 {
        self.orders
            .iter()
            .fold(0, |acc, order| acc + order.quantity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{order, price_level, px};

    #[test]
    fn price_level_constructor_works() {
        let level = price_level(Side::Ask, 500);

        assert_eq!(level.price, px(500));
        assert_eq!(level.side, Side::Ask);
        assert_eq!(level.orders, vec![]);
    }

    #[test]
    fn price_level_add_order_works() {
        let mut level = price_level(Side::Ask, 100);

        level
            .add_order(order(Side::Ask, 100, 10, "ex_1"))
            .expect("order should be added");

        assert!(!level.is_empty());
    }

    #[test]
    fn price_level_add_order_invalid_level_doesnt_change_orders_of_level() {
        let mut level = price_level(Side::Bid, 100);

        let result = level.add_order(order(Side::Ask, 100, 10, "ex_1"));

        assert!(result.is_err_and(|x| x == OrderError::InvalidSide));
        assert!(level.is_empty());
    }

    #[test]
    fn price_level_remove_order_works() {
        let mut level = price_level(Side::Ask, 100);
        let to_remove = order(Side::Ask, 100, 10, "ex_1");
        let exchange_id = to_remove.exchange_id.clone();

        level.add_order(to_remove).expect("order should be added");

        assert!(level.remove_order(&exchange_id).is_some());
        assert!(level.is_empty());
    }

    #[test]
    fn price_level_remote_nonexistent_doesnt_crash() {
        let mut level = price_level(Side::Ask, 100);

        level.remove_order(&ExchangeId("ex_1".to_owned()));

        assert!(level.is_empty());
    }

    #[test]
    fn price_level_is_empty_works() {
        let mut level = price_level(Side::Ask, 100);

        assert!(level.is_empty());

        level
            .add_order(order(Side::Ask, 100, 10, "ex_1"))
            .expect("order should be added");

        assert!(!level.is_empty());
    }

    #[test]
    fn price_level_total_quantity_works() {
        let mut level = price_level(Side::Bid, 100);

        level
            .add_order(order(Side::Bid, 100, 10, "order_1"))
            .expect("order 1 should be added");
        level
            .add_order(order(Side::Bid, 100, 35, "order_2"))
            .expect("order 2 should be added");

        assert_eq!(level.total_quantity(), 45);
    }
}
