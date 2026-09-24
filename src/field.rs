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
        trim_decimals(buf);
    }
}

/// A number cell as a `fmt` table shows it, or `None` when the cell shows as
/// written. A cell with more than `decimals` digits after its point is
/// rounded to that many (`None` keeps every digit), its trailing zeros
/// dropped: `3.14159265` shows `3.141593` and `2.50000000` shows `2.5`, while
/// `2.50` stays as written. A half rounds away from zero in a cell with no
/// exponent and up to 15 significant digits, zeros at the end not counted
/// (`2.675` to two decimals shows `2.68`); any other number rounds as its
/// float does (`2.675e0` shows `2.67`).
/// A cell with an exponent stays as written unless rounding changes
/// its value: `1e5` stays, `1e-7` shows `0`. With `human`, a number whose
/// absolute value is 1000 or more once rounded takes a suffix instead:
/// `1234567` shows `1.23M`, and one too big for E shows an exponent,
/// `1.23e300`. A cell that is not a finite number shows as written.
pub fn table_num(cell: &str, decimals: Option<u8>, human: bool) -> Option<String> {
    let n = Field::Str(cell).num_opt().filter(|n| n.is_finite())?;
    let text = cell.trim();
    if human && rounds_to_1000_or_more(text, n, decimals) {
        return Some(human_num(text, n));
    }
    let decimals = usize::from(decimals?);
    let exponent = text.contains(['e', 'E']);
    let written_decimals = text.split_once('.').map_or(0, |(_, f)| f.len());
    if !exponent && written_decimals <= decimals {
        return None;
    }
    let s = round_cell(text, n, decimals);
    (!exponent || s.parse::<f64>() != Ok(n)).then_some(s)
}

/// `text`, whose value is `n`, to at most `decimals` decimals: on its own
/// digits when it is a plain decimal [`Decimal`] can hold, else through `n`.
fn round_cell(text: &str, n: f64, decimals: usize) -> String {
    match Decimal::parse(text) {
        Some(mut d) => {
            d.round_at(d.point + decimals);
            d.to_text()
        }
        None => round_num(n, decimals),
    }
}

/// A cell written as a plain decimal, like `-12.50`, kept as its digits so
/// it rounds as written and not through a float.
struct Decimal {
    negative: bool,
    /// The digits without the point, with the whole part's leading zeros and
    /// the fraction's trailing zeros dropped: `-012.50` keeps `125`.
    digits: Vec<u8>,
    /// How many of `digits` come before the point.
    point: usize,
}

impl Decimal {
    /// Read `text`: a sign, digits and at most one point, and at most 15
    /// significant digits, all of which a float keeps. `None` for anything
    /// else.
    fn parse(text: &str) -> Option<Decimal> {
        let (negative, unsigned) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text.strip_prefix('+').unwrap_or(text)),
        };
        let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
        let all_digits = whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit());
        if whole.is_empty() && fraction.is_empty() || !all_digits {
            return None;
        }
        let whole = whole.trim_start_matches('0');
        let fraction = fraction.trim_end_matches('0');
        let digits = [whole, fraction].concat();
        // Zeros at either end hold no digit a float could lose.
        let significant = digits.trim_matches('0').len();
        (significant <= 15).then_some(Decimal {
            negative,
            digits: digits.into_bytes(),
            point: whole.len(),
        })
    }

    /// Keep the first `keep` digits, a half rounding away from zero.
    fn round_at(&mut self, keep: usize) {
        if keep >= self.digits.len() {
            return;
        }
        let up = self.digits[keep] >= b'5';
        self.digits.truncate(keep);
        if !up {
            return;
        }
        for digit in self.digits.iter_mut().rev() {
            if *digit < b'9' {
                *digit += 1;
                return;
            }
            *digit = b'0';
        }
        // Every kept digit was a 9: 99.96 to one decimal is 100.0.
        self.digits.insert(0, b'1');
        self.point += 1;
    }

    /// The number as text, its trailing zeros dropped, and `0` for one that
    /// rounded to zero from either side.
    fn to_text(&self) -> String {
        let (whole, fraction) = self.digits.split_at(self.point.min(self.digits.len()));
        let mut s = String::new();
        if self.negative && self.digits.iter().any(|&b| b != b'0') {
            s.push('-');
        }
        if whole.is_empty() {
            s.push('0');
        }
        s.extend(whole.iter().map(|&b| char::from(b)));
        s.push('.');
        s.extend(fraction.iter().map(|&b| char::from(b)));
        trim_decimals(&mut s);
        s
    }
}

