//! The chart data model behind the `graph` sink: what a chart shows, apart
//! from how it is drawn. The collectors read the charted columns out of the
//! buffered output; `graph.rs` draws the result in the terminal and `svg.rs`
//! as SVG.

use crate::color::Ramp;
use crate::csv;
use crate::field::Field;
use crate::graph::XAxis;
use crate::plan::{GraphKind, GraphSpec};

/// The characters a terminal renderer draws with (Unicode by default).
#[derive(Clone, Copy, Debug)]
pub struct Glyphs {
    /// A full bar cell.
    pub full: char,
    /// Eighth-width bar tails, 1/8..8/8; `None` draws no partial cell.
    pub partial: Option<[char; 8]>,
    /// Eighth-height spark levels, lowest first.
    pub levels: [char; 8],
    /// The vertical axis rule.
    pub axis_v: char,
    /// The corner where the two axes meet.
    pub axis_corner: char,
    /// The horizontal axis rule.
    pub axis_h: char,
    /// The tick on the left edge of a canvas row.
    pub axis_tick: char,
    /// Heatmap shades, lightest (empty) first.
    pub shades: [char; 5],
    /// A braille cell from its dot bits (0 = empty).
    pub braille: fn(u8) -> char,
    /// The multi-series legend marker.
    pub legend: char,
}

/// The braille cell for `bits`: the glyphs start at U+2800 and the dot bits
/// index into that block.
fn braille_unicode(bits: u8) -> char {
    char::from_u32(0x2800 + bits as u32).unwrap_or(' ')
}

/// The ASCII braille stand-in: any lit dot draws `*`, an empty cell draws a
/// space.
fn braille_ascii(bits: u8) -> char {
    if bits == 0 { ' ' } else { '*' }
}

impl Glyphs {
    /// The Unicode block/braille glyph set — what every chart draws with today.
    pub fn unicode() -> Self {
        Glyphs {
            full: '█',
            partial: Some(['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█']),
            levels: ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'],
            axis_v: '│',
            axis_corner: '└',
            axis_h: '─',
            axis_tick: '┤',
            shades: [' ', '░', '▒', '▓', '█'],
            braille: braille_unicode,
            legend: '●',
        }
    }

    /// Plain ASCII, for terminals without block glyphs: `#` bars with no
    /// partial cells, `" .:-=+*#"` spark levels, `| + - |` axes, `" .:*#"`
    /// shades, `*` for any lit braille cell or the legend marker.
    pub fn ascii() -> Self {
        Glyphs {
            full: '#',
            partial: None,
            levels: [' ', '.', ':', '-', '=', '+', '*', '#'],
            axis_v: '|',
            axis_corner: '+',
            axis_h: '-',
            axis_tick: '|',
            shades: [' ', '.', ':', '*', '#'],
            braille: braille_ascii,
            legend: '*',
        }
    }
}

/// An explicit axis range, `Some((lo, hi))` from `-x`/`-y`, or `None` for the
/// data's own extent. A range *is* the axis: the chart spans it, and the values
/// outside it are dropped (and counted) rather than squeezed in.
pub type AxisRange = Option<(f64, f64)>;

/// Base chart dimensions at scale 1: width in cells, canvas rows.
const BASE_W: usize = 80;
const BASE_H: usize = 15;

/// The most cells a chart dimension may ask for (`-b`, `-W`, `-H`, and what
/// `-s` may scale a default up to). A chart is drawn into a buffer of
/// `cols * rows` cells, so an unbounded value is an allocation no terminal
/// could show — and a big enough one wraps that product round to some smaller
/// count, leaving a buffer the drawing then indexes past the end of. 4096 is
/// far past any terminal and keeps the product well inside a `usize`.
pub const MAX_CELLS: usize = 4096;

/// The chart size in cells: an explicit `width`/`height` wins; else the
/// terminal width (or 80) and 15 rows, both times `scale`. Floors keep a
/// tiny chart drawable, and the same [`MAX_CELLS`] ceiling the flags carry
/// caps the result, so a large `-s` cannot ask for the allocation `-W` may not.
pub fn chart_size(
    scale: f64,
    term_width: Option<usize>,
    width: Option<usize>,
    height: Option<usize>,
) -> (usize, usize) {
    let scaled = |base: usize| (base as f64 * scale).round() as usize;
    let w = width.unwrap_or_else(|| scaled(term_width.unwrap_or(BASE_W)));
    let h = height.unwrap_or_else(|| scaled(BASE_H));
    (w.clamp(16, MAX_CELLS), h.clamp(2, MAX_CELLS))
}

/// Everything about a chart that is not its data.
#[derive(Clone, Debug)]
pub struct Frame {
    pub title: String,
    pub xlabel: Option<String>,
    pub ylabel: Option<String>,
    /// Total chart width in cells.
    pub width: usize,
    /// Canvas rows (scatter, line, heatmap).
    pub height: usize,
    pub glyphs: Glyphs,
    pub ramp: Option<Ramp>,
    /// ANSI colour is on.
    pub color: bool,
    /// The value axis is on a log10 scale.
    pub log: bool,
    /// Dropped-data notes, e.g. "skipped 3 non-numeric".
    pub notes: Vec<String>,
}

impl Frame {
    /// A frame with the given title and size, drawn with the Unicode glyphs and
    /// no labels, ramp, log scale or notes.
    pub fn new(title: String, width: usize, height: usize, color: bool) -> Self {
        Frame {
            title,
            xlabel: None,
            ylabel: None,
            width,
            height,
            glyphs: Glyphs::unicode(),
            ramp: None,
            color,
            log: false,
            notes: Vec::new(),
        }
    }

    /// The notes as a summary tail: `  (a)  (b)`, empty when there are none.
    pub fn notes_tail(&self) -> String {
        self.notes.iter().map(|n| format!("  ({n})")).collect()
    }

    /// The notes joined with two spaces (the SVG footer).
    pub fn notes_line(&self) -> String {
        self.notes.join("  ")
    }
}

/// The `i`th of `n` steps from `lo` to `hi` — a bin edge, or a cell's corner.
///
/// Written as a weighted sum of the two ends rather than `lo + (hi - lo) * i /
/// n`, because an axis can be wider than an `f64` can subtract: `1e308 -
/// -1e308` is infinity, and every edge worked out from it a NaN or an
/// infinity. Splitting the weight keeps each term inside the range.
///
/// Only the edges *between* the ends are worked out that way: the weighted sum
/// divides and multiplies, so it lands near an end rather than on it (an epoch
/// in milliseconds came back as `1700000000000.000244`). The ends are handed
/// back as they came in.
pub fn lerp(lo: f64, hi: f64, i: usize, n: usize) -> f64 {
    if i == 0 {
        return lo;
    }
    if i == n {
        return hi;
    }
    let (i, n) = (i as f64, n as f64);
    lo / n * (n - i) + hi / n * i
}

/// How far `v` sits along `[lo, hi]`, as a fraction — what a value's bin is
/// worked out from. `None` when the span is empty (every value is the same),
/// which puts them all in the first bin.
///
/// Halving both ends before subtracting is the same trick [`lerp`] plays: the
/// span of an axis running `-1e308` to `1e308` overflows an `f64`, and the
/// halves do not.
fn axis_frac(v: f64, lo: f64, hi: f64) -> Option<f64> {
    let span = hi * 0.5 - lo * 0.5;
    (span > 0.0).then(|| (v * 0.5 - lo * 0.5) / span)
}

