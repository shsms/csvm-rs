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

/// The chart size in cells: an explicit `width`/`height` wins; else the
/// terminal width (or 80) and 15 rows, both times `scale`. Floors keep a
/// tiny chart drawable.
pub fn chart_size(
    scale: f64,
    term_width: Option<usize>,
    width: Option<usize>,
    height: Option<usize>,
) -> (usize, usize) {
    let scaled = |base: usize| (base as f64 * scale).round() as usize;
    let w = width.unwrap_or_else(|| scaled(term_width.unwrap_or(BASE_W)));
    let h = height.unwrap_or_else(|| scaled(BASE_H));
    (w.max(16), h.max(2))
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
        let (dlo, dhi) = crate::graph::minmax(values)?;
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

/// One bar row: its label and one value per series (`None` where the cell was
/// not numeric).
pub type BarRow = (String, Vec<Option<f64>>);

/// Labelled bars: one row per label, one value per series.
pub struct BarData {
    pub value_names: Vec<String>,
    pub rows: Vec<BarRow>,
    /// An explicit value axis (`-y`/`--yrange`) instead of the data's own
    /// range: a bar past it draws to the edge, still printing its real value.
    pub axis: AxisRange,
}

/// A sparkline's values, already bucketed to the chart width.
pub struct SparkData {
    pub values: Vec<f64>,
    /// An explicit value range (`-y`/`--yrange`): the levels scale to it
    /// instead of to the values' own min/max.
    pub range: AxisRange,
}

/// One input row of an xy chart: the raw x cell, its plotted x, one y per
/// series (`None` where the cell was not numeric), and the `--color-by` value.
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
    /// points' own extent. The points are already clipped to them.
    pub xrange: AxisRange,
    pub yrange: AxisRange,
}

impl XyData {
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
}

/// A collected chart: its data and the notes about what was dropped.
pub struct Collected {
    pub data: ChartData,
    pub notes: Vec<String>,
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

/// The note for `n` dropped non-numeric cells, or none.
fn skipped_note(n: u64) -> Option<String> {
    (n > 0).then(|| format!("skipped {n} non-numeric"))
}

/// Keep the values inside `range` (when given); the count dropped goes to the
/// note.
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

/// The note for `n` values dropped for falling outside an axis range, or none.
fn clipped_note(n: u64) -> Option<String> {
    (n > 0).then(|| format!("clipped {n} out of range"))
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
    // A category axis labels its ends with the raw cells, so they have to come
    // from the rows that survived the clip, not the original first and last.
    let XyData { rows, xaxis, .. } = xy;
    if let XAxis::Ends(first, last) = xaxis {
        let cell = |r: Option<&XyRow>| r.map(|r| r.xcell.clone()).unwrap_or_default();
        *first = cell(rows.first());
        *last = cell(rows.last());
    }
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
fn collect_xy(
    text: &str,
    xname: &str,
    xpos: usize,
    names: &[String],
    ypos: &[usize],
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
            color_by: None,
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
        },
        skipped,
    )
}

/// The default chart title: the value column, or `y vs x` for one series.
pub fn default_title(g: &GraphSpec) -> String {
    match g.kind {
        GraphKind::Bar => g.cols[1].name.clone(),
        GraphKind::Scatter | GraphKind::Line if g.cols.len() == 2 => {
            format!("{} vs {}", g.cols[1].name, g.cols[0].name)
        }
        GraphKind::Scatter | GraphKind::Line => format!("vs {}", g.cols[0].name),
        GraphKind::Hist | GraphKind::Spark => g.cols[0].name.clone(),
    }
}

/// Read the charted columns out of the buffered output (its first line is the
/// header) into the chart's data, with the notes about dropped rows. Cells that
/// are not numbers are dropped *loudly* — counted here and reported by the
/// renderer, the "strict and loud" policy.
pub fn collect(text: &str, g: &GraphSpec, width: usize) -> Collected {
    let mut notes = Vec::new();
    let data = match g.kind {
        GraphKind::Hist => {
            let (values, skipped) = collect_numeric(text, g.cols[0].pos);
            let (values, clipped) = clip(values, g.opts.xrange);
            notes.extend(skipped_note(skipped));
            notes.extend(clipped_note(clipped));
            ChartData::Hist(HistData::build(&values, g.opts.bins, g.opts.xrange))
        }
        GraphKind::Spark => {
            let (values, skipped) = collect_numeric(text, g.cols[0].pos);
            let (values, clipped) = clip(values, g.opts.yrange);
            notes.extend(skipped_note(skipped));
            notes.extend(clipped_note(clipped));
            ChartData::Spark(SparkData {
                values: bucket(&values, width),
                range: g.opts.yrange,
            })
        }
        GraphKind::Bar => {
            let value_pos: Vec<usize> = g.cols[1..].iter().map(|c| c.pos).collect();
            let (rows, skipped, truncated) =
                collect_bars(text, g.cols[0].pos, &value_pos, MAX_BARS);
            if truncated > 0 {
                notes.push(format!("+{truncated} more not shown"));
            }
            notes.extend(skipped_note(skipped));
            ChartData::Bar(BarData {
                value_names: g.cols[1..].iter().map(|c| c.name.clone()).collect(),
                rows,
                axis: g.opts.yrange,
            })
        }
        GraphKind::Scatter | GraphKind::Line => {
            let ypos: Vec<usize> = g.cols[1..].iter().map(|c| c.pos).collect();
            let names: Vec<String> = g.cols[1..].iter().map(|c| c.name.clone()).collect();
            let (mut xy, skipped) = collect_xy(text, &g.cols[0].name, g.cols[0].pos, &names, &ypos);
            xy.connect = g.kind == GraphKind::Line;
            (xy.xrange, xy.yrange) = (g.opts.xrange, g.opts.yrange);
            let clipped = clip_xy(&mut xy, g.opts.xrange, g.opts.yrange);
            // Only the category/row-index axis distorts spacing; numeric and
            // time axes are true.
            if matches!(xy.xaxis, XAxis::Ends(..)) {
                notes.push("even row spacing".to_string());
            }
            notes.extend(skipped_note(skipped));
            notes.extend(clipped_note(clipped));
            ChartData::Xy(xy)
        }
    };
    Collected { data, notes }
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
    fn xy_series_drops_blank_ys_per_series() {
        let d = XyData {
            xname: "x".into(),
            names: vec!["a".into(), "b".into()],
            rows: vec![
                XyRow {
                    xcell: "1".into(),
                    x: 1.0,
                    ys: vec![Some(2.0), None],
                    color_by: None,
                },
                XyRow {
                    xcell: "2".into(),
                    x: 2.0,
                    ys: vec![Some(3.0), Some(4.0)],
                    color_by: None,
                },
            ],
            xaxis: XAxis::Numeric,
            connect: false,
            xrange: None,
            yrange: None,
        };
        assert_eq!(
            d.series(),
            vec![vec![(1.0, 2.0), (2.0, 3.0)], vec![(2.0, 4.0)]]
        );
    }
}
