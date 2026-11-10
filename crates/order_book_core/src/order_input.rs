//! Typed, unvalidated order requests, independent of transport or serialization.

use derive_builder::Builder;

use crate::instrument::InstrumentSpec;
use crate::order::{Order, OrderValidationError, Side};
use crate::order_type::{OrderType, TimeInForce};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderKind {
    Market,
    Limit,
    StopMarket,
    StopLimit,
}

/// Raw prices are minor units; quantity is in base units of the supplied spec.
/// The builder checks field presence only. Call `into_order` for domain validation.
///
/// ```
/// use order_book_core::instrument::InstrumentSpec;
/// use order_book_core::order::{OrderValidationError, Side};
/// use order_book_core::order_input::{OrderInput, OrderKind};
/// use order_book_core::order_type::TimeInForce;
///
/// let order = OrderInput::builder()
///     .kind(OrderKind::Limit)
///     .side(Side::Bid)
///     .client_id("client")
///     .quantity(5)
///     .price(10_000)
///     .tif(TimeInForce::GTC)
///     .build()?
///     .into_order(&InstrumentSpec::cents())?;
/// assert_eq!(order.original_quantity.base(), 5);
/// # Ok::<(), OrderValidationError>(())
/// ```
#[derive(Debug, Clone, Builder)]
#[builder(
    pattern = "owned",
    derive(Debug, Clone),
    build_fn(error = "OrderValidationError")
)]
pub struct OrderInput {
    pub kind: OrderKind,
    pub side: Side,
    #[builder(setter(into))]
    pub client_id: String,
    pub quantity: u64,
    #[builder(default, setter(strip_option))]
    pub price: Option<u64>,
    #[builder(default, setter(strip_option))]
    pub trigger: Option<u64>,
    #[builder(default, setter(strip_option))]
    pub tif: Option<TimeInForce>,
}

impl OrderInput {
    pub fn builder() -> OrderInputBuilder {
        OrderInputBuilder::default()
    }

