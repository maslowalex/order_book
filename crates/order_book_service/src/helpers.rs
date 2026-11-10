use order_book_core::instrument::InstrumentSpec;
use order_book_core::order::{Order, OrderValidationError, Side};
use order_book_core::order_input::{OrderInput, OrderKind};
use order_book_core::order_type::TimeInForce;

/// Build a fresh GTC limit order for the workshop.
/// Prices are in minor units and quantities are in base units of `spec`.
pub fn limit_order(
    spec: &InstrumentSpec,
    side: Side,
    price: u64,
    quantity: u64,
    client_id: &str,
) -> Result<Order, OrderValidationError> {
    OrderInput::builder()
        .kind(OrderKind::Limit)
        .side(side)
        .client_id(client_id)
        .quantity(quantity)
        .price(price)
        .tif(TimeInForce::GTC)
        .build()?
        .into_order(spec)
}
