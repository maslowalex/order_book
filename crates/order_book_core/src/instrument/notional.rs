/// A notional value, in units of (one minor price unit × one base quantity
/// unit). Integral by construction, so comparing a notional against a floor
/// never touches `Decimal` and never rounds.
///
/// `u128` is exactly wide enough and not one bit wider: the largest product two
/// `u64`s can make is `(2^64 − 1)² = 2^128 − 2^65 + 1`, which is below
/// `u128::MAX = 2^128 − 1`. This is the same lesson as the `u128` widening in
/// the pro-rata allocator, one dimension over — except there the headroom is
/// comfortable and here it is a single bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Notional(pub(super) u128);

impl Notional {
    pub const fn raw(self) -> u128 {
        self.0
    }
}
