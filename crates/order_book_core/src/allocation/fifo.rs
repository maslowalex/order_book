use super::{Fill, Maker, MatchingAlgorithm};
use crate::instrument::qty::Qty;
use std::cmp;

#[derive(Debug, Clone, Copy, Default)]
pub struct FifoMatcher;

#[hotpath::measure_all]
impl MatchingAlgorithm for FifoMatcher {
    fn allocate(&self, available: Qty, makers: &[Maker], _now: u128) -> Vec<Fill> {
        let mut fills: Vec<Fill> = vec![];
        let mut left: Qty = available;

        for (i, maker) in makers.iter().enumerate() {
            if left.is_zero() {
                break;
            }
            if maker.remaining_quantity.is_zero() {
                continue;
            }

            let fill_quantity = cmp::min(left, maker.remaining_quantity);

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
    use crate::allocation::test_helpers::*;

    // ---------------------------------------------------------------- FIFO

    #[test]
    fn empty_level_yields_no_fills() {
        let makers = level_of(&[]);
        let fills = FifoMatcher.allocate(qty(100), &makers, NOW);

        assert!(fills.is_empty());
        assert_contract(&fills, qty(100), &makers);
    }

    #[test]
    fn nothing_available_yields_no_fills() {
        let makers = level_of(&[10, 10]);
        let fills = FifoMatcher.allocate(qty(0), &makers, NOW);

        assert!(fills.is_empty());
        assert_contract(&fills, qty(0), &makers);
    }

    #[test]
    fn partial_fill_of_the_front_order() {
        let makers = level_of(&[10]);
        let fills = FifoMatcher.allocate(qty(4), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 4)]);
        assert_contract(&fills, qty(4), &makers);
    }

    /// Contract (2): consuming the front order EXACTLY must not emit a
    /// `{ order_index: 1, quantity: 0 }` for the untouched maker behind it.
    /// Self-trade prevention reads "was this order allocated to", so a zero
    /// fill would cancel a resting order the taker never reached.
    #[test]
    fn exact_boundary_does_not_emit_a_zero_fill() {
        let makers = level_of(&[10, 10]);
        let fills = FifoMatcher.allocate(qty(10), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 10)]);
        assert_contract(&fills, qty(10), &makers);
    }

    #[test]
    fn walks_the_queue_front_to_back() {
        let makers = level_of(&[10, 10, 10]);
        let fills = FifoMatcher.allocate(qty(25), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 10), fill(1, 10), fill(2, 5),]);
        assert_contract(&fills, qty(25), &makers);
    }

    /// Uneven sizes: the countdown must track what is actually left rather
    /// than assume a uniform level. `12 = 5 + 3 + 4`, so only the last maker
    /// is partial.
    #[test]
    fn uneven_quantities_drain_in_order() {
        let makers = level_of(&[5, 3, 7]);
        let fills = FifoMatcher.allocate(qty(12), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 5), fill(1, 3), fill(2, 4),]);
        assert_contract(&fills, qty(12), &makers);
    }

    /// Contract (4) caps at the level total: a taker bigger than the level
    /// takes 30, not the 100 it asked for. The engine leans on this to know
    /// the level is drained and it should move on to the next price.
    #[test]
    fn taker_larger_than_the_level_takes_everything() {
        let makers = level_of(&[10, 10, 10]);
        let fills = FifoMatcher.allocate(qty(100), &makers, NOW);

        assert_eq!(fills.len(), 3);
        assert_eq!(fills.iter().map(|f| f.quantity.base()).sum::<u64>(), 30);
        assert_contract(&fills, qty(100), &makers);
    }

    /// The case that separates the two zeros. At index 1 the maker is empty
    /// but `left` is still 5, so it must be SKIPPED, not read as "we're full".
    /// Breaking here returns 10 where the contract demands `min(15, 20) = 15`.
    /// It also pins that `order_index` indexes the slice, not the live orders
    /// within it — index 2, never index 1.
    #[test]
    fn skips_a_zero_remaining_order_in_the_middle() {
        let makers = level_of(&[10, 0, 10]);
        let fills = FifoMatcher.allocate(qty(15), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 10), fill(2, 5),]);
        assert_contract(&fills, qty(15), &makers);
    }

    /// Both zeros at once: the taker is full AND the trailing maker is empty.
    /// Either reason alone is enough to emit nothing for it.
    #[test]
    fn skips_a_trailing_zero_remaining_order() {
        let makers = level_of(&[10, 0]);
        let fills = FifoMatcher.allocate(qty(10), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 10)]);
        assert_contract(&fills, qty(10), &makers);
    }
}
