//! Tick size, lot size, and the integer lattice they define.
//!
//! An order book is not a continuum. Every venue publishes a contract
//! specification saying what a legal price and a legal quantity look like, and
//! every price and quantity the book ever sees is a point on the discrete grid
//! that spec defines. This module is that grid.
//!
//! # Vocabulary
//!
//! The literature overloads "lot size" badly, so fix the meanings once:
//!
//! - **Tick size** — the minimum price increment. Legal prices are integer
//!   multiples of it. `0.25` for the E-mini S&P, `0.01` for most US equities.
//! - **Lot size** — the minimum *quantity* increment. Binance calls this
//!   `stepSize`; equities call the same idea a round lot. It is **not** the
//!   futures contract multiplier ("1 contract = 1000 barrels"), which scales
//!   notional rather than constraining quantity, and is a different concept
//!   wearing a similar name.
//! - **Scale** — how many decimal places the integer encoding carries.
//!   `price_scale: 2` means [`Price`] counts cents. This is the parameter that
//!   makes the number `10025` mean anything at all; without it the integer is
//!   not a price, it is just an integer.
//!
//! # Why integers
//!
//! Prices and quantities are stored as scaled integers in minor units — cents,
//! satoshis, lamports — which is what ITCH-style market data feeds put on the
//! wire, and what an exchange keeps in memory. `Decimal` survives in this
//! module and nowhere else: it is a good type for parsing and formatting at the
//! API boundary, and a bad one behind it. It is 16 bytes against 8, and its
//! `Ord` has to align two scales before it can answer, so a `BTreeMap` keyed by
//! `Decimal` runs a software routine at every level of every descent where an
//! integer runs one instruction.
//!
//! Integers also delete a bug rather than merely speeding things up. `100.0`
//! and `100.00` are `Ord`-equal but not identical `Decimal`s, so they collide
//! onto one book level while the level keeps whichever *scale* happened to
//! create it — making the scale a trade prints at depend on arrival order.
//! `10000` has no such freedom.
//!
//! # Why the fields are private
//!
//! [`InstrumentSpec`] is the only way to construct a [`Price`] or a [`Qty`], so
//! an off-tick price cannot be built through this API at all. The bit pattern
//! exists — nothing stops a `u64` from holding `100_003` — but no safe path
//! reaches it, which is the same guarantee `String` gives over `Vec<u8>`:
//! invalid UTF-8 is representable in the bytes and unconstructable through the
//! type. That is what turns "we assume normalization happened upstream" from a
//! comment into an invariant.

use crate::types::Side;
use rust_decimal::Decimal;

/// `rust_decimal` carries at most 28 decimal places, so a scale beyond this
/// could never round-trip and is rejected when the spec is built.
const MAX_SCALE: u32 = 28;

/// A price in minor units of the quote currency — cents for a USD-quoted
/// instrument, satoshis for a BTC-quoted one.
///
/// Unsigned, so a negative price is unrepresentable rather than validated. The
/// cost of that choice is real and worth naming: instruments *can* trade
/// negative (WTI in April 2020, and calendar spreads routinely), and this book
/// cannot model them. The benefit is that every price in the engine is known
/// non-negative without a single runtime check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Price(u64);

impl Price {
    /// The underlying count of minor units. Meaningless without the spec that
    /// produced it — use [`InstrumentSpec::to_decimal`] to get a number a human
    /// can read.
    pub const fn minor(self) -> u64 {
        self.0
    }

    /// Crate-internal construction, bypassing the tick check. Callable only
    /// where the value provably came off the lattice already — copying a price
    /// out of a level, or a test fixture building on a known grid. Public
    /// callers go through [`InstrumentSpec`].
    // Unused until the engine itself speaks `Price`; the module lands first so
    // the migration that follows is a pure type change with nothing new in it.
    #[allow(dead_code)]
    pub(crate) const fn from_minor_unchecked(minor: u64) -> Self {
        Price(minor)
    }
}

/// A quantity in base units of the asset — satoshis for BTC, whole shares for
/// an equity quoted in shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Qty(u64);

impl Qty {
    pub const ZERO: Qty = Qty(0);

