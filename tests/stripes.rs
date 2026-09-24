//! Striped `fmt` tables end to end, and the question csvm asks the terminal
//! about its background: csvm runs on a terminal the test plays
//! (`support/pty.rs`), which answers, types ahead or interrupts as each test
//! needs.

mod common;
#[path = "support/pty.rs"]
mod pty;
use common::temp_csv;
use pty::{Terminal, csvm};

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

/// How csvm asks the terminal for its background (OSC 11).
const QUERY: &str = "\x1b]11;?";
/// A terminal's answer: a white background (OSC 11), then its attributes
/// (DA1), which end the wait.
const WHITE: &[u8] = b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[?62;22c";
/// A table whose first and third data rows are striped.
const TABLE: &str = "a,b\nx,1\ny,2\nz,3\n";

/// The shell words that run csvm, unpaged, with `args` over `input`.
fn striped(args: &[&str], input: &Path) -> String {
    let input = input.to_str().unwrap();
    let args: Vec<&str> = ["--no-pager"]
        .iter()
        .chain(args)
        .chain([&input])
        .copied()
        .collect();
    csvm(&args)
}

/// Colour at 24 bits, so a shade shows as its RGB.
const TRUECOLOR: (&str, &str) = ("COLORTERM", "truecolor");

#[test]
fn a_table_is_striped_off_the_background_the_terminal_reports() {
    let input = temp_csv(TABLE);
    let mut term = Terminal::shell(&striped(&["fmt -s"], &input), &[TRUECOLOR]);
    term.wait_for(QUERY);
    term.send(WHITE);
    assert!(term.finish().success(), "{:?}", term.text());
    let shown = term.text();
    // A little darker than white: the first data row and every other one.
    assert!(shown.contains("\x1b[48;2;237;237;237mx"), "{shown:?}");
    assert!(shown.contains("\x1b[48;2;237;237;237mz"), "{shown:?}");
    assert!(!shown.contains("237my"), "{shown:?}");
}

#[test]
fn a_slow_answer_is_read_and_not_left_for_the_shell() {
    let input = temp_csv(TABLE);
    // The terminal stays open after csvm, so an answer it left behind
    // would be echoed there.
    let line = format!("{}; sleep 1", striped(&["fmt -s"], &input));
    let mut term = Terminal::shell(&line, &[TRUECOLOR]);
    term.wait_for(QUERY);
    thread::sleep(Duration::from_millis(500));
    term.send(WHITE);
    assert!(term.finish().success(), "{:?}", term.text());
    let shown = term.text();
    assert!(shown.contains("\x1b[48;2;237;237;237mx"), "{shown:?}");
    assert!(!shown.contains("rgb:"), "{shown:?}");
}

#[test]
fn keys_typed_ahead_are_left_for_the_next_reader() {
    let input = temp_csv(TABLE);
    // Typed while an earlier command runs, and read after csvm.
    let line = format!(
        "sleep 0.5; {}; read line; echo \"got=[$line]\"",
        striped(&["fmt -s"], &input)
    );
    let mut term = Terminal::shell(&line, &[TRUECOLOR]);
    term.send(b"hello\n");
    assert!(term.finish().success(), "{:?}", term.text());
    let shown = term.text();
    // Not asked: reading the answer would have taken the keys.
    assert!(!shown.contains(QUERY), "{shown:?}");
    assert!(shown.contains("got=[hello]"), "{shown:?}");
}

#[test]
fn ctrl_c_while_asking_ends_the_run_with_the_terminal_restored() {
    let input = temp_csv(TABLE);
    // This terminal never answers, so csvm is still waiting when ^C comes.
    let line = format!("exec {}", striped(&["fmt -s"], &input));
    let mut term = Terminal::shell(&line, &[TRUECOLOR]);
    term.wait_for(QUERY);
    thread::sleep(Duration::from_millis(100));
    let interrupted = Instant::now();
    term.send(b"\x03");
    let status = term.finish();
    assert!(status.signal().is_some(), "{status:?}");
    // At once, not after the rest of the wait.
    assert!(interrupted.elapsed() < Duration::from_millis(500));
    assert!(term.cooked(), "the terminal is left raw");
}

#[test]
fn only_the_terminal_stdout_is_on_is_asked() {
    let input = temp_csv(TABLE);
    // Another terminal, kept open to draw on.
    let other = Terminal::shell("sleep 5", &[]);
    let line = format!(
        "{} > {}",
        striped(&["fmt -s"], &input),
        other.path().display()
    );
    let mut term = Terminal::shell(&line, &[TRUECOLOR]);
    assert!(term.finish().success(), "{:?}", term.text());
    assert!(!term.text().contains(QUERY), "{:?}", term.text());
    // The table went to the other terminal, unshaded.
    other.wait_for("z");
    assert!(!other.text().contains("48;"), "{:?}", other.text());
}

#[test]
fn a_background_job_does_not_ask() {
    let input = temp_csv(TABLE);
    // With job control, `&` runs csvm in a process group of its own, which
    // the terminal would stop for changing its mode.
    let line = format!("set -m; {} & wait", striped(&["fmt -s"], &input));
    let mut term = Terminal::shell(&line, &[TRUECOLOR]);
    assert!(term.finish().success(), "{:?}", term.text());
    assert!(term.text().contains('z'), "{:?}", term.text());
    assert!(!term.text().contains(QUERY), "{:?}", term.text());
}

#[test]
fn a_terminal_that_does_not_answer_leaves_colorfgbg_to_say() {
    let input = temp_csv(TABLE);
    let env = [TRUECOLOR, ("COLORFGBG", "15;0")];
    let started = Instant::now();
    let mut term = Terminal::shell(&striped(&["fmt -s"], &input), &env);
    assert!(term.finish().success(), "{:?}", term.text());
    // The wait is capped.
    assert!(started.elapsed() < Duration::from_secs(5));
    let shown = term.text();
    assert!(shown.contains(QUERY), "{shown:?}");
    // A little lighter than black.
    assert!(shown.contains("\x1b[48;2;18;18;18mx"), "{shown:?}");
    // At 256 colours, the nearest palette entry to it.
    let mut term = Terminal::shell(&striped(&["fmt -s"], &input), &env[1..]);
    assert!(term.finish().success(), "{:?}", term.text());
    assert!(term.text().contains("\x1b[48;5;233mx"), "{:?}", term.text());
    // A plain fmt, or no colour: the terminal is not asked, and nothing is
    // shaded.
    for args in [&["fmt"][..], &["--color", "never", "fmt -s"]] {
        let mut term = Terminal::shell(&striped(args, &input), &env);
        assert!(term.finish().success(), "{:?}", term.text());
        assert!(!term.text().contains(QUERY), "{args:?}: {:?}", term.text());
        assert!(!term.text().contains("48;"), "{args:?}: {:?}", term.text());
    }
}
