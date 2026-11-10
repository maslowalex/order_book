//! Pluggable storage for the active bid and ask books.
//!
//! Allocation policy and storage layout are deliberately independent axes:
//! [`MatchingAlgorithm`](crate::allocation::MatchingAlgorithm) decides how a
//! taker's quantity is shared within one level, while [`OrderBookStore`]
//! decides how levels are found and held.

use crate::instrument::InstrumentSpec;
use std::fmt::Debug;

pub mod book_level;
use crate::instrument::price::Price;
use crate::order::Side;
use book_level::BookLevel;

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