    pub const fn base(self) -> u64 {
        self.0
    }

    pub(crate) const fn from_base_unchecked(base: u64) -> Self {
        Qty(base)
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
}

// Quantities add and subtract like the counts they are, and the operators are
// carried rather than replaced by `.base()` calls at the ~50 arithmetic sites
// — a type you have to unwrap to use is a type nobody uses.
//
// Overflow behaviour is inherited verbatim from `u64`: panic in debug, wrap in
// release. That is what the raw `u64` fields did before this type existed, and
// changing it here would smuggle a behavioural change into a type migration.
// `PriceLevel::total_quantity` folds without a check and can still overflow a
// deep enough level; that hazard predates this type and is left visible.
impl std::ops::Add for Qty {
    type Output = Qty;
    fn add(self, rhs: Qty) -> Qty {
        Qty(self.0 + rhs.0)
    }
}

impl std::ops::Sub for Qty {
    type Output = Qty;
    fn sub(self, rhs: Qty) -> Qty {
        Qty(self.0 - rhs.0)
    }
}

impl std::ops::AddAssign for Qty {
    fn add_assign(&mut self, rhs: Qty) {
        self.0 += rhs.0;
    }
}

impl std::ops::SubAssign for Qty {
    fn sub_assign(&mut self, rhs: Qty) {
        self.0 -= rhs.0;
    }
}

impl std::iter::Sum for Qty {
    fn sum<I: Iterator<Item = Qty>>(iter: I) -> Qty {
        iter.fold(Qty::ZERO, |acc, q| acc + q)
    }
}

impl<'a> std::iter::Sum<&'a Qty> for Qty {
    fn sum<I: Iterator<Item = &'a Qty>>(iter: I) -> Qty {
        iter.copied().sum()
    }
}

/// A *difference* between two prices, counted in ticks — which is how a spread
/// is actually quoted on a trading floor ("it's two ticks wide"), not in
/// currency.
///
/// A separate type because a price difference is not a price: you cannot rest
/// an order at a spread, and adding two prices together is meaningless. Keeping
/// them distinct makes `Price + Price` a compile error rather than a plausible
/// line of code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ticks(u64);

impl Ticks {
    pub const fn count(self) -> u64 {
        self.0
    }

    #[allow(dead_code)] // see the note on `Price::from_minor_unchecked`
    pub(crate) const fn from_count(count: u64) -> Self {
        Ticks(count)
    }
}

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
pub struct Notional(u128);

impl Notional {
    pub const fn raw(self) -> u128 {
        self.0
    }
}

/// Which way [`InstrumentSpec::round_price`] moves a price that is not already
/// on the tick grid.
///
/// Deliberately without a `Nearest`: there is no safe default direction for an
/// *order* price. Nearest can move a resting bid up into a spread the sender
/// was deliberately staying out of, and the caller who wants that behaviour
/// should say so by choosing `Up` or `Down` with their eyes open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rounding {
    /// Toward zero — the next legal price at or below the input.
    Down,
    /// Away from zero — the next legal price at or above the input.
    Up,
    /// Away from the touch: a bid rounds down, an ask rounds up. Never improves
    /// the sender's price, so it cannot manufacture a fill they did not ask
    /// for — but it does rest liquidity at a price they never named, which is
    /// why it is opt-in rather than the book's default.
    TowardPassive(Side),
}

/// Why a value could not be placed on the lattice.
///
/// Every variant carries the offending input so the message can say what was
/// wrong rather than merely that something was. These are conversion failures,
/// raised at the boundary — the engine downstream cannot produce them, because
/// downstream every value is already a [`Price`] or a [`Qty`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecError {
    /// Prices are unsigned; see the note on [`Price`].
    PriceNegative(Decimal),
    /// Finer than the instrument's minor unit — e.g. `100.001` where the
    /// instrument quotes in cents. Distinguished from `PriceOffTick` because
    /// the fix is different: this is a scale mistake, not a grid mistake.
    PriceTooPrecise {
        price: Decimal,
        price_scale: u32,
    },
    /// On the minor-unit grid but not on the tick grid — e.g. `100.03` where
    /// the tick is `0.25`.
    PriceOffTick {
        price: Decimal,
        tick_size: Decimal,
    },
    /// Outside the spec's `min_price..=max_price`, or too large for `u64`.
    PriceOutOfRange(Decimal),

    QtyNegative(Decimal),
    QtyTooPrecise {
        qty: Decimal,
        qty_scale: u32,
    },
    QtyOffLot {
        qty: Decimal,
        lot_size: Decimal,
    },
    QtyOutOfRange(Decimal),

    /// The spec itself is contradictory — a zero tick, a min above a max.
    InvalidSpec(&'static str),
}

