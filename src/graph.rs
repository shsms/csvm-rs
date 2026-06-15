//! Terminal-native charts (the `graph` sink). Draws with Unicode block and
//! braille glyphs straight to the terminal — no plotting dependency, matching
//! csvm's point-it-at-a-CSV-and-get-an-answer flow. Histogram, horizontal bar,
//! sparkline, and braille scatter/line (multi-series, coloured).

use crate::color::{Rgb, Style};
use crate::field::format_num;

/// Eighth-width block glyphs, 1/8…8/8, for sub-character bar lengths.
const BLOCKS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// Eighth-height block glyphs, 1/8…8/8, for sparkline levels.
const VBLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Default cap on the number of bars drawn (one terminal line each); excess is
/// dropped and reported rather than flooding the screen ("no silent caps").
pub const MAX_BARS: usize = 50;

/// Base chart dimensions at `scale = 1.0`: total width in columns and the
/// scatter/line canvas height in rows. A single `--scale` multiplies both.
const BASE_W: usize = 80;
const BASE_H: usize = 15;

/// Resolve a `--scale` factor to a `(width, height)` in terminal cells, clamped
/// so a tiny scale still leaves room to draw.
pub fn chart_size(scale: f64) -> (usize, usize) {
    let dim = |base: usize, min: usize| ((base as f64 * scale).round() as usize).max(min);
    (dim(BASE_W, 16), dim(BASE_H, 2))
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
    /// the column name; `width` is the total chart width (the bars fill what is
    /// left after the axis).
    pub fn render(&self, title: &str, width: usize) -> String {
        let nbins = self.counts.len();
        let span = self.hi - self.lo;
        let step = if nbins > 0 { span / nbins as f64 } else { 0.0 };

        // Left axis: each bin's lower edge, right-aligned to a common width.
        let edges: Vec<String> = (0..nbins)
            .map(|i| format_num(self.lo + step * i as f64))
            .collect();
        let axis_w = edges.iter().map(String::len).max().unwrap_or(1);

        let max_count = self.counts.iter().copied().max().unwrap_or(0);
        let bars = width.saturating_sub(axis_w + 12).max(10);

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
    width: usize,
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
    let w = width.saturating_sub(label_w + 14).max(10);
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
pub fn render_spark(title: &str, values: &[f64], width: usize, skipped: u64) -> String {
    if values.is_empty() {
        return format!("{title}: no numeric values to plot (skipped {skipped} non-numeric)\n");
    }
    let cols = width.max(1);
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

// --- braille canvas (scatter / line) ---------------------------------------

/// Dot bit per (row-in-cell, col-in-cell). A braille cell is 2×4 dots; the glyph
/// is `U+2800 + bits`. Layout: dots 1-3 then 7 down the left column, 4-6 then 8
/// down the right (the Unicode ordering).
const BRAILLE_DOTS: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

/// Distinct foreground colours for multi-series charts (cycled).
const SERIES_RGB: [Rgb; 6] = [
    Rgb(0x4f, 0xc3, 0xf7), // cyan
    Rgb(0xff, 0x8a, 0x65), // orange
    Rgb(0x81, 0xc7, 0x84), // green
    Rgb(0xba, 0x68, 0xc8), // purple
    Rgb(0xff, 0xd5, 0x4f), // yellow
    Rgb(0xe5, 0x73, 0x73), // red
];

fn series_style(i: usize) -> Style {
    Style {
        fg: Some(SERIES_RGB[i % SERIES_RGB.len()]),
        ..Style::default()
    }
}

/// A braille pixel grid: `w`×`h` terminal cells, each holding 2×4 dots, so the
/// effective resolution is `2w`×`4h` pixels. Origin is top-left.
struct Braille {
    w: usize,
    h: usize,
    bits: Vec<u8>,
}

impl Braille {
    fn new(w: usize, h: usize) -> Self {
        Braille {
            w,
            h,
            bits: vec![0; w * h],
        }
    }

    fn pw(&self) -> usize {
        self.w * 2
    }
    fn ph(&self) -> usize {
        self.h * 4
    }

    /// Light the dot at pixel `(x, y)`; out-of-range coordinates are ignored.
    fn set(&mut self, x: usize, y: usize) {
        if x >= self.pw() || y >= self.ph() {
            return;
        }
        let (cx, cy) = (x / 2, y / 4);
        self.bits[cy * self.w + cx] |= BRAILLE_DOTS[y % 4][x % 2];
    }

    /// Draw a line between two pixels (Bresenham), for connected `line` charts.
    fn line(&mut self, x0: isize, y0: isize, x1: isize, y1: isize) {
        let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
        let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
        let (mut x, mut y, mut err) = (x0, y0, dx + dy);
        loop {
            if x >= 0 && y >= 0 {
                self.set(x as usize, y as usize);
            }
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }
}

fn braille_char(bits: u8) -> char {
    char::from_u32(0x2800 + bits as u32).unwrap_or(' ')
}

/// How to label the x-axis of a scatter/line chart.
pub enum XAxis {
    /// A numeric x: interpolate ticks over the range and format as numbers.
    Numeric,
    /// A timestamp x (epoch seconds): interpolate ticks and format as dates.
    Time,
    /// A category / row-index x: only the first and last raw cells are known.
    Ends(String, String),
}

/// Tick `(fraction, label)` pairs evenly spaced over `[lo, hi]`.
fn ticks(lo: f64, hi: f64, k: usize, fmt: impl Fn(f64) -> String) -> Vec<(f64, String)> {
    (0..k)
        .map(|i| {
            let t = if k <= 1 {
                0.0
            } else {
                i as f64 / (k - 1) as f64
            };
            (t, fmt(lo + (hi - lo) * t))
        })
        .collect()
}

/// "Nice" numeric ticks at round multiples of a 1/2/5×10ⁿ step (so labels read
/// 0, 50, 100, … not 49.166667), as `(fraction, label)` pairs over `[lo, hi]`.
pub(crate) fn nice_ticks(lo: f64, hi: f64, target: usize) -> Vec<(f64, String)> {
    let span = hi - lo;
    if span <= 0.0 || target == 0 {
        return vec![(0.0, format_num(lo))];
    }
    let raw = span / target as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let norm = raw / mag;
    let step = mag
        * if norm < 1.5 {
            1.0
        } else if norm < 3.0 {
            2.0
        } else if norm < 7.0 {
            5.0
        } else {
            10.0
        };
    let mut out = Vec::new();
    let mut v = (lo / step).ceil() * step;
    while v <= hi + step * 1e-9 {
        out.push(((v - lo) / span, format_num(v)));
        // When the values dwarf the step (huge magnitude, tiny span), `v + step`
        // can't change the float — stop rather than loop forever.
        let next = v + step;
        if next <= v {
            break;
        }
        v = next;
    }
    out
}

/// How many ticks fit across `width` columns given a label of `label_w` chars
/// (with a small gap), clamped to a sensible 2..=7.
fn tick_count(width: usize, label_w: usize) -> usize {
    (width / (label_w + 3)).clamp(2, 7)
}

/// Lay tick labels into a `width`-wide row, each centred on its fractional
/// position; a label that would collide with the previous one is dropped. With
/// `force_ends`, the first/last labels are pinned to the left/right edges (for
/// evenly-spaced ticks whose ends are the data range) and middles also stay
/// clear of the reserved last label; without it (nice ticks, whose ends aren't
/// the range) every label sits at its true position.
fn place_ticks(width: usize, labels: &[(f64, String)], force_ends: bool) -> String {
    let mut buf = vec![' '; width];
    let n = labels.len();
    let last_w = labels.last().map_or(0, |(_, l)| l.chars().count());
    let last_start = width.saturating_sub(last_w);
    let mut last_end = 0usize;
    for (i, (t, lab)) in labels.iter().enumerate() {
        let lw = lab.chars().count();
        let centred = ((t * width.saturating_sub(1) as f64).round() as usize)
            .saturating_sub(lw / 2)
            .min(width.saturating_sub(lw));
        let (start, is_end) = match (force_ends, i) {
            (true, 0) => (0, true),
            (true, i) if i == n - 1 => (last_start, true),
            _ => (centred, false),
        };
        if !is_end && (start < last_end || (force_ends && start + lw + 1 > last_start)) {
            continue;
        }
        for (j, ch) in lab.chars().enumerate() {
            if let Some(slot) = buf.get_mut(start + j) {
                *slot = ch;
            }
        }
        last_end = start + lw + 1;
    }
    let s: String = buf.into_iter().collect();
    s.trim_end().to_string()
}

/// Build the x-axis label row (without the gutter prefix) for `xaxis` over the
/// data range `[xlo, xhi]` and a `width`-wide canvas.
fn x_label_row(xaxis: &XAxis, xlo: f64, xhi: f64, width: usize) -> String {
    let (labels, force_ends) = match xaxis {
        XAxis::Ends(lo, hi) => (vec![(0.0, lo.clone()), (1.0, hi.clone())], true),
        // Round 1/2/5 ticks at their true positions (ends aren't forced).
        XAxis::Numeric => {
            let lw = format_num(xlo).len().max(format_num(xhi).len());
            (nice_ticks(xlo, xhi, tick_count(width, lw)), false)
        }
        // Drop the date from time labels when every tick is the same day.
        XAxis::Time => {
            let (fmt, lw): (fn(f64) -> String, usize) = if crate::datetime::same_day(xlo, xhi) {
                (crate::datetime::format_time, 8) // HH:MM:SS
            } else {
                (crate::datetime::format_epoch, 19) // yyyy-mm-dd HH:MM:SS
            };
            (ticks(xlo, xhi, tick_count(width, lw), fmt), true)
        }
    };
    place_ticks(width, &labels, force_ends)
}

/// Render a scatter (`connect=false`) or line (`connect=true`) chart of one or
/// more y-series against a shared x, on a braille canvas with a labelled frame.
/// Multiple series get distinct colours (when `colors`); on a shared cell the
/// first series wins the glyph (overlap is approximate, as in other terminal
/// plotters). `width`/`height` are the canvas size in terminal cells. `skipped`
/// counts dropped non-numeric points. `xaxis` selects how the bottom axis is
/// graduated (numeric/time interpolate intermediate ticks; a category axis shows
/// only the first/last cells).
#[allow(clippy::too_many_arguments)] // a chart has many independent display inputs
pub fn render_xy(
    title: &str,
    names: &[String],
    series: &[Vec<(f64, f64)>],
    width: usize,
    height: usize,
    colors: bool,
    connect: bool,
    skipped: u64,
    xaxis: XAxis,
) -> String {
    let total: usize = series.iter().map(Vec::len).sum();
    if total == 0 {
        return format!("{title}: no numeric points to plot (skipped {skipped} non-numeric)\n");
    }
    let mut xlo = f64::INFINITY;
    let mut xhi = f64::NEG_INFINITY;
    let mut ylo = f64::INFINITY;
    let mut yhi = f64::NEG_INFINITY;
    for pts in series {
        for &(x, y) in pts {
            xlo = xlo.min(x);
            xhi = xhi.max(x);
            ylo = ylo.min(y);
            yhi = yhi.max(y);
        }
    }
    let (xspan, yspan) = (xhi - xlo, yhi - ylo);

    // Left gutter holds the y-axis labels (top = yhi, bottom = ylo).
    let yhi_s = format_num(yhi);
    let ylo_s = format_num(ylo);
    let gutter = yhi_s.len().max(ylo_s.len());
    let wcells = width.saturating_sub(gutter + 3).max(4);
    let hcells = height.max(2);

    let map = |x: f64, y: f64, b: &Braille| {
        let px = if xspan > 0.0 {
            ((x - xlo) / xspan * (b.pw() - 1) as f64).round() as isize
        } else {
            (b.pw() / 2) as isize
        };
        // y is flipped: the top row is the high value.
        let py = if yspan > 0.0 {
            ((b.ph() - 1) as f64 - (y - ylo) / yspan * (b.ph() - 1) as f64).round() as isize
        } else {
            (b.ph() / 2) as isize
        };
        (px, py)
    };

    let canvases: Vec<Braille> = series
        .iter()
        .map(|pts| {
            let mut b = Braille::new(wcells, hcells);
            if connect {
                for pair in pts.windows(2) {
                    let (x0, y0) = map(pair[0].0, pair[0].1, &b);
                    let (x1, y1) = map(pair[1].0, pair[1].1, &b);
                    b.line(x0, y0, x1, y1);
                }
                if pts.len() == 1 {
                    let (x, y) = map(pts[0].0, pts[0].1, &b);
                    if x >= 0 && y >= 0 {
                        b.set(x as usize, y as usize);
                    }
                }
            } else {
                for &(x, y) in pts {
                    let (px, py) = map(x, y, &b);
                    if px >= 0 && py >= 0 {
                        b.set(px as usize, py as usize);
                    }
                }
            }
            b
        })
        .collect();

    let multi = series.len() > 1;
    let mut out = String::new();
    out.push_str(title);
    out.push('\n');
    for cy in 0..hcells {
        // Gutter label: yhi on the first row, ylo on the last.
        let label = if cy == 0 {
            &yhi_s
        } else if cy == hcells - 1 {
            &ylo_s
        } else {
            ""
        };
        out.push_str(&format!("{label:>gutter$} ┤"));
        for cx in 0..wcells {
            let hit = canvases
                .iter()
                .enumerate()
                .find(|(_, c)| c.bits[cy * wcells + cx] != 0);
            match hit {
                None => out.push(' '),
                Some((si, c)) => {
                    let ch = braille_char(c.bits[cy * wcells + cx]).to_string();
                    if colors && multi {
                        out.push_str(&series_style(si).paint(&ch));
                    } else {
                        out.push_str(&ch);
                    }
                }
            }
        }
        out.push('\n');
    }
    // Bottom axis with graduated x labels (interpolated ticks for a numeric or
    // time axis; first/last cells for a category axis).
    out.push_str(&format!("{:>gutter$} └{}\n", "", "─".repeat(wcells)));
    out.push_str(&format!(
        "{:>gutter$}  {}\n",
        "",
        x_label_row(&xaxis, xlo, xhi, wcells)
    ));

    out.push_str(&format!("points={total}"));
    if skipped > 0 {
        out.push_str(&format!("  (skipped {skipped} non-numeric)"));
    }
    out.push('\n');
    if multi {
        let legend: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                if colors {
                    format!("{} {n}", series_style(i).paint("●"))
                } else {
                    n.clone()
                }
            })
            .collect();
        out.push_str(&legend.join("  "));
        out.push('\n');
    }
    out
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
        let s = h.render("amount", 40);
        assert!(s.starts_with("amount\n"));
        assert!(s.contains("n=3"));
        assert!(s.contains("min=1"));
        assert!(s.contains("max=3"));
        assert!(s.contains("(skipped 2 non-numeric)"));
    }

    #[test]
    fn bars_anchor_positive_at_left_edge() {
        let rows = [("a".to_string(), 2.0), ("b".to_string(), 4.0)];
        let s = render_bars("v", &rows, 30, 0, 0);
        assert!(s.starts_with("v\n"));
        // All-positive: the zero baseline is the left edge, so bars start there.
        let a_line = s.lines().nth(1).unwrap();
        assert!(a_line.contains("│█"), "{a_line}");
        assert!(s.contains("bars=2"));
    }

    #[test]
    fn bars_diverge_around_zero_for_negatives() {
        let rows = [("pos".to_string(), 5.0), ("neg".to_string(), -5.0)];
        let s = render_bars("d", &rows, 40, 0, 0);
        let pos = s.lines().find(|l| l.contains("pos")).unwrap();
        let neg = s.lines().find(|l| l.contains("neg")).unwrap();
        // The negative bar starts left of where the positive bar starts.
        let bar_start = |l: &str| l.find('█').unwrap();
        assert!(bar_start(neg) < bar_start(pos), "neg={neg} pos={pos}");
    }

    #[test]
    fn bars_report_skipped_and_truncated() {
        let rows = [("a".to_string(), 1.0)];
        let s = render_bars("v", &rows, 30, 3, 2);
        assert!(s.contains("(+2 more not shown)"), "{s}");
        assert!(s.contains("(skipped 3 non-numeric)"), "{s}");
    }

    #[test]
    fn spark_is_one_line_scaled_to_width() {
        let s = render_spark("v", &[1.0, 2.0, 3.0, 4.0], 4, 0);
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
        let s = render_spark("v", &vals, 10, 0);
        assert_eq!(s.lines().nth(1).unwrap().chars().count(), 10);
    }

    #[test]
    fn braille_set_maps_pixels_to_dot_bits() {
        let mut b = Braille::new(1, 1); // one cell = 2×4 pixels
        b.set(0, 0); // dot 1
        assert_eq!(b.bits[0], 0x01);
        b.set(1, 3); // dot 8
        assert_eq!(b.bits[0], 0x01 | 0x80);
        b.set(99, 99); // out of range — ignored
        assert_eq!(b.bits[0], 0x01 | 0x80);
        assert_eq!(braille_char(0x01), '⠁');
    }

    #[test]
    fn render_xy_frames_a_scatter() {
        let pts = vec![vec![(0.0, 0.0), (1.0, 1.0), (2.0, 2.0)]];
        let s = render_xy(
            "y vs x",
            &["y".into()],
            &pts,
            10,
            4,
            false,
            false,
            0,
            XAxis::Numeric,
        );
        assert!(s.starts_with("y vs x\n"));
        assert!(s.contains('┤')); // y-axis border
        assert!(s.contains('└')); // bottom axis
        assert!(s.contains("points=3"));
        // The axis labels span the data range.
        assert!(s.contains('0') && s.contains('2'));
    }

    #[test]
    fn render_xy_uses_end_labels_for_a_category_axis() {
        // Row-index fallback: positions are 1,2,3 but the axis shows real ends.
        let pts = vec![vec![(1.0, 0.0), (2.0, 1.0), (3.0, 2.0)]];
        let ends = XAxis::Ends("2024-01-01".to_string(), "2024-01-03".to_string());
        let s = render_xy("y vs t", &["y".into()], &pts, 40, 4, false, true, 0, ends);
        assert!(s.contains("2024-01-01") && s.contains("2024-01-03"), "{s}");
    }

    #[test]
    fn nice_ticks_are_round() {
        // target 5 over 0..100 ⇒ a step of 20: 0,20,40,60,80,100.
        let labels: Vec<String> = nice_ticks(0.0, 100.0, 5)
            .into_iter()
            .map(|(_, l)| l)
            .collect();
        assert_eq!(labels, ["0", "20", "40", "60", "80", "100"], "{labels:?}");
        // No repeating decimals even when the range doesn't divide evenly.
        let messy: Vec<String> = nice_ticks(0.0, 295.0, 6)
            .into_iter()
            .map(|(_, l)| l)
            .collect();
        assert!(messy.iter().all(|l| !l.contains('.')), "{messy:?}");
    }

    #[test]
    fn nice_ticks_terminate_on_huge_magnitude_tiny_span() {
        // The step can be smaller than a float ULP at this magnitude, so naive
        // `v += step` would never advance — must still terminate (and bounded).
        let out = nice_ticks(1e10, 1e10 + 2e-6, 5);
        assert!(out.len() < 1000, "should not blow up: {}", out.len());
    }

    #[test]
    fn render_xy_numeric_axis_has_intermediate_ticks() {
        // A wide numeric axis over 0..100 should graduate beyond just the ends.
        let pts = vec![(0..=100).map(|i| (i as f64, i as f64)).collect()];
        let s = render_xy(
            "y vs x",
            &["y".into()],
            &pts,
            80,
            6,
            false,
            false,
            0,
            XAxis::Numeric,
        );
        // More than the two ends (0 and 100) — an intermediate tick near 50.
        assert!(s.contains("50"), "{s}");
    }

    #[test]
    fn render_xy_empty_is_loud() {
        let s = render_xy(
            "y vs x",
            &["y".into()],
            &[vec![]],
            80,
            15,
            false,
            false,
            5,
            XAxis::Numeric,
        );
        assert!(s.contains("no numeric points to plot"));
        assert!(s.contains("skipped 5"));
    }

    #[test]
    fn render_xy_multi_series_adds_a_legend_when_coloured() {
        let series = vec![vec![(0.0, 0.0)], vec![(0.0, 1.0)]];
        let names = ["a".to_string(), "b".to_string()];
        let s = render_xy("t", &names, &series, 8, 4, true, false, 0, XAxis::Numeric);
        assert!(s.contains('\x1b')); // coloured glyphs
        assert!(s.contains('●')); // legend markers
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
