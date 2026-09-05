//! The CLI end to end: what only `main` decides (input opening, the header
//! flag, exit codes) is covered by running the built binary.

mod common;
use common::temp_csv;

use std::io::Write;
use std::process::{Command, Stdio};

/// Run the binary with `args`, feeding `stdin`; returns (exit ok, stdout, stderr).
fn csvm(args: &[&str], stdin: &str) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_csvm"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn csvm");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

const HEADERLESS: &str = "1,alice\n2,bob\n";

#[test]
fn header_flag_names_or_auto_names_a_headerless_input() {
    let file = temp_csv(HEADERLESS);
    let path = file.to_str().unwrap();
    for (args, expected) in [
        (
            vec!["--header", "id,name", "cols name,id", path],
            "name,id\nalice,1\nbob,2\n",
        ),
        (
            vec!["--header", "-", "cols c2,c1", path],
            "c2,c1\nalice,1\nbob,2\n",
        ),
        (
            vec!["--header=id,name", "select id > 1", path],
            "id,name\n2,bob\n",
        ),
    ] {
        let (ok, out, err) = csvm(&args, "");
        assert!(ok, "{args:?}: {err}");
        assert_eq!(out, expected, "{args:?}");
    }
    // The same over stdin: the first line is chained back as data.
    for (args, expected) in [
        (
            vec!["--header", "id,name", "cols name,id"],
            "name,id\nalice,1\nbob,2\n",
        ),
        (
            vec!["--header", "-", "cols c2,c1"],
            "c2,c1\nalice,1\nbob,2\n",
        ),
    ] {
        let (ok, out, err) = csvm(&args, HEADERLESS);
        assert!(ok, "{args:?}: {err}");
        assert_eq!(out, expected, "{args:?}");
    }
    // Without the flag the first line is the header, file or stdin.
    let (ok, out, _) = csvm(&["cols 1", path], "");
    assert!(ok);
    assert_eq!(out, "1\n2\n");
    let (ok, out, _) = csvm(&["cols 1"], HEADERLESS);
    assert!(ok);
    assert_eq!(out, "1\n2\n");
}

#[test]
fn header_flag_over_a_sharded_file_keeps_every_row_once() {
    let mut content = String::new();
    for i in 0..2000 {
        content.push_str(&format!("{i},x\n"));
    }
    let file = temp_csv(&content);
    let (ok, out, err) = csvm(
        &[
            "-n",
            "4",
            "--header",
            "id,v",
            "cols id",
            file.to_str().unwrap(),
        ],
        "",
    );
    assert!(ok, "{err}");
    let ids: Vec<&str> = out.lines().skip(1).collect();
    assert_eq!(ids.len(), 2000);
    assert_eq!(ids[0], "0");
    assert_eq!(ids[1999], "1999");
}

#[test]
fn empty_input_errors_unless_the_header_is_named() {
    let empty = temp_csv("");
    let path = empty.to_str().unwrap();
    for args in [
        vec!["head", path],
        vec!["head"],
        vec!["--header", "-", "head", path],
        vec!["--header", "-", "head"],
    ] {
        let (ok, _, err) = csvm(&args, "");
        assert!(!ok, "{args:?} should fail");
        assert!(err.contains("input is empty"), "{args:?}: {err}");
    }
    // A named header makes an empty input a legal zero-row table.
    for args in [
        vec!["--header", "a,b", "cols a", path],
        vec!["--header", "a,b", "cols a"],
    ] {
        let (ok, out, err) = csvm(&args, "");
        assert!(ok, "{args:?}: {err}");
        assert_eq!(out, "a\n", "{args:?}");
    }
}

#[test]
fn removed_spellings_fail_with_a_pointer() {
    for (script, pointer) in [
        ("hdr a,b", "--header a,b,c"),
        ("group k", "agg count by k"),
        ("delta a", "add a_delta = a - prev(a)"),
        ("add x y", "add x = y"),
    ] {
        let (ok, _, err) = csvm(&[script], "a,b\n1,2\n");
        assert!(!ok, "{script}");
        assert!(err.contains(pointer), "{script}: {err}");
    }
    let (ok, _, err) = csvm(&["--print-engine", "head"], "a\n1\n");
    assert!(!ok);
    assert!(err.contains("unknown option"), "{err}");
}