/// The contract specification: what a legal price and a legal quantity look
/// like for one instrument.
///
/// This type replaces an assumption. The book used to carry a comment saying
/// that quantities were stored at the lowest fraction and normalized "on the
/// higher levels" — a description of an invariant that nothing enforced and
/// nothing could. The spec is where that normalization actually happens, and
/// [`Price`]/[`Qty`] are the proof it happened.
///
/// `Copy`, and it must stay that way: every mutating benchmark clones the whole
/// book once per iteration, so a heap field here (an owned symbol, say) would
/// show up in the numbers. Symbol identity belongs to a future
/// `Instrument { symbol, spec }` that embeds this, not to the lattice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstrumentSpec {
    price_scale: u32,
    qty_scale: u32,
    /// In minor units. `25` with `price_scale: 2` is a tick of `0.25`.
    tick_size: u64,
    /// In base units.
    lot_size: u64,
    min_price: Price,
    max_price: Option<Price>,
    min_qty: Qty,
    max_qty: Option<Qty>,
    min_notional: Option<Notional>,
}

impl InstrumentSpec {
    /// The four parameters that define the lattice. Fallible, because a spec
    /// is itself a validated type — a zero tick would make every price legal
    /// and divide by zero, which is exactly the failure this module exists to
    /// prevent.
    ///
    /// Defaults the bounds to `min_price = one tick` and `min_qty = one lot`,
    /// so a zero price and a zero quantity are rejected out of the box rather
    /// than by remembering to set a bound.
    pub fn new(
        price_scale: u32,
        qty_scale: u32,
        tick_size: u64,
        lot_size: u64,
    ) -> Result<Self, SpecError> {
        if price_scale > MAX_SCALE || qty_scale > MAX_SCALE {
            return Err(SpecError::InvalidSpec(
                "scale exceeds the 28 decimal places rust_decimal can carry",
            ));
        }
        if tick_size == 0 {
            return Err(SpecError::InvalidSpec("tick_size must be non-zero"));
        }
        if lot_size == 0 {
            return Err(SpecError::InvalidSpec("lot_size must be non-zero"));
        }

        Ok(InstrumentSpec {
            price_scale,
            qty_scale,
            tick_size,
            lot_size,
            min_price: Price(tick_size),
            max_price: None,
            min_qty: Qty(lot_size),
            max_qty: None,
            min_notional: None,
        })
    }

    /// Prices in cents on a one-cent tick, quantities in whole units on a
    /// one-unit lot. The grid the test suite has always used implicitly.
    ///
    /// Named rather than a `Default` impl on purpose: a *default* lattice is
    /// the "someone upstream handled it" assumption sneaking back in through a
    /// derive. Callers should be able to point at the line where they chose a
    /// grid.
    pub fn cents() -> Self {
        InstrumentSpec::new(2, 0, 1, 1).expect("2/0/1/1 is a valid spec")
    }

    pub fn with_price_range(mut self, min: Price, max: Option<Price>) -> Result<Self, SpecError> {
        if !min.0.is_multiple_of(self.tick_size) {
            return Err(SpecError::InvalidSpec("min_price is not on the tick grid"));
        }
        if let Some(max) = max {
            if !max.0.is_multiple_of(self.tick_size) {
                return Err(SpecError::InvalidSpec("max_price is not on the tick grid"));
            }
            if max < min {
                return Err(SpecError::InvalidSpec("max_price below min_price"));
            }
        }
        self.min_price = min;
        self.max_price = max;
        Ok(self)
    }

