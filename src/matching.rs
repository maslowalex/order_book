use crate::orderbook::{OrderBook, OrderBookError};
use crate::types::{ClientId, ExchangeId, Order, Price, Side};

/// A single executed fill between a resting maker and an incoming taker.
///
/// The trade always prints at the **maker's** price (price-time priority: the
/// resting order set the price, the taker accepted it). `quantity` is the filled
/// amount for this fill, not the size of either order.
#[derive(Debug, Clone, PartialEq)]
pub struct Trade {
    pub price: Price,
    pub quantity: u64,
    pub maker_order_id: ExchangeId,
    pub taker_order_id: ExchangeId,
    pub maker_client: ClientId,
    pub taker_client: ClientId,
    pub taker_side: Side,
    pub timestamp: u128,
}

/// What ultimately happened to the incoming (taker) order after `submit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Limit order didn't cross; the whole order now rests in the book.
    Rested,
    /// Limit order partially filled; the remainder rests in the book.
    PartiallyFilledAndRested,
    /// Order fully filled — nothing left to rest.
    Filled,
    /// Market order's unfilled remainder was discarded (IOC semantics).
    Killed,
}

/// The result of submitting one order: every fill it caused, what became of it,
/// and any resting orders we cancelled to prevent self-trading.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionReport {
    pub order_id: ExchangeId,
    pub trades: Vec<Trade>,
    pub outcome: SubmitOutcome,
    /// Resting orders cancelled because they belonged to the taker's own client
    /// (self-trade prevention).
    pub cancelled: Vec<ExchangeId>,
}

impl OrderBook {
    /// Submit an order to be matched against the book, resting any remainder
    /// (limit) or discarding it (market). Contrast with `add_order`, which
    /// always rests without matching (used for seeding the book).
    pub fn submit(&mut self, order: Order) -> Result<ExecutionReport, OrderBookError> {
        let _ = order;
        todo!("1.1 step B: matching loop")
    }
}
