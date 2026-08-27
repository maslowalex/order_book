use std::time::{SystemTime, UNIX_EPOCH};

use crate::allocation::Maker;

/// Re-exported so the ~70 `use crate::types::Price` sites keep working. The
/// type itself lives in [`crate::instrument`], with the tick grid that gives it
/// meaning — a price is not a standalone number, it is a point on an
/// instrument's lattice.
pub use crate::instrument::{Price, Qty};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExchangeId(pub String);

impl ExchangeId {
    pub fn from_sequence(seq: u64) -> ExchangeId {
        ExchangeId(format!("exchId-{}", seq))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Ask,
    Bid,
}

/// What happens to the unfilled remainder of a limit order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeInForce {
    /// Good Till Cancel: the remainder rests until filled or cancelled.
    GTC,
    /// Immediate Or Cancel: fill what crosses right now, discard the rest.
    IOC,
    /// Fill Or Kill: fill completely right now, or do nothing at all.
    FOK,
}

/// Execution style, carrying exactly the data that style needs — a `Market`
/// order has no price to carry, a stop can't exist without a trigger, and TIF
/// only means something for limits. Invalid combinations (a GTC market order,
/// a stop with no trigger) are unrepresentable rather than validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType {
    /// Execute at `price` or better; `tif` decides the remainder's fate.
    Limit { price: Price, tif: TimeInForce },
    /// Execute now at the best available prices; inherently IOC.
    Market,
    /// Parks until the market trades at/past `trigger`, then becomes `Market`.
    StopMarket { trigger: Price },
    /// Parks until `trigger`, then becomes `Limit { price, tif }`.
    StopLimit {
        trigger: Price,
        price: Price,
        tif: TimeInForce,
    },
}

impl OrderType {
    pub fn limit_gtc(price: Price) -> Self {
        OrderType::Limit {
            price,
            tif: TimeInForce::GTC,
        }
    }

    pub fn limit_ioc(price: Price) -> Self {
        OrderType::Limit {
            price,
            tif: TimeInForce::IOC,
        }
    }

    pub fn limit_fok(price: Price) -> Self {
        OrderType::Limit {
            price,
            tif: TimeInForce::FOK,
        }
    }

    pub fn stop_market(trigger: Price) -> Self {
        OrderType::StopMarket { trigger }
    }

    pub fn stop_limit(trigger: Price, price: Price) -> Self {
        OrderType::StopLimit {
            trigger,
            price,
            tif: TimeInForce::GTC,
        }
    }

    /// The price this order rests at in the book — now (`Limit`) or after
    /// triggering (`StopLimit`). `None` for market-style orders, which never
    /// rest.
    pub fn limit_price(&self) -> Option<Price> {
        match self {
            OrderType::Limit { price, .. } | OrderType::StopLimit { price, .. } => Some(*price),
            OrderType::Market | OrderType::StopMarket { .. } => None,
        }
    }

