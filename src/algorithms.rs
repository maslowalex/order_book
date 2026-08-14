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
* 5. every quantity is a whole number of lots
*
* (5) needs no runtime check and gets none. Every input is a lot multiple
* because `Qty` cannot hold anything else, and both allocators preserve that:
* FIFO because the minimum of two multiples is a multiple, pro-rata because it
* is told the lot and floors to it. The clause is written down anyway, because
* the one algorithm that could break it looks correct without it — see the
* counterexample on `ProRataMatcher`.
*/

use crate::instrument::Qty;
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
    pub quantity: Qty,
}

pub trait MatchingAlgorithm {
    fn allocate(&self, available: Qty, orders: &[Order]) -> Vec<Fill>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FifoMatcher;

impl MatchingAlgorithm for FifoMatcher {
    fn allocate(&self, available: Qty, orders: &[Order]) -> Vec<Fill> {
        let mut fills: Vec<Fill> = vec![];
        let mut left: Qty = available;

        for (i, order) in orders.iter().enumerate() {
            if left.is_zero() {
                break;
            }
            if order.remaining_quantity.is_zero() {
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
    fn allocate(&self, available: Qty, orders: &[Order]) -> Vec<Fill> {
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
        let total: u128 = orders
            .iter()
            .map(|o| u128::from(o.remaining_quantity.base()))
            .sum();

        // Nothing to apportion: the taker swallows the level whole and every
        // maker gets its full remaining. This is also the only branch an empty
        // level can reach — `total == 0` makes the comparison trivially true,
        // which is why it has to come before any division by `total`.
        if u128::from(available) >= total {
            return orders
                .iter()
                .enumerate()
                .filter(|(_, o)| !o.remaining_quantity.is_zero())
                .map(|(i, o)| Fill {
                    order_index: i,
                    quantity: o.remaining_quantity,
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
        let mut shares: Vec<u64> = orders
            .iter()
            .map(|o| {
                let exact = u128::from(available) * u128::from(o.remaining_quantity.base()) / total;
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

        for (i, order) in orders.iter().enumerate() {
            if left == 0 {
                break;
            }
            if order.remaining_quantity.base() - shares[i] >= self.lot_size {
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
    use crate::test_helpers::order;
    use crate::types::Side;
    use proptest::prelude::*;
    use std::collections::HashMap;

    /// Base units as a `Qty`. The allocator's unit is whatever the instrument
    /// says a lot is; these tests run on a unit lot, so base units and lots
    /// coincide and the numbers below read the same as they always did.
    fn qty(n: u64) -> Qty {
        Qty::from_base_unchecked(n)
    }

    /// The matcher the pre-lot cases run against. Unit lot, so every
    /// quantity is a whole lot and the numbers read exactly as before.
    fn pro_rata() -> ProRataMatcher {
        ProRataMatcher::new(1)
    }

    fn fill(order_index: usize, quantity: u64) -> Fill {
        Fill {
            order_index,
            quantity: qty(quantity),
        }
    }

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
    fn assert_contract(fills: &[Fill], available: Qty, orders: &[Order]) {
        let total: u128 = orders
            .iter()
            .map(|o| u128::from(o.remaining_quantity.base()))
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
            assert!(
                !fill.quantity.is_zero(),
                "(2) zero-quantity fill in {fills:?}"
            );
            assert!(
                fill.quantity <= orders[fill.order_index].remaining_quantity,
                "(3) fill of {:?} over-fills maker {}, which has {:?} left",
                fill.quantity,
                fill.order_index,
                orders[fill.order_index].remaining_quantity
            );

            previous = Some(fill.order_index);
            allocated += u128::from(fill.quantity.base());
        }

        assert_eq!(
            allocated,
            u128::from(available.base()).min(total),
            "(4) allocation must be exactly min(available, level total)"
        );
    }

    /// The pro-rata-specific bound: when `available < total`, every maker ends
    /// at `floor(available * qty / total)` or exactly one unit above it — the
    /// floor pass gives the first, the remainder sweep gives at most one more.
    /// A maker whose share is zero and who gets no crumb is ABSENT from
    /// `fills` rather than present with quantity 0, which the lookup default
    /// accounts for.
    fn assert_proportional(fills: &[Fill], available: Qty, orders: &[Order], lot: u64) {
        let available = available.base();
        let total: u128 = orders
            .iter()
            .map(|o| u128::from(o.remaining_quantity.base()))
            .sum();

        // `available >= total` is the take-everything branch — no proportional
        // arithmetic happens, so there is nothing to bound here.
        if u128::from(available) >= total {
            return;
        }

        let allocated: HashMap<usize, u64> = fills
            .iter()
            .map(|f| (f.order_index, f.quantity.base()))
            .collect();

        for (i, order) in orders.iter().enumerate() {
            let exact = u128::from(available) * u128::from(order.remaining_quantity.base()) / total;
            let floor_share = ((exact / u128::from(lot)) * u128::from(lot)) as u64;
            let got = allocated.get(&i).copied().unwrap_or(0);

            assert!(
                got == floor_share || got == floor_share + lot,
                "maker {i} (qty {:?}) got {got}, expected its lot-floored share {floor_share} or one lot more",
                order.remaining_quantity
            );
            assert!(
                got.is_multiple_of(lot),
                "(5) maker {i} got {got}, which is not a whole number of {lot}-unit lots"
            );
        }
    }

    // ---------------------------------------------------------------- FIFO

    #[test]
    fn empty_level_yields_no_fills() {
        let orders = level_of(&[]);
        let fills = FifoMatcher.allocate(qty(100), &orders);

        assert!(fills.is_empty());
        assert_contract(&fills, qty(100), &orders);
    }

    #[test]
    fn nothing_available_yields_no_fills() {
        let orders = level_of(&[10, 10]);
        let fills = FifoMatcher.allocate(qty(0), &orders);

        assert!(fills.is_empty());
        assert_contract(&fills, qty(0), &orders);
    }

    #[test]
    fn partial_fill_of_the_front_order() {
        let orders = level_of(&[10]);
        let fills = FifoMatcher.allocate(qty(4), &orders);

        assert_eq!(fills, vec![fill(0, 4)]);
        assert_contract(&fills, qty(4), &orders);
    }

    /// Contract (2): consuming the front order EXACTLY must not emit a
    /// `{ order_index: 1, quantity: 0 }` for the untouched maker behind it.
    /// Self-trade prevention reads "was this order allocated to", so a zero
    /// fill would cancel a resting order the taker never reached.
    #[test]
    fn exact_boundary_does_not_emit_a_zero_fill() {
        let orders = level_of(&[10, 10]);
        let fills = FifoMatcher.allocate(qty(10), &orders);

        assert_eq!(fills, vec![fill(0, 10)]);
        assert_contract(&fills, qty(10), &orders);
    }

    #[test]
    fn walks_the_queue_front_to_back() {
        let orders = level_of(&[10, 10, 10]);
        let fills = FifoMatcher.allocate(qty(25), &orders);

        assert_eq!(fills, vec![fill(0, 10), fill(1, 10), fill(2, 5),]);
        assert_contract(&fills, qty(25), &orders);
    }

    /// Uneven sizes: the countdown must track what is actually left rather
    /// than assume a uniform level. `12 = 5 + 3 + 4`, so only the last maker
    /// is partial.
    #[test]
    fn uneven_quantities_drain_in_order() {
        let orders = level_of(&[5, 3, 7]);
        let fills = FifoMatcher.allocate(qty(12), &orders);

        assert_eq!(fills, vec![fill(0, 5), fill(1, 3), fill(2, 4),]);
        assert_contract(&fills, qty(12), &orders);
    }

    /// Contract (4) caps at the level total: a taker bigger than the level
    /// takes 30, not the 100 it asked for. The engine leans on this to know
    /// the level is drained and it should move on to the next price.
    #[test]
    fn taker_larger_than_the_level_takes_everything() {
        let orders = level_of(&[10, 10, 10]);
        let fills = FifoMatcher.allocate(qty(100), &orders);

        assert_eq!(fills.len(), 3);
        assert_eq!(fills.iter().map(|f| f.quantity.base()).sum::<u64>(), 30);
        assert_contract(&fills, qty(100), &orders);
    }

    /// The case that separates the two zeros. At index 1 the maker is empty
    /// but `left` is still 5, so it must be SKIPPED, not read as "we're full".
    /// Breaking here returns 10 where the contract demands `min(15, 20) = 15`.
    /// It also pins that `order_index` indexes the slice, not the live orders
    /// within it — index 2, never index 1.
    #[test]
    fn skips_a_zero_remaining_order_in_the_middle() {
        let orders = level_of(&[10, 0, 10]);
        let fills = FifoMatcher.allocate(qty(15), &orders);

        assert_eq!(fills, vec![fill(0, 10), fill(2, 5),]);
        assert_contract(&fills, qty(15), &orders);
    }

    /// Both zeros at once: the taker is full AND the trailing maker is empty.
    /// Either reason alone is enough to emit nothing for it.
    #[test]
    fn skips_a_trailing_zero_remaining_order() {
        let orders = level_of(&[10, 0]);
        let fills = FifoMatcher.allocate(qty(10), &orders);

        assert_eq!(fills, vec![fill(0, 10)]);
        assert_contract(&fills, qty(10), &orders);
    }

    // ------------------------------------------------------------ PRO-RATA

    /// The curriculum's worked example (LEARNING_PLAN.md 5.1): makers of
    /// 100/200/300, incoming 300 → 50/100/150. It divides evenly, so the
    /// remainder pass never runs — which is precisely why this example hides
    /// the entire difficulty of the algorithm.
    #[test]
    fn pro_rata_splits_the_textbook_example_evenly() {
        let orders = level_of(&[100, 200, 300]);
        let fills = pro_rata().allocate(qty(300), &orders);

        assert_eq!(fills, vec![fill(0, 50), fill(1, 100), fill(2, 150),]);
        assert_contract(&fills, qty(300), &orders);
        assert_proportional(&fills, qty(300), &orders, 1);
    }

    #[test]
    fn pro_rata_taker_larger_than_the_level_takes_everything() {
        let orders = level_of(&[10, 20, 30]);
        let fills = pro_rata().allocate(qty(1000), &orders);

        assert_eq!(fills, vec![fill(0, 10), fill(1, 20), fill(2, 30),]);
        assert_contract(&fills, qty(1000), &orders);
    }

    /// `available == total` is the boundary of the take-everything branch:
    /// still no proportional arithmetic, and no crumbs.
    #[test]
    fn pro_rata_taker_exactly_the_level_takes_everything() {
        let orders = level_of(&[10, 20, 30]);
        let fills = pro_rata().allocate(qty(60), &orders);

        assert_eq!(fills.iter().map(|f| f.quantity.base()).sum::<u64>(), 60);
        assert_eq!(fills.len(), 3);
        assert_contract(&fills, qty(60), &orders);
    }

    /// Ten across three equal makers: each share floors to 3, totalling 9, so
    /// one crumb is left over and time priority hands it to the front.
    #[test]
    fn pro_rata_hands_a_single_crumb_to_the_front() {
        let orders = level_of(&[10, 10, 10]);
        let fills = pro_rata().allocate(qty(10), &orders);

        assert_eq!(fills, vec![fill(0, 4), fill(1, 3), fill(2, 3),]);
        assert_contract(&fills, qty(10), &orders);
        assert_proportional(&fills, qty(10), &orders, 1);
    }

    /// Eleven across the same three: floors to 3 each again, but now there are
    /// TWO crumbs, so the front two each take one. Catches a remainder pass
    /// that places a single unit and stops.
    #[test]
    fn pro_rata_walks_the_queue_placing_every_crumb() {
        let orders = level_of(&[10, 10, 10]);
        let fills = pro_rata().allocate(qty(11), &orders);

        assert_eq!(fills, vec![fill(0, 4), fill(1, 4), fill(2, 3),]);
        assert_contract(&fills, qty(11), &orders);
        assert_proportional(&fills, qty(11), &orders, 1);
    }

    /// The counterexample to keep: `[3, 4]` taking 3 floors to `[1, 1]`, and
    /// time priority gives the crumb to the front — so the SMALLER maker walks
    /// away with more. Pro-rata is NOT monotone in size. Do not "fix" this;
    /// largest-fractional-part rounding buys an O(n log n) sort and a tiebreak
    /// rule, and still isn't monotone in general.
    #[test]
    fn pro_rata_is_not_monotone_in_size() {
        let orders = level_of(&[3, 4]);
        let fills = pro_rata().allocate(qty(3), &orders);

        assert_eq!(fills, vec![fill(0, 2), fill(1, 1),]);
        assert_contract(&fills, qty(3), &orders);
        assert_proportional(&fills, qty(3), &orders, 1);
    }

    /// A taker too small to give anyone a proportional unit: every share
    /// floors to zero and the lone crumb goes to the front, so pro-rata
    /// degenerates toward FIFO. Contract (2) means the two starved makers are
    /// ABSENT, not present with quantity 0 — step 3's self-trade filter
    /// cancels exactly the orders that appear here.
    #[test]
    fn pro_rata_degenerates_to_fifo_for_a_tiny_taker() {
        let orders = level_of(&[10, 10, 10]);
        let fills = pro_rata().allocate(qty(1), &orders);

        assert_eq!(fills, vec![fill(0, 1)]);
        assert_contract(&fills, qty(1), &orders);
        assert_proportional(&fills, qty(1), &orders, 1);
    }

    /// Every share floors to zero and the crumbs alone decide the outcome:
    /// three units across four makers of 1 go to the front three, and the
    /// fourth is dropped by the `quantity > 0` filter.
    #[test]
    fn pro_rata_allocates_purely_from_the_remainder_pass() {
        let orders = level_of(&[1, 1, 1, 1]);
        let fills = pro_rata().allocate(qty(3), &orders);

        assert_eq!(fills, vec![fill(0, 1), fill(1, 1), fill(2, 1),]);
        assert_contract(&fills, qty(3), &orders);
        assert_proportional(&fills, qty(3), &orders, 1);
    }

    /// A zero-quantity maker adds nothing to `total`, floors to zero, has no
    /// headroom to take a crumb, and is filtered out — so index 2 keeps its
    /// slice position. Fifteen of a 20-deep level floors to `[7, 0, 7]` with
    /// one crumb to the front.
    #[test]
    fn pro_rata_skips_a_zero_remaining_order() {
        let orders = level_of(&[10, 0, 10]);
        let fills = pro_rata().allocate(qty(15), &orders);

        assert_eq!(fills, vec![fill(0, 8), fill(2, 7),]);
        assert_contract(&fills, qty(15), &orders);
        assert_proportional(&fills, qty(15), &orders, 1);
    }

    #[test]
    fn pro_rata_empty_level_yields_no_fills() {
        let orders = level_of(&[]);
        let fills = pro_rata().allocate(qty(100), &orders);

        assert!(fills.is_empty());
        assert_contract(&fills, qty(100), &orders);
    }

    /// `available == 0` is NOT the take-everything branch (0 < total), so this
    /// runs the floor pass for real and leans on the `quantity > 0` filter to
    /// come back empty.
    #[test]
    fn pro_rata_nothing_available_yields_no_fills() {
        let orders = level_of(&[10, 10]);
        let fills = pro_rata().allocate(qty(0), &orders);

        assert!(fills.is_empty());
        assert_contract(&fills, qty(0), &orders);
    }

    /// `available * qty` overflows `u64` long before the numbers get silly:
    /// 9e18 × 1e19 needs 128 bits. Do this arithmetic in `u64` and the test
    /// panics with "attempt to multiply with overflow" rather than failing an
    /// assertion. The shares here divide exactly — 5/9 and 4/9 of the taker —
    /// so no remainder pass is involved and only the width is under test.
    #[test]
    fn pro_rata_survives_products_that_overflow_u64() {
        let orders = level_of(&[10_000_000_000_000_000_000, 8_000_000_000_000_000_000]);
        let fills = pro_rata().allocate(qty(9_000_000_000_000_000_000), &orders);

        assert_eq!(
            fills,
            vec![
                fill(0, 5_000_000_000_000_000_000),
                fill(1, 4_000_000_000_000_000_000),
            ]
        );
        assert_contract(&fills, qty(9_000_000_000_000_000_000), &orders);
    }

    /// `total` itself overflows `u64`: two makers at `u64::MAX` sum past the
    /// type, so summing quantities in `u64` panics before any multiplication
    /// even happens. Each maker gets exactly half the taker.
    #[test]
    fn pro_rata_survives_a_level_total_that_overflows_u64() {
        let orders = level_of(&[u64::MAX, u64::MAX]);
        let fills = pro_rata().allocate(qty(100), &orders);

        assert_eq!(fills, vec![fill(0, 50), fill(1, 50),]);
        assert_contract(&fills, qty(100), &orders);
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
        let orders = level_of(&[10, 20]);
        let fills = ProRataMatcher::new(10).allocate(qty(10), &orders);

        assert_eq!(fills, vec![fill(0, 10)]);
        assert_contract(&fills, qty(10), &orders);
        assert_proportional(&fills, qty(10), &orders, 10);
    }

    /// The lot floor bites before the crumbs do: 5 lots of 10 across makers of
    /// 10/20/30 gives exact shares 8.3/16.6/25, which floor to 0/10/20 — so 20
    /// of the 50 is still unplaced and the sweep walks the queue handing out
    /// one lot each.
    #[test]
    fn pro_rata_walks_the_queue_placing_whole_lots() {
        let orders = level_of(&[10, 20, 30]);
        let fills = ProRataMatcher::new(10).allocate(qty(50), &orders);

        assert_eq!(
            fills,
            vec![fill(0, 10), fill(1, 20), fill(2, 20)],
            "shares floor to 0/10/20, then one lot each to the front three"
        );
        assert_contract(&fills, qty(50), &orders);
        assert_proportional(&fills, qty(50), &orders, 10);
    }

    /// A taker smaller than one lot cannot exist — `Qty` cannot hold it — so
    /// the smallest real case is exactly one lot, and pro-rata degenerates to
    /// giving it to the front of the queue.
    #[test]
    fn pro_rata_gives_a_lone_lot_to_the_front() {
        let orders = level_of(&[100, 100, 100]);
        let fills = ProRataMatcher::new(100).allocate(qty(100), &orders);

        assert_eq!(fills, vec![fill(0, 100)]);
        assert_contract(&fills, qty(100), &orders);
    }

    /// Taking everything needs no lot arithmetic at all: each maker's full
    /// remaining is already a whole number of lots.
    #[test]
    fn pro_rata_takes_the_whole_level_in_lots() {
        let orders = level_of(&[10, 20, 30]);
        let fills = ProRataMatcher::new(10).allocate(qty(1000), &orders);

        assert_eq!(fills, vec![fill(0, 10), fill(1, 20), fill(2, 30)]);
        assert_contract(&fills, qty(1000), &orders);
    }

    /// A lot big enough to swallow every proportional share leaves the
    /// remainder sweep as the only mechanism that allocates anything — the
    /// lot-scale echo of `pro_rata_allocates_purely_from_the_remainder_pass`.
    #[test]
    fn pro_rata_allocates_purely_from_the_lot_sweep() {
        let orders = level_of(&[500, 500, 500, 500]);
        let fills = ProRataMatcher::new(500).allocate(qty(1500), &orders);

        assert_eq!(fills, vec![fill(0, 500), fill(1, 500), fill(2, 500)]);
        assert_contract(&fills, qty(1500), &orders);
    }

    #[test]
    #[should_panic(expected = "lot_size must be non-zero")]
    fn a_zero_lot_matcher_is_rejected_loudly() {
        ProRataMatcher::new(0);
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

            let available = qty(available);

            let fifo = FifoMatcher.allocate(available, &orders);
            assert_contract(&fifo, available, &orders);

            let pro_rata = pro_rata().allocate(available, &orders);
            assert_contract(&pro_rata, available, &orders);
            assert_proportional(&pro_rata, available, &orders, 1);
        }

        /// The same contract on a level where lots actually bind. Every input
        /// is scaled to a multiple of `lot`, exactly as an `InstrumentSpec`
        /// would have produced it — so a fill that is not a whole lot is a
        /// real defect and not an artefact of the generator.
        ///
        /// Running only the unit-lot case above would have made clause (5)
        /// vacuous: with `lot == 1` every integer is a whole lot, and a
        /// pro-rata that ignored lots entirely would pass.
        #[test]
        fn pro_rata_honours_the_contract_on_a_non_unit_lot(
            available_lots in 0u64..100_000,
            lots in prop::collection::vec(0u64..1_000, 0..24),
            lot in prop::sample::select(vec![2u64, 5, 10, 100]),
        ) {
            let quantities: Vec<u64> = lots.iter().map(|c| c * lot).collect();
            let orders = level_of(&quantities);
            let available = qty(available_lots * lot);

            let fills = ProRataMatcher::new(lot).allocate(available, &orders);

            assert_contract(&fills, available, &orders);
            assert_proportional(&fills, available, &orders, lot);
        }

        /// FIFO needs no lot parameter, and this is why: `min(left, remaining)`
        /// of two lot multiples is a lot multiple, so the grid survives without
        /// the allocator knowing it exists.
        #[test]
        fn fifo_stays_on_the_lot_grid_without_being_told_about_it(
            available_lots in 0u64..100_000,
            lots in prop::collection::vec(0u64..1_000, 0..24),
            lot in prop::sample::select(vec![2u64, 5, 10, 100]),
        ) {
            let quantities: Vec<u64> = lots.iter().map(|c| c * lot).collect();
            let orders = level_of(&quantities);
            let available = qty(available_lots * lot);

            let fills = FifoMatcher.allocate(available, &orders);

            assert_contract(&fills, available, &orders);
            for f in &fills {
                prop_assert!(f.quantity.base().is_multiple_of(lot));
            }
        }
    }
}
