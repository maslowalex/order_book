use ::bbtree::unsync::{BBTreeMap, bbtree_map::Values};

use super::book_level::BookLevel;
use super::{OrderBookStore, StoreError};
use crate::instrument::InstrumentSpec;
use crate::instrument::price::Price;
use crate::order::Side;

/// B+ tree storage with integer inline keys and best-first iteration.
///
/// Asks use minor units directly; bids complement them to reverse ordering
/// without wrapping the key type and losing bbtree's automatic u64 strategy.
#[derive(Debug, Clone, Default)]
pub struct BBTreeStore {
    bids: BBTreeMap<u64, BookLevel>,
    asks: BBTreeMap<u64, BookLevel>,
}

impl OrderBookStore for BBTreeStore {
    type Levels<'a> = Values<'a, u64, BookLevel>;

    fn try_new(_spec: InstrumentSpec) -> Result<Self, StoreError> {
        Ok(Self::default())
    }

    #[inline]
    fn ensure_level(&mut self, side: Side, price: Price) -> Result<&mut BookLevel, StoreError> {
        Ok(match side {
            Side::Bid => self.bids.entry(!price.minor()),
            Side::Ask => self.asks.entry(price.minor()),
        }
        .or_insert_with(|| BookLevel::new(price, side)))
    }

    #[inline]
    fn level_mut(&mut self, side: Side, price: Price) -> Option<&mut BookLevel> {
        match side {
            Side::Bid => self.bids.get_mut(&!price.minor()),
            Side::Ask => self.asks.get_mut(&price.minor()),
        }
    }

    #[inline]
    fn level(&self, side: Side, price: Price) -> Option<&BookLevel> {
        match side {
            Side::Bid => self.bids.get(&!price.minor()),
            Side::Ask => self.asks.get(&price.minor()),
        }
    }

    #[inline]
    fn best_level(&self, side: Side) -> Option<&BookLevel> {
        match side {
            Side::Bid => self.bids.first_key_value(),
            Side::Ask => self.asks.first_key_value(),
        }
        .map(|(_, level)| level)
    }

    #[inline]
    fn best_level_mut(&mut self, side: Side) -> Option<&mut BookLevel> {
        match side {
            Side::Bid => self.bids.first_entry(),
            Side::Ask => self.asks.first_entry(),
        }
        .map(|entry| entry.into_mut())
    }

    #[inline]
    fn remove_level(&mut self, side: Side, price: Price) -> Option<BookLevel> {
        match side {
            Side::Bid => self.bids.remove(&!price.minor()),
            Side::Ask => self.asks.remove(&price.minor()),
        }
    }

    #[inline]
    fn levels(&self, side: Side) -> Self::Levels<'_> {
        match side {
            Side::Bid => self.bids.values(),
            Side::Ask => self.asks.values(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::BTreeStore;

    fn assert_same_levels(tree: &BTreeStore, bbtree: &BBTreeStore, side: Side) {
        assert_eq!(tree.best_price(side), bbtree.best_price(side));
        assert_eq!(
            tree.levels(side)
                .map(|level| level.price)
                .collect::<Vec<_>>(),
            bbtree
                .levels(side)
                .map(|level| level.price)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn many_levels_split_drain_and_reinsert_like_btree() {
        let mut tree = BTreeStore::default();
        let mut bbtree = BBTreeStore::default();
        const LEVELS: u64 = 10_000;
        for side in [Side::Bid, Side::Ask] {
            // A permutation inserts out of order and spans many leaf pages.
            for i in 0..LEVELS {
                let price = Price::from_minor_unchecked((i * 7919) % LEVELS);
                tree.ensure_level(side, price).unwrap();
                bbtree.ensure_level(side, price).unwrap();
            }
            assert_same_levels(&tree, &bbtree, side);
            let mut clone = bbtree.clone();
            for i in (0..LEVELS).step_by(2) {
                let price = Price::from_minor_unchecked(i);
                assert_eq!(
                    tree.remove_level(side, price).unwrap().price,
                    bbtree.remove_level(side, price).unwrap().price
                );
                assert!(bbtree.level(side, price).is_none());
                assert!(bbtree.level_mut(side, price).is_none());
            }
            assert_same_levels(&tree, &bbtree, side);
            assert_eq!(clone.levels(side).count(), LEVELS as usize);
            for i in (0..LEVELS).step_by(2) {
                let price = Price::from_minor_unchecked(i);
                tree.ensure_level(side, price).unwrap();
                bbtree.ensure_level(side, price).unwrap();
            }
            assert_same_levels(&tree, &bbtree, side);
            while let Some(price) = tree.best_price(side) {
                assert_eq!(bbtree.best_level_mut(side).unwrap().price, price);
                tree.remove_level(side, price).unwrap();
                bbtree.remove_level(side, price).unwrap();
                assert_eq!(tree.best_price(side), bbtree.best_price(side));
            }
            assert_same_levels(&tree, &bbtree, side);
            // Mutating a clone cannot refill the original's drained side.
            clone.remove_level(side, Price::from_minor_unchecked(0));
            assert!(bbtree.best_level(side).is_none());
        }
    }

    #[test]
    fn integer_key_boundaries_preserve_side_order() {
        let mut store = BBTreeStore::default();
        let prices = [0, 1, u64::MAX / 2, u64::MAX - 1, u64::MAX];
        for side in [Side::Bid, Side::Ask] {
            for minor in prices {
                store
                    .ensure_level(side, Price::from_minor_unchecked(minor))
                    .unwrap();
            }
            let mut expected = prices.to_vec();
            if side == Side::Bid {
                expected.reverse();
            }
            assert_eq!(
                store
                    .levels(side)
                    .map(|level| level.price.minor())
                    .collect::<Vec<_>>(),
                expected
            );
            for minor in expected {
                let price = Price::from_minor_unchecked(minor);
                assert_eq!(store.best_price(side), Some(price));
                assert_eq!(store.level(side, price).unwrap().side, side);
                store.remove_level(side, price).unwrap();
            }
            assert!(store.best_price(side).is_none());
        }
    }
}