    pub fn with_qty_range(mut self, min: Qty, max: Option<Qty>) -> Result<Self, SpecError> {
        if !min.0.is_multiple_of(self.lot_size) {
            return Err(SpecError::InvalidSpec("min_qty is not on the lot grid"));
        }
        if let Some(max) = max {
            if !max.0.is_multiple_of(self.lot_size) {
                return Err(SpecError::InvalidSpec("max_qty is not on the lot grid"));
            }
            if max < min {
                return Err(SpecError::InvalidSpec("max_qty below min_qty"));
            }
        }
        self.min_qty = min;
        self.max_qty = max;
        Ok(self)
    }

    // ------------------------------------------------------------ accessors

    pub const fn price_scale(&self) -> u32 {
        self.price_scale
    }
    pub const fn qty_scale(&self) -> u32 {
        self.qty_scale
    }
    /// In minor units.
    pub const fn tick_size(&self) -> u64 {
        self.tick_size
    }
    /// In base units.
    pub const fn lot_size(&self) -> u64 {
        self.lot_size
    }
    pub const fn min_price(&self) -> Price {
        self.min_price
    }
    pub const fn max_price(&self) -> Option<Price> {
        self.max_price
    }
    pub const fn min_qty(&self) -> Qty {
        self.min_qty
    }
    pub const fn max_qty(&self) -> Option<Qty> {
        self.max_qty
    }
    pub const fn min_notional(&self) -> Option<Notional> {
        self.min_notional
    }

    // ------------------------------------------------------- onto the grid

    /// Place a decimal price on the lattice, or say precisely why it does not
    /// belong there.
    ///
    /// Range is deliberately NOT checked here: `min_price`/`max_price` are
    /// admission policy, enforced once at the book's ingress, while tick
    /// alignment is a property of the value itself. Conflating them would mean
    /// a price that is perfectly well-formed could not be named in a test.
    pub fn price(&self, d: Decimal) -> Result<Price, SpecError> {
        let minor = match split_minor(d, self.price_scale) {
            Ok((minor, false)) => minor,
            Ok((_, true)) => {
                return Err(SpecError::PriceTooPrecise {
                    price: d,
                    price_scale: self.price_scale,
                });
            }
            Err(Sign::Negative) => return Err(SpecError::PriceNegative(d)),
            Err(Sign::TooLarge) => return Err(SpecError::PriceOutOfRange(d)),
        };

        self.price_from_minor(minor)
            .map_err(|_| SpecError::PriceOffTick {
                price: d,
                tick_size: self.tick_decimal(),
            })
    }

    /// The integer-native door, for callers that already hold minor units —
    /// an ITCH-style feed hands you `1002500`, not `"100.25"`, and making it
    /// build a `Decimal` just to have it taken apart again would be silly.
    pub fn price_from_minor(&self, minor: u64) -> Result<Price, SpecError> {
        if !minor.is_multiple_of(self.tick_size) {
            return Err(SpecError::PriceOffTick {
                price: Decimal::from_i128_with_scale(i128::from(minor), self.price_scale),
                tick_size: self.tick_decimal(),
            });
        }
        Ok(Price(minor))
    }

    pub fn qty(&self, d: Decimal) -> Result<Qty, SpecError> {
        let base = match split_minor(d, self.qty_scale) {
            Ok((base, false)) => base,
            Ok((_, true)) => {
                return Err(SpecError::QtyTooPrecise {
                    qty: d,
                    qty_scale: self.qty_scale,
                });
            }
            Err(Sign::Negative) => return Err(SpecError::QtyNegative(d)),
            Err(Sign::TooLarge) => return Err(SpecError::QtyOutOfRange(d)),
        };

        self.qty_from_base(base).map_err(|_| SpecError::QtyOffLot {
            qty: d,
            lot_size: self.lot_decimal(),
        })
    }

    pub fn qty_from_base(&self, base: u64) -> Result<Qty, SpecError> {
        if !base.is_multiple_of(self.lot_size) {
            return Err(SpecError::QtyOffLot {
                qty: Decimal::from_i128_with_scale(i128::from(base), self.qty_scale),
                lot_size: self.lot_decimal(),
            });
        }
        Ok(Qty(base))
    }

