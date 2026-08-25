//! End-to-end tests for the unified expression grammar: the math function set
//! (`sqrt`/`pow`/`exp`/`log`/…), value-level comparison operands (arithmetic,
//! functions, and boolean subexpressions inside `select`/ternary tests), and
//! stateful (`prev()`/`rownum()`) expressions in `select`.

use csvm::exec::{self, RunOpts};
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static SEQ: AtomicU32 = AtomicU32::new(0);

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

fn temp_csv(content: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "csvm_expr_{}_{}.csv",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, content).unwrap();
    path
}

fn run_file_str(script: &str, path: &std::path::Path, threads: usize) -> String {
    let mut plan = csvm::parse::parse(script).unwrap();
    let (header, data_start, file_len) = exec::read_header_from_path(path).unwrap();
    let out_header = plan.resolve(&header).unwrap();
    let opts = RunOpts {
        chunk_size: 1 << 20,
        threads,
        temp_dir: std::env::temp_dir(),
        sort_buffer: 1 << 20,
    };
    let mut out = Vec::new();
    exec::run_file(
        &plan,
        &out_header,
        &opts,
        path,
        data_start,
        file_len,
        &mut out,
    )
    .unwrap();
    String::from_utf8(out).unwrap()
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

// --- value-level comparison operands ----------------------------------------

#[test]
fn bool_value_as_select_comparison_operand() {
    // A parenthesized bool composes into a larger value operand in select,
    // same as the add-side `(a > 0) ++ '!'`.
    let input = "a\n1\n-2\n";
    assert_eq!(
        run_checked("select (a > 0) ++ 'x' == 'tx'", input),
        "a\n1\n"
    );
}

#[test]
fn deeply_nested_expressions_parse_in_linear_time() {
    // Each token position parses once; nesting depth must not multiply work.
    let t = std::time::Instant::now();
    let deep = format!("select {}a + b{} > 0", "(".repeat(24), ")".repeat(24));
    csvm::parse::parse(&deep).unwrap();
    let calls = format!("add x {}a{}", "abs(".repeat(24), ")".repeat(24));
    csvm::parse::parse(&calls).unwrap();
    assert!(
        t.elapsed() < std::time::Duration::from_secs(1),
        "parser is not linear in nesting depth: {:?}",
        t.elapsed()
    );
}

#[test]
fn malformed_group_reports_the_offending_token() {
    // The error must point at the actual problem (the dangling ')'), not at a
    // healthy token earlier in the group.
    let e = csvm::parse::parse("select (a > 0 &&)")
        .unwrap_err()
        .to_string();
    assert!(e.contains("found ')'"), "wrong error position: {e}");
}

#[test]
fn select_with_arithmetic_comparison() {
    assert_eq!(
        run_checked("select price * qty >= 30", NUM),
        "price,qty\n10,3\n20,2\n"
    );
}

#[test]
fn select_with_function_call() {
    let input = "x\n0.5\n-3\n2\n";
    assert_eq!(run_checked("select abs(x) > 1", input), "x\n-3\n2\n");
}

#[test]
fn bool_equality_in_a_ternary_test() {
    // The motivating case: same-sign detection via boolean equality.
    let input = "grid_active,grid_reactive\n5,3\n-2,4\n-1,-1\n";
    assert_eq!(
        run_checked(
            "add side ((grid_active >= 0.0) == (grid_reactive >= 0.0)) ? 'leading' : 'lagging' \
             | cols side",
            input
        ),
        "side\nleading\nlagging\nleading\n"
    );
}

#[test]
fn parenthesized_bool_as_a_value() {
    // Bare parens at expression end already worked; a bool value followed by
    // more expression (concat) needs the paren-bool-as-value grammar fix.
    assert_eq!(
        run_checked("add ok (price > 8) | cols ok", NUM),
        "ok\nt\nt\nf\n"
    );
    assert_eq!(
        run_checked("add s (price > 8) ++ '!' | cols s", NUM),
        "s\nt!\nt!\nf!\n"
    );
}

#[test]
fn add_bool_of_computed_comparison() {
    assert_eq!(
        run_checked("add big price * qty > 25 | cols big", NUM),
        "big\nt\nt\nf\n"
    );
}

#[test]
fn concat_comparison_is_lexical() {
    let input = "first,last\nAda,Lovelace\nAlan,Turing\n";
    assert_eq!(
        run_checked("select first ++ last == 'AdaLovelace'", input),
        "first,last\nAda,Lovelace\n"
    );
}

#[test]
fn arithmetic_beats_auto_mode_against_a_bare_column() {
    // `x * 1` is statically numeric, so the untyped column on the right is
    // coerced numerically ("9" < "10" numerically, not lexically).
    let input = "a,b\n9,10\n";
    assert_eq!(run_checked("select a * 1 < b", input), "a,b\n9,10\n");
}

#[test]
fn select_arithmetic_on_non_numeric_aborts() {
    // Specifically the runtime coercion error, not a parse failure.
    let e = run("select x * 2 > 1", "x\nhello\n", 1).unwrap_err();
    assert!(
        e.contains("non-numeric value 'hello'"),
        "unexpected error: {e}"
    );
}

// --- add column typing ------------------------------------------------------

#[test]
fn add_ternary_column_types_later_comparisons_numeric() {
    // Agreeing numeric ?: branches type the new column, so a later equality
    // against it compares numerically (01 == 1), not lexically.
    let input = "x,y\n5,01\n-1,02\n";
    assert_eq!(
        run_checked("add flag x > 0 ? 1 : 0 | select flag == y | cols x", input),
        "x\n5\n"
    );
}

#[test]
fn add_column_copy_inherits_explicit_type_overrides() {
    // An explicit to-num is a type signal that survives `add z a`: the copy is
    // strictly numeric, so text on the other side aborts.
    let e = run(
        "to-num a | add z a | select z > name",
        "a,name\n1,apple\n",
        1,
    )
    .unwrap_err();
    assert!(
        e.contains("non-numeric value 'apple'"),
        "unexpected error: {e}"
    );
    // And to-str pins the copy lexical instead of per-row auto-detect.
    let input = "id,other\n9,100\n20,3\n";
    assert_eq!(
        run_checked(
            "to-str id | add id2 id | select id2 > other | cols id",
            input
        ),
        "id\n9\n"
    );
}

// --- stateful select --------------------------------------------------------

#[test]
fn select_prev_keeps_rows_where_value_changed() {
    let input = "val\n1\n1\n2\n2\n2\n3\n";
    // Row 1: prev() reads the current cell, so `!=` is false and it's dropped —
    // matching the `add` convention (delta 0 on row 1).
    assert_eq!(run_checked("select val != prev(val)", input), "val\n2\n3\n");
}

#[test]
fn select_rownum_samples_rows() {
    let input = "x\na\nb\nc\nd\ne\n";
    assert_eq!(
        run_checked("select rownum() % 2 == 1", input),
        "x\na\nc\ne\n"
    );
}

#[test]
fn stateful_select_is_thread_independent_over_a_file() {
    let mut content = String::from("id,val\n");
    for i in 0..3000u32 {
        content.push_str(&format!("{i},{}\n", (i * 7) % 5));
    }
    let path = temp_csv(&content);
    let serial = run("select val != prev(val)", &content, 1).unwrap();
    for n in [1usize, 2, 4, 8, 16] {
        assert_eq!(
            run_file_str("select val != prev(val)", &path, n),
            serial,
            "threads={n}"
        );
    }
    std::fs::remove_file(&path).ok();
}

#[test]
fn color_predicate_rejects_stateful_expressions() {
    // Rejected at parse time — an unresolvable colour rule is silently dropped
    // at resolve time (cosmetic policy), which would hide the mistake.
    let e = csvm::parse::parse("color red rownum() > 5")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("not allowed in a color"),
        "unexpected error: {e}"
    );
    let e = csvm::parse::parse("color red x != prev(x)")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("not allowed in a color"),
        "unexpected error: {e}"
    );
}
