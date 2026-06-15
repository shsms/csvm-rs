//! Terminal-native charts (the `graph` sink). Draws with Unicode block glyphs
//! straight to the terminal — no plotting dependency, matching csvm's
//! point-it-at-a-CSV-and-get-an-answer flow. Histogram, horizontal bar, and
//! sparkline so far; scatter/line on a braille canvas are the follow-ups (see
//! `todo.org`).

use crate::field::format_num;

/// Eighth-width block glyphs, 1/8…8/8, for sub-character bar lengths.
const BLOCKS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// Eighth-height block glyphs, 1/8…8/8, for sparkline levels.
const VBLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Default cap on the number of bars drawn (one terminal line each); excess is
/// dropped and reported rather than flooding the screen ("no silent caps").
pub const MAX_BARS: usize = 50;

/// Terminal width in columns, from `$COLUMNS` or the conventional 80 fallback
/// (the design's 80×24 default — no ioctl dependency).
fn term_cols() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok())
        .filter(|&c| c > 0)
        .unwrap_or(80)
}

/// A finished histogram: `bins` equal-width buckets spanning `[lo, hi]`, the last
/// inclusive of `hi`. `skipped` counts non-empty cells that weren't finite
/// numbers (reported below the chart, never plotted — the "strict and loud"
/// policy from the design note).
pub struct Histogram {
    pub lo: f64,
    pub hi: f64,
    pub counts: Vec<u64>,
    pub total: u64,
    pub skipped: u64,
}

impl Histogram {
    /// Bin `values` (already parsed to finite numbers) into `bins` buckets.
    /// Returns `None` when there is nothing to plot (no finite values). The
    /// default bin count is Sturges' rule, capped so the chart stays readable.
    pub fn build(values: &[f64], bins: Option<usize>, skipped: u64) -> Option<Histogram> {
        if values.is_empty() {
            return None;
        }
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for &v in values {
            lo = lo.min(v);
            hi = hi.max(v);
        }
        let n = values.len();
        // Sturges: ⌈log2 n⌉ + 1, capped at 50; explicit --bins wins.
        let nbins = bins
            .unwrap_or_else(|| ((n as f64).log2().ceil() as usize + 1).clamp(1, 50))
            .max(1);
        let mut counts = vec![0u64; nbins];
        let span = hi - lo;
        for &v in values {
            // A zero span (all values equal) puts everything in bin 0.
            let idx = if span > 0.0 {
                (((v - lo) / span) * nbins as f64).floor() as usize
            } else {
                0
            };
            counts[idx.min(nbins - 1)] += 1;
        }
        Some(Histogram {
            lo,
            hi,
            counts,
            total: n as u64,
            skipped,
        })
    }

    /// Render to a multi-line string: a right-aligned bin-edge axis, a block bar
    /// per bin, and the count, followed by a summary line. `title` defaults to
    /// the column name; `bar_width` defaults to the terminal width minus the axis.
    pub fn render(&self, title: &str, bar_width: Option<usize>) -> String {
        let nbins = self.counts.len();
        let span = self.hi - self.lo;
        let step = if nbins > 0 { span / nbins as f64 } else { 0.0 };

        // Left axis: each bin's lower edge, right-aligned to a common width.
        let edges: Vec<String> = (0..nbins)
            .map(|i| format_num(self.lo + step * i as f64))
            .collect();
        let axis_w = edges.iter().map(String::len).max().unwrap_or(1);

        let max_count = self.counts.iter().copied().max().unwrap_or(0);
        let bars = bar_width.unwrap_or_else(|| term_cols().saturating_sub(axis_w + 12).max(10));

        let mut out = String::new();
        out.push_str(title);
        out.push('\n');
        for (edge, &count) in edges.iter().zip(&self.counts) {
            out.push_str(&format!(
                "{edge:>axis_w$} │{} {count}\n",
                bar(count, max_count, bars)
            ));
        }
        out.push_str(&format!(
            "n={}  min={}  max={}  bins={}",
            self.total,
            format_num(self.lo),
            format_num(self.hi),
            nbins
        ));
        if self.skipped > 0 {
            out.push_str(&format!("  (skipped {} non-numeric)", self.skipped));
        }
        out.push('\n');
        out
    }
}

