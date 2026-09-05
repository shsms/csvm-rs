//! End-to-end tests for the `add` computed-column command: arithmetic,
//! functions, string concat, ternary/boolean values, replace-vs-append, and the
//! stateful `prev()`/`rownum()` forms — including that a stateful `add` is
//! independent of the thread count (it can't shard) while a pure `add` shards
//! identically.

use csvm::exec::{self, RunOpts};
use std::io::BufReader;

mod common;
use common::temp_csv;

fn run(script: &str, input: &str, threads: usize) -> Result<String, String> {
    let mut plan = csvm::parse::parse(script).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(input.as_bytes());
    let header = exec::read_header(&mut reader).map_err(|e| e.to_string())?;
    let out_header = plan.resolve(&header).map_err(|e| e.to_string())?;
    let opts = RunOpts {
        chunk_size: 64, // small, to exercise multi-chunk handling
        threads,
        temp_dir: std::env::temp_dir(),
        sort_buffer: 8 << 20,
    };
    let mut out = Vec::new();
    exec::run(&plan, &out_header, &opts, &mut reader, &mut out).map_err(|e| e.to_string())?;
    String::from_utf8(out).map_err(|e| e.to_string())
}

/// Run single- and multi-threaded; assert identical and return the output. This
/// is the key invariant for `add`: a pure expression shards/streams in parallel,
/// a stateful one (`prev`/`rownum`) falls back to an ordered path — either way
/// the result must not depend on the thread count.
fn run_checked(script: &str, input: &str) -> String {
    let serial = run(script, input, 1).expect("serial run failed");
    let parallel = run(script, input, 4).expect("parallel run failed");
    assert_eq!(
        serial, parallel,
        "thread count changed output for: {script}"
    );
    serial
}

