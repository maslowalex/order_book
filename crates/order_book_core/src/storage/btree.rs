use std::cmp::Reverse;
use std::collections::{BTreeMap, btree_map};

use crate::instrument::InstrumentSpec;
use crate::types::{Price, Side};

use super::{BookLevel, OrderBookStore, StoreError};

/// Tier-0 baseline: the original pair of ordered maps.
#[derive(Debug, Clone, Default)]
pub struct BTreeStore {
    bids: BTreeMap<Reverse<Price>, BookLevel>,
    asks: BTreeMap<Price, BookLevel>,
}

pub enum BTreeLevels<'a> {
    Bids(btree_map::Values<'a, Reverse<Price>, BookLevel>),
    Asks(btree_map::Values<'a, Price, BookLevel>),
}

impl<'a> Iterator for BTreeLevels<'a> {
    type Item = &'a BookLevel;

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
    fn ensure_level(&mut self, side: Side, price: Price) -> Result<&mut BookLevel, StoreError> {
        Ok(match side {
            Side::Bid => self
                .bids
                .entry(Reverse(price))
                .or_insert_with(|| BookLevel::new(price, side)),
            Side::Ask => self
                .asks
                .entry(price)
                .or_insert_with(|| BookLevel::new(price, side)),
        })
    }

    #[inline]
    fn level_mut(&mut self, side: Side, price: Price) -> Option<&mut BookLevel> {
        match side {
            Side::Bid => self.bids.get_mut(&Reverse(price)),
            Side::Ask => self.asks.get_mut(&price),
        }
    }

    #[inline]
    fn level(&self, side: Side, price: Price) -> Option<&BookLevel> {
        match side {
            Side::Bid => self.bids.get(&Reverse(price)),
            Side::Ask => self.asks.get(&price),
        }
    }

    #[inline]
    fn best_level(&self, side: Side) -> Option<&BookLevel> {
        match side {
            Side::Bid => self.bids.first_key_value().map(|(_, level)| level),
            Side::Ask => self.asks.first_key_value().map(|(_, level)| level),
        }
    }

    #[inline]
    fn best_level_mut(&mut self, side: Side) -> Option<&mut BookLevel> {
        match side {
            Side::Bid => self.bids.first_entry().map(|entry| entry.into_mut()),
            Side::Ask => self.asks.first_entry().map(|entry| entry.into_mut()),
        }
    }

    #[inline]
    fn remove_level(&mut self, side: Side, price: Price) -> Option<BookLevel> {
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
