//! End-to-end tests driving the public API the way the binary does: parse a
//! script, read the header, resolve, and run. Covers the csvm example set (in
//! the pipe syntax), implicit numeric behavior, regex, quoting, the sort
//! pipeline, and sharded file processing.

use csvm::exec::{self, RunOpts};
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const INPUT: &str = "\
id,fieldA,fieldB,countA,countZ
1,t,x,3,5
2,f,y,0,0
3,t,x,-2,0
4,t,z,7,9
5,f,y,0,2
";

fn run(script: &str, input: &str, threads: usize) -> Result<String, String> {
    let mut plan = csvm::parse::parse(script).map_err(|e| e.to_string())?;
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
        run_checked("cols id fieldA countZ", INPUT),
        "id,fieldA,countZ\n1,t,5\n2,f,0\n3,t,0\n4,t,9\n5,f,2\n"
    );
}

#[test]
fn cols_exclude_keeps_rest() {
    assert_eq!(
        run_checked("cols -v fieldA,fieldB,countA", INPUT),
        "id,countZ\n1,5\n2,0\n3,0\n4,9\n5,2\n"
    );
}

#[test]
fn select_string_equality() {
    assert_eq!(
        run_checked("select fieldA == 't' && countZ != '0'", INPUT),
        "id,fieldA,fieldB,countA,countZ\n1,t,x,3,5\n4,t,z,7,9\n"
    );
}

#[test]
fn select_implicit_numeric() {
    // No to-num: a comparison against a number is numeric. -2 is not > 0.
    assert_eq!(
        run_checked(
            "select fieldA == 't' && (countZ > 0 || countA > 0) | cols id",
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
            "to-num countA | select countA > 0 | to-str countA | cols id,countA",
            INPUT
        ),
        "id,countA\n1,3\n4,7\n"
    );
}

#[test]
fn regex_match_and_negation() {
    assert_eq!(
        run_checked("select fieldB =~ '^x$' | cols id", INPUT),
        "id\n1\n3\n"
    );
    assert_eq!(
        run_checked("select fieldB !~ '[xy]' | cols id", INPUT),
        "id\n4\n"
    );
}

#[test]
fn sort_numeric_reverse() {
    assert_eq!(
        run_checked("sort countZ=nr | cols id,countZ", INPUT),
        "id,countZ\n4,9\n1,5\n5,2\n2,0\n3,0\n"
    );
}

#[test]
fn sort_multi_key_lexical_then_reverse() {
    // forward by fieldA, then reverse by id (lexical) within ties
    assert_eq!(
        run_checked("sort fieldA id=r | cols id,fieldA", INPUT),
        "id,fieldA\n5,f\n2,f\n4,t\n3,t\n1,t\n"
    );
}

#[test]
fn three_stage_pipeline() {
    // filter, sort, drop — splits into transform/sort/transform stages.
    assert_eq!(
        run_checked(
            "select fieldA == 't' | sort countZ=n | cols -v fieldB,countA",
            INPUT,
        ),
        "id,fieldA,countZ\n3,t,0\n1,t,5\n4,t,9\n"
    );
}

#[test]
fn quoted_fields_roundtrip() {
    let input = "name,n\n\"last, first\",1\nplain,2\n";
    assert_eq!(
        run_checked("select n > 0", input),
        "name,n\n\"last, first\",1\nplain,2\n"
    );
}

static SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_csv(content: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "csvm_it_{}_{}.csv",
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

#[test]
fn sharded_file_matches_serial_for_many_thread_counts() {
    // Rows of varied length so shard boundaries (at total/N offsets) land at
    // different points inside the data, stressing the line-boundary snapping.
    let mut content = String::from("id,grp,val\n");
    for i in 0..3000u32 {
        content.push_str(&format!(
            "{i},{},{}\n",
            i % 5,
            "v".repeat((i % 23) as usize)
        ));
    }
    let path = temp_csv(&content);

    for script in [
        "cols val,id",
        "select grp == '2'",
        "cols -v grp",
        "select grp == '1' && id > 1000",
    ] {
        // Serial (reader over the same bytes) is the ground truth.
        let serial = run(script, &content, 1).unwrap();
        for n in [1usize, 2, 3, 5, 8, 16, 64] {
            assert_eq!(
                run_file_str(script, &path, n),
                serial,
                "script={script} threads={n}"
            );
        }
    }
    std::fs::remove_file(&path).ok();
}

#[test]
fn sharded_file_no_trailing_newline() {
    // Last line lacks a newline — boundary snapping must still cover it once.
    let content = "id,v\n1,a\n2,b\n3,c\n4,d"; // no final '\n'
    let path = temp_csv(content);
    let serial = run("cols id,v", content, 1).unwrap();
    for n in [1usize, 2, 4, 8] {
        assert_eq!(run_file_str("cols id,v", &path, n), serial, "threads={n}");
    }
    std::fs::remove_file(&path).ok();
}

#[test]
fn unknown_column_errors() {
    let err = run("cols nope", INPUT, 1).unwrap_err();
    assert!(err.contains("column not found"), "got: {err}");
}

#[test]
fn bad_script_is_a_parse_error() {
    let err = run("frobnicate a", INPUT, 1).unwrap_err();
    assert!(err.contains("unknown command"), "got: {err}");
}
