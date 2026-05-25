//! csvm — a multithreaded CSV manipulation tool with a Lisp command language.
//!
//! A script is compiled by `tulisp` into a plain-Rust execution plan once at
//! startup, then run over the rows with no Lisp in the hot path. See
//! `CLAUDE.md` for the full architecture.

pub mod csv;
pub mod error;
pub mod field;
pub mod plan;

pub use error::Error;

/// The crate version, surfaced by the CLI.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
