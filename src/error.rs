//! The crate's unified error type.

use crate::field::NumError;
use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// The script could not be compiled into a plan: a malformed form, an
    /// unknown verb/operator, or a non-numeric literal where a number is
    /// required. Carries a human-readable message.
    Compile(String),
    /// A column referenced by the script is not present in the header.
    Column(String),
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
            Error::Column(name) => write!(f, "column not found in header: {name}"),
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

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
