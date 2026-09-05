//! Benchmarks for the compiled-plan hot path.
//!
//! A script is parsed into a [`Plan`] once and then run over an in-memory CSV
//! dataset; the cases mirror the work the binary does — projection, filtering,
//! conversion, sorting, and alignment — reported as input throughput. The data
//! lives in memory, so these isolate the engine from disk I/O, and every run
//! processes byte-identical input (the generator is a pure function of the row
//! index) so the numbers are reproducible. Single-threaded on purpose: this
//! measures the per-row engine, not thread scaling.

use std::hint::black_box;
use std::io::Cursor;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};

use csvm::plan::Plan;
use csvm::{csv, exec, parse};
use exec::RunOpts;

/// Rows in the in-memory dataset: large enough to be representative, small
/// enough for criterion to gather many samples quickly.
const ROWS: u64 = 50_000;

/// A deterministic 6-column dataset — each cell is a pure function of the row
/// index (via splitmix64), so the bytes are identical from run to run.
fn dataset(rows: u64) -> String {
    let regions = ["north", "south", "east", "west"];
    let statuses = ["active", "inactive", "pending"];
    let mut s = String::from("id,region,name,flag,amount,status\n");
    use std::fmt::Write as _;
    for i in 0..rows {
        let h = splitmix64(i);
        let region = regions[(h % 4) as usize];
        let name_n = h % 1000;
        let flag = if h & 1 == 0 { "t" } else { "f" };
        let amount = h % 100_000;
        let status = statuses[(h % 3) as usize];
        let _ = writeln!(s, "{i},{region},name{name_n},{flag},{amount},{status}");
    }
    s
}

/// splitmix64: a pure function of the index, so the dataset is reproducible.
fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn run_opts() -> RunOpts {
    RunOpts {
        chunk_size: 1 << 20,
        threads: 1,
        temp_dir: std::env::temp_dir(),
        sort_buffer: 256 << 20,
    }
}

/// Split a dataset into its parsed header and the data-row bytes, so the per-row
/// work can be measured without re-reading the header each iteration.
fn header_and_body(full: &str) -> (Vec<String>, Vec<u8>) {
    let nl = full.find('\n').expect("dataset has a header line");
    (
        csv::parse_header(&full[..nl]),
        full.as_bytes()[nl + 1..].to_vec(),
    )
}

/// Run a pre-compiled plan over `body` (header already split off), serializing
/// to a fresh buffer — the unit of work being measured.
fn run_once(plan: &Plan, out_header: &[String], opts: &RunOpts, body: &[u8]) {
    let mut reader = Cursor::new(body);
    let mut out = Vec::new();
    exec::run(plan, out_header, opts, &mut reader, &mut out).expect("run failed");
    black_box(&out);
}

/// The per-row engine across the command set: projection, filtering, numeric
/// conversion, and sorting. Throughput is the data-row bytes streamed in.
fn pipeline(c: &mut Criterion) {
    let full = dataset(ROWS);
    let (header, body) = header_and_body(&full);
    let opts = run_opts();

    let cases = [
        (
            "filter",
            "select flag == 't' && amount > 50000 && status =~ '^a' | cols id,region,amount",
        ),
        ("projection", "cols id,region,amount"),
        ("drop_columns", "cols -v name,flag,status"),
        ("select_string", "select region == 'north'"),
        ("select_numeric", "select amount > 50000"),
        // Two bare columns ordered with `>` → the per-row auto-detect path. It
        // parses two columns (vs select_numeric's one col + literal), so it runs
        // a bit lower; the auto-detect branch itself is ~free (wall-clock: auto
        // tracks an explicit `num(…) >` within noise).
        ("select_auto", "select amount > id"),
        ("select_regex", "select name =~ '^name1'"),
        ("num_cast", "add amount = num(amount)"),
        ("sort_numeric", "sort amount=n"),
        ("sort_lexical", "sort region=s"),
        // Auto (the bare-column default): a text key pays a failed number parse
        // per cell, a numeric key the same parse `=n` does plus a tag byte.
        ("sort_auto_text", "sort region"),
        ("sort_auto_numeric", "sort amount"),
        ("sort_multikey", "sort region amount=n"),
        // group-by folds every row into a `Grouper` (the streaming reduce path);
        // low- and high-cardinality cases stress the per-row update vs the
        // per-group accumulator growth respectively.
        (
            "group_low_card",
            "group region | agg count, sum(amount), mean(amount)",
        ),
        ("group_high_card", "group name | agg sum(amount)"),
    ];

    let mut group = c.benchmark_group("pipeline");
    group.throughput(Throughput::Bytes(body.len() as u64));
    for (name, script) in cases {
        // Parse + resolve once — the binary compiles the script a single time.
        let mut plan = parse::parse(script).expect("parse failed");
        let out_header = plan.resolve(&header).expect("resolve failed");
        group.bench_function(name, |b| {
            b.iter(|| run_once(&plan, &out_header, &opts, &body));
        });
    }
    group.finish();
}

/// Parsing happens once per invocation; measure the compile step in isolation
/// on a multi-stage script (filter, then sort, then projection).
fn parsing(c: &mut Criterion) {
    let script = "select flag == 't' && amount > 50000 && status =~ '^a' \
                  | sort amount=nr | cols id,region,amount";
    c.bench_function("parse", |b| {
        b.iter(|| black_box(parse::parse(black_box(script)).expect("parse failed")));
    });
}

/// `fmt` re-renders produced CSV as an aligned table (numeric columns
/// right-justified). Feed it projected CSV and measure just the alignment pass.
fn align(c: &mut Criterion) {
    let full = dataset(ROWS / 5);
    let (header, body) = header_and_body(&full);
    let opts = run_opts();

    let mut plan = parse::parse("cols id,region,amount | fmt").expect("parse failed");
    let out_header = plan.resolve(&header).expect("resolve failed");
    let mut reader = Cursor::new(body.as_slice());
    let mut csv_out = Vec::new();
    exec::run(&plan, &out_header, &opts, &mut reader, &mut csv_out).expect("run failed");

    let mut group = c.benchmark_group("fmt");
    group.throughput(Throughput::Bytes(csv_out.len() as u64));
    group.bench_function("render", |b| {
        b.iter(|| {
            let mut out = Vec::new();
            exec::render(&csv_out, &plan, false, &mut out).expect("fmt failed");
            black_box(&out);
        });
    });
    group.finish();
}

criterion_group!(benches, pipeline, parsing, align);
criterion_main!(benches);
