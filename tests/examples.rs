//! End-to-end tests driving the public API the way the binary does: compile a
//! script, read the header, resolve, and run. Covers the csvm README examples
//! (translated to Lisp), implicit numeric behavior, regex, quoting, the sort
//! pipeline, and compile-time pipeline generation.

use csvm::exec::{self, RunOpts};
use std::io::BufReader;

const INPUT: &str = "\
id,fieldA,fieldB,countA,countZ
1,t,x,3,5
2,f,y,0,0
3,t,x,-2,0
4,t,z,7,9
5,f,y,0,2
";

fn run(script: &str, input: &str, threads: usize) -> Result<String, String> {
    let mut plan = csvm::compile::compile(script).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(input.as_bytes());
    let header = exec::read_header(&mut reader).map_err(|e| e.to_string())?;
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

/// Run single-threaded and multi-threaded; assert identical, and return it.
fn run_checked(script: &str, input: &str) -> String {
    let serial = run(script, input, 1).expect("serial run failed");
    let parallel = run(script, input, 4).expect("parallel run failed");
    assert_eq!(
        serial, parallel,
        "thread count changed the output for: {script}"
    );
    serial
}

#[test]
fn cols_keep_in_order() {
    assert_eq!(
        run_checked("(cols id fieldA countZ)", INPUT),
        "id,fieldA,countZ\n1,t,5\n2,f,0\n3,t,0\n4,t,9\n5,f,2\n"
    );
}

#[test]
fn drop_cols_keeps_rest() {
    assert_eq!(
        run_checked("(drop-cols fieldA fieldB countA)", INPUT),
        "id,countZ\n1,5\n2,0\n3,0\n4,9\n5,2\n"
    );
}

#[test]
fn select_string_equality() {
    assert_eq!(
        run_checked(r#"(select (and (== fieldA "t") (!= countZ "0")))"#, INPUT),
        "id,fieldA,fieldB,countA,countZ\n1,t,x,3,5\n4,t,z,7,9\n"
    );
}

#[test]
fn select_implicit_numeric() {
    // No to-num: a comparison against a number is numeric. -2 is not > 0.
    assert_eq!(
        run_checked(
            r#"(select (and (== fieldA "t") (or (> countZ 0) (> countA 0)))) (cols id)"#,
            INPUT,
        ),
        "id\n1\n4\n"
    );
}

#[test]
fn explicit_to_num_to_str_roundtrip() {
    // to-num then to-str canonicalizes the number but is otherwise transparent.
    assert_eq!(
        run_checked(
            "(to-num countA) (select (> countA 0)) (to-str countA) (cols id countA)",
            INPUT
        ),
        "id,countA\n1,3\n4,7\n"
    );
}

#[test]
fn regex_match_and_negation() {
    assert_eq!(
        run_checked(r#"(select (=~ fieldB "^x$")) (cols id)"#, INPUT),
        "id\n1\n3\n"
    );
    assert_eq!(
        run_checked(r#"(select (!~ fieldB "[xy]")) (cols id)"#, INPUT),
        "id\n4\n"
    );
}

#[test]
fn sort_numeric_reverse() {
    assert_eq!(
        run_checked(
            "(sort-by (countZ :reverse :numeric)) (cols id countZ)",
            INPUT
        ),
        "id,countZ\n4,9\n1,5\n5,2\n2,0\n3,0\n"
    );
}

#[test]
fn sort_multi_key_lexical_then_reverse() {
    // forward by fieldA, then reverse by id (lexical) within ties
    assert_eq!(
        run_checked("(sort-by fieldA (id :reverse)) (cols id fieldA)", INPUT),
        "id,fieldA\n5,f\n2,f\n4,t\n3,t\n1,t\n"
    );
}

#[test]
fn three_stage_pipeline() {
    // filter, sort, drop — splits into transform/sort/transform stages.
    assert_eq!(
        run_checked(
            r#"(select (== fieldA "t")) (sort-by (countZ :numeric)) (drop-cols fieldB countA)"#,
            INPUT,
        ),
        "id,fieldA,countZ\n3,t,0\n1,t,5\n4,t,9\n"
    );
}

#[test]
fn quoted_fields_roundtrip() {
    let input = "name,n\n\"last, first\",1\nplain,2\n";
    assert_eq!(
        run_checked("(select (> n 0))", input),
        "name,n\n\"last, first\",1\nplain,2\n"
    );
}

#[test]
fn conditional_step_via_when() {
    // (when t ...) emits the step; (when nil ...) skips it.
    assert_eq!(
        run_checked("(when t (drop-cols fieldA)) (cols id fieldB)", INPUT),
        "id,fieldB\n1,x\n2,y\n3,x\n4,z\n5,y\n"
    );
    assert_eq!(
        run_checked("(when nil (drop-cols fieldA)) (cols id fieldA)", INPUT),
        "id,fieldA\n1,t\n2,f\n3,t\n4,t\n5,f\n"
    );
}

#[test]
fn unknown_column_errors() {
    let err = run("(cols nope)", INPUT, 1).unwrap_err();
    assert!(err.contains("column not found"), "got: {err}");
}

#[test]
fn bad_script_is_a_compile_error() {
    let err = run("(select (frobnicate a b))", INPUT, 1).unwrap_err();
    assert!(err.contains("unknown operator"), "got: {err}");
}
