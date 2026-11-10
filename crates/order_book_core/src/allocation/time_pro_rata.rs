use super::{Fill, Maker, MatchingAlgorithm};
use crate::instrument::qty::Qty;

/// Time-weighted pro-rata: a maker's share scales with its size **and** with
/// how long it has been resting, so liquidity that has been standing there
/// taking risk outranks liquidity that arrived one order ago with the same
/// size. Pro-rata is the special case where every maker has rested equally
/// long, and the property tests assert exactly that.
///
/// # Why this one needs capping and pro-rata does not
///
/// Pro-rata's weight IS its size, so below the take-everything branch
/// (`available < total`) every share lands strictly under the maker's own
/// remaining, and a floor pass can never over-allocate anyone. The moment the
/// weight stops being the size that guarantee dies. Makers holding `[1, 100]`,
/// aged `[10, 1]`, taker bringing `50`:
///
/// ```text
/// w = [1·10, 100·1] = [10, 100]     Σw = 110
/// f₁ = ⌊50·10/110⌋ = 4              — against a maker holding 1
/// ```
///
/// Handing out 4 where 1 exists breaks contract (3), so the excess has to go
/// back into the pot and be re-apportioned among whoever can still take it.
/// That is the water-filling loop below, and it is the only structural
/// difference from `ProRataMatcher`.
///
/// # Why one capping pass may cap several makers at once
///
/// Removing capped makers only ever RAISES the survivors' shares: each capped
/// `c` satisfies `q_c ≤ pool·w_c/W`, so
/// `pool' = pool − Σq_c ≥ pool·(W − Σw_c)/W = pool·W'/W`, hence
/// `pool'/W' ≥ pool/W`. Flooring is monotone, so this survives both the integer
/// division and the lot floor. Anyone over their cap now is still over it after
/// the redistribution, so there is no need to cap one at a time and re-test —
/// and since every pass removes at least one maker, the loop runs at most once
/// per maker.
///
/// # Lots, again
///
/// Everything the `ProRataMatcher` doc comment says about floor division
/// leaving the lattice applies here verbatim — this allocator multiplies and
/// divides quantities, so it is told the lot and floors every share to it. The
/// crumb sweep then hands out whole lots, front of the queue first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeProRataMatcher {
    lot_size: u64,
    tick: u128,
    max_age_ticks: u128,
}

impl TimeProRataMatcher {
    /// One arrival, one tick.
    ///
    /// `Maker.arrival` is an engine-assigned counter — it advances by one each
    /// time an order comes to rest, not with the wall clock — so an arrival is
    /// already the finest resolution there is and the default tick has nothing
    /// to coarsen. A tick above one buckets makers together: `tick = 10` means
    /// the ten most recent arrivals at a level all count as equally new, which
    /// blunts the size-vs-patience trade deliberately.
    ///
    /// (Were `arrival` a nanosecond timestamp, a coarser default would be
    /// mandatory — two makers a microsecond apart would otherwise differ in
    /// weight by a factor of a thousand, which is noise dressed up as
    /// priority. It is not, so this is one.)
    pub const DEFAULT_TICK: u128 = 1;

    /// 65,535 arrivals of dwell at the default tick. Past the cap, patience
    /// stops paying: an order that watched a million others come and go and one
    /// that watched a hundred thousand weigh the same.
    pub const DEFAULT_MAX_AGE_TICKS: u128 = 65_535;

    /// Takes the lot in base units — `InstrumentSpec::lot_size()`. No
    /// `Default`, for the reason on `ProRataMatcher::new`: a matcher that
    /// guessed `1` would look right in every unit-lot test and quietly produce
    /// off-lot fills everywhere else.
    pub fn new(lot_size: u64) -> Self {
        assert!(lot_size > 0, "lot_size must be non-zero");
        TimeProRataMatcher {
            lot_size,
            tick: Self::DEFAULT_TICK,
            max_age_ticks: Self::DEFAULT_MAX_AGE_TICKS,
        }
    }

    /// The resolution ages are measured at, in units of whatever clock the
    /// caller puts in `Maker.arrival` — arrivals, as the engine uses it.
    pub fn with_tick(mut self, tick: u128) -> Self {
        assert!(tick > 0, "tick must be non-zero");
        self.tick = tick;
        self
    }

    /// The age every older maker is treated as. Bounded by `u64::MAX` so that
    /// `remaining_quantity · age` — both u64-sized — cannot leave `u128`.
    pub fn with_max_age(mut self, max_age_ticks: u128) -> Self {
        assert!(
            max_age_ticks > 0 && max_age_ticks <= u128::from(u64::MAX),
            "max_age_ticks must be non-zero and fit u64"
        );
        self.max_age_ticks = max_age_ticks;
        self
    }