    /// Snap an off-grid price onto the lattice in a named direction.
    ///
    /// Offered so a gateway *can* round and own that decision explicitly. The
    /// book itself never calls this: it rejects. Rounding inside the engine
    /// would change the economics of someone's order without telling them, and
    /// — the point that matters here — it would relocate the normalization
    /// assumption one layer down rather than enforcing anything.
    pub fn round_price(&self, d: Decimal, rounding: Rounding) -> Result<Price, SpecError> {
        let (floor_minor, has_remainder) = match split_minor(d, self.price_scale) {
            Ok(split) => split,
            Err(Sign::Negative) => return Err(SpecError::PriceNegative(d)),
            Err(Sign::TooLarge) => return Err(SpecError::PriceOutOfRange(d)),
        };

        let up = match rounding {
            Rounding::Down => false,
            Rounding::Up => true,
            // A bid is passive when it is lower; an ask when it is higher.
            Rounding::TowardPassive(Side::Bid) => false,
            Rounding::TowardPassive(Side::Ask) => true,
        };

        let down_tick = floor_minor / self.tick_size;
        let exact = floor_minor.is_multiple_of(self.tick_size) && !has_remainder;

        let minor = if up && !exact {
            down_tick
                .checked_add(1)
                .and_then(|t| t.checked_mul(self.tick_size))
                .ok_or(SpecError::PriceOutOfRange(d))?
        } else {
            down_tick * self.tick_size
        };

        Ok(Price(minor))
    }

    // ------------------------------------------------------- off the grid

    pub fn to_decimal(&self, price: Price) -> Decimal {
        Decimal::from_i128_with_scale(i128::from(price.0), self.price_scale)
    }

    pub fn qty_to_decimal(&self, qty: Qty) -> Decimal {
        Decimal::from_i128_with_scale(i128::from(qty.0), self.qty_scale)
    }

    pub fn tick_decimal(&self) -> Decimal {
        Decimal::from_i128_with_scale(i128::from(self.tick_size), self.price_scale)
    }

    pub fn lot_decimal(&self) -> Decimal {
        Decimal::from_i128_with_scale(i128::from(self.lot_size), self.qty_scale)
    }

    // ----------------------------------------------------------- lattice math

    /// How many ticks apart two prices are. Exact and order-independent: both
    /// arguments are on the grid by construction, so their difference is a
    /// whole number of ticks and the division cannot leave a remainder.
    pub fn ticks_between(&self, a: Price, b: Price) -> Ticks {
        let gap = a.0.abs_diff(b.0);
        Ticks(gap / self.tick_size)
    }

    /// Price × quantity, widened. See [`Notional`] for why `u128` is exactly
    /// the right width.
    pub fn notional(&self, price: Price, qty: Qty) -> Notional {
        Notional(u128::from(price.0) * u128::from(qty.0))
    }
}

/// Why a decimal could not become an unsigned integer at all, before any
/// question of ticks or lots arises.
enum Sign {
    Negative,
    TooLarge,
}

/// Decompose `d` onto the grid of `10^-scale`, exactly.
///
/// Returns the floor in minor units, plus whether anything was left over below
/// that grid — so a caller demanding exactness can reject, and a caller that
/// rounds has the floor already in hand.
///
/// The arithmetic runs on the mantissa, NOT by multiplying or dividing
/// `Decimal`s. `d * 10^scale` then `.fract() == 0` looks equivalent and is not:
/// `Decimal` multiplication can round at the 28th digit, so that test answers a
/// question about the product rather than about the input. Here `d = m / 10^s`
/// with `m` and `s` taken straight off the value, and the only operations are
/// an exact integer multiply or an exact integer divide with its remainder.
fn split_minor(d: Decimal, scale: u32) -> Result<(u64, bool), Sign> {
    let mantissa = d.mantissa();
    if mantissa < 0 {
        return Err(Sign::Negative);
    }

    let value_scale = d.scale();
    let (quotient, remainder) = if scale >= value_scale {
        // Coarser grid than the value needs: shifting left is exact, and there
        // is nothing below the grid by definition.
        let shift = pow10(scale - value_scale).ok_or(Sign::TooLarge)?;
        (mantissa.checked_mul(shift).ok_or(Sign::TooLarge)?, 0)
    } else {
        // The value is finer than the grid. The remainder is precisely the part
        // that does not fit — a price of `100.001` on a cent grid lands here.
        let shift = pow10(value_scale - scale).ok_or(Sign::TooLarge)?;
        (mantissa / shift, mantissa % shift)
    };

    let minor = u64::try_from(quotient).map_err(|_| Sign::TooLarge)?;
    Ok((minor, remainder != 0))
}

