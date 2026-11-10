use order_book_core::instrument::InstrumentSpec;
use order_book_core::order::{Order, Side};
use order_book_core::order_type::OrderType;

/// Build a fresh GTC limit order for the workshop.
/// Prices are in minor units and quantities are in base units of `spec`.
pub fn limit_order(
    spec: &InstrumentSpec,
    side: Side,
    price: u64,
    quantity: u64,
    client_id: &str,
) -> Order {
    Order::builder()
        .side(side)
        .order_type(OrderType::limit_gtc(
            spec.price_from_minor(price)
                .expect("workshop price must be on the instrument's tick grid"),
        ))
        .quantity(
            spec.qty_from_base(quantity)
                .expect("workshop quantity must be on the instrument's lot grid"),
        )
        .client_id(client_id)
        // The builder requires an ID; submit replaces it with an engine-assigned one.
        .exchange_id("assigned-on-submit")
        .build()
}
