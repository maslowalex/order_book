//! Pluggable storage for the active bid and ask books.
//!
//! Allocation policy and storage layout are deliberately independent axes:
//! [`MatchingAlgorithm`](crate::allocation::MatchingAlgorithm) decides how a
//! taker's quantity is shared within one level, while [`OrderBookStore`]
//! decides how levels are found and held.

use std::cmp::Reverse;
use std::collections::btree_map;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::fmt::Debug;

use crate::instrument::InstrumentSpec;
use crate::types::{ExchangeId, Order, OrderType, Price, PriceLevel, Side};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    /// A dense tick ladder cannot allocate a finite array without an upper
    /// price bound.
    UnboundedPriceRange,
    /// The configured price span cannot be represented or allocated safely.
    PriceRangeTooLarge,
    /// The price is outside the span this store was constructed for.
    PriceOutsideRange,
    /// Only limit orders can live in active-book storage.
    NotRestable,
    /// An order was offered to a level on the opposite side.
    InvalidSide,
}

/// The minimum active-book interface needed by matching and public queries.
///
/// The trait is intentionally statically dispatched. Its iterator is a GAT so
/// each backend can expose ordered levels without boxing or allocating on the
/// read path.
pub trait OrderBookStore: Clone + Debug {
    type Levels<'a>: Iterator<Item = &'a PriceLevel>
    where
        Self: 'a;

    fn try_new(spec: InstrumentSpec) -> Result<Self, StoreError>
    where
        Self: Sized;

    fn add(&mut self, order: Order) -> Result<(), StoreError>;
    fn cancel(&mut self, side: Side, price: Price, id: &ExchangeId) -> Option<Order>;
    fn level(&self, side: Side, price: Price) -> Option<&PriceLevel>;
    fn best_level(&self, side: Side) -> Option<&PriceLevel>;
    fn best_level_mut(&mut self, side: Side) -> Option<&mut PriceLevel>;
    fn remove_level(&mut self, side: Side, price: Price) -> Option<PriceLevel>;
    fn levels(&self, side: Side) -> Self::Levels<'_>;

    #[inline]
    fn best_price(&self, side: Side) -> Option<Price> {
        self.best_level(side).map(|level| level.price)
    }
}

/// Tier-0 baseline: the original pair of ordered maps.
#[derive(Debug, Clone, Default)]
pub struct BTreeStore {
    bids: BTreeMap<Reverse<Price>, PriceLevel>,
    asks: BTreeMap<Price, PriceLevel>,
}

pub enum BTreeLevels<'a> {
    Bids(btree_map::Values<'a, Reverse<Price>, PriceLevel>),
    Asks(btree_map::Values<'a, Price, PriceLevel>),
}

impl<'a> Iterator for BTreeLevels<'a> {
    type Item = &'a PriceLevel;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            BTreeLevels::Bids(levels) => levels.next(),
            BTreeLevels::Asks(levels) => levels.next(),
        }
    }
}

impl OrderBookStore for BTreeStore {
    type Levels<'a> = BTreeLevels<'a>;

    fn try_new(_spec: InstrumentSpec) -> Result<Self, StoreError> {
        Ok(Self::default())
    }

    #[inline]
    fn add(&mut self, order: Order) -> Result<(), StoreError> {
        let OrderType::Limit { price, .. } = order.order_type else {
            return Err(StoreError::NotRestable);
        };
        let side = order.side;
        let level = match side {
            Side::Bid => self
                .bids
                .entry(Reverse(price))
                .or_insert_with(|| PriceLevel::new(price, side)),
            Side::Ask => self
                .asks
                .entry(price)
                .or_insert_with(|| PriceLevel::new(price, side)),
        };
        level
            .add_order(order)
            .map(|_| ())
            .map_err(|_| StoreError::InvalidSide)
    }

    #[inline]
    fn cancel(&mut self, side: Side, price: Price, id: &ExchangeId) -> Option<Order> {
        let (removed, empty) = match side {
            Side::Bid => {
                let level = self.bids.get_mut(&Reverse(price))?;
                let removed = level.remove_order(id);
                (removed, level.is_empty())
            }
            Side::Ask => {
                let level = self.asks.get_mut(&price)?;
                let removed = level.remove_order(id);
                (removed, level.is_empty())
            }
        };
        if empty {
            self.remove_level(side, price);
        }
        removed
    }

    #[inline]
    fn level(&self, side: Side, price: Price) -> Option<&PriceLevel> {
        match side {
            Side::Bid => self.bids.get(&Reverse(price)),
            Side::Ask => self.asks.get(&price),
        }
    }

