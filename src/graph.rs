//! Terminal-native charts (the `graph` sink). Draws a [`crate::chart`] model
//! with Unicode block and braille glyphs straight to the terminal — no plotting
//! dependency, matching csvm's point-it-at-a-CSV-and-get-an-answer flow.
//! Histogram, horizontal bar, sparkline, braille scatter/line (multi-series,
//! coloured) and shaded heatmap.

use crate::chart::{
    BarData, ChartData, Frame, Glyphs, HeatData, HistData, SparkData, XyData, bar_value, hist_len,
    value_pos,
};
use crate::color::{Ramp, Rgb, Style};
use crate::field::format_num;

/// Format `v` rounded to `step`'s precision (one digit finer than the step's
/// magnitude), then trimmed — keeps bin-edge labels readable.
fn fmt_to_step(v: f64, step: f64) -> String {
    if step <= 0.0 || !step.is_finite() {
        return format_num(v);
    }
    let decimals = (1.0 - step.log10().floor()).clamp(0.0, 6.0) as i32;
    let factor = 10f64.powi(decimals);
    format_num((v * factor).round() / factor)
}

/// Min and max of `values`, or `None` when empty. The chart data builders lean
/// on it for their own bounds, and the terminal renderer for a spark's; it
/// takes any iterator, so a caller need not gather the values into a slice
/// first.
pub(crate) fn minmax(values: impl IntoIterator<Item = f64>) -> Option<(f64, f64)> {
    let mut it = values.into_iter();
    let first = it.next()?;
    Some(it.fold((first, first), |(lo, hi), v| (lo.min(v), hi.max(v))))
}

/// The left gutter of a framed chart (scatter, line, heatmap): the wider of the
/// two y-bound labels, which is what the gutter has to hold.
pub(crate) fn gutter_width(lo: f64, hi: f64) -> usize {
    format_num(hi).len().max(format_num(lo).len())
}

/// How many cells of a `width`-wide chart are left for the canvas once the
/// gutter for the y bounds `[lo, hi]` and the axis rule are taken out. Shared
/// with the heatmap collector, which sizes its grid to the canvas it will be
/// drawn in.
pub(crate) fn canvas_cells(width: usize, lo: f64, hi: f64) -> usize {
    width.saturating_sub(gutter_width(lo, hi) + 3).max(4)
}

/// Draw `data` in the terminal within `frame`. Each kind has its own drawer;
/// a chart with nothing to plot becomes a one-line diagnostic.
pub fn render(frame: &Frame, data: &ChartData) -> String {
    match data {
        ChartData::Hist(None) => empty_line(frame, "no numeric values to plot"),
        ChartData::Hist(Some(h)) => render_hist(frame, h),
        ChartData::Bar(b) if b.rows.is_empty() => empty_line(frame, "no numeric values to plot"),
        ChartData::Bar(b) => render_bars(frame, b),
        ChartData::Spark(s) if s.values.is_empty() => {
            empty_line(frame, "no numeric values to plot")
        }
        ChartData::Spark(s) => render_spark(frame, s),
        ChartData::Xy(xy) if xy.rows.is_empty() => empty_line(frame, "no numeric points to plot"),
        ChartData::Xy(xy) => render_xy(frame, xy, xy.connect),
        ChartData::Heat(h) if h.total == 0 => empty_line(frame, "no numeric points to plot"),
        ChartData::Heat(h) => render_heat(frame, h),
    }
}

/// `TITLE: MESSAGE (note)…` for a chart with nothing to draw — the dropped-row
/// counts still get reported ("strict and loud").
fn empty_line(frame: &Frame, message: &str) -> String {
    let notes: Vec<String> = frame.notes.iter().map(|n| format!("({n})")).collect();
    let mut s = format!("{}: {message}", frame.title);
    if !notes.is_empty() {
        s.push(' ');
        s.push_str(&notes.join(" "));
    }
    s.push('\n');
    s
}

/// The notes shown in the summary tail: the spacing note goes into the title
/// instead (see [`render_xy`]), so it is left out here.
fn tail_notes(frame: &Frame) -> String {
    frame
        .notes
        .iter()
        .filter(|n| n.as_str() != "even row spacing")
        .map(|n| format!("  ({n})"))
        .collect()
}

/// Paint `text` with the frame's ramp at `v` in `[lo, hi]` — the `-r/--ramp`
/// gradient. Terminal colour is opt-in, so this is a no-op unless both a ramp
/// and `frame.color` are on; empty text is left alone so no chart grows a pair
/// of escapes around nothing.
fn paint(frame: &Frame, v: f64, lo: f64, hi: f64, text: &str) -> String {
    paint_with(frame.ramp, frame.color, v, lo, hi, text)
}

/// [`paint`] with the ramp given outright, for the one caller that has a ramp
/// the frame does not: `-c/--color-by` colours with no `-r` on the command
/// line (see [`render_xy`]).
fn paint_with(ramp: Option<Ramp>, color: bool, v: f64, lo: f64, hi: f64, text: &str) -> String {
    match (ramp, color) {
        (Some(r), true) if !text.is_empty() => r.at(v, lo, hi).paint(text),
        _ => text.to_string(),
    }
}

