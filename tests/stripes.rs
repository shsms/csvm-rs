//! Striped `fmt` tables end to end: csvm runs on a terminal (a pty from
//! util-linux `script`) that, like many, does not answer the question about
//! its background, so `$COLORFGBG` has to. Skipped where `script` is not the
//! util-linux one.

mod common;
#[path = "support/terminal.rs"]
mod terminal;
use common::temp_csv;
use terminal::{have_script, script};

use std::time::{Duration, Instant};

/// Run csvm with `args` on a terminal whose background `$COLORFGBG` names
/// black; returns what reached it.
fn on_black_terminal(args: &[&str]) -> String {
    let csvm = env!("CARGO_BIN_EXE_csvm");
    let line = format!("{csvm} --no-pager {}", args.join(" "));
    let out = script(&line).env("COLORFGBG", "15;0").output().unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_table_on_a_terminal_is_striped_off_its_background() {
    if !have_script() {
        return;
    }
    let input = temp_csv("a,b\nx,1\ny,2\nz,3\n");
    let path = input.display().to_string();
    let started = Instant::now();
    let shown = on_black_terminal(&["'fmt -s'", &path]);
    // The terminal did not answer, and csvm did not wait on it for long.
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(shown.contains("\x1b]11;?"), "{shown:?}");
    // The first row after the header, and every other one, on a grey a
    // little lighter than black, as a 256-colour background.
    assert!(shown.contains("\x1b[48;5;233mx"), "{shown:?}");
    assert!(shown.contains("\x1b[48;5;233mz"), "{shown:?}");
    assert_eq!(shown.matches("48;5;").count(), 6, "{shown:?}");
    // A plain fmt, or no colour: the terminal is not asked, and nothing is
    // shaded.
    for plain in [
        on_black_terminal(&["fmt", &path]),
        on_black_terminal(&["--color", "never", "'fmt -s'", &path]),
    ] {
        assert!(!plain.contains("\x1b]11;?"), "{plain:?}");
        assert!(!plain.contains("48;"), "{plain:?}");
    }
}
