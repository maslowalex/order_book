/*
* Provides a pluggable implementations for actual matching logic, e.g.
* Deciding who at the price levels gets filled
*
* Contract (documented on the trait, enforced by debug_assert_fills,
* with total = Σ remaining_quantity):
* 1. order_index strictly increasing, each < orders.len()
* 2. quantity > 0 — no zero fills ("allocated" means "touched", and STP keys off exactly that)
* 3. quantity <= orders[order_index].remaining_quantity
* 4. Σ quantity == min(available, total)
*
*/

use crate::types::Order;
use std::cmp;

/// One resting order's share of an incoming taker, at a single price level.
/// Index-based, not id-based: `ExchangeId` is a `String`, so an id-carrying
/// `Fill` would heap-allocate per fill on the hot path AND force a second
/// linear scan to find the maker again. An index is `Copy`, free, and — the
/// 6.2a point — carries zero information about how orders are stored
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    pub order_index: usize,
    pub quantity: u64,
}

pub trait MatchingAlgorithm {
    fn allocate(&self, available: u64, orders: &[Order]) -> Vec<Fill>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FifoMatcher;

impl MatchingAlgorithm for FifoMatcher {
    fn allocate(&self, available: u64, orders: &[Order]) -> Vec<Fill> {
        let mut fills: Vec<Fill> = vec![];
        let mut left: u64 = available;

        for (i, order) in orders.iter().enumerate() {
            if left == 0 {
                break;
            }
            if order.remaining_quantity == 0 {
                continue;
            }

            let fill_quantity = cmp::min(left, order.remaining_quantity);

            let fill = Fill {
                order_index: i,
                quantity: fill_quantity,
            };

            left -= fill_quantity;
            fills.push(fill);
        }

        fills
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::order;
    use crate::types::Side;

    /// A price level's queue described only by what an allocator can see: each
    /// order's remaining quantity, in FIFO order. Side and price are arbitrary
    /// — `allocate` never looks at them.
    fn level_of(quantities: &[u64]) -> Vec<Order> {
        quantities
            .iter()
            .enumerate()
            .map(|(i, &qty)| order(Side::Ask, 100, qty, None, &format!("m{i}")))
            .collect()
    }

    /// The four trait rules, checked on every case. This grows up into
    /// `debug_assert_fills` in `matching.rs` once the engine allocates through
    /// the trait, so it is not throwaway — and it will police `ProRataMatcher`
    /// unchanged in step 2.
    fn assert_contract(fills: &[Fill], available: u64, orders: &[Order]) {
        let total: u128 = orders
            .iter()
            .map(|o| u128::from(o.remaining_quantity))
            .sum();

        let mut previous: Option<usize> = None;
        let mut allocated: u128 = 0;

        for fill in fills {
            assert!(
                previous.is_none_or(|p| fill.order_index > p),
                "(1) order_index must strictly increase, got {fills:?}"
            );
            assert!(
                fill.order_index < orders.len(),
                "(1) order_index {} out of range for a level of {}",
                fill.order_index,
                orders.len()
            );
            assert!(fill.quantity > 0, "(2) zero-quantity fill in {fills:?}");
            assert!(
                fill.quantity <= orders[fill.order_index].remaining_quantity,
                "(3) fill of {} over-fills maker {}, which has {} left",
                fill.quantity,
                fill.order_index,
                orders[fill.order_index].remaining_quantity
            );

            previous = Some(fill.order_index);
            allocated += u128::from(fill.quantity);
        }

        assert_eq!(
            allocated,
            u128::from(available).min(total),
            "(4) allocation must be exactly min(available, level total)"
        );
    }

    #[test]
    fn empty_level_yields_no_fills() {
        let orders = level_of(&[]);
        let fills = FifoMatcher.allocate(100, &orders);

        assert!(fills.is_empty());
        assert_contract(&fills, 100, &orders);
    }

    #[test]
    fn nothing_available_yields_no_fills() {
        let orders = level_of(&[10, 10]);
        let fills = FifoMatcher.allocate(0, &orders);

        assert!(fills.is_empty());
        assert_contract(&fills, 0, &orders);
    }

    #[test]
    fn partial_fill_of_the_front_order() {
        let orders = level_of(&[10]);
        let fills = FifoMatcher.allocate(4, &orders);

        assert_eq!(
            fills,
            vec![Fill {
                order_index: 0,
                quantity: 4
            }]
        );
        assert_contract(&fills, 4, &orders);
    }

    /// Contract (2): consuming the front order EXACTLY must not emit a
    /// `{ order_index: 1, quantity: 0 }` for the untouched maker behind it.
    /// Self-trade prevention reads "was this order allocated to", so a zero
    /// fill would cancel a resting order the taker never reached.
    #[test]
    fn exact_boundary_does_not_emit_a_zero_fill() {
        let orders = level_of(&[10, 10]);
        let fills = FifoMatcher.allocate(10, &orders);

        assert_eq!(
            fills,
            vec![Fill {
                order_index: 0,
                quantity: 10
            }]
        );
        assert_contract(&fills, 10, &orders);
    }

    #[test]
    fn walks_the_queue_front_to_back() {
        let orders = level_of(&[10, 10, 10]);
        let fills = FifoMatcher.allocate(25, &orders);

        assert_eq!(
            fills,
            vec![
                Fill {
                    order_index: 0,
                    quantity: 10
                },
                Fill {
                    order_index: 1,
                    quantity: 10
                },
                Fill {
                    order_index: 2,
                    quantity: 5
                },
            ]
        );
        assert_contract(&fills, 25, &orders);
    }

    /// Uneven sizes: the countdown must track what is actually left rather
    /// than assume a uniform level. `12 = 5 + 3 + 4`, so only the last maker
    /// is partial.
    #[test]
    fn uneven_quantities_drain_in_order() {
        let orders = level_of(&[5, 3, 7]);
        let fills = FifoMatcher.allocate(12, &orders);

        assert_eq!(
            fills,
            vec![
                Fill {
                    order_index: 0,
                    quantity: 5
                },
                Fill {
                    order_index: 1,
                    quantity: 3
                },
                Fill {
                    order_index: 2,
                    quantity: 4
                },
            ]
        );
        assert_contract(&fills, 12, &orders);
    }

    /// Contract (4) caps at the level total: a taker bigger than the level
    /// takes 30, not the 100 it asked for. The engine leans on this to know
    /// the level is drained and it should move on to the next price.
    #[test]
    fn taker_larger_than_the_level_takes_everything() {
        let orders = level_of(&[10, 10, 10]);
        let fills = FifoMatcher.allocate(100, &orders);

        assert_eq!(fills.len(), 3);
        assert_eq!(fills.iter().map(|f| f.quantity).sum::<u64>(), 30);
        assert_contract(&fills, 100, &orders);
    }

    /// The case that separates the two zeros. At index 1 the maker is empty
    /// but `left` is still 5, so it must be SKIPPED, not read as "we're full".
    /// Breaking here returns 10 where the contract demands `min(15, 20) = 15`.
    /// It also pins that `order_index` indexes the slice, not the live orders
    /// within it — index 2, never index 1.
    #[test]
    fn skips_a_zero_remaining_order_in_the_middle() {
        let orders = level_of(&[10, 0, 10]);
        let fills = FifoMatcher.allocate(15, &orders);

        assert_eq!(
            fills,
            vec![
                Fill {
                    order_index: 0,
                    quantity: 10
                },
                Fill {
                    order_index: 2,
                    quantity: 5
                },
            ]
        );
        assert_contract(&fills, 15, &orders);
    }

    /// Both zeros at once: the taker is full AND the trailing maker is empty.
    /// Either reason alone is enough to emit nothing for it.
    #[test]
    fn skips_a_trailing_zero_remaining_order() {
        let orders = level_of(&[10, 0]);
        let fills = FifoMatcher.allocate(10, &orders);

        assert_eq!(
            fills,
            vec![Fill {
                order_index: 0,
                quantity: 10
            }]
        );
        assert_contract(&fills, 10, &orders);
    }
}