/// A binned distribution: `counts` equal-width buckets over `[lo, hi]`, the
/// last inclusive of `hi`.
pub struct HistData {
    pub lo: f64,
    pub hi: f64,
    pub counts: Vec<u64>,
    pub total: u64,
}

impl HistData {
    /// Bin finite `values`; `None` when there are none. The default bin count
    /// is Sturges' rule (⌈log2 n⌉ + 1), capped at 50; `bins` wins when given.
    /// `span` is an explicit axis range (`-x`/`--xrange`): the bins then cover
    /// exactly it, so a range widens or narrows the axis rather than only
    /// filtering — the values are already clipped to it.
    pub fn build(values: &[f64], bins: Option<usize>, span: AxisRange) -> Option<HistData> {
        // No values is still an empty chart, range or not.
        let (dlo, dhi) = crate::graph::minmax(values.iter().copied())?;
        let (lo, hi) = span.unwrap_or((dlo, dhi));
        let n = values.len();
        let nbins = bins
            .unwrap_or_else(|| ((n as f64).log2().ceil() as usize + 1).clamp(1, 50))
            .max(1);
        let mut counts = vec![0u64; nbins];
        for &v in values {
            let idx = match axis_frac(v, lo, hi) {
                Some(t) => (t * nbins as f64).floor() as usize,
                // A zero span (all values equal) puts everything in bin 0.
                None => 0,
            };
            counts[idx.min(nbins - 1)] += 1;
        }
        Some(HistData {
            lo,
            hi,
            counts,
            total: n as u64,
        })
    }
}

/// A 2-D density grid: how many points fall in each of `cols` × `rows` equal
/// cells over `[xlo, xhi]` × `[ylo, yhi]`. A scatter of millions of points is a
/// blob; binning it to the canvas keeps the chart O(canvas) whatever the input
/// size, and the count is what the cell is shaded by.
pub struct HeatData {
    /// The x column's name (the CSV header's first cell under `--data`).
    pub xname: String,
    /// The y column's name.
    pub yname: String,
    pub xlo: f64,
    pub xhi: f64,
    pub ylo: f64,
    pub yhi: f64,
    pub cols: usize,
    pub rows: usize,
    /// Row-major counts; row 0 is the lowest y band.
    pub counts: Vec<u64>,
    pub total: u64,
    /// How to graduate the x axis, as for a scatter (the x column goes through
    /// the same numeric / time / category modes).
    pub xaxis: XAxis,
}

/// The extent a heatmap's grid spans, on both of its binned axes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub xlo: f64,
    pub xhi: f64,
    pub ylo: f64,
    pub yhi: f64,
}

impl HeatData {
    /// The extent the grid spans: an explicit `xrange`/`yrange` where one was
    /// given (`-x`/`-y` *is* the axis, so it sets the bin spans and the points
    /// outside it are already clipped away), else the points' own spread on
    /// that axis, folded in one pass. An axis with neither a range nor a point
    /// spans nothing and reads `(0, 0)`.
    ///
    /// The collector asks for this before it counts the cells — the y bounds
    /// decide how wide the canvas is, and so how many columns the grid has —
    /// and hands the same answer to [`heat_counts`].
    pub fn bounds(points: &[(f64, f64)], xrange: AxisRange, yrange: AxisRange) -> Bounds {
        let mut it = points.iter().copied();
        let spread = it.next().map(|(x, y)| {
            it.fold((x, x, y, y), |(xlo, xhi, ylo, yhi), (x, y)| {
                (xlo.min(x), xhi.max(x), ylo.min(y), yhi.max(y))
            })
        });
        let (xlo, xhi) = xrange
            .or_else(|| spread.map(|(xlo, xhi, _, _)| (xlo, xhi)))
            .unwrap_or((0.0, 0.0));
        let (ylo, yhi) = yrange
            .or_else(|| spread.map(|(_, _, ylo, yhi)| (ylo, yhi)))
            .unwrap_or((0.0, 0.0));
        Bounds { xlo, xhi, ylo, yhi }
    }

    /// The ends of the count axis the cells are shaded along: one point at the
    /// low end, the busiest cell at the high one, both through [`hist_len`] so
    /// `log` scales them the way it scales the shades.
    ///
    /// Both renderers ask the model for this, so a cell cannot take one shade
    /// in the terminal and another in the SVG.
    pub fn count_bounds(&self, log: bool) -> (f64, f64) {
        let max = self.counts.iter().copied().max().unwrap_or(0);
        (hist_len(1, log), hist_len(max, log))
    }

    /// The lower edges of cell `i`'s x and y bands — the corner `--data` names
    /// the cell by.
    pub fn cell_lo(&self, i: usize) -> (f64, f64) {
        let (cx, cy) = (i % self.cols, i / self.cols);
        // Each corner is a step along its axis, so an axis too wide to
        // subtract still names real numbers (see [`lerp`]).
        (
            lerp(self.xlo, self.xhi, cx, self.cols),
            lerp(self.ylo, self.yhi, cy, self.rows),
        )
    }
}

/// Row-major counts of the `(x, y)` points falling in each of `cols` × `rows`
/// equal cells over `b` (what [`HeatData::bounds`] worked out for the same
/// points). Row 0 is the lowest y band. A chart whose points were all dropped
/// still gets a grid of the right shape with nothing in it: the renderers
/// report an empty chart from `total`, and `--data` still writes the column
/// names. A dimension of zero is treated as one — a grid has to have a band
/// for a point to fall in.
pub fn heat_counts(points: &[(f64, f64)], cols: usize, rows: usize, b: Bounds) -> Vec<u64> {
    let (cols, rows) = (cols.max(1), rows.max(1));
    let mut counts = vec![0u64; cols * rows];
    // Which band `v` falls in: the last one is inclusive of the high end, and
    // a zero span puts everything in band 0.
    let cell = |v: f64, lo: f64, hi: f64, n: usize| -> usize {
        match axis_frac(v, lo, hi) {
            Some(t) => ((t * n as f64).floor() as usize).min(n - 1),
            None => 0,
        }
    };
    for &(x, y) in points {
        counts[cell(y, b.ylo, b.yhi, rows) * cols + cell(x, b.xlo, b.xhi, cols)] += 1;
    }
    counts
}

/// One bar row: its label and one value per series (`None` where the cell was
/// not numeric).
pub type BarRow = (String, Vec<Option<f64>>);

/// Labelled bars: one row per label, one value per series.
pub struct BarData {
    /// The label column's name (the CSV header's first cell under `--data`).
    pub label_name: String,
    pub value_names: Vec<String>,
    pub rows: Vec<BarRow>,
    /// An explicit value axis (`-y`/`--yrange`) instead of the data's own
    /// range: a bar past it draws to the edge, still printing its real value.
    pub axis: AxisRange,
}

impl BarData {
    /// The value axis the bars are drawn against, in axis units (`log` maps a
    /// real value through [`bar_value`]): an explicit `-y` axis, else a
    /// baseline at 0 stretched over every series' drawable values, so the bars
    /// of a group read against one scale. A value a log axis cannot place has
    /// no bar and so no say in the bounds.
    ///
    /// Both renderers ask the model for this, so a bar cannot land in one place
    /// in the terminal and another in the SVG.
    pub fn axis_bounds(&self, log: bool) -> (f64, f64) {
        // The bounds of an explicit axis always have a bar value: the parser
        // rejects a non-positive `-y` bound under `--log`, so the fallback here
        // is the no-axis case.
        self.axis
            .and_then(|(lo, hi)| Some((bar_value(lo, log)?, bar_value(hi, log)?)))
            .unwrap_or_else(|| {
                let drawn = || {
                    self.rows
                        .iter()
                        .flat_map(|(_, vs)| vs.iter().flatten())
                        .filter_map(|&v| bar_value(v, log))
                };
                (
                    drawn().fold(0.0_f64, f64::min),
                    drawn().fold(0.0_f64, f64::max),
                )
            })
    }
}

