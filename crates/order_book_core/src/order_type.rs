use crate::instrument::price::Price;

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
