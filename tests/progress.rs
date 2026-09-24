//! The progress meter end to end: csvm runs with stderr on a terminal the
//! test plays (`support/pty.rs`) and its output going to a file.

mod common;
#[path = "support/pty.rs"]
mod pty;
use common::temp_csv;
use pty::Terminal;

#[test]
fn a_slow_run_shows_how_far_it_has_read_then_clears_the_line() {
    let csvm = env!("CARGO_BIN_EXE_csvm");
    // Half the input, a pause past the meter's delay, then the rest.
    let slow = "(printf 'a\\n1\\n'; sleep 1.5; printf '2\\n')";
    // Each run writes its own file (the shell's `>` overwrites it, and it is
    // removed when dropped), so the runs can wait out their pauses at once.
    let out: Vec<_> = (0..3).map(|_| temp_csv("")).collect();
    let lines = [
        format!("{slow} | {csvm} 'select a > 0' > {}", out[0].display()),
        // A device such as /dev/null is no pipe: nothing else draws there.
        format!("{slow} | {csvm} 'select a > 0' > /dev/null"),
        // --no-progress keeps stderr quiet, and so does output into a pipe,
        // whose reader (a `less`, say) may be drawing on the same terminal.
        format!(
            "{slow} | {csvm} --no-progress 'select a > 0' > {}",
            out[1].display()
        ),
        format!(
            "{slow} | {csvm} 'select a > 0' | cat > {}",
            out[2].display()
        ),
    ];
    let mut terms: Vec<Terminal> = lines.iter().map(|l| Terminal::shell(l, &[])).collect();
    let shown: Vec<String> = terms
        .iter_mut()
        .map(|term| {
            term.finish();
            term.text()
        })
        .collect();
    assert!(shown[0].contains("csvm: 2 B read"), "{:?}", shown[0]);
    assert!(shown[0].ends_with("\r\x1b[K"), "{:?}", shown[0]);
    assert!(shown[1].contains("csvm: 2 B read"), "{:?}", shown[1]);
    for quiet in &shown[2..] {
        assert!(!quiet.contains("csvm:"), "{quiet:?}");
    }
    for out in &out {
        assert_eq!(std::fs::read_to_string(&**out).unwrap(), "a\n1\n2\n");
    }
}