/// Draw one labelled horizontal bar per `(label, value)` row, anchored at a zero
/// baseline so negative values extend left (a diverging bar chart). Labels are
/// right-aligned to a common width. `skipped`/`truncated` are reported in the
/// summary. Best used after group-by, where there are few rows.
pub fn render_bars(
    title: &str,
    rows: &[(String, f64)],
    bar_width: Option<usize>,
    skipped: u64,
    truncated: usize,
) -> String {
    if rows.is_empty() {
        return format!("{title}: no numeric values to plot (skipped {skipped} non-numeric)\n");
    }
    let label_w = rows
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0);
    // The baseline is always 0, so a column of positive values bars from the left.
    let mut lo = 0.0f64;
    let mut hi = 0.0f64;
    for (_, v) in rows {
        lo = lo.min(*v);
        hi = hi.max(*v);
    }
    let span = hi - lo;
    let w = bar_width.unwrap_or_else(|| term_cols().saturating_sub(label_w + 14).max(10));
    let zero = pos_in(0.0, lo, span, w);

    let mut out = String::new();
    out.push_str(title);
    out.push('\n');
    for (label, v) in rows {
        let p = pos_in(*v, lo, span, w);
        let (a, b) = (zero.min(p), zero.max(p));
        let mut field = vec![' '; w];
        for cell in field.iter_mut().take(b).skip(a) {
            *cell = '█';
        }
        let bar: String = field.into_iter().collect();
        out.push_str(&format!("{label:>label_w$} │{bar} {}\n", format_num(*v)));
    }
    out.push_str(&format!("bars={}", rows.len()));
    if truncated > 0 {
        out.push_str(&format!("  (+{truncated} more not shown)"));
    }
    if skipped > 0 {
        out.push_str(&format!("  (skipped {skipped} non-numeric)"));
    }
    out.push('\n');
    out
}

/// Column position of `v` within a `width`-wide field spanning `[lo, lo+span]`.
fn pos_in(v: f64, lo: f64, span: f64, width: usize) -> usize {
    if span > 0.0 {
        (((v - lo) / span) * width as f64).round() as usize
    } else {
        0
    }
    .min(width)
}

/// Render a one-line sparkline of `values` (downsampled to `width` by bucket
/// averaging), with a title and a min/max summary. Each cell is an eighth-height
/// block scaled to the value range.
pub fn render_spark(title: &str, values: &[f64], width: Option<usize>, skipped: u64) -> String {
    if values.is_empty() {
        return format!("{title}: no numeric values to plot (skipped {skipped} non-numeric)\n");
    }
    let cols = width.unwrap_or_else(|| term_cols().min(80)).max(1);
    // Bucket-average so a long series collapses to one cell per column.
    let buckets: Vec<f64> = if values.len() <= cols {
        values.to_vec()
    } else {
        (0..cols)
            .map(|i| {
                let start = i * values.len() / cols;
                let end = ((i + 1) * values.len() / cols).max(start + 1);
                let slice = &values[start..end.min(values.len())];
                slice.iter().sum::<f64>() / slice.len() as f64
            })
            .collect()
    };
    let lo = buckets.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = buckets.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let span = hi - lo;
    let line: String = buckets
        .iter()
        .map(|&v| {
            // A flat series sits mid-height; otherwise scale into the 8 levels.
            let level = if span > 0.0 {
                (((v - lo) / span) * 7.0).round() as usize
            } else {
                3
            };
            VBLOCKS[level.min(7)]
        })
        .collect();
    let mut out = format!(
        "{title}\n{line}\nmin={}  max={}",
        format_num(lo),
        format_num(hi)
    );
    if skipped > 0 {
        out.push_str(&format!("  (skipped {skipped} non-numeric)"));
    }
    out.push('\n');
    out
}