fn run_file_str(script: &str, path: &std::path::Path, threads: usize) -> String {
    let mut plan = csvm::parse::parse(script).unwrap();
    let (header, data_start, file_len) = exec::read_header_from_path(path).unwrap();
    let out_header = plan.resolve(&header).unwrap();
    let opts = RunOpts {
        chunk_size: 1 << 20,
        threads,
        temp_dir: std::env::temp_dir(),
        sort_buffer: 8 << 20,
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

#[test]
fn arithmetic_appends_a_numeric_column() {
    assert_eq!(
        run_checked("add total = price * qty", NUM),
        "price,qty,total\n10,3,30\n20,2,40\n5,4,20\n"
    );
}

#[test]
fn exponent_literals() {
    assert_eq!(
        run_checked("add v = price * 1e3 + qty * 2.5E-3 - 1e+2 | cols v", NUM),
        "v\n9900.0075\n19900.005\n4900.01\n"
    );
    // An exponent past the f64 range overflows to inf, as a cell would.
    assert_eq!(
        run_checked("add v = 1e400 | add w = -1e400 | cols v,w", NUM),
        "v,w\ninf,-inf\ninf,-inf\ninf,-inf\n"
    );
}

#[test]
fn a_quoted_name_loses_its_quotes() {
    assert_eq!(
        run_checked("add 'total price' = price * qty | cols 'total price'", NUM),
        "total price\n30\n40\n20\n"
    );
}

#[test]
fn precedence_and_parens() {
    // (price - qty) * 2, not price - qty*2
    assert_eq!(
        run_checked("add v = (price - qty) * 2 | cols v", NUM),
        "v\n14\n36\n2\n"
    );
}

#[test]
fn replace_existing_column_in_place() {
    // Replaces `price`, keeping column position; does not append.
    assert_eq!(
        run_checked("add price = price * 2", NUM),
        "price,qty\n20,3\n40,2\n10,4\n"
    );
}

#[test]
fn functions() {
    let input = "x\n2.4\n-3.7\n";
    assert_eq!(
        run_checked("add r = round(x) | add a = abs(x) | cols r,a", input),
        "r,a\n2,2.4\n-4,3.7\n"
    );
    assert_eq!(
        run_checked("add m = min(price, qty) | cols m", NUM),
        "m\n3\n2\n4\n"
    );
}

#[test]
fn string_concat_and_funcs() {
    let input = "first,last\nAda,Lovelace\nAlan,Turing\n";
    assert_eq!(
        run_checked("add full = first ++ ' ' ++ last | cols full", input),
        "full\nAda Lovelace\nAlan Turing\n"
    );
    assert_eq!(
        run_checked("add u = upper(first) | cols u", input),
        "u\nADA\nALAN\n"
    );
}

#[test]
fn ternary_and_boolean_value() {
    assert_eq!(
        run_checked("add tier = price > 8 ? 'hi' : 'lo' | cols tier", NUM),
        "tier\nhi\nhi\nlo\n"
    );
    // A bare comparison yields t/f.
    assert_eq!(
        run_checked("add ok = price > 8 | cols ok", NUM),
        "ok\nt\nt\nf\n"
    );
}

#[test]
fn prev_computes_step_delta() {
    // The headline use case: difference between successive rows. Row 1 is 0.
    assert_eq!(
        run_checked("add d = price - prev(price) | cols d", NUM),
        "d\n0\n10\n-15\n"
    );
}

#[test]
fn rownum_is_one_based() {
    assert_eq!(
        run_checked("add n = rownum() | cols n", NUM),
        "n\n1\n2\n3\n"
    );
}

#[test]
fn prev_after_a_reshaping_cols_uses_the_new_layout() {
    // `prev(C)` must read C at its position *after* `cols` (not in the original
    // wider row), so the previous row is snapshotted in the post-cols layout.
    let input = "A,B,C,D\n1,99,10,x\n2,99,30,y\n3,99,25,z\n";
    assert_eq!(
        run_checked("cols A,C | add Q = C - prev(C) | cols Q", input),
        "Q\n0\n20\n-5\n"
    );
    // Same for a rename before the add.
    let input = "a,b\n1,5\n2,12\n3,4\n";
    assert_eq!(
        run_checked("rename b=val | add d = val - prev(val) | cols d", input),
        "d\n0\n7\n-8\n"
    );
}

#[test]
fn step_differences_with_prev() {
    assert_eq!(
        run_checked(
            "add price_delta = price - prev(price) | cols price_delta",
            NUM
        ),
        "price_delta\n0\n10\n-15\n"
    );
    assert_eq!(
        run_checked(
            "add price_delta = price - prev(price) | add qty_delta = qty - prev(qty) \
             | cols price_delta,qty_delta",
            NUM
        ),
        "price_delta,qty_delta\n0,0\n10,-1\n-15,2\n"
    );
}

#[test]
fn delta_command_points_at_add() {
    let err = run("delta price", NUM, 1).unwrap_err();
    assert!(
        err.contains("add price_delta = price - prev(price)"),
        "{err}"
    );
}

#[test]
fn step_difference_is_thread_independent_over_a_file() {
    let mut content = String::from("id,val\n");
    for i in 0..3000u32 {
        content.push_str(&format!("{i},{}\n", (i * 17) % 89));
    }
    let path = temp_csv(&content);
    let script = "add val_delta = val - prev(val) | cols val_delta";
    let serial = run(script, &content, 1).unwrap();
    for n in [1usize, 2, 4, 8, 16] {
        assert_eq!(run_file_str(script, &path, n), serial, "threads={n}");
    }
}

#[test]
fn stateful_add_is_thread_independent_over_a_file() {
    // Varied row lengths so shard boundaries fall mid-data; a stateful add must
    // still produce the serial result at every thread count (it can't shard).
    let mut content = String::from("id,val\n");
    for i in 0..3000u32 {
        content.push_str(&format!("{i},{}\n", (i * 31) % 97));
    }
    let path = temp_csv(&content);
    let serial = run("add d = val - prev(val)", &content, 1).unwrap();
    for n in [1usize, 2, 3, 5, 8, 16] {
        assert_eq!(
            run_file_str("add d = val - prev(val)", &path, n),
            serial,
            "threads={n}"
        );
    }
}

#[test]
fn pure_add_shards_identically_over_a_file() {
    let mut content = String::from("id,val\n");
    for i in 0..3000u32 {
        content.push_str(&format!("{i},{}\n", i % 13));
    }
    let path = temp_csv(&content);
    let serial = run("add t = val * 2 + 1", &content, 1).unwrap();
    for n in [1usize, 2, 4, 8, 16] {
        assert_eq!(
            run_file_str("add t = val * 2 + 1", &path, n),
            serial,
            "threads={n}"
        );
    }
}

#[test]
fn divide_by_zero_aborts() {
    assert!(run("add x = price / 0", NUM, 1).is_err());
}

#[test]
fn non_numeric_in_arithmetic_aborts() {
    let input = "a\nhello\n";
    assert!(run("add x = a * 2", input, 1).is_err());
}

#[test]
fn unknown_function_is_a_compile_error() {
    assert!(csvm::parse::parse("add x = frobnicate(price)").is_err());
}

#[test]
fn add_then_select_on_the_new_column() {
    // The new numeric column is usable by a later numeric comparison.
    assert_eq!(
        run_checked(
            "add total = price * qty | select total >= 30 | cols total",
            NUM
        ),
        "total\n30\n40\n"
    );
}