/// A sparkline's values, already bucketed to the chart width.
pub struct SparkData {
    /// The charted column's name (the CSV value header under `--data`).
    pub name: String,
    /// The bucketed values, in the input's own units — a log axis maps them
    /// through [`value_pos`] as they are drawn.
    pub values: Vec<f64>,
    /// An explicit value range (`-y`/`--yrange`): the levels scale to it
    /// instead of to the values' own min/max. Real units, like `values`.
    pub range: AxisRange,
}

impl SparkData {
    /// The value axis the levels scale to: an explicit `-y` range, else the
    /// values' own spread, else nothing at all. Real units, as the model keeps
    /// them; a log axis maps them at draw time.
    ///
    /// Both renderers ask the model for this, so a cell cannot sit at one
    /// height in the terminal and another in the SVG.
    pub fn bounds(&self) -> (f64, f64) {
        self.range
            .or_else(|| crate::graph::minmax(self.values.iter().copied()))
            .unwrap_or((0.0, 0.0))
    }
}

/// One input row of an xy chart: the raw x cell, its plotted x, one y per
/// series (`None` where the cell was not numeric, or not positive under a log
/// axis), and the `--color-by` value. The ys are in the input's own units — a
/// log axis maps them through [`value_pos`] as they are drawn.
pub struct XyRow {
    pub xcell: String,
    pub x: f64,
    pub ys: Vec<Option<f64>>,
    pub color_by: Option<f64>,
}

/// A scatter/line chart's rows and how to graduate its x axis.
pub struct XyData {
    pub xname: String,
    pub names: Vec<String>,
    pub rows: Vec<XyRow>,
    pub xaxis: XAxis,
    /// Join the points of each series (a `line` chart, not a `scatter`).
    pub connect: bool,
    /// Explicit axis ranges (`-x`/`-y`): the canvas spans these instead of the
    /// points' own extent. The points are already clipped to them. Real units,
    /// like the ys `yrange` bounds.
    pub xrange: AxisRange,
    pub yrange: AxisRange,
    /// The `-c/--color-by` column's name, when the script asked for one. This
    /// is the *plan's* answer, not the data's: a colour column whose every cell
    /// was non-numeric still makes this a colour-by chart (drawn with no
    /// colour, and counted in the notes), so the renderers never quietly fall
    /// back to density.
    pub color_by: Option<String>,
}

impl XyData {
    /// The `-c/--color-by` value of every plotted point of the first series, in
    /// the same order as `series()[0]`. `--color-by` needs a single y series
    /// (the parser says so), so this is the whole per-point colouring.
    pub fn color_values(&self) -> Vec<Option<f64>> {
        self.rows
            .iter()
            .filter(|r| r.ys.first().is_some_and(Option::is_some))
            .map(|r| r.color_by)
            .collect()
    }

    /// The chart's extent, `(xlo, xhi, ylo, yhi)` over the plotted points, with
    /// an explicit `-x`/`-y` range in place of the points' own spread on that
    /// axis — a range *is* the axis. The ys are real, as the model keeps them;
    /// a log axis maps them at draw time. A chart with no point has no extent,
    /// and the empty fold's infinities come back for the renderers to read as
    /// "nothing to draw".
    ///
    /// Both renderers ask the model for this, so the terminal chart and the SVG
    /// cannot frame the same points differently.
    pub fn bounds(&self) -> (f64, f64, f64, f64) {
        let (mut xlo, mut xhi) = (f64::INFINITY, f64::NEG_INFINITY);
        let (mut ylo, mut yhi) = (f64::INFINITY, f64::NEG_INFINITY);
        for r in &self.rows {
            for y in r.ys.iter().flatten() {
                xlo = xlo.min(r.x);
                xhi = xhi.max(r.x);
                ylo = ylo.min(*y);
                yhi = yhi.max(*y);
            }
        }
        let (xlo, xhi) = self.xrange.unwrap_or((xlo, xhi));
        let (ylo, yhi) = self.yrange.unwrap_or((ylo, yhi));
        (xlo, xhi, ylo, yhi)
    }

    /// The ends of the `-c/--color-by` ramp: the lowest and highest colour
    /// value over the plotted points, or `None` when no plotted point carried
    /// one. One derivation for both renderers, so a point's colour does not
    /// depend on which of them drew it.
    pub fn color_bounds(&self) -> Option<(f64, f64)> {
        crate::graph::minmax(
            self.rows
                .iter()
                .filter(|r| r.ys.first().is_some_and(Option::is_some))
                .filter_map(|r| r.color_by),
        )
    }

    /// The plotted points per y-series.
    pub fn series(&self) -> Vec<Vec<(f64, f64)>> {
        let mut out = vec![Vec::new(); self.names.len()];
        for r in &self.rows {
            for (i, y) in r.ys.iter().enumerate() {
                if let Some(y) = y {
                    out[i].push((r.x, *y));
                }
            }
        }
        out
    }
}

/// One chart's data, by kind.
pub enum ChartData {
    /// `None` when no value was numeric.
    Hist(Option<HistData>),
    Bar(BarData),
    Spark(SparkData),
    Xy(XyData),
    /// Always present, empty (`total == 0`) when no point survived.
    Heat(HeatData),
}

/// What a chart could not use, counted rather than described. Every collector
/// reports the same five counts, and one place turns them into the notes a
/// chart prints under itself — so the wording and the order of the notes do not
/// depend on which kind dropped what.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Drops {
    /// Cells that held no number.
    pub skipped: u64,
    /// Values outside an `-x`/`-y` range.
    pub clipped: u64,
    /// Values a log axis cannot place (not positive).
    pub dropped: u64,
    /// `-c/--color-by` cells that held no number. Those points are still
    /// plotted, just left uncoloured.
    pub color: u64,
    /// Bar labels past the drawn cap.
    pub truncated: usize,
    /// The grid a heatmap's `-b` was cut down to, when what draws it — the
    /// terminal canvas, an SVG's plot area — could not hold the one it asked
    /// for.
    pub grid_capped: Option<(usize, usize)>,
}

