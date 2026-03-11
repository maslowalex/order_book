use rust_decimal::Decimal;
use std::time::{SystemTime, UNIX_EPOCH};

pub type Price = Decimal;

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
    pub exchange_id: String,
    pub client_id: String,
    pub order_type: OrderType,
    pub price: Price,
    pub side: Side,
    pub quantity: u64,
    pub timestamp: u128, // matters, because of the FIFO processing we need to know the time of the order
    pub lifecycle: OrderLifecycle,
}

#[derive(Debug, Clone)]
pub struct OrderBuilder {
    exchange_id: Option<String>,
    client_id: Option<String>,
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
        self.exchange_id = Some(exchange_id.into());
        self
    }

    pub fn client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = Some(client_id.into());
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

#[derive(Debug, Clone)]
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

    pub fn remove_order(&mut self, order_exchange_id: &str) -> Option<Order> {
        let idx = self
            .orders
            .iter()
            .position(|order| order.exchange_id == order_exchange_id)?;

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

    #[test]
    fn price_level_constructor_works() {
        let price = Decimal::new(500, 2);
        let side = Side::Ask;
        let price_level = PriceLevel::new(price, side);

        assert_eq!(price_level.price, price);
        assert_eq!(price_level.side, side);
        assert_eq!(price_level.orders, vec![]);
    }

    #[test]
    fn price_level_add_order_works() {
        let mut price_level = price_level_ask(None);
        let order = Order::builder()
            .side(Side::Ask)
            .price(Decimal::new(100, 2))
            .client_id("client_id")
            .exchange_id("exchange_id")
            .quantity(10)
            .build();

        _ = price_level.add_order(order);

        assert!(!price_level.is_empty());
    }

    #[test]
    fn price_level_add_order_invalid_level_doesnt_change_orders_of_level() {
        let mut price_level = price_level_bid(None);

        let order = Order::builder()
            .side(Side::Ask)
            .price(Decimal::new(100, 2))
            .client_id("client_id")
            .exchange_id("exchange_id")
            .quantity(10)
            .build();

        let result = price_level.add_order(order);

        assert!(result.is_err_and(|x| x == OrderError::InvalidSide));

        assert!(price_level.is_empty());
    }

    #[test]
    fn price_level_remove_order_works() {
        let mut price_level = price_level_ask(None);
        let order = Order::builder()
            .side(Side::Ask)
            .price(Decimal::new(100, 2))
            .client_id("client_id")
            .exchange_id("exchange_id")
            .quantity(10)
            .build();
        let order_id = order.exchange_id.clone();

        let result = price_level.add_order(order);

        assert!(result.is_ok());

        let result = price_level.remove_order(&order_id);

        assert!(result.is_some());
        assert!(price_level.is_empty());
    }

    #[test]
    fn price_level_remote_nonexistent_doesnt_crash() {
        let mut price_level = price_level_ask(None);

        price_level.remove_order("exchange_id");

        assert!(price_level.is_empty());
    }

    #[test]
    fn price_level_is_empty_works() {
        let mut price_level = price_level_ask(None);

        assert!(price_level.is_empty());

        let order = Order::builder()
            .side(Side::Ask)
            .price(Decimal::new(100, 2))
            .client_id("client_id")
            .exchange_id("exchange_id")
            .quantity(10)
            .build();

        _ = price_level.add_order(order);

        assert!(!price_level.is_empty());
    }

    #[test]
    fn price_level_total_quantity_works() {
        let mut price_level = price_level_bid(None);
        let order1 = Order::builder()
            .side(Side::Bid)
            .price(Decimal::new(100, 2))
            .client_id("client_id_1")
            .exchange_id("order_1")
            .quantity(10)
            .build();
        let order2 = Order::builder()
            .side(Side::Bid)
            .price(Decimal::new(100, 2))
            .client_id("client_id_2")
            .exchange_id("order_2")
            .quantity(35)
            .build();

        price_level
            .add_order(order1)
            .expect("Order 1 doesn't added");
        price_level
            .add_order(order2)
            .expect("Order 2 doesn't added");

        assert_eq!(price_level.total_quantity(), 45);
    }

    fn price_level_ask(price: Option<i64>) -> PriceLevel {
        let price = match price {
            Some(price) => Decimal::new(price, 2),

            _ => Decimal::new(100, 2),
        };
        let side = Side::Ask;

        PriceLevel::new(price, side)
    }

    fn price_level_bid(price: Option<i64>) -> PriceLevel {
        let price = match price {
            Some(price) => Decimal::new(price, 2),

            _ => Decimal::new(100, 2),
        };

        let side = Side::Bid;

        PriceLevel::new(price, side)
    }
}
