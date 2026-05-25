//! Deterministic 10-column CSV generator for benchmarking csvm.
//!
//! Same row count always produces the same file (values come from a splitmix64
//! hash of the row index, not an RNG), so benchmark numbers are reproducible.
//!
//! ```sh
//! cargo run --release --example gen_csv -- 3000000 /tmp/huge.csv
//! cargo run --release --example gen_csv -- 1000000 > data.csv
//! ```
//!
//! Columns: id, grp, region, name, flag, amount, price, qty, status, score —
//! a mix of integers, a float, categoricals, a `t`/`f` flag, and a `name<N>`
//! field (handy for regex), so every csvm feature has something to chew on.

use std::env;
use std::fs::File;
use std::io::{self, BufWriter, Write};

fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let rows: u64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3_000_000);
    let mut out: Box<dyn Write> = match args.next() {
        Some(path) => Box::new(BufWriter::new(File::create(path)?)),
        None => Box::new(BufWriter::new(io::stdout().lock())),
    };

    let regions = ["north", "south", "east", "west"];
    let statuses = ["active", "inactive", "pending"];

    let mut line = String::new();
    out.write_all(b"id,grp,region,name,flag,amount,price,qty,status,score\n")?;
    for i in 0..rows {
        let h = splitmix64(i);
        let grp = i % 7;
        let region = regions[(h % 4) as usize];
        let name_n = h % 1000;
        let flag = if h & 1 == 0 { "t" } else { "f" };
        let amount = h % 100_000;
        let price = (h % 100_000) as f64 / 100.0; // 0.00 .. 999.99
        let qty = (h >> 8) % 500;
        let status = statuses[(h % 3) as usize];
        let score = (h >> 16) % 101;

        line.clear();
        use std::fmt::Write as _;
        let _ = writeln!(
            line,
            "{i},{grp},{region},name{name_n},{flag},{amount},{price:.2},{qty},{status},{score}"
        );
        out.write_all(line.as_bytes())?;
    }
    out.flush()
}

/// splitmix64: a pure function of the index, so the file is reproducible.
fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
