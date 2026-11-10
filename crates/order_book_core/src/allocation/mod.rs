/*
* Provides a pluggable implementations for actual matching logic, e.g.
* Deciding who at the price levels gets filled
*
* Contract (documented on the trait, enforced by debug_assert_fills,
* with total = Σ remaining_quantity):
* 1. order_index strictly increasing, each < makers.len()
* 2. quantity > 0 — no zero fills ("allocated" means "touched", and STP keys off exactly that)
* 3. quantity <= makers[order_index].remaining_quantity
* 4. Σ quantity == min(available, total)
* 5. every quantity is a whole number of lots
*
* (5) needs no runtime check and gets none. Every input is a lot multiple
* because `Qty` cannot hold anything else, and the allocators preserve that:
* FIFO because the minimum of two multiples is a multiple, the weighted ones
* because they are told the lot and floor to it. The clause is written down
* anyway, because the algorithms that could break it look correct without it —
* see the counterexample on `ProRataMatcher`.
*/

use crate::instrument::qty::Qty;

/// One resting order's share of an incoming taker, at a single price level.
/// Index-based, not id-based: `ExchangeId` is a `String`, so an id-carrying
/// `Fill` would heap-allocate per fill on the hot path AND force a second
/// linear scan to find the maker again. An index is `Copy`, free, and — the
/// 6.2a point — carries zero information about how orders are stored
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    pub order_index: usize,
    pub quantity: Qty,
}

/// One resting order as an allocator sees it: how much is left to fill, and
/// when it joined the queue.
///
/// Deliberately NOT an `Order`. An allocation policy has no business reading
/// prices, ids, or order types, and — the 6.2a point — a storage layout that
/// holds nothing resembling an `Order` can still hand out this view. What the
/// engine puts in `arrival` is its own decision, which is the second reason
/// this type exists: `Order.timestamp` is supplied by the client and never
/// re-stamped on receipt, so weighting by it would let a sender backdate its
/// way to the front, and would age a stop from before it parked rather than
/// from when it actually joined the level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Maker {
    pub remaining_quantity: Qty,
    pub arrival: u128,
}

pub trait MatchingAlgorithm {
    /// Apportion `available` across one price level.
    ///
    /// `makers` is the level's queue in arrival order, **oldest first** — every
    /// allocator here breaks ties by position, so the caller owes it that
    /// ordering. `now` is the taker's arrival instant, the reference ages are
    /// measured against; allocators that don't weight by time ignore it.
    ///
    /// Nothing is read from a clock inside: an allocation is a pure function of
    /// its arguments, so it replays identically off a log.
    fn allocate(&self, available: Qty, makers: &[Maker], now: u128) -> Vec<Fill>;

    /// The lot this allocator floors its shares to, if it needs to know one.
    ///
    /// Part of the contract, not a foreign concern: clause (5) says every fill
    /// is a whole lot, and an allocator that genuinely multiplies and divides
    /// quantities can only honour it if it was told where the grid is. `None`
    /// means "closed under the lattice for free" — the FIFO answer.
    ///
    /// The engine reads this to check the matcher against the instrument it is
    /// about to trade. A pro-rata built for lot `1` in a lot-`100` book does
    /// not merely print badly: its off-lot fills are subtracted into the
    /// makers, leaving them off-grid, resting, and matchable. See the
    /// counterexample on [`ProRataMatcher`](pro_rata::ProRataMatcher).
    fn lot_size(&self) -> Option<u64> {
        None
    }
}

pub mod fifo;
pub mod pro_rata;
pub mod time_pro_rata;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod test_helpers;
