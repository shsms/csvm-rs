//! The progress meter end to end: csvm runs with stderr on a terminal the
//! test plays (`support/pty.rs`) and its output going to a file.

mod common;
#[path = "support/pty.rs"]
mod pty;
use common::temp_csv;
use pty::Terminal;

/// Run `shell` on a terminal the test plays, so stdin, stdout and stderr
/// start out on it; returns what reached it.
fn on_terminal(shell: &str) -> String {
    let mut term = Terminal::shell(shell, &[]);
    term.finish();
    term.text()
}

#[test]
fn a_slow_run_shows_how_far_it_has_read_then_clears_the_line() {
    // The shell's `>` overwrites it, and it is removed when dropped.
    let out = temp_csv("");
    let csvm = env!("CARGO_BIN_EXE_csvm");
    // Half the input, a pause past the meter's delay, then the rest.
    let slow = "(printf 'a\\n1\\n'; sleep 1.5; printf '2\\n')";
    let shown = on_terminal(&format!(
        "{slow} | {csvm} 'select a > 0' > {}",
        out.display()
    ));
    assert!(shown.contains("csvm: 2 B read"), "{shown:?}");
    assert!(shown.ends_with("\r\x1b[K"), "{shown:?}");
    assert_eq!(std::fs::read_to_string(&*out).unwrap(), "a\n1\n2\n");
    // A device such as /dev/null is no pipe: nothing else draws there.
    let shown = on_terminal(&format!("{slow} | {csvm} 'select a > 0' > /dev/null"));
    assert!(shown.contains("csvm: 2 B read"), "{shown:?}");
    // --no-progress keeps stderr quiet, and so does output into a pipe,
    // whose reader (a `less`, say) may be drawing on the same terminal.
    for quiet in [
        format!(
            "{slow} | {csvm} --no-progress 'select a > 0' > {}",
            out.display()
        ),
        format!("{slow} | {csvm} 'select a > 0' | cat > {}", out.display()),
    ] {
        let shown = on_terminal(&quiet);
        assert!(!shown.contains("csvm:"), "{quiet}: {shown:?}");
        assert_eq!(std::fs::read_to_string(&*out).unwrap(), "a\n1\n2\n");
    }
}
