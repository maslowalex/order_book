use crate::order::ExchangeId;
use crate::trade::Trade;

/// What ultimately happened to the incoming (taker) order after `submit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Limit order didn't cross; the whole order now rests in the book.
    Rested,
    /// Limit order partially filled; the remainder rests in the book.
    PartiallyFilledAndRested,
    /// Order fully filled — nothing left to rest.
    Filled,
    /// The unfilled remainder was discarded (market/IOC), or a FOK couldn't
    /// fill completely and executed nothing.
    Killed,
    /// A stop order parked in the stop book, waiting for its trigger.
    StopPending,
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
    /// Stop orders this submit's trades set off, one report per activated
    /// stop, in trigger order. FLAT: a triggered stop's own trades may
    /// trigger further stops, but those land here too (their `triggered` is
    /// always empty), so consumers never recurse.
    pub triggered: Vec<ExecutionReport>,
}
