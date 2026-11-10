use super::pro_rata::ProRataMatcher;
use super::time_pro_rata::TimeProRataMatcher;
use super::{Fill, Maker};
use crate::instrument::qty::Qty;
use std::collections::HashMap;

/// The taker's arrival, for every case where nothing is weighted by time.
/// Far enough ahead of any arrival stamped below that ages are positive.
pub(super) const NOW: u128 = 1_000_000;

/// Base units as a `Qty`. The allocator's unit is whatever the instrument
/// says a lot is; these tests run on a unit lot, so base units and lots
/// coincide and the numbers below read the same as they always did.
pub(super) fn qty(n: u64) -> Qty {
    Qty::from_base_unchecked(n)
}

/// The matcher the pre-lot cases run against. Unit lot, so every
/// quantity is a whole lot and the numbers read exactly as before.
pub(super) fn pro_rata() -> ProRataMatcher {
    ProRataMatcher::new(1)
}

pub(super) fn fill(order_index: usize, quantity: u64) -> Fill {
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
pub(super) fn level_of(quantities: &[u64]) -> Vec<Maker> {
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
pub(super) fn level_with_arrivals(makers: &[(u64, u128)]) -> Vec<Maker> {
    makers
        .iter()
        .map(|&(quantity, arrival)| Maker {
            remaining_quantity: qty(quantity),
            arrival,
        })
        .collect()
}

/// The four trait rules, checked on every case. This grows up into
/// `debug_assert_fills` in `orderbook/matching.rs` once the engine allocates through
/// the trait, so it is not throwaway — and it will police `ProRataMatcher`
/// unchanged in step 2.
pub(super) fn assert_contract(fills: &[Fill], available: Qty, makers: &[Maker]) {
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
pub(super) fn assert_proportional(fills: &[Fill], available: Qty, makers: &[Maker], lot: u64) {
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

/// Unit lot and a one-per-arrival tick, so an age in ticks IS the raw gap
/// and every number below can be checked by hand.
pub(super) fn time_pro_rata() -> TimeProRataMatcher {
    TimeProRataMatcher::new(1).with_tick(1)
}