/// Whether the absolute value of `text`, whose value is `n`, is 1000 or more
/// once rounded to `decimals` (`None` keeps every digit).
fn rounds_to_1000_or_more(text: &str, n: f64, decimals: Option<u8>) -> bool {
    if n.abs() >= 1000.0 {
        return true;
    }
    // Rounding moves a number by half a unit at most, so only one from 999.5
    // up can round up to 1000.
    n.abs() >= 999.5
        && decimals.is_some_and(|d| {
            let rounded = round_cell(text, n, d.into());
            rounded.parse::<f64>().is_ok_and(|r| r.abs() >= 1000.0)
        })
}

/// `n` to at most `decimals` decimals, its trailing zeros dropped, and `0`
/// for a number that rounds to zero from either side. When the shortest text
/// that reads back as `n` fits, it is the answer, so no digit past the
/// float's precision shows: `0.1` and not `0.10000000000000001`.
fn round_num(n: f64, decimals: usize) -> String {
    let shortest = n.to_string();
    let mut s = if shortest.split_once('.').map_or(0, |(_, f)| f.len()) <= decimals {
        shortest
    } else {
        format!("{n:.decimals$}")
    };
    trim_decimals(&mut s);
    if s == "-0" {
        s.remove(0);
    }
    s
}

/// `text`, whose value is `n` and whose absolute value is 1000 or more once
/// rounded, to three significant digits with a k, M, G, T, P or E suffix for
/// each power of 1000. A number too big for E shows three significant digits
/// and an exponent: `1.23e300`.
fn human_num(text: &str, n: f64) -> String {
    const SUFFIXES: [char; 6] = ['k', 'M', 'G', 'T', 'P', 'E'];
    let sign = if n < 0.0 { "-" } else { "" };
    let (mut out, exponent) = three_digits(text, n);
    let power = exponent.div_euclid(3);
    let suffix = usize::try_from(power - 1)
        .ok()
        .and_then(|i| SUFFIXES.get(i));
    let Some(suffix) = suffix else {
        out.insert(1, '.');
        trim_decimals(&mut out);
        return format!("{sign}{out}e{exponent}");
    };
    // `123` with its point after the first, second or third digit.
    out.insert(1 + exponent.rem_euclid(3) as usize, '.');
    trim_decimals(&mut out);
    format!("{sign}{out}{suffix}")
}

/// The absolute value of `text`, whose value is `n`, to three significant
/// digits: the digits, and the power of ten of the first. A plain decimal
/// rounds on its own digits, a half away from zero (see [`Decimal`]);
/// anything else rounds through `n`.
fn three_digits(text: &str, n: f64) -> (String, i32) {
    if let Some(mut d) = Decimal::parse(text)
        && let Some(first) = d.digits.iter().position(|&b| b != b'0')
    {
        d.round_at(first + 3);
        let first = d.digits.iter().position(|&b| b != b'0').unwrap_or(0);
        let mut digits: String = d
            .digits
            .iter()
            .skip(first)
            .take(3)
            .map(|&b| char::from(b))
            .collect();
        while digits.len() < 3 {
            digits.push('0');
        }
        let exponent = d.point as i32 - 1 - first as i32;
        return (digits, exponent);
    }
    let s = format!("{:.2e}", n.abs());
    let (mantissa, exponent) = s.split_once('e').expect("`{:e}` writes an `e`");
    let exponent = exponent.parse().expect("`{:e}` writes a whole exponent");
    (mantissa.replace('.', ""), exponent)
}