/// Draw a histogram: a right-aligned bin-edge axis, a block bar per bin, and the
/// count, followed by a summary line. The bars fill what the axis leaves of
/// `frame.width`.
fn render_hist(frame: &Frame, h: &HistData) -> String {
    let nbins = h.counts.len();
    // Each edge is a step along the axis, not `lo` plus a multiple of a width:
    // an axis can be wider than an `f64` can subtract (see [`chart::lerp`]).
    let edge = |i: usize| crate::chart::lerp(h.lo, h.hi, i, nbins);
    let step = if nbins > 0 { edge(1) - h.lo } else { 0.0 };

    // Left axis: each bin's lower edge, rounded to the bin step's precision
    // (so a step of ~9 reads 16.7, not 16.688889), right-aligned.
    let edges: Vec<String> = (0..nbins).map(|i| fmt_to_step(edge(i), step)).collect();
    let axis_w = edges.iter().map(String::len).max().unwrap_or(1);

    let max_count = h.counts.iter().copied().max().unwrap_or(0);
    let bars = frame.width.saturating_sub(axis_w + 12).max(10);
    let axis_v = frame.glyphs.axis_v;

    let mut out = String::new();
    out.push_str(&frame.title);
    out.push('\n');
    if let Some(y) = &frame.ylabel {
        out.push_str(y);
        out.push('\n');
    }
    for (edge, &count) in edges.iter().zip(&h.counts) {
        let bar = bar_len(
            hist_len(count, frame.log),
            hist_len(max_count, frame.log),
            bars,
            &frame.glyphs,
        );
        // The ramp runs over the raw counts — the numbers printed beside the
        // bars — whatever the axis does with their lengths.
        let bar = paint(frame, count as f64, 0.0, max_count as f64, &bar);
        out.push_str(&format!("{edge:>axis_w$} {axis_v}{bar} {count}\n"));
    }
    if let Some(x) = &frame.xlabel {
        out.push_str(&format!("{:>axis_w$}  {x}\n", ""));
    }
    out.push_str(&format!(
        "n={}  min={}  max={}  bins={}",
        h.total,
        format_num(h.lo),
        format_num(h.hi),
        nbins
    ));
    out.push_str(&frame.notes_tail());
    out.push('\n');
    out
}

/// Draw one labelled horizontal bar per row, anchored at a zero baseline so
/// negative values extend left (a diverging bar chart). Labels are right-aligned
/// to a common width. Best used after group-by, where there are few rows.
///
/// With several value columns the row becomes a *group*: one line per series,
/// the label on the first of them and blank on the rest, followed by a legend.
/// Grouped bars take the series palette; a colour has to mean either "which
/// series" or "how big", so the parser rejects a `-r/--ramp` there. A single
/// series ramps by value.
fn render_bars(frame: &Frame, b: &BarData) -> String {
    // A row carries one value per series; `None` is a cell that was not a
    // number, which prints its label and nothing else.
    let nseries = b.value_names.len().max(1);
    let multi = nseries > 1;
    let label_w = b
        .rows
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0);
    // The baseline is always 0 — a real 0 on a linear axis, a value of 1 on a
    // log one — so a column of larger values bars from the left. An explicit
    // axis (`-y`) replaces the data's own range: a bar past it draws to the
    // edge, and the baseline moves onto the axis. The model works the bounds
    // out, so the SVG draws the same bars.
    let (lo, hi) = b.axis_bounds(frame.log);
    let span = hi - lo;
    let w = frame.width.saturating_sub(label_w + 14).max(10);
    let zero = pos_in(0.0f64.clamp(lo, hi), lo, span, w);
    let axis_v = frame.glyphs.axis_v;

    let mut out = String::new();
    out.push_str(&frame.title);
    out.push('\n');
    if let Some(y) = &frame.ylabel {
        out.push_str(y);
        out.push('\n');
    }
    for row in &b.rows {
        for i in 0..nseries {
            // Each series keeps its real value (which is what prints) and the
            // value the bar is drawn at: the same number, or its log10 on a log
            // axis, where a value that is not positive has no bar at all.
            let v = row.1.get(i).copied().flatten();
            let at = v.and_then(|v| bar_value(v, frame.log));
            // The drawn length is clamped to the axis; the printed value is real.
            let mut field = vec![' '; w];
            if let Some(at) = at {
                let p = pos_in(at.clamp(lo, hi), lo, span, w);
                let (from, to) = (zero.min(p), zero.max(p));
                for cell in field.iter_mut().take(to).skip(from) {
                    *cell = frame.glyphs.full;
                }
            }
            let drawn: String = field.into_iter().collect();
            // One series: the ramp runs over the same bounds the bars are drawn
            // against, so a colour is a position on the axis. Several: the
            // series palette says which column a bar belongs to. A row a log
            // axis cannot place has no bar to paint either way.
            let drawn = match at {
                Some(_) if multi && frame.color => series_style(i).paint(&drawn),
                Some(at) => paint(frame, at, lo, hi, &drawn),
                None => drawn,
            };
            // The label heads its group; the rows below it line up under a gap.
            let label = if i == 0 { row.0.as_str() } else { "" };
            let value = v.map(format_num).unwrap_or_default();
            out.push_str(&format!("{label:>label_w$} {axis_v}{drawn} {value}\n"));
        }
    }
    if let Some(x) = &frame.xlabel {
        out.push_str(&format!("{:>label_w$}  {x}\n", ""));
    }
    // The count is of labels — the bars of a group belong to one row.
    out.push_str(&format!("bars={}", b.rows.len()));
    out.push_str(&frame.notes_tail());
    out.push('\n');
    if multi {
        out.push_str(&legend_line(&b.value_names, &frame.glyphs, frame.color));
    }
    out
}

/// The `● name` legend line of a multi-series chart: one entry per series,
/// painted in that series' colour. Colour is what tells the series apart, so
/// with it off the markers go too and only the names are listed.
fn legend_line(names: &[String], glyphs: &Glyphs, color: bool) -> String {
    let entries: Vec<String> = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            if color {
                series_style(i).paint(&format!("{} {n}", glyphs.legend))
            } else {
                n.clone()
            }
        })
        .collect();
    format!("{}\n", entries.join("  "))
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

