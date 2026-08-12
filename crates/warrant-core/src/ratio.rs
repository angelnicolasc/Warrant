//! An exact ratio.
//!
//! Coverage is reported as a fraction with both terms visible — `14 of 37`
//! rather than `0.378`. A percentage alone hides whether the denominator was
//! 3 or 3000, and the size of the denominator is most of the information.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A non-negative rational, stored unreduced so the original counts survive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ratio {
    /// The part.
    pub numerator: u64,
    /// The whole.
    pub denominator: u64,
}

impl Ratio {
    /// A ratio of zero over zero — the honest value when there was nothing to
    /// measure. Renders as `n/a`, never as `0%`.
    pub const UNDEFINED: Ratio = Ratio { numerator: 0, denominator: 0 };

    /// Build a ratio. The numerator is clamped to the denominator, because a
    /// coverage above 1 is always a bug in the caller rather than a finding.
    pub fn new(numerator: u64, denominator: u64) -> Self {
        Ratio { numerator: numerator.min(denominator), denominator }
    }

    /// Whether there was anything to measure.
    pub fn is_defined(&self) -> bool {
        self.denominator > 0
    }

    /// As a fraction in `[0, 1]`, or `None` if the denominator is zero.
    pub fn as_f64(&self) -> Option<f64> {
        (self.denominator > 0).then(|| self.numerator as f64 / self.denominator as f64)
    }

    /// Percentage rounded to the nearest integer, or `None` if undefined.
    pub fn percent(&self) -> Option<u32> {
        self.as_f64().map(|f| (f * 100.0).round() as u32)
    }
}

impl fmt::Display for Ratio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.percent() {
            Some(p) => write!(f, "{p}%"),
            None => write!(f, "n/a"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undefined_is_not_zero_percent() {
        assert_eq!(Ratio::UNDEFINED.percent(), None);
        assert_eq!(Ratio::UNDEFINED.to_string(), "n/a");
        assert_eq!(Ratio::new(0, 1).to_string(), "0%");
    }

    #[test]
    fn rounds_to_nearest_percent() {
        assert_eq!(Ratio::new(14, 37).percent(), Some(38));
        assert_eq!(Ratio::new(1, 3).percent(), Some(33));
        assert_eq!(Ratio::new(2, 3).percent(), Some(67));
    }

    #[test]
    fn coverage_cannot_exceed_the_whole() {
        assert_eq!(Ratio::new(9, 4), Ratio { numerator: 4, denominator: 4 });
    }
}
