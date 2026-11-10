use crate::instrument::{price::Price, qty::Qty};
use crate::order::{ClientId, ExchangeId, Side};

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
