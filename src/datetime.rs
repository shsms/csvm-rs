//! Minimal timestamp parsing for chart axes — no dependency. Parses common
//! ISO-8601 / `yyyy-mm-dd[ T]HH:MM[:SS[.fff]][Z|±HH:MM]` strings to epoch
//! seconds (UTC) and formats epoch seconds back to `yyyy-mm-dd HH:MM:SS`. Used
//! so `graph scatter`/`line` can plot a string timestamp column on a true time
//! axis instead of by row order.

/// Days from the civil date to 1970-01-01 (Howard Hinnant's algorithm; proleptic
/// Gregorian, no leap seconds). `m` is 1..=12.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`]: civil `(y, m, d)` from days since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parse a timestamp to epoch seconds (UTC), or `None` if `s` isn't a
/// recognizable date/time (so non-temporal columns fall through to other
/// handling). Accepts `yyyy-mm-dd`, optionally followed by `T`/space and
/// `HH:MM[:SS[.fff]]`, optionally followed by `Z` or `±HH:MM`/`±HHMM`/`±HH`. A
/// naive time (no zone) is treated as UTC. The year must be ≥4 digits so a
/// dash-separated category (`a-b-c`) isn't mistaken for a date.
pub fn parse_epoch(s: &str) -> Option<f64> {
    let s = s.trim();
    let (date, rest) = match s.find(['T', ' ']) {
        Some(i) => (&s[..i], s[i + 1..].trim()),
        None => (s, ""),
    };

    let mut dp = date.split('-');
    let ys = dp.next()?;
    if ys.len() < 4 {
        return None;
    }
    let y: i64 = ys.parse().ok()?;
    let mo: i64 = dp.next()?.parse().ok()?;
    let d: i64 = dp.next()?.parse().ok()?;
    if dp.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }

    let (mut hh, mut mi, mut ss, mut tz) = (0i64, 0i64, 0f64, 0i64);
    if !rest.is_empty() {
        let (time, tzs): (&str, Option<&str>) = if let Some(t) = rest.strip_suffix(['Z', 'z']) {
            (t, None)
        } else if let Some(i) = rest.find(['+', '-']) {
            (&rest[..i], Some(&rest[i..]))
        } else {
            (rest, None)
        };

        let mut tp = time.split(':');
        hh = tp.next()?.parse().ok()?;
        mi = tp.next()?.parse().ok()?;
        if let Some(sec) = tp.next() {
            ss = sec.parse().ok()?;
        }
        if tp.next().is_some() || !(0..=23).contains(&hh) || !(0..=59).contains(&mi) {
            return None;
        }
        if !(0.0..61.0).contains(&ss) {
            return None;
        }

        if let Some(tzs) = tzs {
            let sign = if tzs.starts_with('-') { -1 } else { 1 };
            let body = &tzs[1..];
            let (th, tm): (i64, i64) = if let Some((a, b)) = body.split_once(':') {
                (a.parse().ok()?, b.parse().ok()?)
            } else if body.len() == 4 {
                (body[..2].parse().ok()?, body[2..].parse().ok()?)
            } else {
                (body.parse().ok()?, 0)
            };
            tz = sign * (th * 3600 + tm * 60);
        }
    }

    let days = days_from_civil(y, mo, d);
    Some(days as f64 * 86400.0 + hh as f64 * 3600.0 + mi as f64 * 60.0 + ss - tz as f64)
}

/// Format epoch seconds (UTC) as `yyyy-mm-dd HH:MM:SS`, for chart axis labels.
pub fn format_epoch(secs: f64) -> String {
    let total = secs.floor() as i64;
    let (days, rem) = (total.div_euclid(86400), total.rem_euclid(86400));
    let (y, m, d) = civil_from_days(days);
    let (hh, mi, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mi:02}:{ss:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_epochs() {
        assert_eq!(parse_epoch("1970-01-01"), Some(0.0));
        assert_eq!(parse_epoch("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_epoch("2024-01-01"), Some(1_704_067_200.0));
        // Space separator + explicit UTC offset (the user's format).
        assert_eq!(
            parse_epoch("2024-01-01 00:00:00+00:00"),
            Some(1_704_067_200.0)
        );
        // A +05:30 zone is 5.5h earlier in UTC.
        assert_eq!(
            parse_epoch("2024-01-01T05:30:00+05:30"),
            Some(1_704_067_200.0)
        );
        // Fractional seconds.
        assert_eq!(parse_epoch("2024-01-01T00:00:00.5Z"), Some(1_704_067_200.5));
    }

    #[test]
    fn rejects_non_timestamps() {
        assert_eq!(parse_epoch("hello"), None);
        assert_eq!(parse_epoch("42"), None); // a plain number isn't a date
        assert_eq!(parse_epoch("1-2-3"), None); // short year ⇒ a category, not a date
        assert_eq!(parse_epoch("2024-13-01"), None); // bad month
        assert_eq!(parse_epoch("2024-01-01T25:00"), None); // bad hour
        assert_eq!(parse_epoch(""), None);
    }

    #[test]
    fn format_round_trips() {
        for ts in [
            "1970-01-01 00:00:00",
            "2024-01-01 00:00:00",
            "2026-06-15 11:56:40",
        ] {
            let e = parse_epoch(ts).unwrap();
            assert_eq!(format_epoch(e), ts, "round-trip {ts}");
        }
    }
}
