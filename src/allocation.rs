/*
* Provides a pluggable implementations for actual matching logic, e.g.
* Deciding who at the price levels gets filled
*
* Contract (documented on the trait, enforced by debug_assert_fills,
* with total = Σ remaining_quantity):
* 1. order_index strictly increasing, each < makers.len()
* 2. quantity > 0 — no zero fills ("allocated" means "touched", and STP keys off exactly that)
* 3. quantity <= makers[order_index].remaining_quantity
* 4. Σ quantity == min(available, total)
* 5. every quantity is a whole number of lots
*
* (5) needs no runtime check and gets none. Every input is a lot multiple
* because `Qty` cannot hold anything else, and the allocators preserve that:
* FIFO because the minimum of two multiples is a multiple, the weighted ones
* because they are told the lot and floor to it. The clause is written down
* anyway, because the algorithms that could break it look correct without it —
* see the counterexample on `ProRataMatcher`.
*/

use crate::instrument::Qty;
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

/// One resting order as an allocator sees it: how much is left to fill, and
/// when it joined the queue.
///
/// Deliberately NOT an `Order`. An allocation policy has no business reading
/// prices, ids, or order types, and — the 6.2a point — a storage layout that
/// holds nothing resembling an `Order` can still hand out this view. What the
/// engine puts in `arrival` is its own decision, which is the second reason
/// this type exists: `Order.timestamp` is supplied by the client and never
/// re-stamped on receipt, so weighting by it would let a sender backdate its
/// way to the front, and would age a stop from before it parked rather than
/// from when it actually joined the level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Maker {
    pub remaining_quantity: Qty,
    pub arrival: u128,
}

pub trait MatchingAlgorithm {
    /// Apportion `available` across one price level.
    ///
    /// `makers` is the level's queue in arrival order, **oldest first** — every
    /// allocator here breaks ties by position, so the caller owes it that
    /// ordering. `now` is the taker's arrival instant, the reference ages are
    /// measured against; allocators that don't weight by time ignore it.
    ///
    /// Nothing is read from a clock inside: an allocation is a pure function of
    /// its arguments, so it replays identically off a log.
    fn allocate(&self, available: Qty, makers: &[Maker], now: u128) -> Vec<Fill>;

