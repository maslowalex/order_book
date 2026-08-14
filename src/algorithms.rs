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

/// Proportional allocation: each resting order gets `available * qty / total`
/// rounded DOWN, and the leftover crumbs go by time priority — front of the
/// queue first. The floor pass alone always under-allocates, so the remainder
/// pass is not a refinement, it is what makes contract (4) reachable at all.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProRataMatcher;

impl MatchingAlgorithm for ProRataMatcher {
    fn allocate(&self, available: u64, orders: &[Order]) -> Vec<Fill> {
        let total: u128 = orders
            .iter()
            .map(|o| u128::from(o.remaining_quantity))
            .sum();

        // Nothing to apportion: the taker swallows the level whole and every
        // maker gets its full remaining. This is also the only branch an empty
        // level can reach — `total == 0` makes the comparison trivially true,
        // which is why it has to come before any division by `total`.
        if u128::from(available) >= total {
            return orders
                .iter()
                .enumerate()
                .filter(|(_, o)| o.remaining_quantity > 0)
                .map(|(i, o)| Fill {
                    order_index: i,
                    quantity: o.remaining_quantity,
                })
                .collect();
        }

        // Floor pass. Below this line `available < total`, so every maker with
        // anything left floors STRICTLY below its own remaining — which is
        // what guarantees the remainder pass always has somewhere to put a
        // crumb, and why the two passes cannot be reordered.
        //
        // The product needs 128 bits. `available` and `remaining_quantity` are
        // both u64, so `available * qty` overflows u64 at sizes an exchange
        // sees routinely; `total` can overflow it before any multiply even
        // happens, from two makers alone.
        let mut shares: Vec<u64> = orders
            .iter()
            .map(|o| (u128::from(available) * u128::from(o.remaining_quantity) / total) as u64)
            .collect();

        // Remainder pass. Each floor above discarded strictly less than one
        // unit, and only makers holding something discard anything at all, so
        // `left` is strictly smaller than the number of such makers — a single
        // front-to-back sweep handing out one unit each places all of it, with
        // no need to wrap around. Time priority decides who eats the crumbs,
        // which is the whole reason pro-rata is not monotone in size.
        let mut left: u64 = available - shares.iter().sum::<u64>();

        for (i, order) in orders.iter().enumerate() {
            if left == 0 {
                break;
            }
            if shares[i] < order.remaining_quantity {
                shares[i] += 1;
                left -= 1;
            }
        }

        // Contract (2): a maker allocated nothing is ABSENT from the result,
        // not present with quantity 0 — downstream reads "appears in fills" as
        // "was touched", and the self-trade filter keys off exactly that.
        shares
            .iter()
            .enumerate()
            .filter(|&(_, &quantity)| quantity > 0)
            .map(|(i, &quantity)| Fill {
                order_index: i,
                quantity,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::order;
    use crate::types::Side;
    use proptest::prelude::*;
    use std::collections::HashMap;

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

    /// The pro-rata-specific bound: when `available < total`, every maker ends
    /// at `floor(available * qty / total)` or exactly one unit above it — the
    /// floor pass gives the first, the remainder sweep gives at most one more.
    /// A maker whose share is zero and who gets no crumb is ABSENT from
    /// `fills` rather than present with quantity 0, which the lookup default
    /// accounts for.
    fn assert_proportional(fills: &[Fill], available: u64, orders: &[Order]) {
        let total: u128 = orders
            .iter()
            .map(|o| u128::from(o.remaining_quantity))
            .sum();

        // `available >= total` is the take-everything branch — no proportional
        // arithmetic happens, so there is nothing to bound here.
        if u128::from(available) >= total {
            return;
        }

        let allocated: HashMap<usize, u64> =
            fills.iter().map(|f| (f.order_index, f.quantity)).collect();

        for (i, order) in orders.iter().enumerate() {
            let floor_share =
                (u128::from(available) * u128::from(order.remaining_quantity) / total) as u64;
            let got = allocated.get(&i).copied().unwrap_or(0);

            assert!(
                got == floor_share || got == floor_share + 1,
                "maker {i} (qty {}) got {got}, expected its floor share {floor_share} or one crumb more",
                order.remaining_quantity
            );
        }
    }

    // ---------------------------------------------------------------- FIFO

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

    // ------------------------------------------------------------ PRO-RATA

    /// The curriculum's worked example (LEARNING_PLAN.md 5.1): makers of
    /// 100/200/300, incoming 300 → 50/100/150. It divides evenly, so the
    /// remainder pass never runs — which is precisely why this example hides
    /// the entire difficulty of the algorithm.
    #[test]
    fn pro_rata_splits_the_textbook_example_evenly() {
        let orders = level_of(&[100, 200, 300]);
        let fills = ProRataMatcher.allocate(300, &orders);

        assert_eq!(
            fills,
            vec![
                Fill {
                    order_index: 0,
                    quantity: 50
                },
                Fill {
                    order_index: 1,
                    quantity: 100
                },
                Fill {
                    order_index: 2,
                    quantity: 150
                },
            ]
        );
        assert_contract(&fills, 300, &orders);
        assert_proportional(&fills, 300, &orders);
    }

    #[test]
    fn pro_rata_taker_larger_than_the_level_takes_everything() {
        let orders = level_of(&[10, 20, 30]);
        let fills = ProRataMatcher.allocate(1000, &orders);

        assert_eq!(
            fills,
            vec![
                Fill {
                    order_index: 0,
                    quantity: 10
                },
                Fill {
                    order_index: 1,
                    quantity: 20
                },
                Fill {
                    order_index: 2,
                    quantity: 30
                },
            ]
        );
        assert_contract(&fills, 1000, &orders);
    }

    /// `available == total` is the boundary of the take-everything branch:
    /// still no proportional arithmetic, and no crumbs.
    #[test]
    fn pro_rata_taker_exactly_the_level_takes_everything() {
        let orders = level_of(&[10, 20, 30]);
        let fills = ProRataMatcher.allocate(60, &orders);

        assert_eq!(fills.iter().map(|f| f.quantity).sum::<u64>(), 60);
        assert_eq!(fills.len(), 3);
        assert_contract(&fills, 60, &orders);
    }

    /// Ten across three equal makers: each share floors to 3, totalling 9, so
    /// one crumb is left over and time priority hands it to the front.
    #[test]
    fn pro_rata_hands_a_single_crumb_to_the_front() {
        let orders = level_of(&[10, 10, 10]);
        let fills = ProRataMatcher.allocate(10, &orders);

        assert_eq!(
            fills,
            vec![
                Fill {
                    order_index: 0,
                    quantity: 4
                },
                Fill {
                    order_index: 1,
                    quantity: 3
                },
                Fill {
                    order_index: 2,
                    quantity: 3
                },
            ]
        );
        assert_contract(&fills, 10, &orders);
        assert_proportional(&fills, 10, &orders);
    }

    /// Eleven across the same three: floors to 3 each again, but now there are
    /// TWO crumbs, so the front two each take one. Catches a remainder pass
    /// that places a single unit and stops.
    #[test]
    fn pro_rata_walks_the_queue_placing_every_crumb() {
        let orders = level_of(&[10, 10, 10]);
        let fills = ProRataMatcher.allocate(11, &orders);

        assert_eq!(
            fills,
            vec![
                Fill {
                    order_index: 0,
                    quantity: 4
                },
                Fill {
                    order_index: 1,
                    quantity: 4
                },
                Fill {
                    order_index: 2,
                    quantity: 3
                },
            ]
        );
        assert_contract(&fills, 11, &orders);
        assert_proportional(&fills, 11, &orders);
    }

    /// The counterexample to keep: `[3, 4]` taking 3 floors to `[1, 1]`, and
    /// time priority gives the crumb to the front — so the SMALLER maker walks
    /// away with more. Pro-rata is NOT monotone in size. Do not "fix" this;
    /// largest-fractional-part rounding buys an O(n log n) sort and a tiebreak
    /// rule, and still isn't monotone in general.
    #[test]
    fn pro_rata_is_not_monotone_in_size() {
        let orders = level_of(&[3, 4]);
        let fills = ProRataMatcher.allocate(3, &orders);

        assert_eq!(
            fills,
            vec![
                Fill {
                    order_index: 0,
                    quantity: 2
                },
                Fill {
                    order_index: 1,
                    quantity: 1
                },
            ]
        );
        assert_contract(&fills, 3, &orders);
        assert_proportional(&fills, 3, &orders);
    }

    /// A taker too small to give anyone a proportional unit: every share
    /// floors to zero and the lone crumb goes to the front, so pro-rata
    /// degenerates toward FIFO. Contract (2) means the two starved makers are
    /// ABSENT, not present with quantity 0 — step 3's self-trade filter
    /// cancels exactly the orders that appear here.
    #[test]
    fn pro_rata_degenerates_to_fifo_for_a_tiny_taker() {
        let orders = level_of(&[10, 10, 10]);
        let fills = ProRataMatcher.allocate(1, &orders);

        assert_eq!(
            fills,
            vec![Fill {
                order_index: 0,
                quantity: 1
            }]
        );
        assert_contract(&fills, 1, &orders);
        assert_proportional(&fills, 1, &orders);
    }

    /// Every share floors to zero and the crumbs alone decide the outcome:
    /// three units across four makers of 1 go to the front three, and the
    /// fourth is dropped by the `quantity > 0` filter.
    #[test]
    fn pro_rata_allocates_purely_from_the_remainder_pass() {
        let orders = level_of(&[1, 1, 1, 1]);
        let fills = ProRataMatcher.allocate(3, &orders);

        assert_eq!(
            fills,
            vec![
                Fill {
                    order_index: 0,
                    quantity: 1
                },
                Fill {
                    order_index: 1,
                    quantity: 1
                },
                Fill {
                    order_index: 2,
                    quantity: 1
                },
            ]
        );
        assert_contract(&fills, 3, &orders);
        assert_proportional(&fills, 3, &orders);
    }

    /// A zero-quantity maker adds nothing to `total`, floors to zero, has no
    /// headroom to take a crumb, and is filtered out — so index 2 keeps its
    /// slice position. Fifteen of a 20-deep level floors to `[7, 0, 7]` with
    /// one crumb to the front.
    #[test]
    fn pro_rata_skips_a_zero_remaining_order() {
        let orders = level_of(&[10, 0, 10]);
        let fills = ProRataMatcher.allocate(15, &orders);

        assert_eq!(
            fills,
            vec![
                Fill {
                    order_index: 0,
                    quantity: 8
                },
                Fill {
                    order_index: 2,
                    quantity: 7
                },
            ]
        );
        assert_contract(&fills, 15, &orders);
        assert_proportional(&fills, 15, &orders);
    }

    #[test]
    fn pro_rata_empty_level_yields_no_fills() {
        let orders = level_of(&[]);
        let fills = ProRataMatcher.allocate(100, &orders);

        assert!(fills.is_empty());
        assert_contract(&fills, 100, &orders);
    }

    /// `available == 0` is NOT the take-everything branch (0 < total), so this
    /// runs the floor pass for real and leans on the `quantity > 0` filter to
    /// come back empty.
    #[test]
    fn pro_rata_nothing_available_yields_no_fills() {
        let orders = level_of(&[10, 10]);
        let fills = ProRataMatcher.allocate(0, &orders);

        assert!(fills.is_empty());
        assert_contract(&fills, 0, &orders);
    }

    /// `available * qty` overflows `u64` long before the numbers get silly:
    /// 9e18 × 1e19 needs 128 bits. Do this arithmetic in `u64` and the test
    /// panics with "attempt to multiply with overflow" rather than failing an
    /// assertion. The shares here divide exactly — 5/9 and 4/9 of the taker —
    /// so no remainder pass is involved and only the width is under test.
    #[test]
    fn pro_rata_survives_products_that_overflow_u64() {
        let orders = level_of(&[10_000_000_000_000_000_000, 8_000_000_000_000_000_000]);
        let fills = ProRataMatcher.allocate(9_000_000_000_000_000_000, &orders);

        assert_eq!(
            fills,
            vec![
                Fill {
                    order_index: 0,
                    quantity: 5_000_000_000_000_000_000
                },
                Fill {
                    order_index: 1,
                    quantity: 4_000_000_000_000_000_000
                },
            ]
        );
        assert_contract(&fills, 9_000_000_000_000_000_000, &orders);
    }

    /// `total` itself overflows `u64`: two makers at `u64::MAX` sum past the
    /// type, so summing quantities in `u64` panics before any multiplication
    /// even happens. Each maker gets exactly half the taker.
    #[test]
    fn pro_rata_survives_a_level_total_that_overflows_u64() {
        let orders = level_of(&[u64::MAX, u64::MAX]);
        let fills = ProRataMatcher.allocate(100, &orders);

        assert_eq!(
            fills,
            vec![
                Fill {
                    order_index: 0,
                    quantity: 50
                },
                Fill {
                    order_index: 1,
                    quantity: 50
                },
            ]
        );
        assert_contract(&fills, 100, &orders);
    }

    // ------------------------------------------------------ BOTH MATCHERS

    proptest! {
        /// The contract belongs to the TRAIT, not to either implementation —
        /// so point it at both over random levels. A rounding slip, a lost
        /// crumb, or a countdown bug surfaces here as a contract violation
        /// instead of as a wrong-looking number in one hand-written case.
        ///
        /// This is `debug_assert_fills` in embryo: step 3 moves the same
        /// checks into the engine, where the whole existing test suite starts
        /// policing every allocation for free.
        #[test]
        fn both_matchers_honour_the_contract(
            available in 0u64..1_000_000,
            quantities in prop::collection::vec(0u64..10_000, 0..24),
        ) {
            let orders = level_of(&quantities);

            let fifo = FifoMatcher.allocate(available, &orders);
            assert_contract(&fifo, available, &orders);

            let pro_rata = ProRataMatcher.allocate(available, &orders);
            assert_contract(&pro_rata, available, &orders);
            assert_proportional(&pro_rata, available, &orders);
        }
    }
}
