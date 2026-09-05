//! End-to-end tests driving the public API the way the binary does: parse a
//! script, read the header, resolve, and run. Covers the csvm example set (in
//! the pipe syntax), implicit numeric behavior, regex, quoting, the sort
//! pipeline, and sharded file processing.

use csvm::exec::{self, RunOpts};
use std::io::BufReader;

mod common;
use common::temp_csv;

const INPUT: &str = "\
id,fieldA,fieldB,countA,countZ
1,t,x,3,5
2,f,y,0,0
3,t,x,-2,0
4,t,z,7,9
5,f,y,0,2
";

fn run(script: &str, input: &str, threads: usize) -> Result<String, String> {
    run_with_header(script, input, None, threads)
}

/// Like [`run`]; with `header`, the input has no header line and these are
/// its column names (the `--header` flag).
fn run_with_header(
    script: &str,
    input: &str,
    header: Option<&[&str]>,
    threads: usize,
) -> Result<String, String> {
    let mut plan = csvm::parse::parse(script).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(input.as_bytes());
    let header = match header {
        Some(h) => h.iter().map(|s| s.to_string()).collect(),
        None => exec::read_header(&mut reader).map_err(|e| e.to_string())?,
    };
    let out_header = plan.resolve(&header).map_err(|e| e.to_string())?;
    let opts = RunOpts {
        chunk_size: 64, // small, to exercise multi-chunk handling
        threads,
        temp_dir: std::env::temp_dir(),
        sort_buffer: 8 << 20,
    };
    let mut out = Vec::new();
    exec::run(&plan, &out_header, &opts, &mut reader, &mut out).map_err(|e| e.to_string())?;
    // `fmt` aligns the final output and `graph` draws it (main does this;
    // mirror it here). Colour off.
    if plan.output == csvm::plan::OutputFormat::Aligned || plan.graph.is_some() {
        let mut aligned = Vec::new();
        exec::render(&out, &plan, false, &mut aligned).map_err(|e| e.to_string())?;
        out = aligned;
    }
    String::from_utf8(out).map_err(|e| e.to_string())
}

/// Run single-threaded and multi-threaded; assert identical, and return it.
fn run_checked(script: &str, input: &str) -> String {
    run_checked_with_header(script, input, None)
}

fn run_checked_with_header(script: &str, input: &str, header: Option<&[&str]>) -> String {
    let serial = run_with_header(script, input, header, 1).expect("serial run failed");
    let parallel = run_with_header(script, input, header, 4).expect("parallel run failed");
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
    // No cast: a comparison against a number is numeric. -2 is not > 0.
    assert_eq!(
        run_checked(
            "select fieldA == 't' && (countZ > 0 || countA > 0) | cols id",
            INPUT,
        ),
        "id\n1\n4\n"
    );
}

