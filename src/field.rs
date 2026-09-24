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
/// - `Num` is a value converted to a number, either explicitly via `num()` or
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
    /// is what serialization and string comparisons see, so a `str()` is never
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

    /// [`Field::coerce_num`] without the error value: empty is `0.0`, text is
    /// `None`. The per-row "soft" numeric read, so it never allocates.
    #[inline]
    pub fn num_soft(&self) -> Option<f64> {
        match self {
            Field::Num(n) => Some(*n),
            Field::Str(s) => parse_num_soft(s),
            Field::Owned(s) => parse_num_soft(s),
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
    parse_num_soft(s).ok_or_else(|| NumError(s.to_owned()))
}

/// [`parse_num`] without the error value: empty is `0.0`, text is `None`.
#[inline]
fn parse_num_soft(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        Some(0.0)
    } else {
        t.parse::<f64>().ok()
    }
}

/// Format a number the way csvm does: in plain notation, to 15 significant
/// digits and at least six decimals, but no decimal past the 17th
/// significant digit (17 tell every double apart), with trailing zeros and
/// a trailing decimal point trimmed. So `25.0 -> "25"`, `25.5 -> "25.5"`,
/// `1e-7 -> "0.0000001"`, `0.1 + 0.2` prints `0.3` (the float's own noise
/// past the 15th digit left out), and `1727136000.123456` keeps its six
/// decimals. From 1e9 up the decimals past the 15th digit can show that
/// noise, and from 1e17 up a number prints its whole value. NaN and inf
/// print as `NaN`, `inf` and `-inf`.
pub fn format_num(n: f64) -> String {
    let mut s = String::new();
    format_num_into(n, &mut s);
    s
}

/// [`format_num`] appended to `buf` (no allocation once `buf` has room).
pub fn format_num_into(n: f64, buf: &mut String) {
    use std::fmt::Write;
    /// Digits a double holds for certain.
    const SIGNIFICANT: i32 = 15;
    /// Decimals a number keeps, up to `MAX_SIGNIFICANT` digits.
    const MIN_DECIMALS: i32 = 6;
    /// Digits that tell every double apart.
    const MAX_SIGNIFICANT: i32 = 17;
    // A whole number that small prints as an integer (and fast); `-0.0`
    // keeps its sign below.
    if n.fract() == 0.0 && n.abs() < 1e15 && (n != 0.0 || n.is_sign_positive()) {
        write!(buf, "{}", n as i64).unwrap();
        return;
    }
    if !n.is_finite() {
        write!(buf, "{n}").unwrap();
        return;
    }
    // The number's exponent. `log10` can land on the wrong side of a power
    // of ten, so close to one it is read off the scientific form instead.
    let log = n.abs().log10();
    let exponent = if (log - log.round()).abs() > 1e-9 {
        log.floor() as i32
    } else {
        let start = buf.len();
        write!(buf, "{n:e}").unwrap();
        let exponent = buf[start..]
            .rsplit_once('e')
            .and_then(|(_, e)| e.parse().ok())
            .unwrap_or(0);
        buf.truncate(start);
        exponent
    };
    // Past 17 significant digits every double is told apart already.
    let decimals = (SIGNIFICANT - 1 - exponent)
        .max(MIN_DECIMALS)
        .min(MAX_SIGNIFICANT - 1 - exponent)
        .max(0) as usize;
    write!(buf, "{n:.decimals$}").unwrap();
    // With decimals the text holds a '.', so trimming the fractional zeros
    // and then the dot can never eat into the integer part or the text
    // before it.
    if decimals > 0 {
        let end = buf.trim_end_matches('0').trim_end_matches('.').len();
        buf.truncate(end);
    }
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
        // 15 significant digits, however small.
        assert_eq!(format_num(1.0 / 3.0), "0.333333333333333");
        assert_eq!(format_num(1e-7), "0.0000001");
        assert_eq!(format_num(-2.5e-10), "-0.00000000025");
        assert_eq!(format_num(123456.789), "123456.789");
        assert_eq!(format_num(0.1 + 0.2), "0.3");
        assert_eq!(format_num(1e20), "100000000000000000000");
        assert_eq!(format_num(100.0), "100");
        assert_eq!(format_num(-0.0), "-0");
        assert_eq!(format_num(-42.0), "-42");
        assert_eq!(format_num(999_999_999_999_999.0), "999999999999999");
        assert_eq!(format_num(1e15), "1000000000000000");
        // Just under a power of ten, and rounding up to one.
        assert_eq!(format_num(9.999999999999994e-5), "0.0000999999999999999");
        assert_eq!(format_num(1e-4_f64.next_down()), "0.0001");
        // Never fewer than six decimals, so big values stay apart.
        assert_eq!(format_num(1727136000.123456), "1727136000.123456");
        assert_eq!(format_num(1727136000.123461), "1727136000.123461");
        assert_eq!(format_num(1e15 + 0.5), "1000000000000000.5");
        // No decimal past the 17th digit; a big number prints its whole value.
        assert_eq!(format_num(1e11 + 0.1), "100000000000.10001");
        assert_eq!(format_num(1e20 + 0.5), "100000000000000000000");
    }

    #[test]
    fn format_num_into_appends() {
        // Trimming stops at the appended number's own decimal point, so a
        // prefix ending in `0` survives; NaN/inf have nothing to trim.
        let mut s = String::from("x=");
        format_num_into(25.0, &mut s);
        assert_eq!(s, "x=25");
        let mut s = String::from("10");
        format_num_into(0.0, &mut s);
        assert_eq!(s, "100");
        let mut s = String::from("n=");
        format_num_into(f64::NAN, &mut s);
        assert_eq!(s, "n=NaN");
        let mut s = String::new();
        format_num_into(f64::INFINITY, &mut s);
        assert_eq!(s, "inf");
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
    fn num_soft_reads_blank_as_zero_and_text_as_none() {
        assert_eq!(Field::Str("").num_soft(), Some(0.0));
        assert_eq!(Field::Str("  ").num_soft(), Some(0.0));
        assert_eq!(Field::Str(" 5 ").num_soft(), Some(5.0));
        assert_eq!(Field::Str("hello").num_soft(), None);
        assert_eq!(Field::Num(3.5).num_soft(), Some(3.5));
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
