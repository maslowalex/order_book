/// A price in minor units of the quote currency — cents for a USD-quoted
/// instrument, satoshis for a BTC-quoted one.
///
/// Unsigned, so a negative price is unrepresentable rather than validated. The
/// cost of that choice is real and worth naming: instruments *can* trade
/// negative (WTI in April 2020, and calendar spreads routinely), and this book
/// cannot model them. The benefit is that every price in the engine is known
/// non-negative without a single runtime check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Price(pub(super) u64);

impl Price {
    /// The underlying count of minor units. Meaningless without the spec that
    /// produced it — use [`InstrumentSpec::to_decimal`](super::InstrumentSpec::to_decimal)(super::InstrumentSpec::to_decimal) to get a number a human
    /// can read.
    pub const fn minor(self) -> u64 {
        self.0
    }

    /// Crate-internal construction, bypassing the tick check. Callable only
    /// where the value provably came off the lattice already — copying a price
    /// out of a level, or a test fixture building on a known grid. Public
    /// callers go through [`InstrumentSpec`](super::InstrumentSpec).
    // Unused until the engine itself speaks `Price`; the module lands first so
    // the migration that follows is a pure type change with nothing new in it.
    #[allow(dead_code)]
    pub(crate) const fn from_minor_unchecked(minor: u64) -> Self {
        Price(minor)
    }
}
