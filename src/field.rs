//! The in-row value representation.
//!
//! A [`Field`] is one CSV cell while it flows through the pipeline. The three
//! variants exist purely for performance: the common case is a borrowed slice
//! straight out of the chunk buffer (no allocation), and we only pay for an
//! owned string or a parsed number when an operation forces it.

use std::borrow::Cow;
use std::fmt;

/// A single CSV cell.
///
/// - `Str` borrows from the chunk buffer — the zero-copy fast path.
/// - `Owned` is allocated: an unescaped quoted field, or a field that had to
///   cross a thread/stage boundary (see [`Field::into_owned`]).
/// - `Num` is a value converted to a number, either explicitly via `to-num` or
///   implicitly by a numeric comparison.
#[derive(Clone, Debug)]
pub enum Field<'a> {
    Str(&'a str),
    Owned(String),
    Num(f64),
}

/// A non-numeric value reached an operation that requires a number. Carries the
/// offending text so the CLI can report it (mirrors csvm's `to_num` error).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumError(pub String);

impl fmt::Display for NumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "non-numeric value '{}'", self.0)
    }
}

impl<'a> Field<'a> {
    /// View the field as text. Numbers are formatted with [`format_num`]; this
    /// is what serialization and string comparisons see, so a `to-str` is never
    /// needed just to print a converted number.
    #[inline]
    pub fn as_str(&self) -> Cow<'_, str> {
        match self {
            Field::Str(s) => Cow::Borrowed(s),
            Field::Owned(s) => Cow::Borrowed(s.as_str()),
            Field::Num(n) => Cow::Owned(format_num(*n)),
        }
    }

    /// Coerce to `f64` for numeric operations. Empty (after trimming) is `0.0`,
    /// matching csvm's `to_num`; anything non-numeric is a [`NumError`].
    #[inline]
    pub fn coerce_num(&self) -> Result<f64, NumError> {
        match self {
            Field::Num(n) => Ok(*n),
            Field::Str(s) => parse_num(s),
            Field::Owned(s) => parse_num(s),
        }
    }

    /// The cell as a number, or `None` when it is empty or not a number — the
    /// strict form of [`Field::coerce_num`], which reads empty as `0.0`.
    #[inline]
    pub fn num_opt(&self) -> Option<f64> {
        match self {
            Field::Num(n) => Some(*n),
            Field::Str(s) => s.trim().parse().ok(),
            Field::Owned(s) => s.trim().parse().ok(),
        }
    }

    /// Detach from the chunk buffer so the field can outlive it (used when a row
    /// crosses a thread or stage boundary, e.g. into a `sort`).
    #[inline]
    pub fn into_owned(self) -> Field<'static> {
        match self {
            Field::Str(s) => Field::Owned(s.to_owned()),
            Field::Owned(s) => Field::Owned(s),
            Field::Num(n) => Field::Num(n),
        }
    }
}

#[inline]
fn parse_num(s: &str) -> Result<f64, NumError> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(0.0);
    }
    t.parse::<f64>().map_err(|_| NumError(s.to_owned()))
}

/// Format a number the way csvm does: six decimal places, then trim trailing
/// zeros and a trailing decimal point. So `25.0 -> "25"`, `25.5 -> "25.5"`,
/// `0.1 -> "0.1"`. Non-finite values fall back to Rust's default rendering.
pub fn format_num(n: f64) -> String {
    if !n.is_finite() {
        return n.to_string();
    }
    let mut s = format!("{n:.6}");
    // "{:.6}" always emits a '.', so trimming the fractional zeros and then the
    // dot can never eat into the integer part.
    let end = s.trim_end_matches('0').trim_end_matches('.').len();
    s.truncate(end);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_num_matches_csvm() {
        assert_eq!(format_num(25.0), "25");
        assert_eq!(format_num(50.0), "50");
        assert_eq!(format_num(25.5), "25.5");
        assert_eq!(format_num(0.1), "0.1");
        assert_eq!(format_num(-25.0), "-25");
        assert_eq!(format_num(1_000_000.0), "1000000");
        assert_eq!(format_num(0.0), "0");
        // 1/3 -> six decimals, no trailing-zero trim needed
        assert_eq!(format_num(1.0 / 3.0), "0.333333");
    }

    #[test]
    fn coerce_num_rules() {
        assert_eq!(Field::Str("25").coerce_num(), Ok(25.0));
        assert_eq!(Field::Str("  ").coerce_num(), Ok(0.0));
        assert_eq!(Field::Str("").coerce_num(), Ok(0.0));
        assert_eq!(Field::Num(3.5).coerce_num(), Ok(3.5));
        assert_eq!(
            Field::Str("hello").coerce_num(),
            Err(NumError("hello".into()))
        );
    }

    #[test]
    fn num_opt_is_strict_about_empty_and_text() {
        assert_eq!(Field::Str("").num_opt(), None);
        assert_eq!(Field::Str("  ").num_opt(), None);
        assert_eq!(Field::Str("hello").num_opt(), None);
        assert_eq!(Field::Str(" 5 ").num_opt(), Some(5.0));
        assert_eq!(Field::Owned("1e3".into()).num_opt(), Some(1000.0));
        assert_eq!(Field::Num(3.5).num_opt(), Some(3.5));
        // Non-finite values parse as numbers, as they do for coerce_num.
        assert!(Field::Str("NaN").num_opt().is_some_and(f64::is_nan));
        assert_eq!(Field::Str("inf").num_opt(), Some(f64::INFINITY));
    }

    #[test]
    fn as_str_views() {
        assert_eq!(Field::Str("hi").as_str(), "hi");
        assert_eq!(Field::Owned("hi".into()).as_str(), "hi");
        assert_eq!(Field::Num(42.0).as_str(), "42");
    }

    #[test]
    fn into_owned_detaches() {
        let s = String::from("borrowed");
        let f = Field::Str(&s);
        let owned: Field<'static> = f.into_owned();
        assert_eq!(owned.as_str(), "borrowed");
    }
}
