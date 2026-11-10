use crate::instrument::qty::Qty;
use crate::instrument::{InstrumentSpec, RejectReason, SpecError};
use crate::order_type::OrderType;
use derive_builder::Builder;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExchangeId(pub String);

impl ExchangeId {
    pub fn from_sequence(seq: u64) -> ExchangeId {
        ExchangeId(format!("exchId-{}", seq))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Ask,
    Bid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OrderLifecycle {
    #[default]
    New,
    PartiallyFilled,
    Filled,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Builder)]
#[builder(
    pattern = "owned",
    derive(Debug, Clone),
    custom_constructor,
    build_fn(error = "OrderValidationError")
)]
pub struct Order {
    #[builder(setter(custom))]
    pub exchange_id: ExchangeId,
    #[builder(setter(custom))]
    pub client_id: ClientId,
    /// Carries the price(s) too — see `OrderType`. A resting order is always
    /// `Limit`, and its price is its level's price.
    pub order_type: OrderType,
    pub side: Side,
    #[builder(setter(name = "quantity"))]
    pub original_quantity: Qty,
    #[builder(
        default = "self.original_quantity.ok_or_else(|| OrderValidationError::missing_field(\"quantity\"))?"
    )]
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
            // forgotten `.order_type(...)` hid intent — now it returns an error
            order_type: None,
            exchange_id: None,
            client_id: None,
            original_quantity: None,
            remaining_quantity: None,
            side: None,
        }
    }
}

#[derive(Debug)]
pub enum OrderValidationError {
    MissingField(String),
    InvalidLifecycle,
    InvalidPrice(SpecError),
    InvalidTrigger(SpecError),
    InvalidQuantity(SpecError),
    UnexpectedField(&'static str),
    Rejected(RejectReason),
}

impl std::fmt::Display for OrderValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingField(field) => write!(f, "missing required field: {field}"),
            Self::InvalidLifecycle => f.write_str("invalid order lifecycle"),
            Self::InvalidPrice(error) => write!(f, "invalid price: {error}"),
            Self::InvalidTrigger(error) => write!(f, "invalid trigger: {error}"),
            Self::InvalidQuantity(error) => write!(f, "invalid quantity: {error}"),
            Self::UnexpectedField(field) => {
                write!(f, "field is not allowed for this order kind: {field}")
            }
            Self::Rejected(reason) => write!(f, "order rejected: {reason}"),
        }
    }
}

impl OrderValidationError {
    pub fn missing_field(field: &str) -> Self {
        Self::MissingField(field.to_owned())
    }
}

impl From<derive_builder::UninitializedFieldError> for OrderValidationError {
    fn from(error: derive_builder::UninitializedFieldError) -> Self {
        let field = match error.field_name() {
            "original_quantity" => "quantity",
            field => field,
        };
        Self::missing_field(field)
    }
}

impl OrderBuilder {
    // Preserve the string-based ID setters; the derive generates the other
    // setters and the fallible build method.
    pub fn exchange_id(mut self, exchange_id: impl Into<String>) -> Self {
        self.exchange_id = Some(ExchangeId(exchange_id.into()));
        self
    }

    pub fn client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = Some(ClientId(client_id.into()));
        self
    }
}

impl Order {
    /// Admission policy shared by raw-input conversion and book submission.
    pub(crate) fn validate_admission(&self, spec: &InstrumentSpec) -> Result<(), RejectReason> {
        for price in self.order_type.prices().into_iter().flatten() {
            spec.admits_price(price)?;
        }
        spec.admits_qty(self.original_quantity)?;

        // Min-notional only applies where a price is known at ingress.
        // `Market` and `StopMarket` carry none — `limit_price()` returns
        // `None` for both — so their value is genuinely unknowable here.
        // Real venues substitute a reference price; this one skips, and says
        // so rather than pretending the rule was applied.
        //
        // Note also what is NOT enforced: a partial fill can leave a resting
        // remainder worth less than the floor. That dust is allowed to rest.
        // Cancelling it would make quantity vanish without a corresponding
        // fill, which is precisely what the conservation and depth-accounting
        // properties forbid — so it would be a much larger change than it
        // looks, and it belongs with a deliberate dust policy, not here.
        if let Some(price) = self.order_type.limit_price() {
            spec.admits_notional(price, self.original_quantity)?;
        }

        Ok(())
    }

    pub fn builder() -> OrderBuilder {
        OrderBuilder::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::qty;

    #[test]
    fn order_builder_preserves_defaults_and_overrides() {
        let builder = Order::builder()
            .exchange_id("exchange")
            .client_id(String::from("client"))
            .side(Side::Bid)
            .order_type(OrderType::Market)
            .quantity(qty(10));
        let timestamp = builder.timestamp.unwrap();
        let order = builder.clone().build().unwrap();
        assert_eq!(order.timestamp, timestamp);
        assert_eq!(order.original_quantity, qty(10));
        assert_eq!(order.remaining_quantity, qty(10));
        assert_eq!(order.lifecycle, OrderLifecycle::New);
        assert_eq!(order.arrival, 0);
        assert_eq!(order.exchange_id, ExchangeId("exchange".into()));
        assert_eq!(order.client_id, ClientId("client".into()));

        let order = builder
            .remaining_quantity(qty(4))
            .timestamp(123)
            .arrival(7)
            .lifecycle(OrderLifecycle::PartiallyFilled)
            .build()
            .unwrap();
        assert_eq!(order.original_quantity, qty(10));
        assert_eq!(order.remaining_quantity, qty(4));
        assert_eq!(order.timestamp, 123);
        assert_eq!(order.arrival, 7);
        assert_eq!(order.lifecycle, OrderLifecycle::PartiallyFilled);
    }

    #[test]
    fn order_builder_reports_missing_required_fields() {
        let complete = Order::builder()
            .exchange_id("exchange")
            .client_id("client")
            .side(Side::Bid)
            .order_type(OrderType::Market)
            .quantity(qty(10));
        for field in ["exchange_id", "client_id", "side", "order_type", "quantity"] {
            let mut builder = complete.clone();
            match field {
                "exchange_id" => builder.exchange_id = None,
                "client_id" => builder.client_id = None,
                "side" => builder.side = None,
                "order_type" => builder.order_type = None,
                "quantity" => builder.original_quantity = None,
                _ => unreachable!(),
            }
            assert!(
                matches!(builder.build(), Err(OrderValidationError::MissingField(name)) if name == field)
            );
        }
    }
}
