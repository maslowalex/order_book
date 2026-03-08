use rust_decimal::Decimal;

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

#[derive(Debug, Clone)]
pub struct Order {
    pub exchange_id: String,
    pub client_id: String,
    pub order_type: OrderType,
    pub price: Price,
    pub side: Side,
    pub quantity: u64,
    pub timestamp: u64, // matters, because of the FIFO processing we need to know the time of the order
    pub lifecycle: OrderLifecycle,
}

#[derive(Debug, Clone)]
pub struct PriceLevel {
    pub price: Price,
    pub side: Side,
    pub orders: Vec<Order>,
}
