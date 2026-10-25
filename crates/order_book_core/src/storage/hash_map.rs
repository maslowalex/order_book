use std::cmp::Reverse;
use std::collections::hash_map::Entry;
use std::collections::{BinaryHeap, HashMap};

use crate::instrument::InstrumentSpec;
use crate::types::{Price, Side};

use super::{BookLevel, OrderBookStore, StoreError};

#[derive(Debug, Clone, Default)]
pub struct HashMapStore {
    bids: HashMap<Price, BookLevel>,
    asks: HashMap<Price, BookLevel>,
    bids_index: BinaryHeap<Price>,
    asks_index: BinaryHeap<Reverse<Price>>,
}

pub struct HashMapLevels<'a> {
    inner: std::vec::IntoIter<&'a BookLevel>,
}

impl<'a> Iterator for HashMapLevels<'a> {
    type Item = &'a BookLevel;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

impl OrderBookStore for HashMapStore {
    type Levels<'a> = HashMapLevels<'a>;

    fn levels(&self, side: Side) -> Self::Levels<'_> {
        let mut levels: Vec<&BookLevel> = match side {
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

    fn ensure_level(&mut self, side: Side, price: Price) -> Result<&mut BookLevel, StoreError> {
        let levels = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        Ok(match levels.entry(price) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                match side {
                    Side::Bid => self.bids_index.push(price),
                    Side::Ask => self.asks_index.push(Reverse(price)),
                }
                entry.insert(BookLevel::new(price, side))
            }
        })
    }

    #[inline]
    fn level(&self, side: Side, price: Price) -> Option<&BookLevel> {
        match side {
            Side::Bid => self.bids.get(&price),
            Side::Ask => self.asks.get(&price),
        }
    }

    fn best_level(&self, side: Side) -> Option<&BookLevel> {
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

    fn best_level_mut(&mut self, side: Side) -> Option<&mut BookLevel> {
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

    fn level_mut(&mut self, side: Side, price: Price) -> Option<&mut BookLevel> {
        match side {
            Side::Bid => self.bids.get_mut(&price),
            Side::Ask => self.asks.get_mut(&price),
        }
    }

    fn remove_level(&mut self, side: Side, price: Price) -> Option<BookLevel> {
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