/// `10^n` as an `i128`, or `None` past what the type holds.
fn pow10(n: u32) -> Option<i128> {
    if n > 38 {
        return None;
    }
    10i128.checked_pow(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rust_decimal::prelude::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).expect("test literal parses")
    }

    /// Quarter-tick prices in cents, quantities in lots of five — the spec the
    /// unit-lot tests cannot exercise.
    fn quarter_lot_five() -> InstrumentSpec {
        InstrumentSpec::new(2, 0, 25, 5).expect("valid spec")
    }

    // ------------------------------------------------------ spec validation

    #[test]
    fn a_zero_tick_is_not_a_spec() {
        assert!(matches!(
            InstrumentSpec::new(2, 0, 0, 1),
            Err(SpecError::InvalidSpec(_))
        ));
    }

    #[test]
    fn a_zero_lot_is_not_a_spec() {
        assert!(matches!(
            InstrumentSpec::new(2, 0, 1, 0),
            Err(SpecError::InvalidSpec(_))
        ));
    }

    #[test]
    fn a_scale_decimal_cannot_carry_is_not_a_spec() {
        assert!(matches!(
            InstrumentSpec::new(29, 0, 1, 1),
            Err(SpecError::InvalidSpec(_))
        ));
    }

    /// The bounds default to one tick and one lot, so zero is rejected without
    /// anyone having to remember to forbid it.
    #[test]
    fn bounds_default_to_one_tick_and_one_lot() {
        let spec = quarter_lot_five();

        assert_eq!(spec.min_price(), spec.price(dec("0.25")).unwrap());
        assert_eq!(spec.min_qty(), spec.qty(dec("5")).unwrap());
        assert_eq!(spec.max_price(), None);
        assert_eq!(spec.max_qty(), None);
    }

    #[test]
    fn a_bound_off_its_own_grid_is_not_a_spec() {
        let spec = quarter_lot_five();
        let off_tick = Price::from_minor_unchecked(3);

        assert!(matches!(
            spec.with_price_range(off_tick, None),
            Err(SpecError::InvalidSpec(_))
        ));
    }

    #[test]
    fn a_max_below_its_min_is_not_a_spec() {
        let spec = quarter_lot_five();
        let low = spec.price(dec("10.00")).unwrap();
        let high = spec.price(dec("20.00")).unwrap();

        assert!(matches!(
            spec.with_price_range(high, Some(low)),
            Err(SpecError::InvalidSpec(_))
        ));
    }

    // ----------------------------------------------------------- conversion

    #[test]
    fn a_price_on_the_grid_converts_to_minor_units() {
        let spec = InstrumentSpec::cents();
        assert_eq!(spec.price(dec("100.25")).unwrap().minor(), 10025);
        assert_eq!(spec.price(dec("0.01")).unwrap().minor(), 1);
    }

    /// Trailing zeros are a `Decimal` scale artifact, not information. All
    /// three of these are the same point on the lattice — which is exactly the
    /// collision that used to make a trade's printed scale depend on which
    /// order happened to create the level first.
    #[test]
    fn trailing_zeros_collapse_to_one_price() {
        let spec = InstrumentSpec::cents();

        let a = spec.price(dec("100")).unwrap();
        let b = spec.price(dec("100.0")).unwrap();
        let c = spec.price(dec("100.00")).unwrap();

        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(a.minor(), 10000);
    }

    #[test]
    fn a_price_finer_than_the_minor_unit_is_rejected() {
        let spec = InstrumentSpec::cents();
        assert!(matches!(
            spec.price(dec("100.001")),
            Err(SpecError::PriceTooPrecise { .. })
        ));
    }

    #[test]
    fn a_price_off_the_tick_grid_is_rejected() {
        let spec = quarter_lot_five();
        assert!(matches!(
            spec.price(dec("100.03")),
            Err(SpecError::PriceOffTick { .. })
        ));
        assert!(spec.price(dec("100.25")).is_ok());
    }

    #[test]
    fn a_negative_price_is_rejected() {
        let spec = InstrumentSpec::cents();
        assert!(matches!(
            spec.price(dec("-1.00")),
            Err(SpecError::PriceNegative(_))
        ));
    }

    #[test]
    fn a_price_beyond_u64_is_rejected() {
        let spec = InstrumentSpec::cents();
        assert!(matches!(
            spec.price(dec("1000000000000000000000")),
            Err(SpecError::PriceOutOfRange(_))
        ));
    }

    #[test]
    fn a_quantity_off_the_lot_grid_is_rejected() {
        let spec = quarter_lot_five();
        assert!(matches!(
            spec.qty(dec("7")),
            Err(SpecError::QtyOffLot { .. })
        ));
        assert_eq!(spec.qty(dec("10")).unwrap().base(), 10);
    }

    /// Eight decimal places of quantity is the satoshi case from the comment
    /// this module replaces: one BTC is 100_000_000 base units.
    #[test]
    fn a_satoshi_scale_quantity_converts() {
        let spec = InstrumentSpec::new(2, 8, 1, 1).unwrap();

        assert_eq!(spec.qty(dec("1")).unwrap().base(), 100_000_000);
        assert_eq!(spec.qty(dec("0.00000001")).unwrap().base(), 1);
        assert!(matches!(
            spec.qty(dec("0.000000001")),
            Err(SpecError::QtyTooPrecise { .. })
        ));
    }

    #[test]
    fn the_integer_native_door_still_checks_the_tick() {
        let spec = quarter_lot_five();

        assert_eq!(spec.price_from_minor(10025).unwrap().minor(), 10025);
        assert!(matches!(
            spec.price_from_minor(10003),
            Err(SpecError::PriceOffTick { .. })
        ));
    }

    // ------------------------------------------------------------- rounding

    #[test]
    fn rounding_moves_to_the_named_side_of_the_grid() {
        let spec = quarter_lot_five();

        let down = spec.round_price(dec("100.30"), Rounding::Down).unwrap();
        let up = spec.round_price(dec("100.30"), Rounding::Up).unwrap();

        assert_eq!(spec.to_decimal(down), dec("100.25"));
        assert_eq!(spec.to_decimal(up), dec("100.50"));
    }

    /// Sub-minor-unit dust still counts as "not on the grid" — `100.2500001`
    /// must round UP to `100.50`, not sit still at `100.25`. Getting this wrong
    /// is the classic off-by-a-hair bug.
    #[test]
    fn rounding_up_notices_dust_below_the_minor_unit() {
        let spec = quarter_lot_five();

        let up = spec.round_price(dec("100.2500001"), Rounding::Up).unwrap();
        assert_eq!(spec.to_decimal(up), dec("100.50"));
    }

    #[test]
    fn rounding_leaves_a_price_already_on_the_grid_alone() {
        let spec = quarter_lot_five();

        for rounding in [Rounding::Down, Rounding::Up] {
            let p = spec.round_price(dec("100.25"), rounding).unwrap();
            assert_eq!(spec.to_decimal(p), dec("100.25"));
        }
    }

    /// Passive means away from the touch, and which way that is depends on the
    /// side: a bid gets cheaper, an ask gets dearer. Neither ever improves the
    /// sender's price.
    #[test]
    fn passive_rounding_depends_on_the_side() {
        let spec = quarter_lot_five();

        let bid = spec
            .round_price(dec("100.30"), Rounding::TowardPassive(Side::Bid))
            .unwrap();
        let ask = spec
            .round_price(dec("100.30"), Rounding::TowardPassive(Side::Ask))
            .unwrap();

        assert_eq!(spec.to_decimal(bid), dec("100.25"));
        assert_eq!(spec.to_decimal(ask), dec("100.50"));
    }

    // -------------------------------------------------------- lattice math

    #[test]
    fn ticks_between_counts_the_grid_not_the_currency() {
        let spec = quarter_lot_five();
        let bid = spec.price(dec("100.00")).unwrap();
        let ask = spec.price(dec("100.50")).unwrap();

        // Half a dollar, but the floor calls it two ticks.
        assert_eq!(spec.ticks_between(ask, bid), Ticks::from_count(2));
        assert_eq!(spec.ticks_between(bid, ask), Ticks::from_count(2));
    }

    #[test]
    fn notional_multiplies_the_two_lattices() {
        let spec = InstrumentSpec::cents();
        let price = spec.price(dec("100.25")).unwrap();
        let qty = spec.qty(dec("3")).unwrap();

        assert_eq!(spec.notional(price, qty).raw(), 10025 * 3);
    }

    /// The widest product two `u64`s can make still fits, with one bit to
    /// spare. If `Notional` were `u64` this would wrap silently.
    #[test]
    fn notional_survives_the_widest_possible_product() {
        let spec = InstrumentSpec::cents();
        let price = Price::from_minor_unchecked(u64::MAX);
        let qty = Qty::from_base_unchecked(u64::MAX);

        let expected = u128::from(u64::MAX) * u128::from(u64::MAX);
        assert_eq!(spec.notional(price, qty).raw(), expected);
        assert!(expected < u128::MAX);
    }

    // ------------------------------------------------------------ proptests

    proptest! {
        /// Round-tripping is where the mantissa arithmetic earns its keep: if
        /// `split_minor` ever rounded, drifted, or mishandled a scale, a value
        /// would come back as a different point on the lattice.
        #[test]
        fn a_lattice_price_round_trips_through_decimal(ticks in 0u64..100_000) {
            let spec = quarter_lot_five();
            let price = spec.price_from_minor(ticks * spec.tick_size()).unwrap();

            prop_assert_eq!(spec.price(spec.to_decimal(price)).unwrap(), price);
        }

        #[test]
        fn a_lattice_quantity_round_trips_through_decimal(lots in 0u64..100_000) {
            let spec = quarter_lot_five();
            let qty = spec.qty_from_base(lots * spec.lot_size()).unwrap();

            prop_assert_eq!(spec.qty(spec.qty_to_decimal(qty)).unwrap(), qty);
        }

        /// Anything strictly between two ticks is rejected — never silently
        /// snapped, and never accepted. `offset` never reaches the tick, so
        /// every generated value is genuinely off-grid.
        #[test]
        fn a_price_between_two_ticks_is_always_rejected(
            ticks in 0u64..100_000,
            offset in 1u64..25,
        ) {
            let spec = quarter_lot_five();
            let off_grid = Decimal::from_i128_with_scale(
                i128::from(ticks * spec.tick_size() + offset),
                spec.price_scale(),
            );

            let rejected = matches!(spec.price(off_grid), Err(SpecError::PriceOffTick { .. }));
            prop_assert!(rejected, "{off_grid} sits between two ticks and must not be accepted");
        }

        /// Rounding is total on non-negative input, lands on the grid, and
        /// lands on the correct side of the input — the three things a caller
        /// relies on when they opt into it.
        #[test]
        fn rounding_always_lands_on_the_grid(
            minor in 0u64..10_000_000,
            up in any::<bool>(),
        ) {
            let spec = quarter_lot_five();
            let d = Decimal::from_i128_with_scale(i128::from(minor), spec.price_scale());
            let rounding = if up { Rounding::Up } else { Rounding::Down };

            let rounded = spec.round_price(d, rounding).unwrap();

            prop_assert!(rounded.minor().is_multiple_of(spec.tick_size()));
            if up {
                prop_assert!(rounded.minor() >= minor);
                prop_assert!(rounded.minor() - minor < spec.tick_size());
            } else {
                prop_assert!(rounded.minor() <= minor);
                prop_assert!(minor - rounded.minor() < spec.tick_size());
            }
        }
    }
}