    #[inline]
    fn best_level(&self, side: Side) -> Option<&PriceLevel> {
        match side {
            Side::Bid => self.bids.first_key_value().map(|(_, level)| level),
            Side::Ask => self.asks.first_key_value().map(|(_, level)| level),
        }
    }

    #[inline]
    fn best_level_mut(&mut self, side: Side) -> Option<&mut PriceLevel> {
        match side {
            Side::Bid => self.bids.first_entry().map(|entry| entry.into_mut()),
            Side::Ask => self.asks.first_entry().map(|entry| entry.into_mut()),
        }
    }

    #[inline]
    fn remove_level(&mut self, side: Side, price: Price) -> Option<PriceLevel> {
        match side {
            Side::Bid => self.bids.remove(&Reverse(price)),
            Side::Ask => self.asks.remove(&price),
        }
    }

    #[inline]
    fn levels(&self, side: Side) -> Self::Levels<'_> {
        match side {
            Side::Bid => BTreeLevels::Bids(self.bids.values()),
            Side::Ask => BTreeLevels::Asks(self.asks.values()),
        }
    }
}

/// A dense array with one slot per legal tick for each side.
///
/// Level lookup is arithmetic and top-of-book reads use cached indices. The
/// trade-off is paid up front: memory is proportional to the instrument's
/// complete configured price span, including empty prices.
#[derive(Debug, Clone)]
pub struct TickLadderStore {
    bids: Vec<Option<PriceLevel>>,
    asks: Vec<Option<PriceLevel>>,
    min_minor: u64,
    tick_size: u64,
    best_bid: Option<usize>,
    best_ask: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct HashMapStore {
    bids: HashMap<Price, PriceLevel>,
    asks: HashMap<Price, PriceLevel>,
    bids_index: BinaryHeap<Price>,
    asks_index: BinaryHeap<Reverse<Price>>,
}

pub struct HashMapLevels<'a> {
    inner: std::vec::IntoIter<&'a PriceLevel>,
}

impl<'a> Iterator for HashMapLevels<'a> {
    type Item = &'a PriceLevel;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

impl OrderBookStore for HashMapStore {
    type Levels<'a> = HashMapLevels<'a>;

    fn levels(&self, side: Side) -> Self::Levels<'_> {
        let mut levels: Vec<&PriceLevel> = match side {
            Side::Bid => self.bids.values().collect(),
            Side::Ask => self.asks.values().collect(),
        };

        match side {
            Side::Bid => {
                levels.sort_unstable_by_key(|level| Reverse(level.price));
            }
            Side::Ask => {
                levels.sort_unstable_by_key(|level| level.price);
            }
        }

        HashMapLevels {
            inner: levels.into_iter(),
        }
    }

    fn try_new(_spec: InstrumentSpec) -> Result<Self, StoreError> {
        Ok(Self::default())
    }

    fn add(&mut self, order: Order) -> Result<(), StoreError> {
        let OrderType::Limit { price, .. } = order.order_type else {
            return Err(StoreError::NotRestable);
        };
        let side = order.side;
        let created_new_level = {
            let levels = match side {
                Side::Bid => &mut self.bids,
                Side::Ask => &mut self.asks,
            };

            match levels.entry(price) {
                Entry::Occupied(mut entry) => {
                    entry
                        .get_mut()
                        .add_order(order)
                        .map_err(|_| StoreError::InvalidSide)?;

                    false
                }

                Entry::Vacant(entry) => {
                    let mut level = PriceLevel::new(price, side);

                    level
                        .add_order(order)
                        .map_err(|_| StoreError::InvalidSide)?;

                    entry.insert(level);
                    true
                }
            }
        };

        if created_new_level {
            match side {
                Side::Ask => self.asks_index.push(Reverse(price)),
                Side::Bid => self.bids_index.push(price),
            }
        }

        Ok(())
    }

    #[inline]
    fn level(&self, side: Side, price: Price) -> Option<&PriceLevel> {
        match side {
            Side::Bid => self.bids.get(&price),
            Side::Ask => self.asks.get(&price),
        }
    }

    fn best_level(&self, side: Side) -> Option<&PriceLevel> {
        match side {
            Side::Bid => self
                .bids_index
                .peek()
                .and_then(|price| self.bids.get(price)),

            Side::Ask => self
                .asks_index
                .peek()
                .and_then(|Reverse(price)| self.asks.get(price)),
        }
    }

    fn best_level_mut(&mut self, side: Side) -> Option<&mut PriceLevel> {
        match side {
            Side::Bid => self
                .bids_index
                .peek()
                .and_then(|price| self.bids.get_mut(price)),

            Side::Ask => self
                .asks_index
                .peek()
                .and_then(|Reverse(price)| self.asks.get_mut(price)),
        }
    }