/// Drop the zeros at the end of a number's decimals, and then its decimal
/// point. A number without one keeps its zeros.
fn trim_decimals(s: &mut String) {
    if s.contains('.') {
        let end = s.trim_end_matches('0').trim_end_matches('.').len();
        s.truncate(end);
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
    fn table_num_rounds_long_decimals_only() {
        let six = |c| table_num(c, Some(6), false);
        assert_eq!(six("3.14159265358979").as_deref(), Some("3.141593"));
        assert_eq!(six("1234567.891").as_deref(), None);
        assert_eq!(six("0.000012345678").as_deref(), Some("0.000012"));
        assert_eq!(six("0.0000001").as_deref(), Some("0"));
        assert_eq!(six("-0.0000001").as_deref(), Some("0"));
        assert_eq!(six("-2.00000049").as_deref(), Some("-2"));
        assert_eq!(six("0.1234565001").as_deref(), Some("0.123457"));
        assert_eq!(six("1e-7").as_deref(), Some("0"));
        assert_eq!(six("1.5e-7").as_deref(), Some("0"));
        // A cell that needs no rounding keeps its own spelling.
        for kept in ["2.50", "1e5", "007", " 42 ", "-0", "0.123456", "1e-3"] {
            assert_eq!(six(kept), None, "{kept}");
        }
        // A cell with more decimals is rounded, even when that keeps its value.
        assert_eq!(six("2.50000000").as_deref(), Some("2.5"));
        assert_eq!(six("0.10000000000000001").as_deref(), Some("0.1"));
        // No digit past the float's precision shows.
        let p = |c, d| table_num(c, Some(d), false);
        assert_eq!(p("0.100000000000000000", 17).as_deref(), Some("0.1"));
        assert_eq!(p("1234567.10000000000", 10).as_deref(), Some("1234567.1"));
        // A half rounds away from zero, on the cell's own digits.
        assert_eq!(p("2.675", 2).as_deref(), Some("2.68"));
        assert_eq!(p("1.015", 2).as_deref(), Some("1.02"));
        assert_eq!(p("2.5", 0).as_deref(), Some("3"));
        assert_eq!(p("-2.5", 0).as_deref(), Some("-3"));
        assert_eq!(p("99.96", 1).as_deref(), Some("100"));
        assert_eq!(p("-0.4", 0).as_deref(), Some("0"));
        // Up to 15 significant digits round on the text; more go through the
        // float, where an exact half goes to the even digit.
        assert_eq!(p("123456789012.125", 2).as_deref(), Some("123456789012.13"));
        assert_eq!(
            p("1234567890123.125", 2).as_deref(),
            Some("1234567890123.12")
        );
        for text in ["", "abc", "NaN", "inf", "-inf"] {
            assert_eq!(six(text), None, "{text}");
        }
        // No decimal point to trim at: whole numbers keep their zeros.
        assert_eq!(table_num("1234.5", Some(0), false).as_deref(), Some("1235"));
        assert_eq!(table_num("100.4", Some(0), false).as_deref(), Some("100"));
        assert_eq!(
            table_num("3.14159", Some(2), false).as_deref(),
            Some("3.14")
        );
        assert_eq!(table_num("3.14159265358979", None, false), None);
    }

    #[test]
    fn table_num_human_suffixes() {
        let human = |c| table_num(c, Some(6), true);
        assert_eq!(human("1000").as_deref(), Some("1k"));
        assert_eq!(human("1234").as_deref(), Some("1.23k"));
        assert_eq!(human("12345").as_deref(), Some("12.3k"));
        assert_eq!(human("123456").as_deref(), Some("123k"));
        assert_eq!(human("1234567").as_deref(), Some("1.23M"));
        assert_eq!(human("-9876543210").as_deref(), Some("-9.88G"));
        assert_eq!(human("1.5e12").as_deref(), Some("1.5T"));
        assert_eq!(human("2e15").as_deref(), Some("2P"));
        assert_eq!(human("3e18").as_deref(), Some("3E"));
        // Too big for E: an exponent.
        assert_eq!(human("4e21").as_deref(), Some("4e21"));
        assert_eq!(human("9.996e20").as_deref(), Some("1e21"));
        assert_eq!(human("1e300").as_deref(), Some("1e300"));
        assert_eq!(human("1.2345e300").as_deref(), Some("1.23e300"));
        assert_eq!(human("-1.2345e300").as_deref(), Some("-1.23e300"));
        // Rounding up to the next suffix, and to a fourth digit.
        assert_eq!(human("999600").as_deref(), Some("1M"));
        assert_eq!(human("-999999").as_deref(), Some("-1M"));
        assert_eq!(human("9996").as_deref(), Some("10k"));
        assert_eq!(human("9995").as_deref(), Some("10k"));
        // A half rounds away from zero here too.
        assert_eq!(human("1245").as_deref(), Some("1.25k"));
        assert_eq!(human("-1245").as_deref(), Some("-1.25k"));
        assert_eq!(human("1245000000000000").as_deref(), Some("1.25P"));
        assert_eq!(human("99960").as_deref(), Some("100k"));
        // Rounded first: a number that shows as 1000 takes a suffix.
        assert_eq!(human("999.9999999").as_deref(), Some("1k"));
        assert_eq!(human("-999.9999999").as_deref(), Some("-1k"));
        assert_eq!(table_num("999.6", Some(0), true).as_deref(), Some("1k"));
        assert_eq!(table_num("999.5", Some(0), true).as_deref(), Some("1k"));
        assert_eq!(table_num("999.9999999", None, true), None);
        // Below 1000 the decimals rule applies.
        assert_eq!(human("999.5"), None);
        assert_eq!(human("0.00123456789").as_deref(), Some("0.001235"));
        assert_eq!(table_num("1234", None, true).as_deref(), Some("1.23k"));
        assert_eq!(table_num("0.00123456789", None, true), None);
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
