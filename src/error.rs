//! The crate's unified error type.

use crate::field::NumError;
use std::fmt;

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