    fn cancel(&mut self, side: Side, price: Price, id: &ExchangeId) -> Option<Order> {
        let (removed, empty) = {
            let levels = match side {
                Side::Bid => &mut self.bids,
                Side::Ask => &mut self.asks,
            };
            let level = levels.get_mut(&price)?;
            let removed = level.remove_order(id);
            (removed, level.is_empty())
        };

        if empty {
            self.remove_level(side, price);
        }
        removed
    }

    fn remove_level(&mut self, side: Side, price: Price) -> Option<PriceLevel> {
        match side {
            Side::Bid => {
                self.bids_index.retain(|el| el != &price);
                self.bids.remove(&price)
            }
            Side::Ask => {
                self.asks_index.retain(|Reverse(el)| el != &price);
                self.asks.remove(&price)
            }
        }
    }
}

pub enum TickLevels<'a> {
    Bids(std::iter::Rev<std::slice::Iter<'a, Option<PriceLevel>>>),
    Asks(std::slice::Iter<'a, Option<PriceLevel>>),
}

impl<'a> Iterator for TickLevels<'a> {
    type Item = &'a PriceLevel;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            TickLevels::Bids(slots) => slots.find_map(Option::as_ref),
            TickLevels::Asks(slots) => slots.find_map(Option::as_ref),
        }
    }
}

impl TickLadderStore {
    #[inline]
    fn index(&self, price: Price) -> Option<usize> {
        let offset = price.minor().checked_sub(self.min_minor)?;
        if !offset.is_multiple_of(self.tick_size) {
            return None;
        }
        let index = usize::try_from(offset / self.tick_size).ok()?;
        (index < self.bids.len()).then_some(index)
    }

    fn repair_best_after_removal(&mut self, side: Side, removed: usize) {
        match side {
            Side::Bid => {
                self.best_bid = self.bids[..removed].iter().rposition(Option::is_some);
            }
            Side::Ask => {
                self.best_ask = self.asks[removed + 1..]
                    .iter()
                    .position(Option::is_some)
                    .map(|offset| removed + 1 + offset);
            }
        }
    }

    /// Number of legal price slots allocated per side.
    pub fn slots_per_side(&self) -> usize {
        self.bids.len()
    }
}

impl OrderBookStore for TickLadderStore {
    type Levels<'a> = TickLevels<'a>;

    fn try_new(spec: InstrumentSpec) -> Result<Self, StoreError> {
        let max = spec.max_price().ok_or(StoreError::UnboundedPriceRange)?;
        let span = max
            .minor()
            .checked_sub(spec.min_price().minor())
            .ok_or(StoreError::PriceRangeTooLarge)?;
        let slots_u64 = span
            .checked_div(spec.tick_size())
            .and_then(|ticks| ticks.checked_add(1))
            .ok_or(StoreError::PriceRangeTooLarge)?;
        let slots = usize::try_from(slots_u64).map_err(|_| StoreError::PriceRangeTooLarge)?;

        let mut bids = Vec::new();
        let mut asks = Vec::new();
        bids.try_reserve_exact(slots)
            .map_err(|_| StoreError::PriceRangeTooLarge)?;
        asks.try_reserve_exact(slots)
            .map_err(|_| StoreError::PriceRangeTooLarge)?;
        bids.resize_with(slots, || None);
        asks.resize_with(slots, || None);

        Ok(Self {
            bids,
            asks,
            min_minor: spec.min_price().minor(),
            tick_size: spec.tick_size(),
            best_bid: None,
            best_ask: None,
        })
    }

    #[inline]
    fn add(&mut self, order: Order) -> Result<(), StoreError> {
        let OrderType::Limit { price, .. } = order.order_type else {
            return Err(StoreError::NotRestable);
        };
        let side = order.side;
        let index = self.index(price).ok_or(StoreError::PriceOutsideRange)?;
        let slots = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        let level = slots[index].get_or_insert_with(|| PriceLevel::new(price, side));
        level
            .add_order(order)
            .map_err(|_| StoreError::InvalidSide)?;

        match side {
            Side::Bid if self.best_bid.is_none_or(|best| index > best) => {
                self.best_bid = Some(index);
            }
            Side::Ask if self.best_ask.is_none_or(|best| index < best) => {
                self.best_ask = Some(index);
            }
            _ => {}
        }
        Ok(())
    }

    #[inline]
    fn cancel(&mut self, side: Side, price: Price, id: &ExchangeId) -> Option<Order> {
        let index = self.index(price)?;
        let slots = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        let removed = slots[index].as_mut()?.remove_order(id);
        if slots[index].as_ref().is_some_and(PriceLevel::is_empty) {
            slots[index] = None;
            let removed_touch = match side {
                Side::Bid => self.best_bid == Some(index),
                Side::Ask => self.best_ask == Some(index),
            };
            if removed_touch {
                self.repair_best_after_removal(side, index);
            }
        }
        removed
    }

