/// A *difference* between two prices, counted in ticks — which is how a spread
/// is actually quoted on a trading floor ("it's two ticks wide"), not in
/// currency.
///
/// A separate type because a price difference is not a price: you cannot rest
/// an order at a spread, and adding two prices together is meaningless. Keeping
/// them distinct makes `Price + Price` a compile error rather than a plausible
/// line of code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ticks(pub(super) u64);

impl Ticks {
    pub const fn count(self) -> u64 {
        self.0
    }

    #[allow(dead_code)] // see the note on `Price::from_minor_unchecked`
    pub(crate) const fn from_count(count: u64) -> Self {
        Ticks(count)
    }
}
