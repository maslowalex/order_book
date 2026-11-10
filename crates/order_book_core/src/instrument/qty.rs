/// A quantity in base units of the asset — satoshis for BTC, whole shares for
/// an equity quoted in shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Qty(pub(super) u64);

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