    /// Validate field combinations, tick/lot alignment, bounds, and minimum notional.
    ///
    /// Limits require price and TIF; stops require a trigger. Other fields are
    /// rejected when inapplicable. No price is inferred and no rounding is applied.
    /// Minimum notional applies only to limit and stop-limit orders, matching the
    /// book's policy. The exchange ID is a placeholder until `OrderBook::submit`.
    pub fn into_order(self, spec: &InstrumentSpec) -> Result<Order, OrderValidationError> {
        let price = |raw| {
            spec.price_from_minor(raw)
                .map_err(OrderValidationError::InvalidPrice)
        };
        let trigger = |raw| {
            spec.price_from_minor(raw)
                .map_err(OrderValidationError::InvalidTrigger)
        };

        // Valid patterns both check field presence and extract the raw values.
        // Error patterns report the first mismatch in price/trigger/TIF order.
        let order_type = match (self.kind, self.price, self.trigger, self.tif) {
            (OrderKind::Market, None, None, None) => OrderType::Market,
            (OrderKind::Limit, Some(raw_price), None, Some(tif)) => OrderType::Limit {
                price: price(raw_price)?,
                tif,
            },
            (OrderKind::StopMarket, None, Some(raw_trigger), None) => OrderType::StopMarket {
                trigger: trigger(raw_trigger)?,
            },
            (OrderKind::StopLimit, Some(raw_price), Some(raw_trigger), Some(tif)) => {
                OrderType::StopLimit {
                    price: price(raw_price)?,
                    trigger: trigger(raw_trigger)?,
                    tif,
                }
            }
            (OrderKind::Market | OrderKind::StopMarket, Some(_), _, _) => {
                return Err(OrderValidationError::UnexpectedField("price"));
            }
            (OrderKind::Limit | OrderKind::StopLimit, None, _, _) => {
                return Err(OrderValidationError::missing_field("price"));
            }
            (OrderKind::Market | OrderKind::Limit, _, Some(_), _) => {
                return Err(OrderValidationError::UnexpectedField("trigger"));
            }
            (OrderKind::StopMarket | OrderKind::StopLimit, _, None, _) => {
                return Err(OrderValidationError::missing_field("trigger"));
            }
            (OrderKind::Market | OrderKind::StopMarket, _, _, Some(_)) => {
                return Err(OrderValidationError::UnexpectedField("tif"));
            }
            (OrderKind::Limit | OrderKind::StopLimit, _, _, None) => {
                return Err(OrderValidationError::missing_field("tif"));
            }
        };
        let quantity = spec
            .qty_from_base(self.quantity)
            .map_err(OrderValidationError::InvalidQuantity)?;

        let order = Order::builder()
            .side(self.side)
            .client_id(self.client_id)
            .exchange_id("assigned-on-submit")
            .order_type(order_type)
            .quantity(quantity)
            .build()?;
        order
            .validate_admission(spec)
            .map_err(OrderValidationError::Rejected)?;
        Ok(order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allocation::fifo::FifoMatcher;
    use crate::instrument::{RejectReason, SpecError};
    use crate::order::OrderLifecycle;
    use crate::orderbook::OrderBook;
    use rust_decimal::Decimal;

    fn input(kind: OrderKind) -> OrderInput {
        OrderInput {
            kind,
            side: Side::Bid,
            client_id: "client".into(),
            quantity: 10,
            price: matches!(kind, OrderKind::Limit | OrderKind::StopLimit).then_some(100),
            trigger: matches!(kind, OrderKind::StopMarket | OrderKind::StopLimit).then_some(125),
            tif: matches!(kind, OrderKind::Limit | OrderKind::StopLimit)
                .then_some(TimeInForce::GTC),
        }
    }

    fn bounded_spec() -> InstrumentSpec {
        let spec = InstrumentSpec::new(2, 0, 5, 5).unwrap();
        spec.with_price_range(
            spec.price_from_minor(50).unwrap(),
            Some(spec.price_from_minor(200).unwrap()),
        )
        .unwrap()
        .with_qty_range(
            spec.qty_from_base(5).unwrap(),
            Some(spec.qty_from_base(20).unwrap()),
        )
        .unwrap()
        .with_min_notional(Decimal::new(10, 0))
        .unwrap()
    }

    #[test]
    fn all_kinds_sides_and_time_in_force_values_build_fresh_orders() {
        let spec = bounded_spec();
        for kind in [
            OrderKind::Market,
            OrderKind::Limit,
            OrderKind::StopMarket,
            OrderKind::StopLimit,
        ] {
            for side in [Side::Bid, Side::Ask] {
                for tif in [TimeInForce::GTC, TimeInForce::IOC, TimeInForce::FOK] {
                    let mut request = input(kind);
                    request.side = side;
                    if request.tif.is_some() {
                        request.tif = Some(tif);
                    }
                    let order = request.into_order(&spec).unwrap();
                    let price = spec.price_from_minor(100).unwrap();
                    let trigger = spec.price_from_minor(125).unwrap();
                    let expected = match kind {
                        OrderKind::Market => OrderType::Market,
                        OrderKind::Limit => OrderType::Limit { price, tif },
                        OrderKind::StopMarket => OrderType::StopMarket { trigger },
                        OrderKind::StopLimit => OrderType::StopLimit {
                            price,
                            trigger,
                            tif,
                        },
                    };
                    assert_eq!(order.order_type, expected);
                    assert_eq!(order.side, side);
                    assert_eq!(order.client_id.0, "client");
                    assert_eq!(order.original_quantity.base(), 10);
                    assert_eq!(order.remaining_quantity, order.original_quantity);
                    assert_eq!(order.lifecycle, OrderLifecycle::New);
                    assert_eq!(order.arrival, 0);
                    let mut book = OrderBook::new(spec, FifoMatcher);
                    assert!(book.submit(order).is_ok());
                }
            }
        }
    }

    #[test]
    fn field_combinations_are_checked_for_every_kind() {
        for kind in [
            OrderKind::Market,
            OrderKind::Limit,
            OrderKind::StopMarket,
            OrderKind::StopLimit,
        ] {
            let valid = input(kind);
            for price in [None, Some(100)] {
                for trigger in [None, Some(125)] {
                    for tif in [None, Some(TimeInForce::GTC)] {
                        let mut request = valid.clone();
                        request.price = price;
                        request.trigger = trigger;
                        request.tif = tif;
                        let result = request.into_order(&InstrumentSpec::cents());
                        let mismatch = [
                            ("price", price.is_some(), valid.price.is_some()),
                            ("trigger", trigger.is_some(), valid.trigger.is_some()),
                            ("tif", tif.is_some(), valid.tif.is_some()),
                        ]
                        .into_iter()
                        .find(|(_, present, required)| present != required);
                        match mismatch {
                            None => assert!(result.is_ok()),
                            Some((field, false, true)) => assert!(matches!(result,
                                Err(OrderValidationError::MissingField(name)) if name == field)),
                            Some((field, true, false)) => assert!(matches!(result,
                                Err(OrderValidationError::UnexpectedField(name)) if name == field)),
                            _ => unreachable!(),
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn conversion_preserves_tick_and_lot_errors() {
        let spec = bounded_spec();
        let mut request = input(OrderKind::Limit);
        request.price = Some(101);
        assert!(matches!(
            request.into_order(&spec),
            Err(OrderValidationError::InvalidPrice(
                SpecError::PriceOffTick { .. }
            ))
        ));
        let mut request = input(OrderKind::StopMarket);
        request.trigger = Some(126);
        assert!(matches!(
            request.into_order(&spec),
            Err(OrderValidationError::InvalidTrigger(
                SpecError::PriceOffTick { .. }
            ))
        ));
        let mut request = input(OrderKind::Market);
        request.quantity = 11;
        assert!(matches!(
            request.into_order(&spec),
            Err(OrderValidationError::InvalidQuantity(
                SpecError::QtyOffLot { .. }
            ))
        ));
    }

    #[test]
    fn conversion_checks_price_trigger_and_quantity_bounds() {
        let spec = bounded_spec();
        for kind in [
            OrderKind::Limit,
            OrderKind::StopMarket,
            OrderKind::StopLimit,
        ] {
            for raw in [0, 45, 205] {
                let mut request = input(kind);
                if kind == OrderKind::Limit {
                    request.price = Some(raw);
                } else {
                    request.trigger = Some(raw);
                }
                let result = request.into_order(&spec);
                if raw < 50 {
                    assert!(matches!(
                        result,
                        Err(OrderValidationError::Rejected(
                            RejectReason::PriceBelowMinimum { .. }
                        ))
                    ));
                } else {
                    assert!(matches!(
                        result,
                        Err(OrderValidationError::Rejected(
                            RejectReason::PriceAboveMaximum { .. }
                        ))
                    ));
                }
            }
        }
        for quantity in [0, 25] {
            let mut request = input(OrderKind::Market);
            request.quantity = quantity;
            let result = request.into_order(&spec);
            if quantity == 0 {
                assert!(matches!(
                    result,
                    Err(OrderValidationError::Rejected(
                        RejectReason::QuantityBelowMinimum { .. }
                    ))
                ));
            } else {
                assert!(matches!(
                    result,
                    Err(OrderValidationError::Rejected(
                        RejectReason::QuantityAboveMaximum { .. }
                    ))
                ));
            }
        }
    }

    #[test]
    fn notional_uses_limit_price_and_is_not_inferred_for_market_orders() {
        let spec = bounded_spec();
        for kind in [OrderKind::Limit, OrderKind::StopLimit] {
            let mut request = input(kind);
            request.price = Some(50); // 0.50 * 10 = 5, below the floor of 10.
            request.trigger = request.trigger.map(|_| 200); // trigger must not determine notional.
            assert!(matches!(
                request.into_order(&spec),
                Err(OrderValidationError::Rejected(
                    RejectReason::NotionalBelowMinimum { .. }
                ))
            ));
            assert!(input(kind).into_order(&spec).is_ok()); // exactly at the floor
        }
        for kind in [OrderKind::Market, OrderKind::StopMarket] {
            let mut request = input(kind);
            request.quantity = 5;
            assert!(request.into_order(&spec).is_ok());
        }
    }

    #[test]
    fn builder_defaults_optional_fields_and_returns_missing_field_errors() {
        let request = OrderInput::builder()
            .kind(OrderKind::Market)
            .side(Side::Ask)
            .client_id("client")
            .quantity(10)
            .build()
            .unwrap();
        assert_eq!(
            (request.price, request.trigger, request.tif),
            (None, None, None)
        );
        assert!(request.into_order(&InstrumentSpec::cents()).is_ok());
        let result = OrderInput::builder()
            .kind(OrderKind::Market)
            .side(Side::Ask)
            .client_id("client")
            .build();
        assert!(
            matches!(result, Err(OrderValidationError::MissingField(name)) if name == "quantity")
        );
    }
}
