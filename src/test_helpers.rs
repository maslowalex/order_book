use crate::types::{Order, Price, PriceLevel, Side};
use rust_decimal::Decimal;

pub fn px(cents: i64) -> Price {
    Decimal::new(cents, 2)
}

pub fn order(side: Side, price: i64, qty: u64, id: &str) -> Order {
    Order::builder()
        .side(side)
        .price(px(price))
        .client_id(id)
        .exchange_id(id)
        .quantity(qty)
        .build()
}

pub fn price_level(side: Side, price: i64) -> PriceLevel {
    PriceLevel::new(px(price), side)
}