/// A horizontal block bar `count/max` of the full width, with an eighth-block
/// fractional tail so short bars stay distinguishable.
fn bar(count: u64, max: u64, width: usize) -> String {
    if max == 0 || width == 0 {
        return String::new();
    }
    let frac = count as f64 / max as f64 * width as f64;
    let full = frac.floor() as usize;
    let mut s: String = "█".repeat(full.min(width));
    let rem = frac - full as f64;
    if rem > 0.0 && full < width {
        let idx = ((rem * 8.0).round() as usize).clamp(1, 8) - 1;
        s.push(BLOCKS[idx]);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bins_span_min_to_max_inclusive() {
        // 0..=9 into 3 bins: edges 0,3,6; the max (9) lands in the last bin.
        let h = Histogram::build(&[0.0, 1.0, 3.0, 4.0, 9.0], Some(3), 0).unwrap();
        assert_eq!(h.lo, 0.0);
        assert_eq!(h.hi, 9.0);
        assert_eq!(h.counts, vec![2, 2, 1]);
        assert_eq!(h.total, 5);
    }

    #[test]
    fn equal_values_collapse_to_one_populated_bin() {
        let h = Histogram::build(&[5.0, 5.0, 5.0], Some(4), 0).unwrap();
        assert_eq!(h.counts.iter().sum::<u64>(), 3);
        assert_eq!(h.counts[0], 3);
    }

    #[test]
    fn empty_values_render_nothing() {
        assert!(Histogram::build(&[], Some(4), 0).is_none());
    }

    #[test]
    fn render_reports_skipped_and_summary() {
        let h = Histogram::build(&[1.0, 2.0, 3.0], Some(2), 2).unwrap();
        let s = h.render("amount", Some(10));
        assert!(s.starts_with("amount\n"));
        assert!(s.contains("n=3"));
        assert!(s.contains("min=1"));
        assert!(s.contains("max=3"));
        assert!(s.contains("(skipped 2 non-numeric)"));
    }

    #[test]
    fn bars_anchor_positive_at_left_edge() {
        let rows = [("a".to_string(), 2.0), ("b".to_string(), 4.0)];
        let s = render_bars("v", &rows, Some(10), 0, 0);
        assert!(s.starts_with("v\n"));
        // All-positive: the zero baseline is the left edge, so bars start there.
        let a_line = s.lines().nth(1).unwrap();
        assert!(a_line.contains("│█"), "{a_line}");
        assert!(s.contains("bars=2"));
    }

    #[test]
    fn bars_diverge_around_zero_for_negatives() {
        let rows = [("pos".to_string(), 5.0), ("neg".to_string(), -5.0)];
        let s = render_bars("d", &rows, Some(10), 0, 0);
        let pos = s.lines().find(|l| l.contains("pos")).unwrap();
        let neg = s.lines().find(|l| l.contains("neg")).unwrap();
        // The negative bar starts left of where the positive bar starts.
        let bar_start = |l: &str| l.find('█').unwrap();
        assert!(bar_start(neg) < bar_start(pos), "neg={neg} pos={pos}");
    }

    #[test]
    fn bars_report_skipped_and_truncated() {
        let rows = [("a".to_string(), 1.0)];
        let s = render_bars("v", &rows, Some(8), 3, 2);
        assert!(s.contains("(+2 more not shown)"), "{s}");
        assert!(s.contains("(skipped 3 non-numeric)"), "{s}");
    }

    #[test]
    fn spark_is_one_line_scaled_to_width() {
        let s = render_spark("v", &[1.0, 2.0, 3.0, 4.0], Some(4), 0);
        let line = s.lines().nth(1).unwrap();
        assert_eq!(line.chars().count(), 4);
        // Ascending values ⇒ the last cell is the tallest block.
        assert_eq!(line.chars().last().unwrap(), '█');
        assert_eq!(line.chars().next().unwrap(), '▁');
        assert!(s.contains("min=1") && s.contains("max=4"));
    }

    #[test]
    fn spark_downsamples_long_series() {
        let vals: Vec<f64> = (0..100).map(|i| i as f64).collect();
        let s = render_spark("v", &vals, Some(10), 0);
        assert_eq!(s.lines().nth(1).unwrap().chars().count(), 10);
    }

    #[test]
    fn bar_scales_and_caps_at_width() {
        assert_eq!(bar(0, 10, 8), "");
        assert_eq!(bar(10, 10, 8), "████████"); // full
        assert!(bar(10, 10, 8).chars().count() == 8);
        // A partial bar gets an eighth-block tail.
        let half = bar(1, 2, 4);
        assert!(half.starts_with("██"));
    }
}
