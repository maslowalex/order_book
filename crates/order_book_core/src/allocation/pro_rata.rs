use super::{Fill, Maker, MatchingAlgorithm};
use crate::instrument::qty::Qty;

/// Proportional allocation: each resting order gets `available * qty / total`
/// rounded DOWN to a whole number of lots, and the leftover lots go by time
/// priority — front of the queue first. The floor pass alone always
/// under-allocates, so the remainder pass is not a refinement, it is what
/// makes contract (4) reachable at all.
///
/// # Why this one has to know the lot size
///
/// Floor division does not preserve divisibility, and the consequence is not a
/// rounding nicety — it is state corruption. Lot size `10`, makers holding
/// `[10, 20]`, taker bringing `10`:
///
/// ```text
/// f₁ = ⌊10·10/30⌋ = 3     f₂ = ⌊10·20/30⌋ = 6     Σ = 9
/// remainder 1 → front by time priority → fills [4, 6]
/// ```
///
/// Neither `4` nor `6` is a whole lot. Worse than a bad print: the engine
/// subtracts those fills from the makers, which are left resting at `6` and
/// `14` — permanently off-lot, sitting in the book, visible in `depth()`, and
/// available to be matched again. The lattice would leak, one partial fill at
/// a time, and no amount of care in the remainder pass repairs a floor pass
/// that already left the grid.
///
/// This is specific to pro-rata. FIFO takes `min(left, remaining)`, and the
/// minimum of two lot multiples is a lot multiple — FIFO is closed under the
/// lattice for free and needs to know nothing about lots. Pro-rata is the only
/// allocator in this crate that can leave the grid, so it is the only one that
/// has to be told where the grid is.
///
/// # Why the fix is exact rather than approximate
///
/// Write `available = L·a`, `qᵢ = L·cᵢ`, `total = L·C`, and floor each share to
/// a lot multiple: `fᵢ = L·⌊a·cᵢ/C⌋`. Then:
///
/// - every `fᵢ` is a lot multiple by construction;
/// - the shortfall `R = available − Σfᵢ` is a non-negative lot multiple, and is
///   smaller than `n·L` because each floor discards strictly less than one lot;
/// - every maker still holding something has at least one lot of headroom,
///   because `available < total` puts its share strictly below its own
///   remaining.
///
/// So a single front-to-back sweep handing out one lot at a time places exactly
/// `R`, needs no second pass, and leaves `Σ = min(available, total)` — in whole
/// lots. The `available ≥ total` branch gives every maker its full remaining,
/// which is a lot multiple already.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProRataMatcher {
    lot_size: u64,
}

impl ProRataMatcher {
    /// Takes the lot in base units — `InstrumentSpec::lot_size()`. There is no
    /// `Default`: a matcher that guessed `1` would silently produce off-lot
    /// fills on every instrument that is not unit-lot, and it would look right
    /// in every unit-lot test.
    pub fn new(lot_size: u64) -> Self {
        assert!(lot_size > 0, "lot_size must be non-zero");
        ProRataMatcher { lot_size }
    }
}

impl MatchingAlgorithm for ProRataMatcher {
    fn lot_size(&self) -> Option<u64> {
        Some(self.lot_size)
    }

