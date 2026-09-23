//! The crate's unified error type.

use crate::field::NumError;
use std::fmt;
use std::ops::Range;
use unicode_width::UnicodeWidthStr;

#[derive(Debug)]
pub enum Error {
    /// The script could not be compiled into a plan: a malformed form, an
    /// unknown verb/operator, or a non-numeric literal where a number is
    /// required. Carries a human-readable message.
    Compile(String),
    /// A column referenced by the script is not present in the header. Carries
    /// the available column names so the message can suggest a near match.
    Column {
        name: String,
        available: Vec<String>,
    },
    /// A column referenced by position (a bare integer) is outside the header.
    /// Carries the text as typed and the columns there are.
    ColumnIndex {
        index: String,
        available: Vec<String>,
    },
    /// A numeric operation reached a non-numeric value at runtime.
    Num(NumError),
    /// I/O failure reading input or writing output.
    Io(std::io::Error),
    /// A runtime condition that isn't one of the above (e.g. a missing header).
    Other(String),
    /// `error`, at `span`: a byte range of the script it was found in. It
    /// displays as `error` alone; [`excerpt`] shows the place.
    At {
        error: Box<Error>,
        span: Range<usize>,
    },
}

impl Error {
    /// `self` placed at `span` in the script, unless it already has a place:
    /// the first place given is the most precise one.
    pub fn at(self, span: Range<usize>) -> Error {
        match self {
            Error::At { .. } => self,
            error => Error::At {
                error: Box::new(error),
                span,
            },
        }
    }

    /// Where in the script the error was found, when that is known.
    pub fn span(&self) -> Option<Range<usize>> {
        match self {
            Error::At { span, .. } => Some(span.clone()),
            _ => None,
        }
    }

    /// The error itself, without its place.
    pub fn unplaced(&self) -> &Error {
        match self {
            Error::At { error, .. } => error,
            error => error,
        }
    }
}

/// The line of `script` that `span` starts on, with a `^` under each column
/// of the span (at least one, and none past the end of the line), for an
/// error message. A script of several lines numbers the line; `color` paints
/// the markers bold red.
pub fn excerpt(script: &str, span: Range<usize>, color: bool) -> String {
    let start = span.start.min(script.len());
    let line_start = script[..start].rfind('\n').map_or(0, |i| i + 1);
    let mut line_end = script[start..]
        .find('\n')
        .map_or(script.len(), |i| start + i);
    // A CRLF line's `\r` is not part of what it shows.
    if line_end > start && script[..line_end].ends_with('\r') {
        line_end -= 1;
    }
    let end = span.end.clamp(start, line_end);
    // A tab would take a terminal's own width; show it as one space so the
    // markers stay under what they mark.
    let shown = |s: &str| s.replace('\t', " ");
    let line = shown(&script[line_start..line_end]);
    let lead = UnicodeWidthStr::width(shown(&script[line_start..start]).as_str());
    let marks = UnicodeWidthStr::width(shown(&script[start..end]).as_str()).max(1);
    let mut markers = "^".repeat(marks);
    if color {
        let style = crate::color::parse_style("bold+red").expect("a valid colour spec");
        // A base colour is the same escape at any depth.
        markers = style.paint(&markers, crate::color::Depth::Ansi256);
    }
    if script.trim_end_matches('\n').contains('\n') {
        let number = script[..line_start].matches('\n').count() + 1;
        let gutter = number.to_string().len();
        format!("{number} | {line}\n{:gutter$} | {:lead$}{markers}", "", "")
    } else {
        format!("  {line}\n  {:lead$}{markers}", "")
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Compile(msg) => write!(f, "{msg}"),
            Error::Column { name, available } => {
                write!(f, "column not found: {name}")?;
                if let Some(s) = did_you_mean(name, available) {
                    write!(f, " (did you mean `{s}`?)")?;
                }
                write!(f, " — have: {}", preview(available))
            }
            Error::ColumnIndex { index, available } => write!(
                f,
                "column index {index} is out of range — have {} columns ({})",
                available.len(),
                preview(available)
            ),
            Error::Num(e) => write!(f, "{e}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Other(msg) => write!(f, "{msg}"),
            Error::At { error, .. } => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<NumError> for Error {
    fn from(e: NumError) -> Self {
        Error::Num(e)
    }
}

/// The closest candidate to `target` by edit distance, if one is near enough to
/// be a plausible typo (within a third of the longer length, min 1). Powers the
/// "did you mean …?" hint for unknown columns and commands.
pub fn did_you_mean<'a, S: AsRef<str>>(target: &str, candidates: &'a [S]) -> Option<&'a str> {
    candidates
        .iter()
        .map(|c| (levenshtein(target, c.as_ref()), c.as_ref()))
        .min_by_key(|(d, _)| *d)
        .filter(|(d, c)| *d <= (target.len().max(c.len()) / 3).max(1))
        .map(|(_, c)| c)
}

/// A short list of `names` for an error message: all of them if few, else the
/// first several with a "+N more" tail.
pub(crate) fn preview(names: &[String]) -> String {
    const SHOW: usize = 8;
    if names.len() <= SHOW {
        names.join(", ")
    } else {
        format!(
            "{}, … (+{} more)",
            names[..SHOW].join(", "),
            names.len() - SHOW
        )
    }
}

/// Levenshtein edit distance (two-row DP).
fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_place_is_kept_once_given_and_does_not_show_in_the_message() {
        let e = Error::Compile("bad".into()).at(3..5).at(0..9);
        assert_eq!(e.span(), Some(3..5));
        assert_eq!(e.to_string(), "bad");
        assert!(matches!(e.unplaced(), Error::Compile(m) if m == "bad"));
        assert_eq!(Error::Compile("x".into()).span(), None);
    }

    #[test]
    fn excerpt_marks_the_span_on_its_line() {
        assert_eq!(
            excerpt("select a >> 1", 10..11, false),
            "  select a >> 1\n            ^"
        );
        // A span with no width still gets a marker: the end of the text.
        assert_eq!(
            excerpt("select a >", 10..10, false),
            "  select a >\n            ^"
        );
        // Wide glyphs before the span count two columns.
        assert_eq!(excerpt("袋 >> 1", 4..6, false), "  袋 >> 1\n     ^^");
        // A script of several lines numbers the line the span is on.
        assert_eq!(
            excerpt("cols a\nselect a >> 1\nfmt", 16..17, false),
            "2 | select a >> 1\n  |          ^"
        );
        // A CRLF script shows its lines without the `\r`.
        assert_eq!(
            excerpt("cols a\r\nselect a >> 1\r\nfmt", 17..24, false),
            "2 | select a >> 1\n  |          ^^^^"
        );
        assert_eq!(
            excerpt("select a >> 1", 10..11, true),
            "  select a >> 1\n            \x1b[1;31m^\x1b[0m"
        );
    }

    #[test]
    fn did_you_mean_finds_near_typos() {
        let cols = [
            "amount".to_string(),
            "region".to_string(),
            "price".to_string(),
        ];
        assert_eq!(did_you_mean("amont", &cols), Some("amount"));
        assert_eq!(did_you_mean("regin", &cols), Some("region"));
        // Too far to be a plausible typo.
        assert_eq!(did_you_mean("zzzzzz", &cols), None);
    }
}