#[test]
fn num_then_str_roundtrip() {
    // num() then str() canonicalizes the number but is otherwise transparent.
    assert_eq!(
        run_checked(
            "add countA = num(countA) | select countA > 0 | add countA = str(countA) | cols id,countA",
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
fn sort_auto_agrees_with_select_on_the_same_column() {
    // The motivating case: `select qty > stock` compares numerically per row,
    // so a bare `sort qty` must order the same column numerically too
    // (lexically 100 < 25 < 9). Thread count must not change the order.
    let input = "id,qty,stock\n1,100,50\n2,9,5\n3,25,3\n4,7,30\n";
    assert_eq!(
        run_checked("select qty > stock | sort qty | cols id,qty", input),
        "id,qty\n2,9\n3,25\n1,100\n"
    );
}

#[test]
fn sort_auto_on_the_in_memory_path_keeps_cell_text() {
    // A blocking stage (`tail`) routes the sort through the in-memory path,
    // which caches each auto key's number instead of converting the cell, so
    // the order is the auto order and the cell text is untouched.
    let input = "n\nx\n10\n\n9\n1.0000001\n";
    assert_eq!(
        run_checked("sort n | tail 5", input),
        "n\n\n1.0000001\n9\n10\nx\n"
    );
    assert_eq!(
        run_checked("sort n=r | tail 5", input),
        "n\nx\n10\n9\n1.0000001\n\n"
    );
    // Two auto keys behind a numeric one: the cached numbers must line up
    // with the auto keys, not with the key list.
    let input = "k,a,b\n1,x,2\n1,10,1\n1,9,\n2,b,c\n1,10,0\n1,x,1\n";
    let expected = "k,a,b\n1,9,\n1,10,0\n1,10,1\n1,x,1\n1,x,2\n2,b,c\n";
    assert_eq!(run_checked("sort k=n a b", input), expected);
    assert_eq!(run_checked("sort k=n a b | tail 6", input), expected);
}

#[test]
fn sort_auto_treats_nan_and_inf_as_numbers_like_num() {
    // f64 parsing accepts NaN and inf, as `num()` does, so auto orders them
    // as numbers (NaN last among them, per total_cmp) before any text.
    let input = "n\nNaN\n5\nabc\ninf\n-inf\n";
    assert_eq!(run_checked("sort n", input), "n\n-inf\n5\ninf\nNaN\nabc\n");
}

#[test]
fn sort_of_a_string_typed_add_column_is_lexical() {
    // The static type of an `add` expression pins the sort mode, so a
    // string column stays lexical under the auto default.
    let input = "id,qty\n1,10\n2,9\n3,100\n";
    assert_eq!(
        run_checked("add s = qty ++ '' | sort s | cols s", input),
        "s\n10\n100\n9\n"
    );
}

#[test]
fn numeric_sort_keeps_cell_text_on_every_path() {
    // `=n` orders by value but must not rewrite the cell (007 stays 007), on
    // the external path and on the in-memory path a blocking stage forces.
    let input = "id,qty\na,007\nb,1e3\nc,\nd,2.50\n";
    let expected = "id,qty\nc,\nd,2.50\na,007\nb,1e3\n";
    assert_eq!(run_checked("sort qty=n", input), expected);
    assert_eq!(run_checked("sort qty=n | uniq", input), expected);
}

#[test]
fn statements_after_a_sort_see_the_full_number() {
    // A number computed before the sort must reach the statements after it
    // with its full value, not the six-decimal output text: the output
    // rounds, the pipeline does not.
    let input = "v\n1.00000021\n1.00000019\n";
    assert_eq!(
        run_checked("add v = num(v) | sort v | select v > 1.0000002", input),
        "v\n1\n"
    );
    assert_eq!(
        run_checked(
            "add v = num(v) | sort v | add d = round((v - 1) * 100000000) | cols d",
            input
        ),
        "d\n19\n21\n"
    );
    // The same through a window after the sort.
    assert_eq!(
        run_checked(
            "add v = num(v) | sort v=r | select v > 1.0000002 | head 1",
            input
        ),
        "v\n1\n"
    );
}

#[test]
fn column_types_follow_the_column_not_its_spelling() {
    // A column typed by `add … num()/str()` is pinned by position at resolve
    // time, so the pin holds however the column is later named: by position,
    // by name, after a rename, or after cols reorders it.
    let input = "id,qty\n1,10\n2,9\n3,100\n";
    let lexical = "id,qty\n1,10\n3,100\n2,9\n";
    assert_eq!(run_checked("add qty = str(qty) | sort 2", input), lexical);
    assert_eq!(
        run_checked(
            "add qty = str(qty) | cols qty,id | sort 1 | cols id,qty",
            input
        ),
        lexical
    );
    assert_eq!(
        run_checked(
            "add qty = num(qty) | rename qty=n | sort n | rename n=qty",
            input
        ),
        "id,qty\n2,9\n1,10\n3,100\n"
    );
    // The same for comparisons: a num()-typed column makes == numeric ...
    let pair = "a,b\n1000,1e3\n10,9\n9,10\n";
    assert_eq!(
        run_checked("add a = num(a) | select a == b", pair),
        "a,b\n1000,1e3\n"
    );
    // ... and a str()-typed column makes an ordering lexical ("9" > "10").
    assert_eq!(
        run_checked("add a = str(a) | select a > b", pair),
        "a,b\n9,10\n"
    );
    // A group key keeps its column's type, so the pin survives the reduce.
    assert_eq!(
        run_checked(
            "add qty = str(qty) | agg count by qty | sort qty | cols qty",
            input
        ),
        "qty\n10\n100\n9\n"
    );
    // ... but a type never leaks onto another column that takes its position
    // after a reduce: `region` stays untyped (text) although `amount` at
    // position 1 was typed numeric, and the stats `field` column is text.
    assert_eq!(
        run_checked(
            "add amount = num(amount) | agg count by region | sort region",
            "amount,region\n10,west\n9,east\n100,north\n"
        ),
        "region,count\neast,1\nnorth,1\nwest,1\n"
    );
    assert_eq!(
        run_checked("add id = num(id) | stats | sort field | cols field", input),
        "field\nid\nqty\n"
    );
    // An `add` column carries its expression's static type, appended or
    // replaced in place, so a positional key on it sorts by that type.
    assert_eq!(
        run_checked("add lbl = upper(qty) | sort 3", input),
        "id,qty,lbl\n1,10,10\n3,100,100\n2,9,9\n"
    );
    assert_eq!(
        run_checked("add qty = num(qty) | add qty = upper(qty) | sort 2", input),
        "id,qty\n1,10\n3,100\n2,9\n"
    );
    // A literal that cannot be a number is a compile error, even inside a
    // colour rule ...
    let err = run("add qty = num(qty) | color red qty == 'x'", input, 1).unwrap_err();
    assert!(err.contains("non-numeric literal 'x'"), "{err}");
    // ... while a colour rule whose column is gone from the output is still
    // dropped, whether it named the column or gave its position.
    assert_eq!(
        run_checked("color red `2` > 5 | cols id", input),
        "id\n1\n2\n3\n"
    );
    assert_eq!(
        run_checked("color red qty > 5 | cols id", input),
        "id\n1\n2\n3\n"
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

fn run_file_str(script: &str, path: &std::path::Path, threads: usize) -> String {
    run_file_with_header(script, path, None, threads)
}

/// Like [`run_file_str`]; with `header`, the whole file is data.
fn run_file_with_header(
    script: &str,
    path: &std::path::Path,
    header: Option<&[&str]>,
    threads: usize,
) -> String {
    let mut plan = csvm::parse::parse(script).unwrap();
    let (header, data_start, file_len) = match header {
        Some(h) => (
            h.iter().map(|s| s.to_string()).collect(),
            0,
            std::fs::metadata(path).unwrap().len(),
        ),
        None => exec::read_header_from_path(path).unwrap(),
    };
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
}

#[test]
fn head_keeps_first_rows() {
    // Plain head: first N rows.
    assert_eq!(run_checked("head 2 | cols id", INPUT), "id\n1\n2\n");
    // select then head: first N *matching* rows.
    assert_eq!(
        run_checked("select fieldA == 't' | head 2 | cols id", INPUT),
        "id\n1\n3\n"
    );
    // sort then head: top N.
    assert_eq!(
        run_checked("sort countZ=nr | head 2 | cols id,countZ", INPUT),
        "id,countZ\n4,9\n1,5\n"
    );
}

#[test]
fn tail_plus_n_starts_at_row_n() {
    // coreutils' `tail -n +N`: from row N on (a streaming skip, unlike `tail N`).
    assert_eq!(run_checked("tail +3 | cols id", INPUT), "id\n3\n4\n5\n");
    assert_eq!(run_checked("tail -n +3 | cols id", INPUT), "id\n3\n4\n5\n");
    // A window: skip 1, then the next 2 — across the test chunk boundary too.
    let long: String = std::iter::once("id\n".to_string())
        .chain((1..=200).map(|i| format!("{i}\n")))
        .collect();
    assert_eq!(run_checked("tail +150 | head 2", &long), "id\n150\n151\n");
    assert_eq!(
        run_checked("head 152 | tail +150", &long),
        "id\n150\n151\n152\n"
    );
    assert_eq!(run_checked("head 2 | tail +5", &long), "id\n");
    // After a sort it skips in sorted order (countZ desc: ids 4,1,5,2,3).
    assert_eq!(
        run_checked("sort countZ=nr | tail +4 | cols id", INPUT),
        "id\n2\n3\n"
    );
}

#[test]
fn rename_changes_header_only() {
    assert_eq!(
        run_checked("rename fieldA=flag, countZ=z | cols id,flag,z", INPUT),
        "id,flag,z\n1,t,5\n2,f,0\n3,t,0\n4,t,9\n5,f,2\n"
    );
    // A later stage can refer to the new name. (countZ > 0 for rows 1, 4, 5.)
    assert_eq!(
        run_checked("rename countZ=z | select z > 0 | cols id", INPUT),
        "id\n1\n4\n5\n"
    );
}

#[test]
fn fmt_aligns_columns() {
    // fmt is whitespace-aligned, not CSV: columns padded to their widest cell.
    // The numeric `id` column is right-justified; the text `fieldB` is left.
    let out = run("select fieldA == 't' | cols id,fieldB | fmt", INPUT, 1).unwrap();
    assert_eq!(out, "id  fieldB\n 1  x\n 3  x\n 4  z\n");
}

#[test]
fn fmt_right_justifies_numeric_columns() {
    // Every data cell numeric (incl. a negative) => the column is right-justified
    // so digits line up, and the header rides with its column's width.
    let out = run_checked("cols id,countA | fmt", INPUT);
    assert_eq!(
        out,
        "id  countA\n 1       3\n 2       0\n 3      -2\n 4       7\n 5       0\n"
    );
}

#[test]
fn fmt_mixes_numeric_and_text_justification() {
    // numeric, text, numeric: id and amount right-justified, name left.
    let out = run_checked_with_header("fmt", HEADERLESS, Some(HEADER));
    assert_eq!(
        out,
        "id  name   amount\n 1  alice     100\n 2  bob       200\n 3  carol     300\n"
    );
}

// Headerless input: every line is data; `--header` supplies the column names.
const HEADERLESS: &str = "1,alice,100\n2,bob,200\n3,carol,300\n";
const HEADER: &[&str] = &["id", "name", "amount"];

/// Four columns for the positional-reference tests: `2` is `qty`, `4` is `name`.
const POSITIONAL: &str = "id,qty,stock,name\n1,100,50,alice\n2,9,20,bob\n";

#[test]
fn columns_by_position_and_range() {
    // Positional references (1-based) and ranges work wherever a column is
    // named; the resolver is shared, so sort sees them too. The auto-named
    // header from `--header -` (`c1,c2,…`) is what makes this useful.
    let input = POSITIONAL;
    assert_eq!(run_checked("cols 4,1", input), "name,id\nalice,1\nbob,2\n");
    assert_eq!(
        run_checked("cols 2-3 | sort 1", input),
        "qty,stock\n9,20\n100,50\n"
    );
    assert_eq!(run_checked("cols -v 2:name", input), "id\n1\n2\n");
    assert_eq!(
        run_checked_with_header("cols 1,b:3", input, Some(&["a", "b", "c", "d"])),
        "a,b,c\nid,qty,stock\n1,100,50\n2,9,20\n"
    );
    let err = run("cols 5", input, 1).unwrap_err();
    assert!(err.contains("column index 5 is out of range"), "{err}");
    // A bad position or a range with an unknown endpoint in `cols -v` is an
    // error too, not a silent no-op (only an unknown bare name is ignored).
    let err = run("cols -v 2-9", input, 1).unwrap_err();
    assert!(err.contains("out of range"), "{err}");
    let err = run("cols -v 9", input, 1).unwrap_err();
    assert!(err.contains("column index 9 is out of range"), "{err}");
    // A column literally named `7` wins over the (non-existent) position 7.
    assert_eq!(run_checked("cols -v 7", "id,7\n1,x\n"), "id\n1\n");
    let err = run("cols -v qty:nope", input, 1).unwrap_err();
    assert!(err.contains("range qty:nope"), "{err}");
    assert!(err.contains("column not found: nope"), "{err}");
    assert_eq!(
        run_checked("cols -v nope", input),
        "id,qty,stock,name\n1,100,50,alice\n2,9,20,bob\n"
    );
    // In an expression a bare integer is a literal; a backticked one is a
    // column position.
    assert_eq!(run_checked("select `2` > 20 | cols id", input), "id\n1\n");
    assert_eq!(run_checked("select 2 > 20", input), "id,qty,stock,name\n");
}

#[test]
fn positional_refs_title_charts_with_the_column_name() {
    let input = POSITIONAL;
    let bar = run("graph bar 4,2", input, 1).unwrap();
    assert_eq!(bar.lines().next(), Some("qty"), "{bar}");
    let line = run("graph line 1,2", input, 1).unwrap();
    assert_eq!(line.lines().next(), Some("qty vs id"), "{line}");
}

#[test]
fn positional_refs_emit_the_real_column_names() {
    // Stages that put a column's name into the output (group keys, agg names,
    // the stats `field` column) must use the header name, not the spec text.
    let input = POSITIONAL;
    assert_eq!(
        run_checked("agg count by 4", input),
        "name,count\nalice,1\nbob,1\n"
    );
    assert_eq!(
        run_checked("agg sum(2) by 4", input),
        "name,qty_sum\nalice,100\nbob,9\n"
    );
    assert_eq!(
        run_checked("stats 2,3 | cols field", input),
        "field\nqty\nstock\n"
    );
}

#[test]
fn fixture_files_are_removed_when_dropped() {
    let path = {
        let fixture = temp_csv("a\n1\n");
        assert!(fixture.exists());
        fixture.to_path_buf()
    };
    assert!(!path.exists());
}

#[test]
fn header_flag_prepends_header_all_lines_are_data() {
    // The first input line is data, not a header — it survives to the output.
    assert_eq!(
        run_checked_with_header("cols id,name,amount", HEADERLESS, Some(HEADER)),
        "id,name,amount\n1,alice,100\n2,bob,200\n3,carol,300\n"
    );
}

#[test]
fn header_flag_columns_usable_downstream() {
    // The given names are referenceable; numeric compare works (no header eaten).
    assert_eq!(
        run_checked_with_header("select amount > 150 | cols name", HEADERLESS, Some(HEADER)),
        "name\nbob\ncarol\n"
    );
}

#[test]
fn header_flag_sharded_file_matches_serial() {
    // Sharded file path (data_start = 0): first line is data in every shard split.
    let path = temp_csv(HEADERLESS);
    let serial = run_file_with_header("select id >= 2", &path, Some(HEADER), 1);
    let parallel = run_file_with_header("select id >= 2", &path, Some(HEADER), 4);
    assert_eq!(serial, parallel);
    assert_eq!(serial, "id,name,amount\n2,bob,200\n3,carol,300\n");
}

#[test]
fn hdr_command_points_at_the_flag() {
    let err = run("hdr id,name,amount", HEADERLESS, 1).unwrap_err();
    assert!(err.contains("--header a,b,c"), "got: {err}");
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

#[test]
fn add_can_name_its_target_by_position() {
    // `add` takes its target by position like any other column reference:
    // the column is replaced in place (the header keeps its name) and typed
    // by the expression, so a later bare sort on it is pinned.
    let input = "id,qty\n1,10\n2,9\n3,100\n";
    assert_eq!(
        run_checked("add 2 = str(`2`) | sort qty", input),
        "id,qty\n1,10\n3,100\n2,9\n"
    );
    assert_eq!(
        run_checked("add 2 = num(`2`) | sort qty", input),
        "id,qty\n2,9\n1,10\n3,100\n"
    );
    // A column literally named `7` wins over the (non-existent) position 7.
    assert_eq!(
        run_checked("add 7 = num(qty) * 2", "7,qty\nx,10\n"),
        "7,qty\n20,10\n"
    );
}

#[test]
fn add_rejects_a_position_that_is_out_of_range() {
    // A bare integer target is a position, so one past the header is an
    // error like anywhere else, not a new column named after the number.
    let input = "id,qty\n1,10\n";
    let err = run("add 7 = num(qty)", input, 1).unwrap_err();
    assert!(err.contains("column index 7 is out of range"), "{err}");
    let err = run("add 0 = num(qty)", input, 1).unwrap_err();
    assert!(err.contains("column index 0 is out of range"), "{err}");
}
