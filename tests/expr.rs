//! End-to-end tests for the unified expression grammar: the math function set
//! (`sqrt`/`pow`/`exp`/`log`/…), value-level comparison operands (arithmetic,
//! functions, and boolean subexpressions inside `select`/ternary tests), and
//! stateful (`prev()`/`rownum()`) expressions in `select`.

use csvm::exec::{self, RunOpts};
use std::io::BufReader;

fn run(script: &str, input: &str, threads: usize) -> Result<String, String> {
    let mut plan = csvm::parse::parse(script).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(input.as_bytes());
    let header = match plan.input_header.as_deref() {
        Some(h) => h.to_vec(),
        None => exec::read_header(&mut reader).map_err(|e| e.to_string())?,
    };
    let out_header = plan.resolve(&header).map_err(|e| e.to_string())?;
    let opts = RunOpts {
        chunk_size: 64, // small, to exercise multi-chunk handling
        threads,
        temp_dir: std::env::temp_dir(),
        sort_buffer: 1 << 20,
    };
    let mut out = Vec::new();
    exec::run(&plan, &out_header, &opts, &mut reader, &mut out).map_err(|e| e.to_string())?;
    String::from_utf8(out).map_err(|e| e.to_string())
}

/// Run single- and multi-threaded; assert identical and return the output.
fn run_checked(script: &str, input: &str) -> String {
    let serial = run(script, input, 1).expect("serial run failed");
    let parallel = run(script, input, 4).expect("parallel run failed");
    assert_eq!(
        serial, parallel,
        "thread count changed output for: {script}"
    );
    serial
}

const NUM: &str = "\
price,qty
10,3
20,2
5,4
";

// --- math functions ---------------------------------------------------------
#[test]
fn sqrt_of_a_column() {
    assert_eq!(
        run_checked("add r sqrt(price * qty) | cols r", NUM),
        "r\n5.477226\n6.324555\n4.472136\n"
    );
}

#[test]
fn pow_is_binary() {
    assert_eq!(
        run_checked("add p pow(qty, 2) | cols p", NUM),
        "p\n9\n4\n16\n"
    );
}

#[test]
fn pow_wrong_arity_is_a_compile_error() {
    // Specifically an arity error, not "unknown function".
    let e = csvm::parse::parse("add p pow(qty)")
        .unwrap_err()
        .to_string();
    assert!(e.contains("argument"), "unexpected error: {e}");
    let e = csvm::parse::parse("add p sqrt(qty, 2)")
        .unwrap_err()
        .to_string();
    assert!(e.contains("argument"), "unexpected error: {e}");
}

#[test]
fn exp_and_log_family() {
    let input = "x\n1\n8\n100\n";
    assert_eq!(
        run_checked("add l log(exp(x)) | cols l", input),
        "l\n1\n8\n100\n"
    );
    assert_eq!(
        run_checked("add l2 log2(x) | add l10 log10(x) | cols l2,l10", input),
        "l2,l10\n0,0\n3,0.90309\n6.643856,2\n"
    );
}

#[test]
fn sign_of_a_column() {
    let input = "x\n5\n-3.2\n0\n";
    assert_eq!(
        run_checked("add s sign(x) | cols s", input),
        "s\n1\n-1\n0\n"
    );
}

#[test]
fn sqrt_of_negative_is_nan_not_an_abort() {
    // Domain edges follow IEEE (like stats' non-finite policy), so a negative
    // doesn't kill the run; NaN renders via the non-finite fallback.
    assert_eq!(
        run_checked("add r sqrt(x) | cols r", "x\n-1\n4\n"),
        "r\nNaN\n2\n"
    );
}