    /// How long this maker has rested, in ticks, floored to one and capped.
    ///
    /// The floor is what keeps a just-arrived maker in the running at all: a
    /// zero weight would mean zero share of everything, so the newest maker at
    /// a level could never trade even against a taker big enough for everyone.
    /// Flooring at one leaves it competing on size alone, which is exactly
    /// pro-rata's answer.
    ///
    /// Note `max(1, elapsed/tick)` rather than `1 + elapsed/tick`: both keep
    /// the weight positive, but only this one leaves real dwell RATIOS
    /// undistorted. With arrivals 0/1/2 read at `now = 3`, this gives ages
    /// 3/2/1 — the truth — where `1 +` gives 4/3/2 and flattens the very
    /// difference the algorithm exists to express.
    ///
    /// The subtraction saturates because `now` can legitimately sit behind an
    /// arrival — clock skew, or a caller that simply doesn't care about time.
    /// Degenerating to "brand new" is the right answer there; panicking or
    /// wrapping to a colossal age is not.
    fn age_ticks(&self, maker: &Maker, now: u128) -> u128 {
        (now.saturating_sub(maker.arrival) / self.tick).clamp(1, self.max_age_ticks)
    }

    /// Weights, normalized so each one fits `u64`.
    ///
    /// The water-fill computes `pool · wᵢ`, and `pool ≤ u64::MAX`, so a weight
    /// that fits u64 keeps the product inside u128 — `(2⁶⁴−1)² < 2¹²⁸` — with
    /// no checked arithmetic anywhere on the path. Only the RATIOS between
    /// weights matter, so scaling them all down by a common power of two costs
    /// nothing but the low bits, and only on levels whose quantities are within
    /// a few bits of `u64::MAX` in the first place. `ProRataMatcher` never
    /// needs this step: its weights are quantities, which already fit.
    ///
    /// Computed once rather than inside the loop — ages don't change between
    /// water-fill passes, only who is still in the running.
    fn weights(&self, makers: &[Maker], now: u128) -> Vec<u128> {
        let raw = |m: &Maker| u128::from(m.remaining_quantity.base()) * self.age_ticks(m, now);

        let max = makers.iter().map(raw).max().unwrap_or(0);
        let shift = max
            .checked_ilog2()
            .map_or(0, |bits| bits.saturating_sub(63));

        // `.max(1)`: a live maker whose weight shifted away to nothing keeps a
        // minimal claim rather than being silently excluded from the level.
        makers.iter().map(|m| (raw(m) >> shift).max(1)).collect()
    }
}

impl MatchingAlgorithm for TimeProRataMatcher {
    fn lot_size(&self) -> Option<u64> {
        Some(self.lot_size)
    }