    #[inline]
    fn level(&self, side: Side, price: Price) -> Option<&PriceLevel> {
        let index = self.index(price)?;
        match side {
            Side::Bid => self.bids[index].as_ref(),
            Side::Ask => self.asks[index].as_ref(),
        }
    }

    #[inline]
    fn best_level(&self, side: Side) -> Option<&PriceLevel> {
        match side {
            Side::Bid => self.best_bid.and_then(|index| self.bids[index].as_ref()),
            Side::Ask => self.best_ask.and_then(|index| self.asks[index].as_ref()),
        }
    }

    #[inline]
    fn best_level_mut(&mut self, side: Side) -> Option<&mut PriceLevel> {
        match side {
            Side::Bid => self.best_bid.and_then(|index| self.bids[index].as_mut()),
            Side::Ask => self.best_ask.and_then(|index| self.asks[index].as_mut()),
        }
    }

    #[inline]
    fn remove_level(&mut self, side: Side, price: Price) -> Option<PriceLevel> {
        let index = self.index(price)?;
        let removed = match side {
            Side::Bid => self.bids[index].take(),
            Side::Ask => self.asks[index].take(),
        };
        let removed_touch = match side {
            Side::Bid => self.best_bid == Some(index),
            Side::Ask => self.best_ask == Some(index),
        };
        if removed.is_some() && removed_touch {
            self.repair_best_after_removal(side, index);
        }
        removed
    }

    #[inline]
    fn levels(&self, side: Side) -> Self::Levels<'_> {
        match side {
            Side::Bid => TickLevels::Bids(self.bids.iter().rev()),
            Side::Ask => TickLevels::Asks(self.asks.iter()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{order, px, qty};

    fn bounded_spec() -> InstrumentSpec {
        InstrumentSpec::cents()
            .with_price_range(px(95), Some(px(105)))
            .unwrap()
    }

    fn check_store_contract<S: OrderBookStore>() {
        let mut store = S::try_new(bounded_spec()).unwrap();
        assert!(store.best_level(Side::Bid).is_none());
        assert!(store.best_level(Side::Ask).is_none());

        store.add(order(Side::Bid, 98, 7, None, "b98")).unwrap();
        store.add(order(Side::Bid, 100, 5, None, "b100")).unwrap();
        store.add(order(Side::Ask, 102, 3, None, "a102")).unwrap();
        store.add(order(Side::Ask, 101, 4, None, "a101")).unwrap();

        assert_eq!(store.best_price(Side::Bid), Some(px(100)));
        assert_eq!(store.best_price(Side::Ask), Some(px(101)));
        assert_eq!(
            store
                .levels(Side::Bid)
                .map(|level| level.price)
                .collect::<Vec<_>>(),
            vec![px(100), px(98)]
        );
        assert_eq!(
            store
                .levels(Side::Ask)
                .map(|level| level.price)
                .collect::<Vec<_>>(),
            vec![px(101), px(102)]
        );

        let removed = store.cancel(Side::Bid, px(100), &ExchangeId("b100".into()));
        assert_eq!(removed.unwrap().remaining_quantity, qty(5));
        assert_eq!(store.best_price(Side::Bid), Some(px(98)));
        assert!(store.level(Side::Bid, px(100)).is_none());

        store.remove_level(Side::Ask, px(101));
        assert_eq!(store.best_price(Side::Ask), Some(px(102)));
    }

    #[test]
    fn btree_satisfies_the_store_contract() {
        check_store_contract::<BTreeStore>();
    }

    #[test]
    fn hash_map_satisfies_the_store_contract() {
        check_store_contract::<HashMapStore>();
    }

    #[test]
    fn tick_ladder_satisfies_the_store_contract() {
        check_store_contract::<TickLadderStore>();
    }

    #[test]
    fn tick_ladder_requires_a_finite_price_range() {
        assert_eq!(
            TickLadderStore::try_new(InstrumentSpec::cents()).unwrap_err(),
            StoreError::UnboundedPriceRange
        );
    }

    #[test]
    fn tick_ladder_maps_both_inclusive_boundaries() {
        let mut store = TickLadderStore::try_new(bounded_spec()).unwrap();
        assert_eq!(store.slots_per_side(), 11);
        store.add(order(Side::Ask, 95, 1, None, "min")).unwrap();
        store.add(order(Side::Bid, 105, 1, None, "max")).unwrap();
        assert_eq!(store.best_price(Side::Ask), Some(px(95)));
        assert_eq!(store.best_price(Side::Bid), Some(px(105)));
    }
}