/// Draw a one-line sparkline (the values are already bucketed to the chart
/// width by the collector), with a title and a min/max summary. Each cell is an
/// eighth-height block scaled to the value range.
fn render_spark(frame: &Frame, s: &SparkData) -> String {
    let (dlo, dhi) = minmax(s.values.iter().copied()).unwrap_or((0.0, 0.0));
    // An explicit range (`-y`) is the axis the levels scale to; the summary
    // still reports the data's own min and max. The model works the axis out,
    // so the SVG draws the same line.
    let (lo, hi) = s.bounds();
    // The values are real; a log axis maps them (and its bounds) as they are
    // drawn, so a level is a position on the axis, not a value.
    let pos = |v: f64| value_pos(v, frame.log);
    let (plo, phi) = (pos(lo), pos(hi));
    let span = phi - plo;
    let levels = frame.glyphs.levels;
    let line: String = s
        .values
        .iter()
        .map(|&v| {
            // A flat series sits mid-height; otherwise scale into the 8 levels.
            let level = if span > 0.0 {
                (((pos(v) - plo) / span) * 7.0).round() as usize
            } else {
                3
            };
            let cell = levels[level.min(7)];
            // Each cell is painted on its own, over the same axis positions the
            // levels scale to, so colour and height tell the same story.
            paint(frame, pos(v), plo, phi, &cell.to_string())
        })
        .collect();
    let mut out = String::new();
    out.push_str(&frame.title);
    out.push('\n');
    if let Some(y) = &frame.ylabel {
        out.push_str(y);
        out.push('\n');
    }
    out.push_str(&line);
    out.push('\n');
    if let Some(x) = &frame.xlabel {
        out.push_str(&format!("  {x}\n"));
    }
    out.push_str(&format!("min={}  max={}", format_num(dlo), format_num(dhi)));
    out.push_str(&frame.notes_tail());
    out.push('\n');
    out
}

/// A horizontal block bar `v/max` of the full width, drawn with `g`'s glyphs,
/// with a fractional tail (when `g` has one) so short bars stay
/// distinguishable. Lengths are `f64` so a log axis can pass its logs.
fn bar_len(v: f64, max: f64, width: usize, g: &Glyphs) -> String {
    if max <= 0.0 || v <= 0.0 || width == 0 {
        return String::new();
    }
    let frac = v / max * width as f64;
    let full = frac.floor() as usize;
    let mut s: String = g.full.to_string().repeat(full.min(width));
    let rem = frac - full as f64;
    if rem > 0.0
        && full < width
        && let Some(partial) = g.partial
    {
        let idx = ((rem * 8.0).round() as usize).clamp(1, 8) - 1;
        s.push(partial[idx]);
    }
    s
}

// --- braille canvas (scatter / line) ---------------------------------------

/// Dot bit per (row-in-cell, col-in-cell). A braille cell is 2×4 dots; the glyph
/// is `U+2800 + bits`. Layout: dots 1-3 then 7 down the left column, 4-6 then 8
/// down the right (the Unicode ordering).
const BRAILLE_DOTS: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

/// Distinct foreground colours for multi-series charts (cycled). The single
/// source of truth for both the terminal and SVG renderers — see [`series_rgb`].
const SERIES_RGB: [Rgb; 6] = [
    Rgb(0x4f, 0xc3, 0xf7), // cyan
    Rgb(0xff, 0x8a, 0x65), // orange
    Rgb(0x81, 0xc7, 0x84), // green
    Rgb(0xba, 0x68, 0xc8), // purple
    Rgb(0xff, 0xd5, 0x4f), // yellow
    Rgb(0xe5, 0x73, 0x73), // red
];

/// The series colour for index `i` (cycled), shared with the SVG renderer.
pub(crate) fn series_rgb(i: usize) -> Rgb {
    SERIES_RGB[i % SERIES_RGB.len()]
}

