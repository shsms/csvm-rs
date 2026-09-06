//! The terminal's size, for charts that should fill the screen.

/// The terminal's column count: `$COLUMNS` when set and numeric, else the
/// size of the terminal on stdout, else `None` (not a terminal, or unknown).
pub fn columns() -> Option<usize> {
    columns_from(std::env::var("COLUMNS").ok().as_deref()).or_else(ioctl_columns)
}

/// `$COLUMNS` read as a column count: a positive integer, else `None` — unset,
/// empty, zero or not a number all mean "ask the terminal instead". Its own
/// function so the rule can be tested without setting a variable the whole
/// process (and every other test thread) would see.
fn columns_from(var: Option<&str>) -> Option<usize> {
    var.and_then(|v| v.parse::<usize>().ok()).filter(|&n| n > 0)
}

#[cfg(unix)]
fn ioctl_columns() -> Option<usize> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ reads the window size of the given descriptor into
    // a `winsize` we own; it writes nothing else and cannot fail unsafely.
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_col > 0).then_some(ws.ws_col as usize)
}

#[cfg(not(unix))]
fn ioctl_columns() -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn columns_from_takes_a_positive_integer_and_nothing_else() {
        assert_eq!(super::columns_from(Some("120")), Some(120));
        // Zero is a width no chart could use, so it is not an answer.
        assert_eq!(super::columns_from(Some("0")), None);
        assert_eq!(super::columns_from(Some("x")), None);
        assert_eq!(super::columns_from(None), None);
    }

    #[test]
    fn columns_is_a_positive_count_or_none() {
        // Under `cargo test` stdout is usually a pipe; either answer is fine,
        // but never zero.
        assert!(super::columns().is_none_or(|n| n > 0));
    }
}
