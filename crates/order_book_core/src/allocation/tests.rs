use super::fifo::FifoMatcher;
use super::pro_rata::ProRataMatcher;
use super::test_helpers::*;
use super::time_pro_rata::TimeProRataMatcher;
use super::*;
use proptest::prelude::*;

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
