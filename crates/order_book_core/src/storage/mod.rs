//! Pluggable storage for the active bid and ask books.
//!
//! Allocation policy and storage layout are deliberately independent axes:
//! [`MatchingAlgorithm`](crate::allocation::MatchingAlgorithm) decides how a
//! taker's quantity is shared within one level, while [`OrderBookStore`]
//! decides how levels are found and held.

use std::fmt::Debug;

use crate::instrument::InstrumentSpec;
use crate::order_queue::LevelQueue;
use crate::types::{Price, Side};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    /// A dense tick ladder cannot allocate a finite array without an upper
    /// price bound.
    UnboundedPriceRange,
    /// The configured price span cannot be represented or allocated safely.
    PriceRangeTooLarge,
    /// The price is outside the span this store was constructed for.
    PriceOutsideRange,
}

/// Metadata for an active price level. Orders live in the book-wide arena.
/// Backends construct levels with `new`; only the engine changes queue links.
#[derive(Debug, Clone)]
pub struct BookLevel {
    pub price: Price,
    pub side: Side,
    pub(crate) queue: LevelQueue,
}

impl BookLevel {
    pub fn new(price: Price, side: Side) -> Self {
        Self {
            price,
            side,
            queue: LevelQueue::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
    pub fn order_count(&self) -> usize {
        self.queue.len()
    }
}

/// Price-level storage, independent of active-order ownership.
/// `ensure_level` must return the requested level or reject without mutation.
/// The engine removes levels after their last order is unlinked.
///
/// The trait is intentionally statically dispatched. Its iterator is a GAT so
/// backends expose ordered levels without boxing. BTree, BBTree, and ladder iteration
/// allocate nothing; HashMap iteration sorts a temporary vector of references.
pub trait OrderBookStore: Clone + Debug {
    type Levels<'a>: Iterator<Item = &'a BookLevel>
    where
        Self: 'a;

    fn try_new(spec: InstrumentSpec) -> Result<Self, StoreError>
    where
        Self: Sized;

    fn ensure_level(&mut self, side: Side, price: Price) -> Result<&mut BookLevel, StoreError>;
    fn level_mut(&mut self, side: Side, price: Price) -> Option<&mut BookLevel>;
    fn level(&self, side: Side, price: Price) -> Option<&BookLevel>;
    fn best_level(&self, side: Side) -> Option<&BookLevel>;
    fn best_level_mut(&mut self, side: Side) -> Option<&mut BookLevel>;
    fn remove_level(&mut self, side: Side, price: Price) -> Option<BookLevel>;
    fn levels(&self, side: Side) -> Self::Levels<'_>;

    #[inline]
    fn best_price(&self, side: Side) -> Option<Price> {
        self.best_level(side).map(|level| level.price)
    }
}

mod bbtree;
mod btree;
mod hash_map;
mod tick_ladder;

pub use bbtree::BBTreeStore;
pub use btree::{BTreeLevels, BTreeStore};
pub use hash_map::{HashMapLevels, HashMapStore};
pub use tick_ladder::{TickLadderStore, TickLevels};

#[cfg(test)]
mod tests;