fn series_style(i: usize) -> Style {
    Style {
        fg: Some(series_rgb(i)),
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

/// How to label the x-axis of a scatter/line chart.
#[derive(Clone, Debug)]
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
fn nice_ticks(lo: f64, hi: f64, target: usize) -> Vec<(f64, String)> {
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

/// Labelled axis ticks for `xaxis` over `[xlo, xhi]`, aiming for `target` ticks:
/// round 1/2/5×10ⁿ numbers (numeric), interpolated timestamps with the date
/// dropped when every tick is the same day (time), or the two end cells
/// (category). Shared by the terminal and SVG renderers so they can't drift.
pub(crate) fn axis_ticks(xaxis: &XAxis, xlo: f64, xhi: f64, target: usize) -> Vec<(f64, String)> {
    match xaxis {
        XAxis::Ends(lo, hi) => vec![(0.0, lo.clone()), (1.0, hi.clone())],
        XAxis::Numeric => nice_ticks(xlo, xhi, target),
        XAxis::Time => {
            let fmt: fn(f64) -> String = if crate::datetime::same_day(xlo, xhi) {
                crate::datetime::format_time // HH:MM:SS
            } else {
                crate::datetime::format_epoch // yyyy-mm-dd HH:MM:SS
            };
            ticks(xlo, xhi, target, fmt)
        }
    }
}

/// A representative axis label width in chars, for choosing how many ticks fit.
pub(crate) fn axis_label_width(xaxis: &XAxis, xlo: f64, xhi: f64) -> usize {
    match xaxis {
        XAxis::Ends(lo, hi) => lo.chars().count().max(hi.chars().count()),
        XAxis::Numeric => format_num(xlo).len().max(format_num(xhi).len()),
        XAxis::Time if crate::datetime::same_day(xlo, xhi) => 8,
        XAxis::Time => 19,
    }
}

/// Build the x-axis label row (without the gutter prefix) for `xaxis` over the
/// data range `[xlo, xhi]` and a `width`-wide canvas.
fn x_label_row(xaxis: &XAxis, xlo: f64, xhi: f64, width: usize) -> String {
    let target = tick_count(width, axis_label_width(xaxis, xlo, xhi));
    let labels = axis_ticks(xaxis, xlo, xhi, target);
    // Numeric (nice) ticks sit at their true positions; time/category ends pin.
    let force_ends = !matches!(xaxis, XAxis::Numeric);
    place_ticks(width, &labels, force_ends)
}

/// Draw a scatter (`connect=false`) or line (`connect=true`) chart of one or
/// more y-series against a shared x, on a braille canvas with a labelled frame.
/// Multiple series get distinct colours (when `frame.color`); on a shared cell
/// the first series wins the glyph (overlap is approximate, as in other terminal
/// plotters). `xy.xaxis` selects how the bottom axis is graduated (numeric/time
/// interpolate intermediate ticks; a category axis shows only the first/last
/// cells) — a category axis says so in the title, since it distorts spacing.
///
/// A *single* series is painted per braille cell when colour is on: by the
/// `-c/--color-by` value of the last point in the cell when the script named
/// such a column, else — with `-r/--ramp` — by how many data points landed in
/// it (density). `-c` implies the default ramp, so it colours with no `-r`
/// given, and a cell with no point of its own stays plain: density counts data
/// points, never the pixels a `line` chart's connecting segments light up.
/// Density counts points per *cell*, so only a terminal chart has it;
/// `--color-by` colours an SVG's points too (see [`crate::svg::xy_chart`]).
fn render_xy(frame: &Frame, xy: &XyData, connect: bool) -> String {
    let series = xy.series();
    let total: usize = series.iter().map(Vec::len).sum();
    // The model frames the chart: the points' own extent, or an explicit
    // `-x`/`-y` range where one was given (the points outside it are already
    // clipped away). The SVG asks it the same question.
    let (xlo, xhi, ylo, yhi) = xy.bounds();
    // The ys are real; a log axis maps them (and its bounds) onto the canvas
    // here, at draw time.
    let pos = |v: f64| value_pos(v, frame.log);
    let (pylo, pyhi) = (pos(ylo), pos(yhi));
    let (xspan, yspan) = (xhi - xlo, pyhi - pylo);

    // Left gutter holds the y-axis labels (top = yhi, bottom = ylo) — the real
    // values, whatever the axis does with them.
    let yhi_s = format_num(yhi);
    let ylo_s = format_num(ylo);
    let gutter = gutter_width(ylo, yhi);
    let wcells = canvas_cells(frame.width, ylo, yhi);
    let hcells = frame.height.max(2);

    let map = |x: f64, y: f64, b: &Braille| {
        let px = if xspan > 0.0 {
            ((x - xlo) / xspan * (b.pw() - 1) as f64).round() as isize
        } else {
            (b.pw() / 2) as isize
        };
        // y is flipped: the top row is the high value.
        let py = if yspan > 0.0 {
            ((b.ph() - 1) as f64 - (pos(y) - pylo) / yspan * (b.ph() - 1) as f64).round() as isize
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
    let colors = frame.color;
    // Whether this is a colour-by chart is the *plan's* answer, not the data's:
    // a colour column with no numbers in it is still one, drawn plain, so it
    // never quietly turns into a density chart the SVG would disagree with.
    // `--color-by` takes a single y series, so it never meets the palette.
    let has_by = !multi && xy.color_by.is_some();
    // `-c/--color-by` implies the default ramp, so it colours without `-r`.
    let ramp = frame.ramp.or_else(|| has_by.then(Ramp::default));
    // One colour value per braille cell, with the ends of the ramp it runs on:
    // the last `-c/--color-by` value seen in the cell, else how many data
    // points landed in it (density). A cell with neither has no colour and
    // stays plain. Only a single series is painted this way, and only with a
    // ramp and terminal colour to paint it, so the grid is built only when it
    // will show — it is one allocation the size of the canvas.
    let cell: Option<(Vec<Option<f64>>, f64, f64)> =
        (colors && !multi && ramp.is_some()).then(|| {
            let mut vals: Vec<Option<f64>> = vec![None; wcells * hcells];
            // The busiest cell is the density ramp's high end; one point is its
            // low end, so an empty grid still spans 1..=1.
            let mut maxcount = 1.0f64;
            for r in &xy.rows {
                let Some(y) = r.ys.first().copied().flatten() else {
                    continue;
                };
                let (px, py) = map(r.x, y, &canvases[0]);
                if px < 0 || py < 0 {
                    continue;
                }
                let (cx, cy) = (px as usize / 2, py as usize / 4);
                if cx >= wcells || cy >= hcells {
                    continue;
                }
                let idx = cy * wcells + cx;
                if has_by {
                    // A cell keeps the last value it was given; a point whose
                    // colour cell was not numeric leaves that alone rather than
                    // blanking it.
                    if r.color_by.is_some() {
                        vals[idx] = r.color_by;
                    }
                } else {
                    let n = vals[idx].unwrap_or(0.0) + 1.0;
                    vals[idx] = Some(n);
                    maxcount = maxcount.max(n);
                }
            }
            // A colour-by column wins over the density: the ramp then spans the
            // column's own range, over every plotted point.
            match has_by {
                true => {
                    let (lo, hi) = xy.color_bounds().unwrap_or((0.0, 0.0));
                    (vals, lo, hi)
                }
                false => (vals, 1.0, maxcount),
            }
        });
    let mut out = String::new();
    // An evenly-spaced (category) x axis is flagged in the title, not the tail.
    out.push_str(&frame.title);
    if frame.notes.iter().any(|n| n == "even row spacing") {
        out.push_str("  (even row spacing)");
    }
    out.push('\n');
    if let Some(y) = &frame.ylabel {
        out.push_str(y);
        out.push('\n');
    }
    for cy in 0..hcells {
        // Gutter label: yhi on the first row, ylo on the last.
        let label = if cy == 0 {
            &yhi_s
        } else if cy == hcells - 1 {
            &ylo_s
        } else {
            ""
        };
        out.push_str(&format!("{label:>gutter$} {}", frame.glyphs.axis_tick));
        for cx in 0..wcells {
            let hit = canvases
                .iter()
                .enumerate()
                .find(|(_, c)| c.bits[cy * wcells + cx] != 0);
            match hit {
                None => out.push(' '),
                Some((si, c)) => {
                    let idx = cy * wcells + cx;
                    let ch = (frame.glyphs.braille)(c.bits[idx]).to_string();
                    if colors && multi {
                        out.push_str(&series_style(si).paint(&ch));
                    } else if let Some((vals, lo, hi)) = &cell {
                        // Only a cell holding a data point has a colour value —
                        // a colour-by one or a density count. A cell lit by a
                        // connecting segment alone has neither, so it is not
                        // the ramp's low end: it keeps the plain glyph.
                        match vals[idx] {
                            Some(v) => out.push_str(&paint_with(ramp, colors, v, *lo, *hi, &ch)),
                            None => out.push_str(&ch),
                        }
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
    out.push_str(&format!(
        "{:>gutter$} {}{}\n",
        "",
        frame.glyphs.axis_corner,
        frame.glyphs.axis_h.to_string().repeat(wcells)
    ));
    out.push_str(&format!(
        "{:>gutter$}  {}\n",
        "",
        x_label_row(&xy.xaxis, xlo, xhi, wcells)
    ));
    if let Some(x) = &frame.xlabel {
        let line = format!("{:>gutter$}  {:^wcells$}", "", x);
        out.push_str(line.trim_end());
        out.push('\n');
    }

    out.push_str(&format!("points={total}"));
    out.push_str(&tail_notes(frame));
    out.push('\n');
    if multi {
        out.push_str(&legend_line(&xy.names, &frame.glyphs, colors));
    }
    out
}

/// The shade level of a cell holding `count` points, against the busiest cell's
/// `max`: 0 (blank) for an empty cell, then 1..=4 over the four visible shades.
/// `log` scales by the count's log, the same axis `hist_len` puts a histogram's
/// bars on, so a few crowded cells don't flatten every other one to one shade.
/// A grid whose busiest cell holds a single point has no spread to show, so
/// every cell it does hold gets the lightest visible shade — the low end of the
/// count axis, which is what the ramp paints such a grid ([`Ramp::at`] with
/// hi == lo), so the chart reads the same with colour and without.
fn shade_level(count: u64, max: u64, log: bool) -> usize {
    if count == 0 {
        return 0;
    }
    let (lo, hi) = (hist_len(1, log), hist_len(max, log));
    if hi <= lo {
        return 1;
    }
    (1 + ((hist_len(count, log) - lo) / (hi - lo) * 3.0).round() as usize).min(4)
}

/// Draw a heatmap: the point counts of a `cols`×`rows` grid, in the same frame
/// as [`render_xy`] — a y-bound gutter, the axis rule, and graduated x labels
/// from `h.xaxis`. The grid *is* the canvas, so the chart is as big as the
/// collector made it (`-b` gives an exact cell count).
///
/// A cell is shaded by how many points landed in it: with terminal colour on,
/// the full block painted along the ramp (the frame's `-r/--ramp`, else the
/// default one — a heatmap has no other use for a colour); with colour off, one
/// of the five block shades, blank for a cell holding nothing. `-l/--log` is
/// the *count* axis here, not the y axis: both dimensions are binned, so the
/// log belongs to the density.
fn render_heat(frame: &Frame, h: &HeatData) -> String {
    let yhi_s = format_num(h.yhi);
    let ylo_s = format_num(h.ylo);
    let gutter = gutter_width(h.ylo, h.yhi);
    let max = h.counts.iter().copied().max().unwrap_or(0);
    // The ramp spans one point to the busiest cell, on the count axis — the
    // model's derivation, so the SVG shades the same cell the same way.
    let ramp = frame.ramp.unwrap_or_default();
    let (clo, chi) = h.count_bounds(frame.log);

    let mut out = String::new();
    // An evenly-spaced (category) x axis is flagged in the title, as in an xy
    // chart, since it distorts spacing the same way.
    out.push_str(&frame.title);
    if frame.notes.iter().any(|n| n == "even row spacing") {
        out.push_str("  (even row spacing)");
    }
    out.push('\n');
    if let Some(y) = &frame.ylabel {
        out.push_str(y);
        out.push('\n');
    }
    // Row 0 of the grid is the lowest y band, so the rows print in reverse.
    for row in (0..h.rows).rev() {
        let label = if row == h.rows - 1 {
            &yhi_s
        } else if row == 0 {
            &ylo_s
        } else {
            ""
        };
        out.push_str(&format!("{label:>gutter$} {}", frame.glyphs.axis_tick));
        for col in 0..h.cols {
            let count = h.counts[row * h.cols + col];
            if frame.color && count > 0 {
                let cell = frame.glyphs.full.to_string();
                let at = hist_len(count, frame.log);
                out.push_str(&paint_with(Some(ramp), true, at, clo, chi, &cell));
            } else {
                out.push(frame.glyphs.shades[shade_level(count, max, frame.log)]);
            }
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "{:>gutter$} {}{}\n",
        "",
        frame.glyphs.axis_corner,
        frame.glyphs.axis_h.to_string().repeat(h.cols)
    ));
    out.push_str(&format!(
        "{:>gutter$}  {}\n",
        "",
        x_label_row(&h.xaxis, h.xlo, h.xhi, h.cols)
    ));
    if let Some(x) = &frame.xlabel {
        let line = format!("{:>gutter$}  {:^wcells$}", "", x, wcells = h.cols);
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.push_str(&format!("points={}", h.total));
    out.push_str(&tail_notes(frame));
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::XyRow;

    /// A single-series `BarData` from `(label, value)` pairs, as `collect`
    /// builds it for `graph bar`.
    fn bar_data(rows: &[(String, f64)]) -> BarData {
        BarData {
            label_name: "label".to_string(),
            value_names: vec!["v".to_string()],
            rows: rows
                .iter()
                .map(|(l, v)| (l.clone(), vec![Some(*v)]))
                .collect(),
            axis: None,
        }
    }

    /// An `XyData` whose rows reproduce `series`: every series shares the x
    /// column, so one row per distinct x with a blank where a series has no
    /// point there.
    fn xy_data(
        names: &[String],
        series: &[Vec<(f64, f64)>],
        xaxis: XAxis,
        connect: bool,
    ) -> XyData {
        let mut xs: Vec<f64> = Vec::new();
        for pts in series {
            for &(x, _) in pts {
                if !xs.contains(&x) {
                    xs.push(x);
                }
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let rows = xs
            .iter()
            .map(|&x| XyRow {
                xcell: format_num(x),
                x,
                ys: series
                    .iter()
                    .map(|pts| pts.iter().find(|(px, _)| *px == x).map(|&(_, y)| y))
                    .collect(),
                color_by: None,
            })
            .collect();
        XyData {
            xname: "x".to_string(),
            names: names.to_vec(),
            rows,
            xaxis,
            connect,
            xrange: None,
            yrange: None,
            color_by: None,
        }
    }

    #[test]
    fn bins_span_min_to_max_inclusive() {
        // 0..=9 into 3 bins: edges 0,3,6; the max (9) lands in the last bin.
        let h = HistData::build(&[0.0, 1.0, 3.0, 4.0, 9.0], Some(3), None).unwrap();
        assert_eq!(h.lo, 0.0);
        assert_eq!(h.hi, 9.0);
        assert_eq!(h.counts, vec![2, 2, 1]);
        assert_eq!(h.total, 5);
    }

    #[test]
    fn equal_values_collapse_to_one_populated_bin() {
        let h = HistData::build(&[5.0, 5.0, 5.0], Some(4), None).unwrap();
        assert_eq!(h.counts.iter().sum::<u64>(), 3);
        assert_eq!(h.counts[0], 3);
    }

    #[test]
    fn empty_values_render_nothing() {
        assert!(HistData::build(&[], Some(4), None).is_none());
    }

    #[test]
    fn hist_edges_stay_real_on_an_axis_too_wide_to_subtract() {
        // The span of -1e308..1e308 overflows an f64, so edges worked out
        // from `hi - lo` printed as NaN and inf down the left of the chart.
        let h = HistData::build(&[-1e308, 0.0, 1e308], Some(4), None).unwrap();
        let s = render_hist(&Frame::new("v".to_string(), 80, 15, false), &h);
        assert!(!s.contains("NaN") && !s.contains("inf"), "{s}");
    }

    #[test]
    fn render_reports_skipped_and_summary() {
        let h = HistData::build(&[1.0, 2.0, 3.0], Some(2), None).unwrap();
        let mut f = Frame::new("amount".to_string(), 40, 15, false);
        f.notes.push("skipped 2 non-numeric".to_string());
        let s = render_hist(&f, &h);
        assert!(s.starts_with("amount\n"));
        assert!(s.contains("n=3"));
        assert!(s.contains("min=1"));
        assert!(s.contains("max=3"));
        assert!(s.contains("(skipped 2 non-numeric)"));
    }

    #[test]
    fn bars_anchor_positive_at_left_edge() {
        let rows = [("a".to_string(), 2.0), ("b".to_string(), 4.0)];
        let s = render_bars(
            &Frame::new("v".to_string(), 30, 15, false),
            &bar_data(&rows),
        );
        assert!(s.starts_with("v\n"));
        // All-positive: the zero baseline is the left edge, so bars start there.
        let a_line = s.lines().nth(1).unwrap();
        assert!(a_line.contains("│█"), "{a_line}");
        assert!(s.contains("bars=2"));
    }

    #[test]
    fn bars_diverge_around_zero_for_negatives() {
        let rows = [("pos".to_string(), 5.0), ("neg".to_string(), -5.0)];
        let s = render_bars(
            &Frame::new("d".to_string(), 40, 15, false),
            &bar_data(&rows),
        );
        let pos = s.lines().find(|l| l.contains("pos")).unwrap();
        let neg = s.lines().find(|l| l.contains("neg")).unwrap();
        // The negative bar starts left of where the positive bar starts.
        let bar_start = |l: &str| l.find('█').unwrap();
        assert!(bar_start(neg) < bar_start(pos), "neg={neg} pos={pos}");
    }

    #[test]
    fn bars_clamp_to_an_explicit_axis_but_print_the_real_value() {
        let rows = [("a".to_string(), 1.0), ("b".to_string(), 9.0)];
        let mut b = bar_data(&rows);
        b.axis = Some((0.0, 2.0));
        let s = render_bars(&Frame::new("v".to_string(), 40, 15, false), &b);
        // The drawn field of a row: between the axis rule and the printed value.
        let field = |name: &str| {
            let line = s.lines().find(|l| l.contains(name)).unwrap();
            line.split_once('│')
                .unwrap()
                .1
                .rsplit_once(' ')
                .unwrap()
                .0
                .to_string()
        };
        // `b` is past the axis top, so its bar fills the field; `a` does not.
        assert!(field("b").chars().all(|c| c == '█'), "{s}");
        assert!(field("a").contains(' '), "{s}");
        // The clamp is only in the drawing: the printed value is still 9.
        assert!(s.lines().any(|l| l.ends_with(" 9")), "{s}");
    }

    #[test]
    fn bars_report_skipped_and_truncated() {
        let rows = [("a".to_string(), 1.0)];
        let mut f = Frame::new("v".to_string(), 30, 15, false);
        f.notes.push("+2 more not shown".to_string());
        f.notes.push("skipped 3 non-numeric".to_string());
        let s = render_bars(&f, &bar_data(&rows));
        assert!(s.contains("(+2 more not shown)"), "{s}");
        assert!(s.contains("(skipped 3 non-numeric)"), "{s}");
    }

    #[test]
    fn spark_is_one_line_scaled_to_width() {
        let s = render_spark(
            &Frame::new("v".to_string(), 4, 15, false),
            &SparkData {
                name: "v".to_string(),
                values: vec![1.0, 2.0, 3.0, 4.0],
                range: None,
            },
        );
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
        let s = render_spark(
            &Frame::new("v".to_string(), 10, 15, false),
            &SparkData {
                name: "v".to_string(),
                values: crate::chart::bucket(&vals, 10),
                range: None,
            },
        );
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
        assert_eq!((Glyphs::unicode().braille)(0x01), '⠁');
    }

    #[test]
    fn render_xy_frames_a_scatter() {
        let pts = vec![vec![(0.0, 0.0), (1.0, 1.0), (2.0, 2.0)]];
        let s = render_xy(
            &Frame::new("y vs x".to_string(), 10, 4, false),
            &xy_data(&["y".into()], &pts, XAxis::Numeric, false),
            false,
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
        let s = render_xy(
            &Frame::new("y vs t".to_string(), 40, 4, false),
            &xy_data(&["y".into()], &pts, ends, true),
            true,
        );
        assert!(s.contains("2024-01-01") && s.contains("2024-01-03"), "{s}");
    }

    /// A 2x2 heat grid over the unit square with the given row-major counts.
    fn heat_data(counts: Vec<u64>) -> HeatData {
        HeatData {
            xname: "x".to_string(),
            yname: "y".to_string(),
            xlo: 0.0,
            xhi: 1.0,
            ylo: 0.0,
            yhi: 1.0,
            cols: 2,
            rows: 2,
            total: counts.iter().sum(),
            counts,
            xaxis: XAxis::Numeric,
        }
    }

    #[test]
    fn render_heat_shades_cells_by_their_count() {
        // Counts 0, 1, 2 and 4 over a busiest cell of 4: an empty cell is
        // blank and the rest climb the five shades.
        let h = heat_data(vec![0, 1, 2, 4]);
        let s = render_heat(&Frame::new("y vs x".to_string(), 12, 2, false), &h);
        assert!(s.starts_with("y vs x\n"), "{s}");
        let rows: Vec<&str> = s.lines().filter(|l| l.contains('┤')).collect();
        assert_eq!(rows.len(), 2, "{s}");
        // The top row is the high y band, so it holds the second half of the
        // counts (2 and 4); the bottom row holds 0 and 1.
        assert!(rows[0].ends_with("▒█"), "{s}");
        assert!(rows[1].ends_with(" ░"), "{s}");
        assert!(s.contains('└') && s.contains("points=7"), "{s}");
    }

    #[test]
    fn render_heat_shades_a_uniform_grid_at_the_low_end() {
        // Every non-empty cell holds one point, so there is no spread to show.
        // The ramp gives such a grid its low end (`Ramp::at` with hi == lo), so
        // the shades must give it the lightest non-empty one — the same chart
        // with and without colour.
        let h = heat_data(vec![0, 1, 1, 1]);
        let s = render_heat(&Frame::new("y vs x".to_string(), 12, 2, false), &h);
        let rows: Vec<&str> = s.lines().filter(|l| l.contains('┤')).collect();
        assert!(rows[0].ends_with("░░"), "{s}");
        assert!(rows[1].ends_with(" ░"), "{s}");
        let mut f = Frame::new("y vs x".to_string(), 12, 2, true);
        f.ramp = Some(crate::color::parse_ramp("blue:red").unwrap());
        let colored = render_heat(&f, &heat_data(vec![0, 1, 1, 1]));
        assert_eq!(
            colored.matches("\x1b[38;2;0;0;238m").count(),
            3,
            "{colored}"
        );
    }

    #[test]
    fn render_heat_paints_the_ramp_when_colour_is_on() {
        let mut f = Frame::new("y vs x".to_string(), 12, 2, true);
        f.ramp = Some(crate::color::parse_ramp("blue:red").unwrap());
        let s = render_heat(&f, &heat_data(vec![0, 1, 2, 4]));
        // One painted glyph per non-empty cell; the lightest is the ramp's low
        // end and the busiest its high end.
        assert_eq!(s.matches("\x1b[38;2;").count(), 3, "{s}");
        assert!(
            s.contains("\x1b[38;2;0;0;238m") && s.contains("\x1b[38;2;205;0;0m"),
            "{s}"
        );
        // `-c/--color-by` is not a heatmap flag, so the ramp is the frame's or
        // the default one — colour alone is enough to paint.
        f.ramp = None;
        assert!(render_heat(&f, &heat_data(vec![0, 1, 2, 4])).contains("\x1b[38;2;"));
    }

    #[test]
    fn fmt_to_step_rounds_to_the_step_precision() {
        assert_eq!(fmt_to_step(16.688889, 9.19), "16.7"); // ~9 step ⇒ 1 decimal
        assert_eq!(fmt_to_step(35.0, 9.19), "35");
        assert_eq!(fmt_to_step(123.4, 100.0), "123"); // big step ⇒ integer
        assert_eq!(fmt_to_step(0.123456, 0.05), "0.123"); // small step ⇒ 3 decimals
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
            &Frame::new("y vs x".to_string(), 80, 6, false),
            &xy_data(&["y".into()], &pts, XAxis::Numeric, false),
            false,
        );
        // More than the two ends (0 and 100) — an intermediate tick near 50.
        assert!(s.contains("50"), "{s}");
    }

    #[test]
    fn render_xy_empty_is_loud() {
        let mut f = Frame::new("y vs x".to_string(), 80, 15, false);
        f.notes.push("skipped 5 non-numeric".to_string());
        let s = render(
            &f,
            &ChartData::Xy(xy_data(&["y".into()], &[vec![]], XAxis::Numeric, false)),
        );
        assert!(s.contains("no numeric points to plot"));
        assert!(s.contains("skipped 5"));
    }

    #[test]
    fn render_xy_paints_one_series_by_density_then_by_colour_by() {
        let blue = "\x1b[38;2;0;0;238m";
        let red = "\x1b[38;2;205;0;0m";
        let mut f = Frame::new("y vs x".to_string(), 12, 4, true);
        f.ramp = Some(crate::color::parse_ramp("blue:red").unwrap());
        let row = |x: f64, y: f64, c: Option<f64>| XyRow {
            xcell: format_num(x),
            x,
            ys: vec![Some(y)],
            color_by: c,
        };
        // Two points share the bottom-left cell; a third sits alone top-right.
        let data = |c: [Option<f64>; 3], by: Option<&str>| XyData {
            xname: "x".to_string(),
            names: vec!["y".to_string()],
            rows: vec![
                row(0.0, 0.0, c[0]),
                row(0.0, 0.0, c[1]),
                row(1.0, 1.0, c[2]),
            ],
            xaxis: XAxis::Numeric,
            connect: false,
            xrange: None,
            yrange: None,
            color_by: by.map(str::to_string),
        };
        let plain = [None, None, None];
        // Density: the busy cell is the ramp's high end, the lone one its low
        // end. The top-right cell is drawn first, so blue comes before red.
        let dense = render_xy(&f, &data(plain, None), false);
        assert!(dense.contains(blue) && dense.contains(red), "{dense}");
        assert!(dense.find(blue) < dense.find(red), "{dense}");
        // A colour-by value wins over the count: the busy cell now holds the
        // low value, so the two colours swap places.
        let vals = [Some(0.0), Some(0.0), Some(5.0)];
        let by = render_xy(&f, &data(vals, Some("z")), false);
        assert!(by.contains(blue) && by.contains(red), "{by}");
        assert!(by.find(red) < by.find(blue), "{by}");
        // `-c` alone colours: with no ramp it falls back to the default one.
        f.ramp = None;
        let bare = render_xy(&f, &data(vals, Some("z")), false);
        assert!(bare.contains("\x1b[38;2;"), "{bare}");
        // A colour column the chart found no number in is still a colour-by
        // chart: nothing is painted, and it never falls back to density.
        assert!(!render_xy(&f, &data(plain, Some("z")), false).contains('\x1b'));
        // No ramp and no colour-by: no paint.
        assert!(!render_xy(&f, &data(plain, None), false).contains('\x1b'));
    }

    #[test]
    fn render_xy_density_leaves_the_line_between_points_plain() {
        // A `line` chart's connecting segments light cells that hold no data
        // point; density counts points, so those cells stay unpainted.
        let mut f = Frame::new("y vs x".to_string(), 24, 4, true);
        f.ramp = Some(crate::color::parse_ramp("blue:red").unwrap());
        let pts = vec![vec![(0.0, 0.0), (9.0, 9.0)]];
        let s = render_xy(
            &f,
            &xy_data(&["y".into()], &pts, XAxis::Numeric, true),
            true,
        );
        // Two points, so two painted cells — the rest of the diagonal is plain.
        assert_eq!(s.matches("\x1b[38;2;").count(), 2, "{s}");
    }

    #[test]
    fn render_xy_multi_series_adds_a_legend_when_coloured() {
        let series = vec![vec![(0.0, 0.0)], vec![(0.0, 1.0)]];
        let names = ["a".to_string(), "b".to_string()];
        let s = render_xy(
            &Frame::new("t".to_string(), 8, 4, true),
            &xy_data(&names, &series, XAxis::Numeric, false),
            false,
        );
        assert!(s.contains('\x1b')); // coloured glyphs
        assert!(s.contains('●')); // legend markers
    }

    #[test]
    fn render_bars_groups_the_series_of_a_row() {
        let b = BarData {
            label_name: "k".to_string(),
            value_names: vec!["n".to_string(), "m".to_string()],
            rows: vec![
                ("a".to_string(), vec![Some(1.0), Some(2.0)]),
                ("b".to_string(), vec![Some(4.0), None]),
            ],
            axis: None,
        };
        let s = render_bars(&Frame::new("t".to_string(), 40, 15, true), &b);
        let rows: Vec<&str> = s.lines().filter(|l| l.contains('│')).collect();
        assert_eq!(rows.len(), 4, "{s}");
        // The label prints on the first series' row and is blank on the rest.
        assert!(
            rows[0].starts_with("a │") && rows[1].starts_with("  │"),
            "{s}"
        );
        assert!(
            rows[2].starts_with("b │") && rows[3].starts_with("  │"),
            "{s}"
        );
        // A row with no value draws no bar and prints no number.
        assert!(
            !rows[3].contains('█') && rows[3].trim_end().ends_with('│'),
            "{s}"
        );
        // Several series take the palette, and the summary counts labels.
        assert!(s.contains("\x1b[38;2;"), "{s}");
        assert!(s.contains("bars=2"), "{s}");
        assert!(s.contains("● n") && s.contains("● m"), "{s}");
    }

    #[test]
    fn spark_levels_follow_the_log_axis_over_real_values() {
        // 1, 10, 100: linearly the first two share the bottom level; on a log
        // axis they are evenly spread over the eight levels.
        let s = SparkData {
            name: "v".to_string(),
            values: vec![1.0, 10.0, 100.0],
            range: None,
        };
        let mut frame = Frame::new("v".to_string(), 8, 4, false);
        let line = |f: &Frame| render_spark(f, &s).lines().nth(1).unwrap().to_string();
        let lin = line(&frame);
        frame.log = true;
        let log = line(&frame);
        // Both axes run 1..100, so the ends match; only the middle moves.
        assert_eq!(
            (lin.chars().next(), lin.chars().last()),
            (Some('▁'), Some('█'))
        );
        assert_eq!(
            (log.chars().next(), log.chars().last()),
            (Some('▁'), Some('█'))
        );
        assert_eq!(lin.chars().nth(1), Some('▂')); // 10 is a tenth of the way up
        assert_eq!(log.chars().nth(1), Some('▅')); // and halfway up in log space
        // The summary reports the real values on either axis.
        assert!(render_spark(&frame, &s).contains("min=1  max=100"));
    }

    #[test]
    fn bar_scales_and_caps_at_width() {
        let g = Glyphs::unicode();
        assert_eq!(bar_len(0.0, 10.0, 8, &g), "");
        assert_eq!(bar_len(10.0, 10.0, 8, &g), "████████"); // full
        assert!(bar_len(10.0, 10.0, 8, &g).chars().count() == 8);
        // A partial bar gets an eighth-block tail.
        let half = bar_len(1.0, 2.0, 4, &g);
        assert!(half.starts_with("██"));
    }
}
