//! csvm — a fast, multithreaded CSV manipulation tool with a pipe command
//! language.
//!
//! A script (`cols a,b | select "amount > 1000" | sort amount=nr`) is parsed
//! into a plain-Rust execution plan once at startup, then run over the rows
//! with no interpreter in the hot path. See `CLAUDE.md` for the architecture.

pub mod chart;
pub mod cli;
pub mod color;
pub mod csv;
pub mod datetime;
pub mod error;
pub mod exec;
pub mod field;
pub mod graph;
pub mod help;
#[cfg(feature = "parquet")]
pub mod parquet;
pub mod parse;
pub mod plan;
pub mod sort;
pub mod stats;
pub mod svg;
pub mod term;

pub use error::Error;

/// The crate version, surfaced by the CLI.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
