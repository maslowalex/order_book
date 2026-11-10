use crate::allocation::Maker;
use crate::instrument::price::Price;
use crate::instrument::qty::Qty;
use crate::order::{ExchangeId, Order, Side};
use crate::order_queue::OrderArena;
use crate::storage::book_level::BookLevel;

/// An owned FIFO snapshot, independent of the book and its arena.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceLevel {
    pub price: Price,
    pub side: Side,
    orders: Vec<Order>,
}

#[derive(Debug, PartialEq)]
pub enum OrderError {
    InvalidSide,
    DuplicateExchangeId,
}

impl PriceLevel {
    pub fn new(price: Price, side: Side) -> Self {
        Self {
            price,
            side,
            orders: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }

    pub fn add_order(&mut self, order: Order) -> Result<&mut Self, OrderError> {
        if order.side != self.side {
            return Err(OrderError::InvalidSide);
        }

        if self
            .orders
            .iter()
            .any(|existing| existing.exchange_id == order.exchange_id)
        {
            return Err(OrderError::DuplicateExchangeId);
        }
        self.orders.push(order);

        Ok(self)
    }

    pub fn remove_order(&mut self, order_exchange_id: &ExchangeId) -> Option<Order> {
        let position = self
            .orders
            .iter()
            .position(|order| &order.exchange_id == order_exchange_id)?;
        Some(self.orders.remove(position))
    }

    pub fn total_quantity(&self) -> Qty {
        self.orders
            .iter()
            .map(|order| order.remaining_quantity)
            .sum()
    }

    /// This level's queue as an allocation policy sees it: remaining size and
    /// arrival, oldest first — which is the ordering
    /// [`MatchingAlgorithm::allocate`](crate::allocation::MatchingAlgorithm::allocate)
    /// requires of its caller, and which `orders` already carries because
    /// `add_order` pushes to the back.
    ///
    /// This is the seam. Everything the matcher is allowed to know about a
    /// resting order passes through here — no prices, no ids, no order types —
    /// so a storage layout that holds nothing resembling an `Order` can still
    /// serve a matcher by yielding these. That is what makes the storage
    /// bake-off possible without touching a line of allocation logic.
    pub fn makers(&self) -> impl Iterator<Item = Maker> + '_ {
        self.orders.iter().map(|order| Maker {
            remaining_quantity: order.remaining_quantity,
            arrival: u128::from(order.arrival),
        })
    }

    /// Resting orders in price-time order.
    pub fn orders(&self) -> impl Iterator<Item = &Order> {
        self.orders.iter()
    }

    pub fn order_count(&self) -> usize {
        self.orders.len()
    }
}

/// A borrowed FIFO view into a level and the book's shared order arena.
/// Iterators borrow the book, so no node or order cloning is needed.
#[derive(Debug, Clone, Copy)]
pub struct PriceLevelView<'a> {
    pub price: Price,
    pub side: Side,
    level: &'a BookLevel,
    nodes: &'a OrderArena,
}

impl<'a> PriceLevelView<'a> {
    pub(crate) fn new(level: &'a BookLevel, nodes: &'a OrderArena) -> Self {
        Self {
            price: level.price,
            side: level.side,
            level,
            nodes,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.level.is_empty()
    }
    pub fn order_count(&self) -> usize {
        self.level.order_count()
    }

    pub fn orders(&self) -> impl ExactSizeIterator<Item = &'a Order> + use<'a> {
        self.level.queue.iter(self.nodes)
    }

    pub fn makers(&self) -> impl ExactSizeIterator<Item = Maker> + use<'a> {
        self.orders().map(|order| Maker {
            remaining_quantity: order.remaining_quantity,
            arrival: u128::from(order.arrival),
        })
    }

    pub fn total_quantity(&self) -> Qty {
        self.orders().map(|order| order.remaining_quantity).sum()
    }

    /// Explicitly clone the orders into a snapshot that can outlive the book.
    pub fn to_owned(&self) -> PriceLevel {
        PriceLevel {
            price: self.price,
            side: self.side,
            orders: self.orders().cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{order, price_level, px, qty};

    #[test]
    fn price_level_constructor_works() {
        let level = price_level(Side::Ask, 500);

        assert_eq!(level.price, px(500));
        assert_eq!(level.side, Side::Ask);
        assert_eq!(level.order_count(), 0);
    }

    #[test]
    fn price_level_add_order_works() {
        let mut level = price_level(Side::Ask, 100);

        level
            .add_order(order(Side::Ask, 100, 10, None, "ex_1"))
            .expect("order should be added");

        assert!(!level.is_empty());
    }

    #[test]
    fn price_level_add_order_invalid_level_doesnt_change_orders_of_level() {
        let mut level = price_level(Side::Bid, 100);

        let result = level.add_order(order(Side::Ask, 100, 10, None, "ex_1"));

        assert!(result.is_err_and(|x| x == OrderError::InvalidSide));
        assert!(level.is_empty());
    }

    #[test]
    fn price_level_remove_order_works() {
        let mut level = price_level(Side::Ask, 100);
        let to_remove = order(Side::Ask, 100, 10, None, "ex_1");
        let exchange_id = to_remove.exchange_id.clone();

        level.add_order(to_remove).expect("order should be added");

        assert!(level.remove_order(&exchange_id).is_some());
        assert!(level.is_empty());
    }

    #[test]
    fn price_level_remote_nonexistent_doesnt_crash() {
        let mut level = price_level(Side::Ask, 100);

        level.remove_order(&ExchangeId("ex_1".to_owned()));

        assert!(level.is_empty());
    }

    #[test]
    fn price_level_is_empty_works() {
        let mut level = price_level(Side::Ask, 100);

        assert!(level.is_empty());

        level
            .add_order(order(Side::Ask, 100, 10, None, "ex_1"))
            .expect("order should be added");

        assert!(!level.is_empty());
    }

    #[test]
    fn price_level_total_quantity_works() {
        let mut level = price_level(Side::Bid, 100);

        level
            .add_order(order(Side::Bid, 100, 10, None, "order_1"))
            .expect("order 1 should be added");
        level
            .add_order(order(Side::Bid, 100, 35, None, "order_2"))
            .expect("order 2 should be added");

        assert_eq!(level.total_quantity(), qty(45));
    }

    #[test]
    fn price_level_total_quantity_with_prefilled_order_works() {
        let mut level = price_level(Side::Bid, 100);
        let partially_filled = order(Side::Bid, 100, 10, Some(5), "order_1");

        level
            .add_order(partially_filled)
            .expect("order 1 should be added");
        level
            .add_order(order(Side::Bid, 100, 35, None, "order_2"))
            .expect("order 2 should be added");

        assert_eq!(level.total_quantity(), qty(40));
    }
}