    /// Every price this order carries — a limit price, a trigger, or both.
    ///
    /// A fixed-size array rather than a `Vec` or a boxed iterator: this runs
    /// once per submitted order on the ingress path, and a heap allocation to
    /// hand back at most two `u64`s would be absurd. Callers `.flatten()` it.
    ///
    /// A trigger is as much a price as a limit is — it is quoted on the same
    /// grid and compared against the same last-trade price — so admission has
    /// to see both, which is why `limit_price()` alone is not enough here.
    pub fn prices(&self) -> [Option<Price>; 2] {
        match self {
            OrderType::Limit { price, .. } => [Some(*price), None],
            OrderType::Market => [None, None],
            OrderType::StopMarket { trigger } => [Some(*trigger), None],
            OrderType::StopLimit { trigger, price, .. } => [Some(*trigger), Some(*price)],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OrderLifecycle {
    #[default]
    New,
    PartiallyFilled, // represents how much is filled
    Filled,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Order {
    pub exchange_id: ExchangeId,
    pub client_id: ClientId,
    /// Carries the price(s) too — see `OrderType`. A resting order is always
    /// `Limit`, and its price is its level's price.
    pub order_type: OrderType,
    pub side: Side,
    pub original_quantity: Qty,
    pub remaining_quantity: Qty,
    pub timestamp: u128, // matters, because of the FIFO processing we need to know the time of the order
    /// When this order joined its price level, on the exchange's own counter —
    /// see [`crate::orderbook::OrderBook::next_arrival`].
    ///
    /// Not a second `timestamp`. `timestamp` is whatever the *client* put on
    /// the message and is never re-stamped on receipt, so an allocator that
    /// weighted by it would let a sender backdate its way to the front of the
    /// queue. This one the exchange assigns, at the moment the order actually
    /// comes to rest — which is also why it is not the submission order: a stop
    /// submitted early can trigger late, and it queues where it landed, not
    /// where it was sent from.
    ///
    /// Zero until the book stamps it. An order that never rested never had an
    /// arrival, and `0` says so.
    ///
    /// # Why `u32`, and what it costs
    ///
    /// Measured, not guessed. `timestamp` is a `u128`, so `Order` aligns to 16
    /// — and at 106 bytes of content it had 6 bytes of tail padding going
    /// spare. A `u32` lands in that padding and `Order` stays **112 bytes**; a
    /// `u64` does not fit and pushes it to **128**. That 14% is paid on every
    /// byte the engine memmoves in `PriceLevel::remove_order` and every byte it
    /// walks scanning a level, and it showed up as +25–35% on the tight-
    /// distribution benchmarks — for a field only `TimeProRataMatcher` reads.
    ///
    /// The price is a ceiling of ~4.29 billion rests per book, enforced below
    /// rather than left to wrap: a wrapped counter would make the newest order
    /// at a level look like the oldest, silently inverting time priority — the
    /// one thing this field exists to get right. A real venue would either
    /// spend the 16 bytes or renumber at a session boundary; this book is
    /// explicit about which it chose.
    pub arrival: u32,
    pub lifecycle: OrderLifecycle,
}

#[derive(Debug, Clone)]
pub struct OrderBuilder {
    exchange_id: Option<ExchangeId>,
    client_id: Option<ClientId>,
    order_type: Option<OrderType>,
    side: Option<Side>,
    original_quantity: Option<Qty>,
    remaining_quantity: Option<Qty>,
    timestamp: Option<u128>,
    arrival: Option<u32>,
    lifecycle: Option<OrderLifecycle>,
}

impl Default for OrderBuilder {
    fn default() -> Self {
        let timestamp_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(); // u128

        OrderBuilder {
            lifecycle: Some(OrderLifecycle::New),
            timestamp: Some(timestamp_ns),
            // Not stamped here, and deliberately not defaulted from a clock the
            // way `timestamp` is: only the book can say when an order joined a
            // level, so a builder that guessed would be guessing about the one
            // thing this field exists to be authoritative about.
            arrival: Some(0),
            // no order_type default: `Market` silently standing in for a
            // forgotten `.order_type(...)` hid intent — now it's a loud panic
            order_type: None,
            exchange_id: None,
            client_id: None,
            original_quantity: None,
            remaining_quantity: None,
            side: None,
        }
    }
}

impl OrderBuilder {
    pub fn build(self) -> Order {
        let original_quantity = self.original_quantity.expect("quantity doesn't provided");
        Order {
            exchange_id: self.exchange_id.expect("exchange_id should be present"),
            client_id: self.client_id.expect("client_id should be present"),
            lifecycle: self
                .lifecycle
                .expect("somehow lifecycle is not provided and default doesn't applied"),
            timestamp: self
                .timestamp
                .expect("somehow timetamp is not provided and default doesn't applied"),
            arrival: self
                .arrival
                .expect("somehow arrival is not provided and default doesn't applied"),
            side: self.side.expect("side doesn't provided"),
            // a fresh order has nothing filled yet, so remaining == original unless
            // a test explicitly seeds a pre-filled resting order via .remaining_quantity()
            remaining_quantity: self.remaining_quantity.unwrap_or(original_quantity),
            original_quantity: self.original_quantity.unwrap_or(original_quantity),
            order_type: self.order_type.expect("order_type doesn't provided"),
        }
    }

    pub fn timestamp(mut self, timestamp: u128) -> Self {
        self.timestamp = Some(timestamp);
        self
    }

    /// Seeds the arrival stamp directly. For tests that need a level with a
    /// known age profile without submitting orders one at a time — the book
    /// overwrites this the moment the order actually rests.
    pub fn arrival(mut self, arrival: u32) -> Self {
        self.arrival = Some(arrival);
        self
    }

    pub fn exchange_id(mut self, exchange_id: impl Into<String>) -> Self {
        self.exchange_id = Some(ExchangeId(exchange_id.into()));
        self
    }

    pub fn client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = Some(ClientId(client_id.into()));
        self
    }

    pub fn order_type(mut self, order_type: OrderType) -> Self {
        self.order_type = Some(order_type);
        self
    }

    pub fn side(mut self, side: Side) -> Self {
        self.side = Some(side);
        self
    }

    pub fn quantity(mut self, quantity: Qty) -> Self {
        self.original_quantity = Some(quantity);
        self
    }

    pub fn remaining_quantity(mut self, remaining_quantity: Qty) -> Self {
        self.remaining_quantity = Some(remaining_quantity);
        self
    }

    pub fn lifecycle(mut self, lifecycle: OrderLifecycle) -> Self {
        self.lifecycle = Some(lifecycle);
        self
    }
}

impl Order {
    pub fn builder() -> OrderBuilder {
        OrderBuilder::default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PriceLevel {
    pub price: Price,
    pub side: Side,
    pub orders: Vec<Order>,
}

#[derive(Debug, PartialEq)]
pub enum OrderError {
    InvalidSide,
}

impl PriceLevel {
    pub fn new(price: Price, side: Side) -> Self {
        Self {
            price,
            side,
            orders: vec![],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }

    pub fn add_order(&mut self, order: Order) -> Result<&mut Self, OrderError> {
        if order.side != self.side {
            return Err(OrderError::InvalidSide);
        }

        self.orders.push(order);

        Ok(self)
    }

    pub fn remove_order(&mut self, order_exchange_id: &ExchangeId) -> Option<Order> {
        let idx = self
            .orders
            .iter()
            .position(|order| &order.exchange_id == order_exchange_id)?;

        Some(self.orders.remove(idx))
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
        assert_eq!(level.orders, vec![]);
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
