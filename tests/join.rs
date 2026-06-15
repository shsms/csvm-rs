//! End-to-end tests for `join`: the four join types, composite/aliased keys,
//! one-to-many fan-out, column-clash suffixing (default and configured), a
//! right-side sub-pipeline, post-join stages, and the error paths. The left
//! side is fed as a stream; the right side is a temp file (its build side).

use csvm::exec::{self, RunOpts};
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_csv(content: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "csvm_join_{}_{}.csv",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, content).unwrap();
    path
}

/// Run `script` over `left` (a streamed input), driving the same steps as the
/// binary: parse, prepare joins (reads right files), resolve, run.
fn run(script: &str, left: &str) -> Result<String, String> {
    let mut plan = csvm::parse::parse(script).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(left.as_bytes());
    let header = match plan.input_header.as_deref() {
        Some(h) => h.to_vec(),
        None => exec::read_header(&mut reader).map_err(|e| e.to_string())?,
    };
    exec::prepare_joins(&mut plan).map_err(|e| e.to_string())?;
    let out_header = plan.resolve(&header).map_err(|e| e.to_string())?;
    let opts = RunOpts {
        chunk_size: 64, // small, to exercise multi-chunk reads on the left
        threads: 1,
        temp_dir: std::env::temp_dir(),
        sort_buffer: 1 << 20,
    };
    let mut out = Vec::new();
    exec::run(&plan, &out_header, &opts, &mut reader, &mut out).map_err(|e| e.to_string())?;
    String::from_utf8(out).map_err(|e| e.to_string())
}

const SALES: &str = "id,sku,qty\n1,A,3\n2,B,1\n3,A,5\n4,C,2\n";
const PRICES: &str = "sku,price,qty\nA,10,100\nB,20,200\nD,40,400\n";

#[test]
fn inner_join_keeps_matches_and_suffixes_clash() {
    let p = temp_csv(PRICES);
    // `qty` is on both sides (non-key) → right one becomes `qty_r`.
    let out = run(&format!("join {} on sku", p.display()), SALES).unwrap();
    assert_eq!(
        out,
        "id,sku,qty,price,qty_r\n\
         1,A,3,10,100\n\
         2,B,1,20,200\n\
         3,A,5,10,100\n"
    );
}

#[test]
fn left_join_pads_unmatched_right() {
    let p = temp_csv(PRICES);
    let out = run(&format!("join -l {} on sku", p.display()), SALES).unwrap();
    assert_eq!(
        out,
        "id,sku,qty,price,qty_r\n\
         1,A,3,10,100\n\
         2,B,1,20,200\n\
         3,A,5,10,100\n\
         4,C,2,,\n"
    );
}

#[test]
fn right_join_appends_unmatched_right_with_key_coalesced() {
    let p = temp_csv(PRICES);
    let out = run(&format!("join -r {} on sku", p.display()), SALES).unwrap();
    // Unmatched right row D: left cols empty except the key `sku`.
    assert_eq!(
        out,
        "id,sku,qty,price,qty_r\n\
         1,A,3,10,100\n\
         2,B,1,20,200\n\
         3,A,5,10,100\n\
         ,D,,40,400\n"
    );
}

#[test]
fn full_join_is_left_then_unmatched_right() {
    let p = temp_csv(PRICES);
    let out = run(&format!("join -F {} on sku", p.display()), SALES).unwrap();
    assert_eq!(
        out,
        "id,sku,qty,price,qty_r\n\
         1,A,3,10,100\n\
         2,B,1,20,200\n\
         3,A,5,10,100\n\
         4,C,2,,\n\
         ,D,,40,400\n"
    );
}

#[test]
fn one_to_many_fans_out() {
    let right = temp_csv("sku,tag\nA,x\nA,y\nB,z\n");
    let out = run(&format!("join {} on sku", right.display()), SALES).unwrap();
    // The two A rows each match both A tags → fan-out.
    assert_eq!(
        out,
        "id,sku,qty,tag\n\
         1,A,3,x\n\
         1,A,3,y\n\
         2,B,1,z\n\
         3,A,5,x\n\
         3,A,5,y\n"
    );
}

#[test]
fn aliased_key_names() {
    let right = temp_csv("item,price\nA,10\nB,20\n");
    let out = run(&format!("join {} on sku=item", right.display()), SALES).unwrap();
    assert_eq!(out, "id,sku,qty,price\n1,A,3,10\n2,B,1,20\n3,A,5,10\n");
}

#[test]
fn composite_key() {
    // Both `sku` and `qty` are keys, so the right `qty` is a key (dropped).
    let right = temp_csv("sku,qty,loc\nA,3,NY\nA,5,LA\n");
    let out = run(&format!("join {} on sku,qty", right.display()), SALES).unwrap();
    assert_eq!(out, "id,sku,qty,loc\n1,A,3,NY\n3,A,5,LA\n");
}

#[test]
fn right_sub_pipeline_filters_and_projects() {
    let p = temp_csv(PRICES);
    // The `|` lives inside the parens; only price>15 (B) survives the right side.
    let out = run(
        &format!(
            "join (cols sku,price | select price > 15) {} on sku",
            p.display()
        ),
        SALES,
    )
    .unwrap();
    assert_eq!(out, "id,sku,qty,price\n2,B,1,20\n");
}

#[test]
fn stages_compose_after_join() {
    let p = temp_csv(PRICES);
    let out = run(
        &format!(
            "join {} on sku | sort price=nr | cols id,price",
            p.display()
        ),
        SALES,
    )
    .unwrap();
    assert_eq!(out, "id,price\n2,20\n1,10\n3,10\n");
}

#[test]
fn configured_suffixes_apply_to_both_sides() {
    let p = temp_csv(PRICES);
    let out = run(
        &format!("join --lsuffix _s --rsuffix _p {} on sku", p.display()),
        SALES,
    )
    .unwrap();
    // Only the clashing `qty` is suffixed; `price` (no clash) is untouched.
    assert_eq!(
        out,
        "id,sku,qty_s,price,qty_p\n\
         1,A,3,10,100\n\
         2,B,1,20,200\n\
         3,A,5,10,100\n"
    );
}

#[test]
fn missing_key_column_errors() {
    let p = temp_csv(PRICES);
    let err = run(&format!("join {} on nope", p.display()), SALES).unwrap_err();
    assert!(err.contains("nope"), "got: {err}");
}

#[test]
fn stdin_right_side_rejected() {
    let err = csvm::parse::parse("join - on sku").unwrap_err().to_string();
    assert!(err.contains("not stdin"), "got: {err}");
}