    fn allocate(&self, available: Qty, makers: &[Maker], now: u128) -> Vec<Fill> {
        // Base units for the duration, `Qty` back on at the end — same reason
        // as `ProRataMatcher`: the widening to u128 is the entire point, and
        // doing the arithmetic through a u64 newtype would hide the overflow
        // this code exists to avoid.
        let available = available.base();
        debug_assert!(
            available.is_multiple_of(self.lot_size),
            "a taker's quantity is a lot multiple by construction"
        );
        let total: u128 = makers
            .iter()
            .map(|m| u128::from(m.remaining_quantity.base()))
            .sum();

        // Nothing to apportion, and the only branch an empty level reaches —
        // it must come before any division by `total`.
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

        let weights = self.weights(makers, now);
        let lot = u128::from(self.lot_size);
        let mut shares: Vec<u64> = vec![0; makers.len()];
        let mut pool = u128::from(available);

        // A maker is still in the running while its share sits below its
        // remaining. That makes `shares` its own bookkeeping: capped makers
        // hold exactly their remaining, and a maker holding nothing starts
        // there — which is how zero-quantity makers stay out of `Σw` without a
        // special case.
        let contending = |shares: &[u64], i: usize| shares[i] < makers[i].remaining_quantity.base();

        loop {
            let weight_total: u128 = weights
                .iter()
                .enumerate()
                .filter(|&(i, _)| contending(&shares, i))
                .map(|(_, weight)| weight)
                .sum();

            // Only reachable with nobody left contending: every weight is at
            // least one, so a live maker always carries weight.
            if weight_total == 0 {
                break;
            }

            // Cap everyone whose proportional share overruns what they hold,
            // all in one pass — see the monotonicity argument on the type.
            let mut capped: u128 = 0;
            for (i, weight) in weights.iter().enumerate() {
                if !contending(&shares, i) {
                    continue;
                }
                let remaining = u128::from(makers[i].remaining_quantity.base());
                if pool * weight / weight_total >= remaining {
                    shares[i] = makers[i].remaining_quantity.base();
                    capped += remaining;
                }
            }
            if capped > 0 {
                pool -= capped;
                continue;
            }

            // Nobody overruns: commit the lot-floored shares and stop. Every
            // share in this pass is computed against the SAME pool — draining
            // it as we go would move the denominator mid-pass.
            let mut allocated: u128 = 0;
            for (i, weight) in weights.iter().enumerate() {
                if !contending(&shares, i) {
                    continue;
                }
                let share = (pool * weight / weight_total / lot) * lot;
                shares[i] = share as u64;
                allocated += share;
            }
            pool -= allocated;
            break;
        }

        // Crumb sweep, identical in shape to pro-rata's. Each lot floor above
        // discarded strictly less than one lot, and every maker still
        // contending sits at least a whole lot under its own remaining (its
        // share didn't overrun, and both are lot multiples) — so one
        // front-to-back pass handing out a single lot each places all of it.
        // Capped makers have no headroom and are skipped by the same test.
        //
        // Headroom by subtraction rather than `share + lot <= remaining`: on a
        // level near `u64::MAX` the addition would overflow before the
        // comparison could reject it.
        let mut left = pool as u64;

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

        // Contract (2): a maker allocated nothing is ABSENT, not present with
        // quantity 0 — downstream reads "appears in fills" as "was touched".
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

    // --------------------------------------------------------- TIME PRO-RATA

    /// The whole point, in one case. Three makers of equal size that arrived
    /// one after another, read at `now = 3`: ages 3/2/1, so weights 30/20/10
    /// and the shares split 7/5/2 with the leftover lot going to the front by
    /// time priority. Pro-rata, which cannot see the difference, would hand out
    /// 5/5/5 — asserted here so the contrast is part of the test rather than a
    /// claim in a comment.
    #[test]
    fn time_pro_rata_favours_the_older_maker() {
        let makers = level_with_arrivals(&[(10, 0), (10, 1), (10, 2)]);
        let fills = time_pro_rata().allocate(qty(15), &makers, 3);

        assert_eq!(fills, vec![fill(0, 8), fill(1, 5), fill(2, 2)]);
        assert_contract(&fills, qty(15), &makers);

        let blind = pro_rata().allocate(qty(15), &makers, 3);
        assert_eq!(blind, vec![fill(0, 5), fill(1, 5), fill(2, 5)]);
    }

    /// Capping, the one structural difference from pro-rata. The doc comment's
    /// counterexample, executable: an ancient maker holding 1 is owed 4 by the
    /// proportional pass, so it is capped at what it actually holds and the
    /// excess goes back into the pot for the maker that can still take it.
    #[test]
    fn time_pro_rata_caps_a_maker_who_is_owed_more_than_it_holds() {
        let makers = level_with_arrivals(&[(1, 0), (100, 9)]);
        let fills = time_pro_rata().allocate(qty(50), &makers, 10);

        assert_eq!(fills, vec![fill(0, 1), fill(1, 49)]);
        assert_contract(&fills, qty(50), &makers);
    }

    /// Capping cascades: one pass is not enough. Maker 1 is UNDER its cap on
    /// the first pass (owed 8 against 10) and over it on the second (owed 49)
    /// once maker 0's excess has been redistributed. A loop that capped once
    /// and committed would leave maker 1 holding 10 against a fill of 49.
    #[test]
    fn time_pro_rata_caps_again_after_redistributing() {
        let makers = level_with_arrivals(&[(1, 0), (10, 990), (100, 999)]);
        let fills = time_pro_rata().allocate(qty(100), &makers, 1000);

        assert_eq!(fills, vec![fill(0, 1), fill(1, 10), fill(2, 89)]);
        assert_contract(&fills, qty(100), &makers);
    }

    /// Equal arrivals collapse the weight to the size, so this IS pro-rata —
    /// same 4/3/3 as `pro_rata_hands_a_single_crumb_to_the_front`, crumb and
    /// all. The proptest below asserts it in general.
    #[test]
    fn time_pro_rata_with_equal_arrivals_is_pro_rata() {
        let makers = level_of(&[10, 10, 10]);
        let fills = time_pro_rata().allocate(qty(10), &makers, NOW);

        assert_eq!(fills, vec![fill(0, 4), fill(1, 3), fill(2, 3)]);
        assert_eq!(fills, pro_rata().allocate(qty(10), &makers, NOW));
        assert_contract(&fills, qty(10), &makers);
    }

    /// Past `max_age_ticks` patience stops paying: one maker resting 1000 ticks
    /// and one resting 10 both weigh their size alone once the cap is 10, so an
    /// even split — not the 100:1 the raw ages would have bought.
    #[test]
    fn time_pro_rata_stops_rewarding_beyond_the_age_cap() {
        let makers = level_with_arrivals(&[(10, 0), (10, 990)]);
        let matcher = TimeProRataMatcher::new(1).with_tick(1).with_max_age(10);

        let fills = matcher.allocate(qty(10), &makers, 1000);

        assert_eq!(fills, vec![fill(0, 5), fill(1, 5)]);
        assert_contract(&fills, qty(10), &makers);
    }

    /// A maker stamped in the future — clock skew, or a caller that doesn't
    /// care — ages to the floor of one tick instead of panicking or wrapping to
    /// a colossal age. It is simply the newest thing at the level, so the
    /// 1000-tick-old maker takes everything the taker brought.
    #[test]
    fn time_pro_rata_treats_an_arrival_after_now_as_brand_new() {
        let makers = level_with_arrivals(&[(10, 0), (10, 2000)]);
        let fills = time_pro_rata().allocate(qty(10), &makers, 1000);

        assert_eq!(fills, vec![fill(0, 10)]);
        assert_contract(&fills, qty(10), &makers);
    }

    #[test]
    fn time_pro_rata_taker_larger_than_the_level_takes_everything() {
        let makers = level_with_arrivals(&[(10, 0), (20, 5)]);
        let fills = time_pro_rata().allocate(qty(1000), &makers, 10);

        assert_eq!(fills, vec![fill(0, 10), fill(1, 20)]);
        assert_contract(&fills, qty(1000), &makers);
    }

    #[test]
    fn time_pro_rata_empty_level_yields_no_fills() {
        let makers = level_of(&[]);
        let fills = time_pro_rata().allocate(qty(100), &makers, NOW);

        assert!(fills.is_empty());
        assert_contract(&fills, qty(100), &makers);
    }

    #[test]
    fn time_pro_rata_nothing_available_yields_no_fills() {
        let makers = level_with_arrivals(&[(10, 0), (10, 5)]);
        let fills = time_pro_rata().allocate(qty(0), &makers, 10);

        assert!(fills.is_empty());
        assert_contract(&fills, qty(0), &makers);
    }

    /// A maker holding nothing never contends, so it carries no weight, takes
    /// no crumb, and is absent from the result — while the maker behind it
    /// keeps its slice position, index 2 and never index 1. Ages 3 and 1 split
    /// the ten as 7/2, and the leftover lot goes to the front.
    #[test]
    fn time_pro_rata_skips_a_zero_remaining_maker() {
        let makers = level_with_arrivals(&[(10, 0), (0, 1), (10, 2)]);
        let fills = time_pro_rata().allocate(qty(10), &makers, 3);

        assert_eq!(fills, vec![fill(0, 8), fill(2, 2)]);
        assert_contract(&fills, qty(10), &makers);
    }

    /// The lot floor bites exactly as it does for pro-rata: proportional shares
    /// of 5 against a lot of 10 floor to nothing, and the whole taker is placed
    /// by the sweep — in whole lots, to the front.
    #[test]
    fn time_pro_rata_never_splits_a_lot() {
        let makers = level_with_arrivals(&[(10, 0), (20, 5)]);
        let fills = TimeProRataMatcher::new(10)
            .with_tick(1)
            .allocate(qty(10), &makers, 10);

        assert_eq!(fills, vec![fill(0, 10)]);
        assert_contract(&fills, qty(10), &makers);
    }

    /// A taker of exactly one lot cannot be split three ways, so time priority
    /// decides outright and the oldest maker takes it.
    #[test]
    fn time_pro_rata_gives_a_lone_lot_to_the_front() {
        let makers = level_with_arrivals(&[(100, 0), (100, 1), (100, 2)]);
        let fills = TimeProRataMatcher::new(100)
            .with_tick(1)
            .allocate(qty(100), &makers, 3);

        assert_eq!(fills, vec![fill(0, 100)]);
        assert_contract(&fills, qty(100), &makers);
    }

    /// Two makers at `u64::MAX` with different ages: the level total overflows
    /// u64 before any multiplication happens, and `qty · age` overflows it
    /// again afterwards, so this is the case the weight renormalization exists
    /// for. The exact split is not the point — that it lands on the contract,
    /// with the older maker ahead, is.
    #[test]
    fn time_pro_rata_survives_weights_that_overflow_u64() {
        let makers = level_with_arrivals(&[(u64::MAX, 0), (u64::MAX, 500)]);
        let fills = time_pro_rata().allocate(qty(100), &makers, 1000);

        assert_contract(&fills, qty(100), &makers);
        assert_eq!(fills.len(), 2);
        assert!(
            fills[0].quantity > fills[1].quantity,
            "the maker resting twice as long should take the larger share, got {fills:?}"
        );
    }

    #[test]
    #[should_panic(expected = "lot_size must be non-zero")]
    fn a_zero_lot_time_pro_rata_matcher_is_rejected_loudly() {
        TimeProRataMatcher::new(0);
    }

    #[test]
    #[should_panic(expected = "tick must be non-zero")]
    fn a_zero_tick_is_rejected_loudly() {
        TimeProRataMatcher::new(1).with_tick(0);
    }
}
