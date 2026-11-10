use crate::instrument::price::Price;
use crate::order::Side;
use crate::order_queue::LevelQueue;

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