impl Drops {
    /// The notes for what was dropped, in one fixed order, empty where nothing
    /// was. This is the whole "strict and loud" wording.
    pub fn notes(&self) -> Vec<String> {
        [
            (self.truncated > 0).then(|| format!("+{} more not shown", self.truncated)),
            (self.skipped > 0).then(|| format!("skipped {} non-numeric", self.skipped)),
            (self.clipped > 0).then(|| format!("clipped {} out of range", self.clipped)),
            (self.dropped > 0).then(|| format!("dropped {} non-positive", self.dropped)),
            (self.color > 0).then(|| format!("{} non-numeric colour cells", self.color)),
            self.grid_capped
                .map(|(c, r)| format!("grid capped to {c}x{r}")),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// A collected chart: its data and the counts of what it could not use.
pub struct Collected {
    pub data: ChartData,
    pub drops: Drops,
}

/// Parse a cell as a finite f64: the numeric-cell rule shared by every chart.
/// `None` for a missing, empty, or non-numeric cell.
fn cell_num(row: &[Field], pos: usize) -> Option<f64> {
    row.get(pos)
        .and_then(Field::num_opt)
        .filter(|v| v.is_finite())
}

/// Apply `f` to each data row of the buffered CSV output, skipping the header.
fn for_each_data_row(text: &str, mut f: impl FnMut(&[Field])) {
    let mut first = true;
    csv::parse_chunk(text, |r| {
        if first {
            first = false;
        } else {
            f(r);
        }
    });
}

/// Keep the values inside `range` (when given); the count dropped is reported.
fn clip(values: Vec<f64>, range: AxisRange) -> (Vec<f64>, u64) {
    let Some((lo, hi)) = range else {
        return (values, 0);
    };
    let before = values.len();
    let kept: Vec<f64> = values
        .into_iter()
        .filter(|v| (lo..=hi).contains(v))
        .collect();
    let dropped = (before - kept.len()) as u64;
    (kept, dropped)
}

/// Where a real value `v` sits on the value axis: `v` itself, or its log10 on
/// a log axis. The chart data keeps real values throughout — every renderer
/// maps through this at draw time, so the labels, the summaries and `--data`
/// all read the numbers that were in the input.
pub fn value_pos(v: f64, log: bool) -> f64 {
    if log { v.log10() } else { v }
}

/// Where a bar of real value `v` is drawn on the value axis: `v` itself, or its
/// log10 on a log axis — `None` for a value a log axis cannot place.
pub fn bar_value(v: f64, log: bool) -> Option<f64> {
    match log {
        true => (v > 0.0).then(|| v.log10()),
        false => Some(v),
    }
}

/// How long a histogram bin's bar is drawn: the count itself, or
/// `log10(count + 1)` on a log axis — the `+ 1` keeps an empty bin at zero
/// length and a count of 1 visible. The printed count stays raw.
pub fn hist_len(count: u64, log: bool) -> f64 {
    let count = count as f64;
    if log { (count + 1.0).log10() } else { count }
}

/// Keep only the positive values — the ones a log axis can place; the count
/// dropped is reported. The values themselves stay real: the log10 happens when
/// they are drawn ([`value_pos`]).
fn drop_non_positive(values: Vec<f64>) -> (Vec<f64>, u64) {
    let before = values.len();
    let kept: Vec<f64> = values.into_iter().filter(|v| *v > 0.0).collect();
    let dropped = (before - kept.len()) as u64;
    (kept, dropped)
}

/// Blank each xy y a log axis cannot place (not positive, counted) and drop the
/// rows left with no y at all. The kept ys stay real: the log10 happens when
/// they are drawn ([`value_pos`]).
fn drop_non_positive_xy(xy: &mut XyData) -> u64 {
    let mut dropped = 0u64;
    xy.rows.retain_mut(|r| {
        for y in &mut r.ys {
            if y.is_some_and(|v| v <= 0.0) {
                *y = None;
                dropped += 1;
            }
        }
        r.ys.iter().any(Option::is_some)
    });
    refresh_ends(xy);
    dropped
}

/// A category axis labels its ends with the raw cells, so they have to come
/// from the rows that survived a drop, not the original first and last.
fn refresh_ends(xy: &mut XyData) {
    let XyData { rows, xaxis, .. } = xy;
    if let XAxis::Ends(first, last) = xaxis {
        let cell = |r: Option<&XyRow>| r.map(|r| r.xcell.clone()).unwrap_or_default();
        *first = cell(rows.first());
        *last = cell(rows.last());
    }
}

/// Apply the axis ranges to already-collected xy rows: a row whose x falls
/// outside `xrange` goes entirely, and a y outside `yrange` is blanked. Every
/// plotted point lost is counted, so the chart still says what it dropped.
fn clip_xy(xy: &mut XyData, xrange: AxisRange, yrange: AxisRange) -> u64 {
    let mut clipped = 0u64;
    if xrange.is_none() && yrange.is_none() {
        return clipped;
    }
    xy.rows.retain_mut(|r| {
        if let Some((lo, hi)) = xrange
            && !(lo..=hi).contains(&r.x)
        {
            clipped += r.ys.iter().filter(|y| y.is_some()).count() as u64;
            return false;
        }
        if let Some((lo, hi)) = yrange {
            for y in &mut r.ys {
                if y.is_some_and(|v| !(lo..=hi).contains(&v)) {
                    *y = None;
                    clipped += 1;
                }
            }
        }
        r.ys.iter().any(Option::is_some)
    });
    refresh_ends(xy);
    clipped
}

/// One column's finite values, plus the non-numeric count.
fn collect_numeric(text: &str, pos: usize) -> (Vec<f64>, u64) {
    let mut values = Vec::new();
    let mut skipped = 0u64;
    for_each_data_row(text, |r| match cell_num(r, pos) {
        Some(v) => values.push(v),
        None => skipped += 1,
    });
    (values, skipped)
}

/// One column's values, ready to chart, and what it took to get there: the
/// finite numbers in it, inside `range`, and under `log` the positive ones —
/// each pass counting what it threw away. The range is in real values, so it
/// clips before the log.
///
/// The hist and spark collectors are this same pipeline. They differ in which
/// axis their range came off — a hist's `-x`, a spark's `-y` — and in `log`: a
/// spark passes `-l` on, a hist passes `false`, because its value axis is the
/// bin count and no input value is dropped for it.
fn numeric_values(text: &str, pos: usize, range: AxisRange, log: bool) -> (Vec<f64>, Drops) {
    let (values, skipped) = collect_numeric(text, pos);
    let (values, clipped) = clip(values, range);
    let (values, dropped) = if log {
        drop_non_positive(values)
    } else {
        (values, 0)
    };
    (
        values,
        Drops {
            skipped,
            clipped,
            dropped,
            ..Drops::default()
        },
    )
}

/// Default cap on the number of bar labels drawn (one terminal line each);
/// excess is counted and reported rather than flooding the screen ("no silent
/// caps").
pub const MAX_BARS: usize = 50;

/// `(label, values)` rows for a bar chart. A row whose every value is
/// non-numeric is skipped; rows past `cap` are counted as truncated.
fn collect_bars(
    text: &str,
    label_pos: usize,
    value_pos: &[usize],
    cap: usize,
) -> (Vec<BarRow>, u64, usize) {
    let mut rows: Vec<BarRow> = Vec::new();
    let mut skipped = 0u64;
    let mut truncated = 0usize;
    for_each_data_row(text, |r| {
        let values: Vec<Option<f64>> = value_pos.iter().map(|&p| cell_num(r, p)).collect();
        if values.iter().all(Option::is_none) {
            skipped += 1;
        } else if rows.len() < cap {
            let label = r
                .get(label_pos)
                .map(|f| f.as_str().into_owned())
                .unwrap_or_default();
            rows.push((label, values));
        } else {
            truncated += 1;
        }
    });
    (rows, skipped, truncated)
}

/// Bucket-average `values` down to `cols` cells (the sparkline's width); a
/// series that already fits is left alone.
pub(crate) fn bucket(values: &[f64], cols: usize) -> Vec<f64> {
    let cols = cols.max(1);
    if values.len() <= cols {
        return values.to_vec();
    }
    (0..cols)
        .map(|i| {
            let start = i * values.len() / cols;
            let end = ((i + 1) * values.len() / cols).max(start + 1);
            let slice = &values[start..end.min(values.len())];
            slice.iter().sum::<f64>() / slice.len() as f64
        })
        .collect()
}

/// Collect the rows of a scatter/line chart (header skipped). A non-numeric y
/// drops just that series' point (counted in `skipped`). The x column is
/// handled in three modes:
/// - **numeric** — plotted as-is, numeric axis labels;
/// - **temporal** — if no value is numeric but every one parses as a timestamp,
///   plot at true epoch positions and label the axis with formatted dates;
/// - **row-index fallback** — otherwise (e.g. categories) plot against the
///   1-based row ordinal and label the axis with the first/last raw cells.
///
/// `color_pos` is the `-c/--color-by` column: each row keeps its value there,
/// for the renderers to map onto the ramp. A non-numeric colour cell simply
/// leaves that point uncoloured (it is not a reason to drop the point); the
/// caller counts those once the clipping is done, from the points it kept.
fn collect_xy(
    text: &str,
    xname: &str,
    xpos: usize,
    names: &[String],
    ypos: &[usize],
    color_pos: Option<usize>,
) -> (XyData, u64) {
    // Every data row becomes a row of the chart straight away, with the numeric
    // reading of its x cell as a provisional position (`NaN` where that cell
    // held no number — `cell_num` never yields one, so it cannot be mistaken
    // for a real x). Which mode the x axis is in is only known once every row
    // has been seen, so the positions are patched afterwards.
    let mut rows: Vec<XyRow> = Vec::new();
    let mut any_numeric_x = false;
    for_each_data_row(text, |r| {
        let x = cell_num(r, xpos);
        any_numeric_x |= x.is_some();
        rows.push(XyRow {
            xcell: r
                .get(xpos)
                .map(|f| f.as_str().into_owned())
                .unwrap_or_default(),
            x: x.unwrap_or(f64::NAN),
            ys: ypos.iter().map(|&p| cell_num(r, p)).collect(),
            color_by: color_pos.and_then(|p| cell_num(r, p)),
        });
    });

    let mut skipped = 0u64;
    let epochs = || {
        rows.iter()
            .map(|r| crate::datetime::parse_epoch(&r.xcell))
            .collect::<Option<Vec<f64>>>()
    };
    let xaxis = if any_numeric_x {
        // Numeric x: plot as-is; a row whose x is not a number goes entirely,
        // and every series' point on it counts as skipped.
        rows.retain(|r| {
            if r.x.is_nan() {
                skipped += r.ys.len() as u64;
                return false;
            }
            true
        });
        XAxis::Numeric
    } else if let Some(epochs) = (!rows.is_empty()).then(epochs).flatten() {
        // No numeric x, but every cell parses as a timestamp: a true time axis
        // (real spacing; the axis ticks format as dates).
        for (r, e) in rows.iter_mut().zip(epochs) {
            r.x = e;
        }
        XAxis::Time
    } else {
        // Row-index fallback: plot against the 1-based ordinal, label the axis
        // with the raw first/last cells.
        let cell = |r: Option<&XyRow>| r.map(|r| r.xcell.clone()).unwrap_or_default();
        let (first, last) = (cell(rows.first()), cell(rows.last()));
        for (i, r) in rows.iter_mut().enumerate() {
            r.x = (i + 1) as f64;
        }
        XAxis::Ends(first, last)
    };
    // A row is kept when at least one series has a value there; the blanks are
    // still counted, one per series.
    rows.retain(|r| {
        skipped += r.ys.iter().filter(|y| y.is_none()).count() as u64;
        r.ys.iter().any(Option::is_some)
    });
    (
        XyData {
            xname: xname.to_string(),
            names: names.to_vec(),
            rows,
            xaxis,
            connect: false,
            xrange: None,
            yrange: None,
            color_by: None,
        },
        skipped,
    )
}

/// How many of the plotted points have no `-c/--color-by` value, or 0 when the
/// chart was never asked for one. Only a *drawn* point can be missing a colour,
/// so this is asked after the ranges and the log axis have taken their rows —
/// a note about colour has to count the same points the chart does.
fn color_drops(xy: &XyData) -> u64 {
    match xy.color_by {
        Some(_) => xy.color_values().iter().filter(|c| c.is_none()).count() as u64,
        None => 0,
    }
}

/// The default chart title: the value column, or `y vs x` for one series.
pub fn default_title(g: &GraphSpec) -> String {
    match g.kind {
        GraphKind::Bar => g.cols[1].name.clone(),
        GraphKind::Heatmap => format!("{} vs {}", g.cols[1].name, g.cols[0].name),
        GraphKind::Scatter | GraphKind::Line if g.cols.len() == 2 => {
            format!("{} vs {}", g.cols[1].name, g.cols[0].name)
        }
        GraphKind::Scatter | GraphKind::Line => format!("vs {}", g.cols[0].name),
        GraphKind::Hist | GraphKind::Spark => g.cols[0].name.clone(),
    }
}

/// Read the charted columns out of the buffered output (its first line is the
/// header) into the chart's data, with the counts of what it could not use.
/// Cells that are not numbers are dropped *loudly* — counted here and reported
/// by the renderer, the "strict and loud" policy.
pub fn collect(text: &str, g: &GraphSpec, width: usize, height: usize) -> Collected {
    let mut drops = Drops::default();
    let data = match g.kind {
        GraphKind::Hist => {
            // A hist's range is its `-x`, the span the bins cover, and its
            // value axis is the count — so `-l` is no reason to drop a value.
            let (values, d) = numeric_values(text, g.cols[0].pos, g.opts.xrange, false);
            drops = d;
            ChartData::Hist(HistData::build(&values, g.opts.bins, g.opts.xrange))
        }
        GraphKind::Spark => {
            let (values, d) = numeric_values(text, g.cols[0].pos, g.opts.yrange, g.opts.log);
            drops = d;
            ChartData::Spark(SparkData {
                name: g.cols[0].name.clone(),
                values: bucket(&values, width),
                range: g.opts.yrange,
            })
        }
        GraphKind::Bar => {
            let value_pos: Vec<usize> = g.cols[1..].iter().map(|c| c.pos).collect();
            // The cap is on the *drawn* labels, one terminal line each.
            // `--data` writes CSV, not a picture, so it keeps every row.
            let cap = if g.opts.data { usize::MAX } else { MAX_BARS };
            let (rows, skipped, truncated) = collect_bars(text, g.cols[0].pos, &value_pos, cap);
            (drops.skipped, drops.truncated) = (skipped, truncated);
            ChartData::Bar(BarData {
                label_name: g.cols[0].name.clone(),
                value_names: g.cols[1..].iter().map(|c| c.name.clone()).collect(),
                rows,
                axis: g.opts.yrange,
            })
        }
        GraphKind::Scatter | GraphKind::Line => {
            let ypos: Vec<usize> = g.cols[1..].iter().map(|c| c.pos).collect();
            let names: Vec<String> = g.cols[1..].iter().map(|c| c.name.clone()).collect();
            let (mut xy, skipped) = collect_xy(
                text,
                &g.cols[0].name,
                g.cols[0].pos,
                &names,
                &ypos,
                g.opts.color_by.as_ref().map(|c| c.pos),
            );
            xy.color_by = g.opts.color_by.as_ref().map(|c| c.name.clone());
            xy.connect = g.kind == GraphKind::Line;
            (xy.xrange, xy.yrange) = (g.opts.xrange, g.opts.yrange);
            // The ranges are in real values, so they clip before the log.
            let clipped = clip_xy(&mut xy, g.opts.xrange, g.opts.yrange);
            let dropped = if g.opts.log {
                drop_non_positive_xy(&mut xy)
            } else {
                0
            };
            (drops.skipped, drops.clipped, drops.dropped, drops.color) =
                (skipped, clipped, dropped, color_drops(&xy));
            ChartData::Xy(xy)
        }
        GraphKind::Heatmap => {
            // One y series and no colour column, so the x column goes through
            // the same three modes a scatter's does.
            let names = vec![g.cols[1].name.clone()];
            let (mut xy, skipped) = collect_xy(
                text,
                &g.cols[0].name,
                g.cols[0].pos,
                &names,
                &[g.cols[1].pos],
                None,
            );
            let clipped = clip_xy(&mut xy, g.opts.xrange, g.opts.yrange);
            (drops.skipped, drops.clipped) = (skipped, clipped);
            // `-l` is the *count* axis here: the y axis stays linear (it is one
            // of the two binned dimensions), so nothing is dropped for it.
            let points: Vec<(f64, f64)> = xy.series().into_iter().next().unwrap_or_default();
            // Without `-b` the grid is the canvas: one cell per character the
            // frame leaves for it, so a heatmap fills the chart it is drawn in.
            // The y bounds size that canvas, so the grid's extent is worked out
            // once here and handed to the builder.
            let b = HeatData::bounds(&points, g.opts.xrange, g.opts.yrange);
            // A grid is capped to whatever has to draw it. In the terminal it
            // *is* the canvas, so `-b N` cannot ask for more of it than the
            // frame has: a 4096-row grid in a 15-row chart is 4096 lines of
            // output, not a picture. An SVG has no cells but it does have
            // pixels, and a grid finer than the plot area draws cells under a
            // pixel — a picture of nothing, counted into a grid of millions of
            // bins. Either cut is said out loud like every other thing a chart
            // could not do. `-D` draws nothing at all, so there `-b` stands
            // whole, up to the [`MAX_CELLS`] the flag itself carries.
            //
            // The cut is what was asked for against what was got, so the ask
            // is read off `-b` *or*, without it, off the canvas: `-W 2000 -S`
            // asks for 1994 columns and gets the plot area's 640, the same
            // cut `-b 4096` gets and reported the same way.
            let cols_fit = crate::graph::canvas_cells(width, b.ylo, b.yhi);
            let rows_fit = height.max(1);
            let (cols_max, rows_max) = if g.opts.data {
                (MAX_CELLS, MAX_CELLS)
            } else if g.opts.svg {
                (crate::svg::PLOT_W as usize, crate::svg::PLOT_H as usize)
            } else {
                (cols_fit, rows_fit)
            };
            let (ask_c, ask_r) = (
                g.opts.bins.unwrap_or(cols_fit),
                g.opts.bins.unwrap_or(rows_fit),
            );
            let (cols, rows) = (ask_c.clamp(1, cols_max), ask_r.clamp(1, rows_max));
            if cols < ask_c || rows < ask_r {
                drops.grid_capped = Some((cols, rows));
            }
            ChartData::Heat(HeatData {
                xname: g.cols[0].name.clone(),
                yname: g.cols[1].name.clone(),
                xlo: b.xlo,
                xhi: b.xhi,
                ylo: b.ylo,
                yhi: b.yhi,
                cols,
                rows,
                counts: heat_counts(&points, cols, rows, b),
                total: points.len() as u64,
                xaxis: xy.xaxis,
            })
        }
    };
    Collected { data, drops }
}

/// The chart's reduced data as CSV (header first), for `--data`: the bins,
/// bars or points a chart would have drawn, written as rows instead. The model
/// holds real values whatever the axis is (a log axis maps them at draw time),
/// so the numbers here are the ones that came out of the pipeline.
pub fn to_csv(data: &ChartData) -> String {
    let mut out = String::new();
    let mut row = |cells: &[Field]| csv::write_row(&mut out, cells);
    // A blank cell for a value the chart had none for.
    let cell = |v: Option<f64>| v.map_or(Field::Str(""), Field::Num);
    match data {
        ChartData::Hist(h) => {
            row(&[
                Field::Str("bin_lo"),
                Field::Str("bin_hi"),
                Field::Str("count"),
            ]);
            if let Some(h) = h {
                let n = h.counts.len();
                for (i, &c) in h.counts.iter().enumerate() {
                    // Each edge is its own step along the axis rather than a
                    // multiple of one bin's width, so an axis too wide to
                    // subtract still writes real numbers (see [`lerp`]).
                    row(&[
                        Field::Num(lerp(h.lo, h.hi, i, n)),
                        Field::Num(lerp(h.lo, h.hi, i + 1, n)),
                        Field::Num(c as f64),
                    ]);
                }
            }
        }
        ChartData::Bar(b) => {
            let mut header = vec![Field::Str(&b.label_name)];
            header.extend(b.value_names.iter().map(|n| Field::Str(n)));
            row(&header);
            for (label, values) in &b.rows {
                let mut cells = vec![Field::Str(label)];
                cells.extend(values.iter().copied().map(cell));
                row(&cells);
            }
        }
        ChartData::Spark(s) => {
            row(&[Field::Str("bucket"), Field::Str(&s.name)]);
            for (i, &v) in s.values.iter().enumerate() {
                row(&[Field::Num((i + 1) as f64), Field::Num(v)]);
            }
        }
        ChartData::Heat(h) => {
            row(&[
                Field::Str(&h.xname),
                Field::Str(&h.yname),
                Field::Str("count"),
            ]);
            // One row per non-empty cell, named by its lower corner — an empty
            // cell is a row a `graph heatmap` never drew.
            for (i, &c) in h.counts.iter().enumerate() {
                if c == 0 {
                    continue;
                }
                let (x, y) = h.cell_lo(i);
                row(&[Field::Num(x), Field::Num(y), Field::Num(c as f64)]);
            }
        }
        ChartData::Xy(xy) => {
            let mut header = vec![Field::Str(&xy.xname)];
            header.extend(xy.names.iter().map(|n| Field::Str(n)));
            row(&header);
            for r in &xy.rows {
                let mut cells = vec![Field::Str(&r.xcell)];
                cells.extend(r.ys.iter().copied().map(cell));
                row(&cells);
            }
        }
    }
    out
}

/// Chart data shaped the way [`collect`] builds it, for the tests of every
/// module that reads the model — `chart`, `graph` and `svg` — so a renderer's
/// idea of a chart's rows cannot drift from the collector's.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use crate::field::format_num;

    /// A bar chart of `names` over `rows`, with an optional `-y` axis.
    pub(crate) fn bar_data(names: &[&str], rows: &[BarRow], axis: AxisRange) -> BarData {
        BarData {
            label_name: "k".to_string(),
            value_names: names.iter().map(|n| n.to_string()).collect(),
            rows: rows.to_vec(),
            axis,
        }
    }

    /// A single-series bar chart, as `collect` builds it for
    /// `graph bar LABEL VALUE`.
    pub(crate) fn one_series(rows: &[(&str, f64)]) -> BarData {
        let rows: Vec<BarRow> = rows
            .iter()
            .map(|(l, v)| (l.to_string(), vec![Some(*v)]))
            .collect();
        bar_data(&["v"], &rows, None)
    }

    /// An xy chart whose rows reproduce `series`. Every series shares the x
    /// column, so this is one row per distinct x with a blank where a series
    /// has no point there — the shape `collect_xy` produces.
    pub(crate) fn xy_data(
        names: &[&str],
        series: &[&[(f64, f64)]],
        xaxis: XAxis,
        connect: bool,
    ) -> XyData {
        let mut xs: Vec<f64> = Vec::new();
        for pts in series {
            for &(x, _) in *pts {
                if !xs.contains(&x) {
                    xs.push(x);
                }
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        XyData {
            xname: "x".to_string(),
            names: names.iter().map(|n| n.to_string()).collect(),
            rows: xs
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
                .collect(),
            xaxis,
            connect,
            xrange: None,
            yrange: None,
            color_by: None,
        }
    }

    /// A 2×2 heat grid over the unit square with the given row-major counts.
    pub(crate) fn heat_data(counts: Vec<u64>) -> HeatData {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chart_size_follows_the_terminal_then_scale_then_overrides() {
        assert_eq!(chart_size(1.0, None, None, None), (80, 15));
        assert_eq!(chart_size(1.0, Some(120), None, None), (120, 15));
        assert_eq!(chart_size(0.5, Some(120), None, None), (60, 8));
        assert_eq!(chart_size(2.0, Some(120), Some(40), Some(3)), (40, 3));
        // Floors: 16 wide, 2 high.
        assert_eq!(chart_size(0.01, None, None, None), (16, 2));
        assert_eq!(chart_size(1.0, None, Some(1), Some(1)), (16, 2));
        // ...and the same ceiling `-W`/`-H` have, so `-s` cannot escape it.
        assert_eq!(chart_size(1000.0, None, None, None), (4096, 4096));
    }

    #[test]
    fn frame_notes_render_as_a_tail_and_a_line() {
        let mut f = Frame::new("t".into(), 80, 15, false);
        assert_eq!(f.notes_tail(), "");
        f.notes.push("skipped 2 non-numeric".into());
        f.notes.push("+1 more not shown".into());
        assert_eq!(
            f.notes_tail(),
            "  (skipped 2 non-numeric)  (+1 more not shown)"
        );
        assert_eq!(f.notes_line(), "skipped 2 non-numeric  +1 more not shown");
    }

    #[test]
    fn lerp_hits_both_ends_exactly() {
        // An epoch-millisecond axis has no spare bits for a rounding error:
        // the weighted sum put the first edge of 1700000000000 at
        // 1700000000000.000244, and `--data` printed that. The ends are the
        // ends, and the steps between them still climb.
        let (lo, n) = (1700000000000.0, 11);
        let hi = lo + 100000.0;
        let edges: Vec<f64> = (0..=n).map(|i| lerp(lo, hi, i, n)).collect();
        assert_eq!(edges[0], lo);
        assert_eq!(edges[n], hi);
        assert!(edges.windows(2).all(|w| w[0] <= w[1]), "{edges:?}");
    }

    #[test]
    fn drops_notes_read_in_one_fixed_order() {
        assert!(Drops::default().notes().is_empty());
        let d = Drops {
            skipped: 1,
            clipped: 2,
            dropped: 3,
            color: 4,
            truncated: 5,
            grid_capped: Some((6, 7)),
        };
        assert_eq!(
            d.notes(),
            [
                "+5 more not shown",
                "skipped 1 non-numeric",
                "clipped 2 out of range",
                "dropped 3 non-positive",
                "4 non-numeric colour cells",
                "grid capped to 6x7",
            ]
        );
    }

    #[test]
    fn hist_data_bins_a_span_wider_than_f64_can_subtract() {
        // -1e308 to 1e308: `hi - lo` is infinity, so a fraction worked out
        // from it is NaN and every value would fall in bin 0. The bins have to
        // divide the axis all the same, and their edges stay real numbers.
        let h = HistData::build(&[-1e308, 0.0, 1e308], Some(4), None).unwrap();
        assert_eq!(h.counts, [1, 0, 1, 1]);
        let edges: Vec<f64> = (0..=4).map(|i| lerp(h.lo, h.hi, i, 4)).collect();
        assert!(edges.iter().all(|e| e.is_finite()), "{edges:?}");
        assert_eq!(edges[0], -1e308);
        assert_eq!(edges[2], 0.0);
        assert_eq!(edges[4], 1e308);
    }

    #[test]
    fn hist_data_bins_with_sturges_by_default() {
        let h = HistData::build(&[1.0, 2.0, 3.0, 4.0], None, None).unwrap();
        assert_eq!((h.lo, h.hi, h.total), (1.0, 4.0, 4));
        assert_eq!(h.counts.len(), 3); // ceil(log2 4) + 1
        assert_eq!(h.counts.iter().sum::<u64>(), 4);
        assert!(HistData::build(&[], None, None).is_none());
        assert_eq!(
            HistData::build(&[5.0, 5.0], Some(2), None).unwrap().counts,
            [2, 0]
        );
    }

    #[test]
    fn heat_counts_bin_points_into_a_grid() {
        let pts = [(0.0, 0.0), (1.0, 1.0), (1.0, 1.0), (0.5, 0.5)];
        let b = HeatData::bounds(&pts, None, None);
        // row 0 = low y. The bands are half-open, so only (0,0) is in the
        // low-left cell: (0.5, 0.5) sits on the mid boundary and joins the two
        // (1, 1)s in the top-right one.
        assert_eq!(heat_counts(&pts, 2, 2, b), [1, 0, 0, 3]);
        // Nothing to bin is still a grid of the right shape with nothing in it.
        let b = HeatData::bounds(&[], None, None);
        assert_eq!(heat_counts(&[], 2, 2, b), [0, 0, 0, 0]);
    }

    #[test]
    fn heat_counts_treats_a_zero_dimension_as_one() {
        // A dimension of zero has no last band to fall into: `n - 1` used to
        // wrap round and index past the end of the grid. One band it is.
        let pts = [(0.0, 0.0), (1.0, 1.0)];
        let b = HeatData::bounds(&pts, None, None);
        assert_eq!(heat_counts(&pts, 0, 2, b), [1, 1]);
        assert_eq!(heat_counts(&pts, 2, 0, b), [1, 1]);
        assert_eq!(heat_counts(&pts, 0, 0, b), [2]);
    }

    #[test]
    fn heat_counts_bin_an_axis_too_wide_to_subtract() {
        // As for a histogram, `hi - lo` over -1e308..1e308 is infinity, so a
        // column worked out from it is NaN and every point lands in column 0.
        let pts = [(-1e308, 0.0), (0.0, 0.0), (1e308, 0.0)];
        let b = HeatData::bounds(&pts, None, None);
        assert_eq!(heat_counts(&pts, 4, 1, b), [1, 0, 1, 1]);
    }

    #[test]
    fn heat_bounds_take_the_ranges_over_the_points_own_spread() {
        let pts = [(1.0, 2.0), (7.0, 3.0)];
        // No range: the points' own extent on both axes.
        assert_eq!(
            HeatData::bounds(&pts, None, None),
            Bounds {
                xlo: 1.0,
                xhi: 7.0,
                ylo: 2.0,
                yhi: 3.0
            }
        );
        // A range *is* the axis, on each axis independently.
        assert_eq!(
            HeatData::bounds(&pts, Some((0.0, 100.0)), Some((0.0, 10.0))),
            Bounds {
                xlo: 0.0,
                xhi: 100.0,
                ylo: 0.0,
                yhi: 10.0
            }
        );
        assert_eq!(
            HeatData::bounds(&pts, Some((0.0, 100.0)), None),
            Bounds {
                xlo: 0.0,
                xhi: 100.0,
                ylo: 2.0,
                yhi: 3.0
            }
        );
        // An axis with neither a range nor a point spans nothing.
        assert_eq!(
            HeatData::bounds(&[], None, None),
            Bounds {
                xlo: 0.0,
                xhi: 0.0,
                ylo: 0.0,
                yhi: 0.0
            }
        );
    }

    #[test]
    fn ascii_glyphs_are_all_ascii() {
        let g = Glyphs::ascii();
        let all: String = [
            g.full,
            g.axis_v,
            g.axis_corner,
            g.axis_h,
            g.axis_tick,
            g.legend,
        ]
        .into_iter()
        .chain(g.levels)
        .chain(g.shades)
        .chain((0u8..=255).map(g.braille))
        .collect();
        assert!(all.is_ascii(), "{all}");
        assert!(g.partial.is_none());
        assert_eq!((g.braille)(0), ' ');
        assert_eq!((g.braille)(0x40), '*');
    }

    #[test]
    fn value_pos_maps_onto_the_axis_only_when_log() {
        assert_eq!(value_pos(100.0, true), 2.0);
        assert_eq!(value_pos(100.0, false), 100.0);
    }

    #[test]
    fn hist_len_keeps_an_empty_bin_at_zero_and_a_count_of_one_visible() {
        // log10(count + 1): the `+ 1` is what stops an empty bin drawing a
        // bar of its own, and a lone point still gets one.
        assert_eq!(hist_len(0, true), 0.0);
        assert!(hist_len(1, true) > 0.0);
        // Off the log axis the length is the count itself.
        assert_eq!(hist_len(0, false), 0.0);
        assert_eq!(hist_len(7, false), 7.0);
    }

    #[test]
    fn to_csv_writes_each_kind_as_rows() {
        let h = HistData {
            lo: 0.0,
            hi: 10.0,
            counts: vec![3, 1],
            total: 4,
        };
        assert_eq!(
            to_csv(&ChartData::Hist(Some(h))),
            "bin_lo,bin_hi,count\n0,5,3\n5,10,1\n"
        );
        assert_eq!(to_csv(&ChartData::Hist(None)), "bin_lo,bin_hi,count\n");
        let b = BarData {
            label_name: "label".into(),
            value_names: vec!["n".into(), "m".into()],
            rows: vec![
                ("a".into(), vec![Some(1.0), None]),
                ("b, c".into(), vec![Some(2.0), Some(3.0)]),
            ],
            axis: None,
        };
        assert_eq!(
            to_csv(&ChartData::Bar(b)),
            "label,n,m\na,1,\n\"b, c\",2,3\n"
        );
        let s = SparkData {
            name: "value".into(),
            values: vec![1.5, 2.0],
            range: None,
        };
        assert_eq!(to_csv(&ChartData::Spark(s)), "bucket,value\n1,1.5\n2,2\n");
        let xy = XyData {
            xname: "t".into(),
            names: vec!["y".into()],
            rows: vec![XyRow {
                xcell: "2024-01-01".into(),
                x: 0.0,
                ys: vec![Some(2.0)],
                color_by: None,
            }],
            xaxis: XAxis::Time,
            connect: false,
            xrange: None,
            yrange: None,
            color_by: None,
        };
        assert_eq!(to_csv(&ChartData::Xy(xy)), "t,y\n2024-01-01,2\n");
    }

    #[test]
    fn to_csv_writes_real_values_under_a_log_axis() {
        // The model keeps real values whatever the axis is, so `--data` needs
        // no undoing and nothing is lost to a round trip through log10.
        let s = SparkData {
            name: "v".into(),
            values: vec![10.0, 100.0, 1000.0],
            range: None,
        };
        assert_eq!(
            to_csv(&ChartData::Spark(s)),
            "bucket,v\n1,10\n2,100\n3,1000\n"
        );
        // A magnitude that 10f64.powf(v.log10()) would not return exactly.
        let s = SparkData {
            name: "v".into(),
            values: vec![5000000000.0],
            range: None,
        };
        assert_eq!(to_csv(&ChartData::Spark(s)), "bucket,v\n1,5000000000\n");
        let xy = XyData {
            xname: "x".into(),
            names: vec!["y".into()],
            rows: vec![XyRow {
                xcell: "1".into(),
                x: 1.0,
                ys: vec![Some(5000000000.0)],
                color_by: None,
            }],
            xaxis: XAxis::Numeric,
            connect: false,
            xrange: None,
            yrange: None,
            color_by: None,
        };
        assert_eq!(to_csv(&ChartData::Xy(xy)), "x,y\n1,5000000000\n");
    }

    /// An xy chart of one series over `pts`, as `collect` builds it.
    fn xy_of(pts: &[(f64, f64)]) -> XyData {
        fixtures::xy_data(&["y"], &[pts], XAxis::Numeric, false)
    }

    #[test]
    fn xy_bounds_fold_the_points_unless_a_range_says_otherwise() {
        let mut d = xy_of(&[(1.0, 20.0), (3.0, 5.0)]);
        assert_eq!(d.bounds(), (1.0, 3.0, 5.0, 20.0));
        // A range *is* the axis, on that axis alone.
        d.xrange = Some((0.0, 10.0));
        assert_eq!(d.bounds(), (0.0, 10.0, 5.0, 20.0));
        d.yrange = Some((0.0, 100.0));
        assert_eq!(d.bounds(), (0.0, 10.0, 0.0, 100.0));
        // Nothing plotted has no extent; the renderers read that as an empty
        // chart rather than drawing an axis over it.
        assert!(!xy_of(&[]).bounds().0.is_finite());
    }

    #[test]
    fn xy_color_bounds_span_the_plotted_points_colour_values() {
        let mut d = xy_of(&[(1.0, 1.0), (2.0, 2.0), (3.0, 3.0)]);
        assert_eq!(d.color_bounds(), None);
        d.rows[0].color_by = Some(4.0);
        d.rows[2].color_by = Some(-1.0);
        assert_eq!(d.color_bounds(), Some((-1.0, 4.0)));
    }

    #[test]
    fn bar_axis_bounds_anchor_at_zero_or_take_the_explicit_axis() {
        let bar = |rows: Vec<BarRow>, axis| fixtures::bar_data(&["a", "b"], &rows, axis);
        let rows = vec![
            ("x".to_string(), vec![Some(2.0), Some(5.0)]),
            ("y".to_string(), vec![Some(-3.0), None]),
        ];
        // The baseline is 0 and the axis spans every series, so a group reads
        // against one scale.
        assert_eq!(bar(rows.clone(), None).axis_bounds(false), (-3.0, 5.0));
        // An explicit `-y` axis replaces that, through the drawn value.
        assert_eq!(
            bar(rows.clone(), Some((0.0, 10.0))).axis_bounds(false),
            (0.0, 10.0)
        );
        assert_eq!(
            bar(rows.clone(), Some((1.0, 100.0))).axis_bounds(true),
            (0.0, 2.0)
        );
        // Under a log axis only the positive values are drawable.
        assert_eq!(bar(rows, None).axis_bounds(true), (0.0, 0.6989700043360189));
        assert_eq!(bar(Vec::new(), None).axis_bounds(false), (0.0, 0.0));
    }

    #[test]
    fn xy_series_drops_blank_ys_per_series() {
        let d = fixtures::xy_data(
            &["a", "b"],
            &[&[(1.0, 2.0), (2.0, 3.0)], &[(2.0, 4.0)]],
            XAxis::Numeric,
            false,
        );
        assert_eq!(
            d.series(),
            vec![vec![(1.0, 2.0), (2.0, 3.0)], vec![(2.0, 4.0)]]
        );
    }
}