    fn allocate(&self, available: Qty, makers: &[Maker], _now: u128) -> Vec<Fill> {
        // Unlike FIFO, which only ever compares and subtracts quantities, this
        // allocator genuinely multiplies and divides them — so it drops to raw
        // base units for the duration and puts the `Qty` wrapper back on at
        // the end. Widening is the whole point here, and `Qty` is a u64: doing
        // the arithmetic through the type would hide exactly the overflow this
        // code exists to avoid.
        let available = available.base();
        debug_assert!(
            available.is_multiple_of(self.lot_size),
            "a taker's quantity is a lot multiple by construction"
        );
        let total: u128 = makers
            .iter()
            .map(|m| u128::from(m.remaining_quantity.base()))
            .sum();

        // Nothing to apportion: the taker swallows the level whole and every
        // maker gets its full remaining. This is also the only branch an empty
        // level can reach — `total == 0` makes the comparison trivially true,
        // which is why it has to come before any division by `total`.
        if u128::from(available) >= total {
            return makers
                .iter()
                .enumerate()
                .filter(|(_, m)| !m.remaining_quantity.is_zero())
                .map(|(i, m)| Fill {
                    order_index: i,
                    quantity: m.remaining_quantity,
                })
                .collect();
        }

        // Floor pass. Below this line `available < total`, so every maker with
        // anything left floors STRICTLY below its own remaining — which is
        // what guarantees the remainder pass always has somewhere to put a
        // lot, and why the two passes cannot be reordered.
        //
        // The product needs 128 bits. `available` and `remaining_quantity` are
        // both u64, so `available * qty` overflows u64 at sizes an exchange
        // sees routinely; `total` can overflow it before any multiply even
        // happens, from two makers alone.
        //
        // The second division is the lot floor. Nesting the two is exact —
        // ⌊⌊x/m⌋/n⌋ == ⌊x/(m·n)⌋ over the non-negative integers — so this is
        // the `L·⌊a·cᵢ/C⌋` of the doc comment, written the way it reads.
        let lot = u128::from(self.lot_size);
        let mut shares: Vec<u64> = makers
            .iter()
            .map(|m| {
                let exact = u128::from(available) * u128::from(m.remaining_quantity.base()) / total;
                ((exact / lot) * lot) as u64
            })
            .collect();

        // Remainder pass. Each floor above discarded strictly less than one
        // LOT, and only makers holding something discard anything at all, so
        // `left` is under one lot per such maker — a single front-to-back
        // sweep handing out one lot each places all of it, with no need to
        // wrap around. Time priority decides who eats the crumbs, which is the
        // whole reason pro-rata is not monotone in size.
        //
        // Headroom is tested by subtraction rather than by `shares[i] + lot <=
        // remaining`: `shares[i]` can sit within a lot of `u64::MAX` on a level
        // that large, and the addition would overflow before the comparison
        // could reject it.
        let mut left: u64 = available - shares.iter().sum::<u64>();

        for (i, maker) in makers.iter().enumerate() {
            if left == 0 {
                break;
            }
            if maker.remaining_quantity.base() - shares[i] >= self.lot_size {
                shares[i] += self.lot_size;
                left -= self.lot_size;
            }
        }

        debug_assert_eq!(
            left, 0,
            "one sweep must place every leftover lot — see the headroom argument above"
        );

        // Contract (2): a maker allocated nothing is ABSENT from the result,
        // not present with quantity 0 — downstream reads "appears in fills" as
        // "was touched", and the self-trade filter keys off exactly that.
        shares
            .iter()
            .enumerate()
            .filter(|&(_, &quantity)| quantity > 0)
            .map(|(i, &quantity)| Fill {
                order_index: i,
                quantity: Qty::from_base_unchecked(quantity),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allocation::test_helpers::*;

    // ------------------------------------------------------------ PRO-RATA

    /// The curriculum's worked example (LEARNING_PLAN.md 5.1): makers of
    /// 100/200/300, incoming 300 → 50/100/150. It divides evenly, so the
    /// remainder pass never runs — which is precisely why this example hides
    /// the entire difficulty of the algorithm.
    #[test]
    fn pro_rata_splits_the_textbook_example_evenly() {
        let makers = level_of(&[100, 200, 300]);
        let fills = pro_rata().allocate(qty(300), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 50), fill(1, 100), fill(2, 150),]);
        assert_contract(&fills, qty(300), &makers);
        assert_proportional(&fills, qty(300), &makers, 1);
    }

    #[test]
    fn pro_rata_taker_larger_than_the_level_takes_everything() {
        let makers = level_of(&[10, 20, 30]);
        let fills = pro_rata().allocate(qty(1000), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 10), fill(1, 20), fill(2, 30),]);
        assert_contract(&fills, qty(1000), &makers);
    }

    /// `available == total` is the boundary of the take-everything branch:
    /// still no proportional arithmetic, and no crumbs.
    #[test]
    fn pro_rata_taker_exactly_the_level_takes_everything() {
        let makers = level_of(&[10, 20, 30]);
        let fills = pro_rata().allocate(qty(60), &makers, NOW);

        assert_eq!(fills.iter().map(|f| f.quantity.base()).sum::<u64>(), 60);
        assert_eq!(fills.len(), 3);
        assert_contract(&fills, qty(60), &makers);
    }

    /// Ten across three equal makers: each share floors to 3, totalling 9, so
    /// one crumb is left over and time priority hands it to the front.
    #[test]
    fn pro_rata_hands_a_single_crumb_to_the_front() {
        let makers = level_of(&[10, 10, 10]);
        let fills = pro_rata().allocate(qty(10), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 4), fill(1, 3), fill(2, 3),]);
        assert_contract(&fills, qty(10), &makers);
        assert_proportional(&fills, qty(10), &makers, 1);
    }

    /// Eleven across the same three: floors to 3 each again, but now there are
    /// TWO crumbs, so the front two each take one. Catches a remainder pass
    /// that places a single unit and stops.
    #[test]
    fn pro_rata_walks_the_queue_placing_every_crumb() {
        let makers = level_of(&[10, 10, 10]);
        let fills = pro_rata().allocate(qty(11), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 4), fill(1, 4), fill(2, 3),]);
        assert_contract(&fills, qty(11), &makers);
        assert_proportional(&fills, qty(11), &makers, 1);
    }

    /// The counterexample to keep: `[3, 4]` taking 3 floors to `[1, 1]`, and
    /// time priority gives the crumb to the front — so the SMALLER maker walks
    /// away with more. Pro-rata is NOT monotone in size. Do not "fix" this;
    /// largest-fractional-part rounding buys an O(n log n) sort and a tiebreak
    /// rule, and still isn't monotone in general.
    #[test]
    fn pro_rata_is_not_monotone_in_size() {
        let makers = level_of(&[3, 4]);
        let fills = pro_rata().allocate(qty(3), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 2), fill(1, 1),]);
        assert_contract(&fills, qty(3), &makers);
        assert_proportional(&fills, qty(3), &makers, 1);
    }

    /// A taker too small to give anyone a proportional unit: every share
    /// floors to zero and the lone crumb goes to the front, so pro-rata
    /// degenerates toward FIFO. Contract (2) means the two starved makers are
    /// ABSENT, not present with quantity 0 — step 3's self-trade filter
    /// cancels exactly the orders that appear here.
    #[test]
    fn pro_rata_degenerates_to_fifo_for_a_tiny_taker() {
        let makers = level_of(&[10, 10, 10]);
        let fills = pro_rata().allocate(qty(1), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 1)]);
        assert_contract(&fills, qty(1), &makers);
        assert_proportional(&fills, qty(1), &makers, 1);
    }

    /// Every share floors to zero and the crumbs alone decide the outcome:
    /// three units across four makers of 1 go to the front three, and the
    /// fourth is dropped by the `quantity > 0` filter.
    #[test]
    fn pro_rata_allocates_purely_from_the_remainder_pass() {
        let makers = level_of(&[1, 1, 1, 1]);
        let fills = pro_rata().allocate(qty(3), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 1), fill(1, 1), fill(2, 1),]);
        assert_contract(&fills, qty(3), &makers);
        assert_proportional(&fills, qty(3), &makers, 1);
    }

    /// A zero-quantity maker adds nothing to `total`, floors to zero, has no
    /// headroom to take a crumb, and is filtered out — so index 2 keeps its
    /// slice position. Fifteen of a 20-deep level floors to `[7, 0, 7]` with
    /// one crumb to the front.
    #[test]
    fn pro_rata_skips_a_zero_remaining_order() {
        let makers = level_of(&[10, 0, 10]);
        let fills = pro_rata().allocate(qty(15), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 8), fill(2, 7),]);
        assert_contract(&fills, qty(15), &makers);
        assert_proportional(&fills, qty(15), &makers, 1);
    }

    #[test]
    fn pro_rata_empty_level_yields_no_fills() {
        let makers = level_of(&[]);
        let fills = pro_rata().allocate(qty(100), &makers, NOW);

        assert!(fills.is_empty());
        assert_contract(&fills, qty(100), &makers);
    }

    /// `available == 0` is NOT the take-everything branch (0 < total), so this
    /// runs the floor pass for real and leans on the `quantity > 0` filter to
    /// come back empty.
    #[test]
    fn pro_rata_nothing_available_yields_no_fills() {
        let makers = level_of(&[10, 10]);
        let fills = pro_rata().allocate(qty(0), &makers, NOW);

        assert!(fills.is_empty());
        assert_contract(&fills, qty(0), &makers);
    }

    /// `available * qty` overflows `u64` long before the numbers get silly:
    /// 9e18 × 1e19 needs 128 bits. Do this arithmetic in `u64` and the test
    /// panics with "attempt to multiply with overflow" rather than failing an
    /// assertion. The shares here divide exactly — 5/9 and 4/9 of the taker —
    /// so no remainder pass is involved and only the width is under test.
    #[test]
    fn pro_rata_survives_products_that_overflow_u64() {
        let makers = level_of(&[10_000_000_000_000_000_000, 8_000_000_000_000_000_000]);
        let fills = pro_rata().allocate(qty(9_000_000_000_000_000_000), &makers, NOW);

        assert_eq!(
            fills,
            vec![
                fill(0, 5_000_000_000_000_000_000),
                fill(1, 4_000_000_000_000_000_000),
            ]
        );
        assert_contract(&fills, qty(9_000_000_000_000_000_000), &makers);
    }

    /// `total` itself overflows `u64`: two makers at `u64::MAX` sum past the
    /// type, so summing quantities in `u64` panics before any multiplication
    /// even happens. Each maker gets exactly half the taker.
    #[test]
    fn pro_rata_survives_a_level_total_that_overflows_u64() {
        let makers = level_of(&[u64::MAX, u64::MAX]);
        let fills = pro_rata().allocate(qty(100), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 50), fill(1, 50),]);
        assert_contract(&fills, qty(100), &makers);
    }

    // -------------------------------------------------------- PRO-RATA LOTS

    /// The counterexample from `ProRataMatcher`'s doc comment, executable.
    ///
    /// A lot-blind allocator produces `[4, 6]` here — the naive floor shares
    /// are 3 and 6, and the single leftover unit goes to the front. Both are
    /// off-lot, and the engine would then leave the makers resting at 6 and 14,
    /// off-lot too. The lot-aware answer floors both shares to zero and hands
    /// the whole leftover lot to the front by time priority.
    #[test]
    fn pro_rata_never_splits_a_lot() {
        let makers = level_of(&[10, 20]);
        let fills = ProRataMatcher::new(10).allocate(qty(10), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 10)]);
        assert_contract(&fills, qty(10), &makers);
        assert_proportional(&fills, qty(10), &makers, 10);
    }

    /// The lot floor bites before the crumbs do: 5 lots of 10 across makers of
    /// 10/20/30 gives exact shares 8.3/16.6/25, which floor to 0/10/20 — so 20
    /// of the 50 is still unplaced and the sweep walks the queue handing out
    /// one lot each.
    #[test]
    fn pro_rata_walks_the_queue_placing_whole_lots() {
        let makers = level_of(&[10, 20, 30]);
        let fills = ProRataMatcher::new(10).allocate(qty(50), &makers, NOW);

        assert_eq!(
            fills,
            vec![fill(0, 10), fill(1, 20), fill(2, 20)],
            "shares floor to 0/10/20, then one lot each to the front three"
        );
        assert_contract(&fills, qty(50), &makers);
        assert_proportional(&fills, qty(50), &makers, 10);
    }

    /// A taker smaller than one lot cannot exist — `Qty` cannot hold it — so
    /// the smallest real case is exactly one lot, and pro-rata degenerates to
    /// giving it to the front of the queue.
    #[test]
    fn pro_rata_gives_a_lone_lot_to_the_front() {
        let makers = level_of(&[100, 100, 100]);
        let fills = ProRataMatcher::new(100).allocate(qty(100), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 100)]);
        assert_contract(&fills, qty(100), &makers);
    }

    /// Taking everything needs no lot arithmetic at all: each maker's full
    /// remaining is already a whole number of lots.
    #[test]
    fn pro_rata_takes_the_whole_level_in_lots() {
        let makers = level_of(&[10, 20, 30]);
        let fills = ProRataMatcher::new(10).allocate(qty(1000), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 10), fill(1, 20), fill(2, 30)]);
        assert_contract(&fills, qty(1000), &makers);
    }

    /// A lot big enough to swallow every proportional share leaves the
    /// remainder sweep as the only mechanism that allocates anything — the
    /// lot-scale echo of `pro_rata_allocates_purely_from_the_remainder_pass`.
    #[test]
    fn pro_rata_allocates_purely_from_the_lot_sweep() {
        let makers = level_of(&[500, 500, 500, 500]);
        let fills = ProRataMatcher::new(500).allocate(qty(1500), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 500), fill(1, 500), fill(2, 500)]);
        assert_contract(&fills, qty(1500), &makers);
    }

    #[test]
    #[should_panic(expected = "lot_size must be non-zero")]
    fn a_zero_lot_matcher_is_rejected_loudly() {
        ProRataMatcher::new(0);
    }
}
