use crate::instrument::{InstrumentSpec, Qty};
use crate::orderbook::OrderBook;
use crate::types::{Order, OrderType, Price, PriceLevel, Side};

/// The grid the suite has always used implicitly: prices in cents on a
/// one-cent tick, quantities in whole units on a one-unit lot.
pub fn spec() -> InstrumentSpec {
    InstrumentSpec::cents()
}

pub fn book() -> OrderBook {
    OrderBook::new(spec())
}

/// A price from a whole number of cents. The signature is unchanged from when
/// this returned a `Decimal`, which is why the ~60 `px(..)` assertions across
/// the suite needed no edit: on a one-cent tick, "the 10025th cent" and
/// "100.25" are the same point, and the assertions only ever compared points.
pub fn px(cents: i64) -> Price {
    Price::from_minor_unchecked(u64::try_from(cents).expect("test prices are non-negative"))
}

/// Base units as a `Qty`. The suite runs on a unit lot, so base units and
/// lots coincide and every quantity literal in the tests reads unchanged.
pub fn qty(n: u64) -> Qty {
    Qty::from_base_unchecked(n)
}

/// A resting GTC limit maker (the only kind of order that can rest).
pub fn order(side: Side, price: i64, quantity: u64, remaining: Option<u64>, id: &str) -> Order {
    let mut builder = Order::builder()
        .side(side)
        .order_type(OrderType::limit_gtc(px(price)))
        .client_id(id)
        .exchange_id(id)
        .quantity(qty(quantity));

    if let Some(remaining) = remaining {
        builder = builder.remaining_quantity(qty(remaining));
    }

    builder.build()
}

pub fn price_level(side: Side, price: i64) -> PriceLevel {
    PriceLevel::new(px(price), side)
}
