//! The terminal's size, for charts that should fill the screen and for the
//! pager, which needs to know whether a table will fit on it.

/// The terminal's column count: `$COLUMNS` when set and numeric, else the
/// size of the terminal on stdout, else `None` (not a terminal, or unknown).
pub fn columns() -> Option<usize> {
    count_from(std::env::var("COLUMNS").ok().as_deref())
        .or_else(|| window().map(|(cols, _)| cols).filter(|&n| n > 0))
}

/// The terminal's row count: `$LINES` when set and numeric, else the size of
/// the terminal on stdout, else `None`.
pub fn rows() -> Option<usize> {
    count_from(std::env::var("LINES").ok().as_deref())
        .or_else(|| window().map(|(_, rows)| rows).filter(|&n| n > 0))
}

/// `$COLUMNS` or `$LINES` read as a count: a positive integer, else `None` —
/// unset, empty, zero or not a number all mean "ask the terminal instead". Its
/// own function so the rule can be tested without setting a variable the whole
/// process (and every other test thread) would see.
fn count_from(var: Option<&str>) -> Option<usize> {
    var.and_then(|v| v.parse::<usize>().ok()).filter(|&n| n > 0)
}

/// The window size of the terminal on stdout, as (columns, rows); `None` when
/// stdout is not a terminal. A count the terminal does not know is zero.
#[cfg(unix)]
fn window() -> Option<(usize, usize)> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ reads the window size of the given descriptor into
    // a `winsize` we own; it writes nothing else and cannot fail unsafely.
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0).then_some((usize::from(ws.ws_col), usize::from(ws.ws_row)))
}

#[cfg(not(unix))]
fn window() -> Option<(usize, usize)> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn count_from_takes_a_positive_integer_and_nothing_else() {
        assert_eq!(super::count_from(Some("120")), Some(120));
        // Zero is a size no chart or table could use, so it is not an answer.
        assert_eq!(super::count_from(Some("0")), None);
        assert_eq!(super::count_from(Some("x")), None);
        assert_eq!(super::count_from(None), None);
    }

    #[test]
    fn the_size_is_a_positive_count_or_none() {
        // Under `cargo test` stdout is usually a pipe; either answer is fine,
        // but never zero.
        assert!(super::columns().is_none_or(|n| n > 0));
        assert!(super::rows().is_none_or(|n| n > 0));
    }
}
