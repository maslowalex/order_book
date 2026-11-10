use super::book_level::BookLevel;
use super::{OrderBookStore, StoreError};
use crate::instrument::InstrumentSpec;
use crate::instrument::price::Price;
use crate::order::Side;

/// A dense array with one slot per legal tick for each side.
///
/// Level lookup is arithmetic and top-of-book reads use cached indices. The
/// trade-off is paid up front: memory is proportional to the instrument's
/// complete configured price span, including empty prices.
#[derive(Debug, Clone)]
pub struct TickLadderStore {
    bids: Vec<Option<BookLevel>>,
    asks: Vec<Option<BookLevel>>,
    min_minor: u64,
    tick_size: u64,
    best_bid: Option<usize>,
    best_ask: Option<usize>,
}

pub enum TickLevels<'a> {
    Bids(std::iter::Rev<std::slice::Iter<'a, Option<BookLevel>>>),
    Asks(std::slice::Iter<'a, Option<BookLevel>>),
}

impl<'a> Iterator for TickLevels<'a> {
    type Item = &'a BookLevel;

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
    fn ensure_level(&mut self, side: Side, price: Price) -> Result<&mut BookLevel, StoreError> {
        let index = self.index(price).ok_or(StoreError::PriceOutsideRange)?;
        match side {
            Side::Bid if self.best_bid.is_none_or(|best| index > best) => {
                self.best_bid = Some(index)
            }
            Side::Ask if self.best_ask.is_none_or(|best| index < best) => {
                self.best_ask = Some(index)
            }
            _ => {}
        }
        let slots = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        Ok(slots[index].get_or_insert_with(|| BookLevel::new(price, side)))
    }

    #[inline]
    fn level_mut(&mut self, side: Side, price: Price) -> Option<&mut BookLevel> {
        let index = self.index(price)?;
        match side {
            Side::Bid => self.bids[index].as_mut(),
            Side::Ask => self.asks[index].as_mut(),
        }
    }

    #[inline]
    fn level(&self, side: Side, price: Price) -> Option<&BookLevel> {
        let index = self.index(price)?;
        match side {
            Side::Bid => self.bids[index].as_ref(),
            Side::Ask => self.asks[index].as_ref(),
        }
    }

    #[inline]
    fn best_level(&self, side: Side) -> Option<&BookLevel> {
        match side {
            Side::Bid => self.best_bid.and_then(|index| self.bids[index].as_ref()),
            Side::Ask => self.best_ask.and_then(|index| self.asks[index].as_ref()),
        }
    }

    #[inline]
    fn best_level_mut(&mut self, side: Side) -> Option<&mut BookLevel> {
        match side {
            Side::Bid => self.best_bid.and_then(|index| self.bids[index].as_mut()),
            Side::Ask => self.best_ask.and_then(|index| self.asks[index].as_mut()),
        }
    }

    #[inline]
    fn remove_level(&mut self, side: Side, price: Price) -> Option<BookLevel> {
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
    use super::super::tests::bounded_spec;
    use super::*;
    use crate::test_helpers::px;

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
        store.ensure_level(Side::Ask, px(95)).unwrap();
        store.ensure_level(Side::Bid, px(105)).unwrap();
        assert_eq!(store.best_price(Side::Ask), Some(px(95)));
        assert_eq!(store.best_price(Side::Bid), Some(px(105)));
    }
}