    /// The lot this allocator floors its shares to, if it needs to know one.
    ///
    /// Part of the contract, not a foreign concern: clause (5) says every fill
    /// is a whole lot, and an allocator that genuinely multiplies and divides
    /// quantities can only honour it if it was told where the grid is. `None`
    /// means "closed under the lattice for free" — the FIFO answer.
    ///
    /// The engine reads this to check the matcher against the instrument it is
    /// about to trade. A pro-rata built for lot `1` in a lot-`100` book does
    /// not merely print badly: its off-lot fills are subtracted into the
    /// makers, leaving them off-grid, resting, and matchable. See the
    /// counterexample on [`ProRataMatcher`].
    fn lot_size(&self) -> Option<u64> {
        None
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FifoMatcher;

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
    use proptest::prelude::*;
    use std::collections::HashMap;

    /// The taker's arrival, for every case where nothing is weighted by time.
    /// Far enough ahead of any arrival stamped below that ages are positive.
    const NOW: u128 = 1_000_000;

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
    /// maker's remaining quantity, in FIFO order.
    ///
    /// Every maker arrives at the SAME instant, which is what makes this the
    /// right helper for the time-blind cases — and, for `TimeProRataMatcher`,
    /// the level where it is pro-rata by definition rather than by accident.
    fn level_of(quantities: &[u64]) -> Vec<Maker> {
        quantities
            .iter()
            .map(|&quantity| Maker {
                remaining_quantity: qty(quantity),
                arrival: 0,
            })
            .collect()
    }

    /// A queue with the arrivals spelled out — `(remaining, arrival)` per maker,
    /// oldest first, as the trait requires.
    fn level_with_arrivals(makers: &[(u64, u128)]) -> Vec<Maker> {
        makers
            .iter()
            .map(|&(quantity, arrival)| Maker {
                remaining_quantity: qty(quantity),
                arrival,
            })
            .collect()
    }

    /// The four trait rules, checked on every case. This grows up into
    /// `debug_assert_fills` in `matching.rs` once the engine allocates through
    /// the trait, so it is not throwaway — and it will police `ProRataMatcher`
    /// unchanged in step 2.
    fn assert_contract(fills: &[Fill], available: Qty, makers: &[Maker]) {
        let total: u128 = makers
            .iter()
            .map(|m| u128::from(m.remaining_quantity.base()))
            .sum();

        let mut previous: Option<usize> = None;
        let mut allocated: u128 = 0;

        for fill in fills {
            assert!(
                previous.is_none_or(|p| fill.order_index > p),
                "(1) order_index must strictly increase, got {fills:?}"
            );
            assert!(
                fill.order_index < makers.len(),
                "(1) order_index {} out of range for a level of {}",
                fill.order_index,
                makers.len()
            );
            assert!(
                !fill.quantity.is_zero(),
                "(2) zero-quantity fill in {fills:?}"
            );
            assert!(
                fill.quantity <= makers[fill.order_index].remaining_quantity,
                "(3) fill of {:?} over-fills maker {}, which has {:?} left",
                fill.quantity,
                fill.order_index,
                makers[fill.order_index].remaining_quantity
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
    fn assert_proportional(fills: &[Fill], available: Qty, makers: &[Maker], lot: u64) {
        let available = available.base();
        let total: u128 = makers
            .iter()
            .map(|m| u128::from(m.remaining_quantity.base()))
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

        for (i, maker) in makers.iter().enumerate() {
            let exact = u128::from(available) * u128::from(maker.remaining_quantity.base()) / total;
            let floor_share = ((exact / u128::from(lot)) * u128::from(lot)) as u64;
            let got = allocated.get(&i).copied().unwrap_or(0);

            assert!(
                got == floor_share || got == floor_share + lot,
                "maker {i} (qty {:?}) got {got}, expected its lot-floored share {floor_share} or one lot more",
                maker.remaining_quantity
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

    // --------------------------------------------------------- TIME PRO-RATA

    /// Unit lot and a one-per-arrival tick, so an age in ticks IS the raw gap
    /// and every number below can be checked by hand.
    fn time_pro_rata() -> TimeProRataMatcher {
        TimeProRataMatcher::new(1).with_tick(1)
    }

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

    // ------------------------------------------------------ EVERY MATCHER

    proptest! {
        /// The contract belongs to the TRAIT, not to any one implementation —
        /// so point it at all three over random levels. A rounding slip, a lost
        /// crumb, or a countdown bug surfaces here as a contract violation
        /// instead of as a wrong-looking number in one hand-written case.
        ///
        /// Arrivals are generated too, and spread over a range wide enough to
        /// clear the one-tick floor, so the time-weighted matcher is exercised
        /// with real age spread rather than in its degenerate pro-rata mode.
        ///
        /// This is `debug_assert_fills` in embryo: the wiring step moves the
        /// same checks into the engine, where the whole existing test suite
        /// starts policing every allocation for free.
        #[test]
        fn every_matcher_honours_the_contract(
            available in 0u64..1_000_000,
            makers in prop::collection::vec((0u64..10_000, 0u128..NOW), 0..24),
        ) {
            let makers = level_with_arrivals(&makers);

            let available = qty(available);

            let fifo = FifoMatcher.allocate(available, &makers, NOW);
            assert_contract(&fifo, available, &makers);

            let pro_rata = pro_rata().allocate(available, &makers, NOW);
            assert_contract(&pro_rata, available, &makers);
            assert_proportional(&pro_rata, available, &makers, 1);

            let time_pro_rata = time_pro_rata().allocate(available, &makers, NOW);
            assert_contract(&time_pro_rata, available, &makers);
        }

        /// Equal arrivals collapse every weight to `size · k` for one common
        /// `k`, and scaling every weight by a constant cancels in the division
        /// — so the time-weighted matcher must agree with pro-rata EXACTLY,
        /// crumb for crumb, not merely closely.
        ///
        /// Quantities stay small enough that `size · k` fits u64, so no weight
        /// renormalization is in play; that path shifts away low bits by
        /// design and is covered by its own case above.
        #[test]
        fn equal_arrivals_make_it_pro_rata(
            available in 0u64..1_000_000,
            quantities in prop::collection::vec(0u64..10_000, 0..24),
            arrival in 0u128..NOW,
        ) {
            let makers: Vec<Maker> = quantities
                .iter()
                .map(|&quantity| Maker { remaining_quantity: qty(quantity), arrival })
                .collect();
            let available = qty(available);

            prop_assert_eq!(
                time_pro_rata().allocate(available, &makers, NOW),
                pro_rata().allocate(available, &makers, NOW)
            );
        }

        /// The property the algorithm exists for: at equal size, resting longer
        /// is never worse. It holds through capping (a capped maker is at its
        /// remaining, which is the most anyone can get) and through the crumb
        /// sweep (which walks oldest-first), so it is a real end-to-end
        /// statement rather than a claim about the floor pass alone.
        #[test]
        fn resting_longer_never_pays_less_at_equal_size(
            available in 0u64..100_000,
            size in 1u64..10_000,
            count in 1usize..12,
        ) {
            // arrival ascending == age descending: maker 0 is the oldest
            let makers: Vec<Maker> = (0..count)
                .map(|i| Maker { remaining_quantity: qty(size), arrival: i as u128 })
                .collect();
            let available = qty(available);

            let fills = time_pro_rata().allocate(available, &makers, count as u128);
            assert_contract(&fills, available, &makers);

            // absent from `fills` means allocated nothing — contract (2)
            let mut got = vec![0u64; count];
            for f in &fills {
                got[f.order_index] = f.quantity.base();
            }
            for window in got.windows(2) {
                prop_assert!(
                    window[0] >= window[1],
                    "a younger maker out-earned an older one at equal size: {got:?}"
                );
            }
        }

        /// The mirror of `pro_rata_honours_the_contract_on_a_non_unit_lot`, and
        /// it exists for the same reason: with `lot == 1` clause (5) is vacuous,
        /// so an implementation that ignored lots entirely would pass every
        /// other property here.
        #[test]
        fn time_pro_rata_honours_the_contract_on_a_non_unit_lot(
            available_lots in 0u64..100_000,
            makers in prop::collection::vec((0u64..1_000, 0u128..NOW), 0..24),
            lot in prop::sample::select(vec![2u64, 5, 10, 100]),
        ) {
            let makers: Vec<Maker> = makers
                .iter()
                .map(|&(lots, arrival)| Maker {
                    remaining_quantity: qty(lots * lot),
                    arrival,
                })
                .collect();
            let available = qty(available_lots * lot);

            let fills = TimeProRataMatcher::new(lot)
                .with_tick(1)
                .allocate(available, &makers, NOW);

            assert_contract(&fills, available, &makers);
            for f in &fills {
                prop_assert!(
                    f.quantity.base().is_multiple_of(lot),
                    "(5) fill of {:?} is not a whole number of {lot}-unit lots",
                    f.quantity
                );
            }
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
            let makers = level_of(&quantities);
            let available = qty(available_lots * lot);

            let fills = ProRataMatcher::new(lot).allocate(available, &makers, NOW);

            assert_contract(&fills, available, &makers);
            assert_proportional(&fills, available, &makers, lot);
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
            let makers = level_of(&quantities);
            let available = qty(available_lots * lot);

            let fills = FifoMatcher.allocate(available, &makers, NOW);

            assert_contract(&fills, available, &makers);
            for f in &fills {
                prop_assert!(f.quantity.base().is_multiple_of(lot));
            }
        }
    }

    // ------------------------------------------------- THE DECLARED LOT SIZE

    /// The flip side of the property above: FIFO declares no lot because it
    /// needs none, and the two allocators that divide declare the one they
    /// were built with. The engine reads exactly this to refuse a matcher
    /// built for the wrong instrument.
    #[test]
    fn only_the_allocators_that_divide_declare_a_lot() {
        assert_eq!(FifoMatcher.lot_size(), None);
        assert_eq!(ProRataMatcher::new(25).lot_size(), Some(25));
        assert_eq!(TimeProRataMatcher::new(25).lot_size(), Some(25));
    }
}
