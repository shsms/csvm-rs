//! SVG rendering for the `graph` sink (`--svg`). Hand-written XML — no plotting
//! dependency, so it ships in the default build; PNG (which would need a raster
//! dependency) stays a feature-gated follow-up. It draws the same
//! [`crate::chart`] model the terminal renderers do, as a standalone `<svg>`
//! document.

use crate::chart::{
    BarData, BarRow, ChartData, Frame, HeatData, SparkData, XyData, bar_value, hist_len, value_pos,
};
use crate::color::{Ramp, rgb_hex};
use crate::field::format_num;
use crate::graph::{XAxis, series_rgb};

const W: f64 = 720.0;
const H: f64 = 440.0;
// Plot area inset for axes and labels.
const L: f64 = 64.0;
const R: f64 = 16.0;
const T: f64 = 32.0;
const B: f64 = 44.0;

/// Optional axis captions (`--xlabel`/`--ylabel`), drawn by [`header`] on every
/// chart kind so the terminal and SVG output can't drift.
#[derive(Clone, Copy, Debug, Default)]
pub struct Labels<'a> {
    pub x: Option<&'a str>,
    pub y: Option<&'a str>,
}

/// The series colour for index `i` as an SVG hex string, from the shared
/// terminal palette ([`crate::graph::series_rgb`]) so the two can't drift.
fn series_hex(i: usize) -> String {
    rgb_hex(&series_rgb(i))
}

/// The fill for a value `v` in `[lo, hi]` on `ramp` (`-r/--ramp`), or the
/// default chart colour when there is no ramp. Unlike the terminal renderers
/// this ignores `frame.color`: an SVG document always has colour.
fn fill_at(ramp: Option<Ramp>, v: f64, lo: f64, hi: f64) -> String {
    match ramp.and_then(|r| r.at(v, lo, hi).fg) {
        Some(rgb) => rgb_hex(&rgb),
        None => series_hex(0),
    }
}

fn pw() -> f64 {
    W - L - R
}
fn ph() -> f64 {
    H - T - B
}

/// Minimal XML text escaping for titles and labels.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn header(title: &str, body: &str, note: &str, labels: Labels) -> String {
    // A footer note (e.g. dropped non-numeric counts) keeps the SVG output as
    // "strict and loud" as the terminal charts.
    let footer = if note.is_empty() {
        String::new()
    } else {
        format!(
            "<text x=\"{x}\" y=\"{y}\" fill=\"#888\">{n}</text>\n",
            x = L,
            y = H - 8.0,
            n = esc(note),
        )
    };
    // `--xlabel`/`--ylabel`, when given: the same captions the terminal chart
    // prints, so the two can't drift. The x caption sits between the tick row
    // and the footer note, which own the lines above and below it.
    let ylabel = match labels.y {
        Some(y) => format!("<text x=\"{L}\" y=\"{}\">{}</text>\n", T - 8.0, esc(y)),
        None => String::new(),
    };
    let xlabel = match labels.x {
        Some(x) => format!(
            "<text x=\"{}\" y=\"{}\" text-anchor=\"middle\">{}</text>\n",
            L + pw() / 2.0,
            H - B + 24.0,
            esc(x)
        ),
        None => String::new(),
    };
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{W}\" height=\"{H}\" \
viewBox=\"0 0 {W} {H}\" font-family=\"sans-serif\" font-size=\"12\">\n\
<rect width=\"{W}\" height=\"{H}\" fill=\"white\"/>\n\
<text x=\"{x}\" y=\"20\" font-size=\"15\" font-weight=\"bold\">{t}</text>\n\
{ylabel}{body}{xlabel}{footer}</svg>\n",
        x = L,
        t = esc(title),
    )
}

/// The L-shaped x/y axis lines bounding the plot area.
fn axis_lines() -> String {
    let (x0, y0, x1, y1) = (L, T, L, T + ph());
    format!(
        "<line x1=\"{x0}\" y1=\"{y0}\" x2=\"{x1}\" y2=\"{y1}\" stroke=\"#888\"/>\n\
<line x1=\"{L}\" y1=\"{by}\" x2=\"{rx}\" y2=\"{by}\" stroke=\"#888\"/>\n",
        by = T + ph(),
        rx = L + pw(),
    )
}

/// y-axis labels at the top (hi) and bottom (lo) of the plot area. The bounds
/// are real values, whatever the axis does with them.
fn ylabels(lo: f64, hi: f64) -> String {
    format!(
        "<text x=\"{x}\" y=\"{ty}\" text-anchor=\"end\">{hi}</text>\n\
<text x=\"{x}\" y=\"{by}\" text-anchor=\"end\">{lo}</text>\n",
        x = L - 6.0,
        ty = T + 4.0,
        by = T + ph(),
        hi = format_num(hi),
        lo = format_num(lo),
    )
}

/// Graduated x-axis labels for `xaxis` over the data range `[xlo, xhi]`:
/// interpolated ticks for a numeric/time axis, or the two end cells for a
/// category axis. Ticks anchor start/middle/end so they don't run off the frame.
fn xlabels(xaxis: &XAxis, xlo: f64, xhi: f64) -> String {
    // Tick count from the plot width and the axis's label size (~7px per char);
    // the labels themselves come from the shared `graph::axis_ticks`.
    let label_px = crate::graph::axis_label_width(xaxis, xlo, xhi) as f64 * 7.0 + 12.0;
    let target = ((pw() / label_px) as usize).clamp(2, 7);
    let labels = crate::graph::axis_ticks(xaxis, xlo, xhi, target);
    let y = T + ph() + 16.0;
    labels
        .iter()
        .map(|(t, lab)| {
            // Anchor by position so edge labels don't run off the frame.
            let anchor = if *t <= 0.001 {
                "start"
            } else if *t >= 0.999 {
                "end"
            } else {
                "middle"
            };
            let x = L + t * pw();
            format!(
                "<text x=\"{x:.1}\" y=\"{y}\" text-anchor=\"{anchor}\">{}</text>\n",
                esc(lab)
            )
        })
        .collect()
}

/// Histogram: one filled bar per bin spanning the plot width. `log` puts the
/// count axis on a log10 scale, as in the terminal chart, and `ramp` fills each
/// bar by its count, as the terminal chart paints it.
// One argument over clippy's bar: the axis knobs are all independent, and
// bundling them would only move the list.
#[allow(clippy::too_many_arguments)]
pub fn hist(
    title: &str,
    lo: f64,
    hi: f64,
    counts: &[u64],
    log: bool,
    ramp: Option<Ramp>,
    note: &str,
    labels: Labels,
) -> String {
    let n = counts.len().max(1);
    let max = counts.iter().copied().max().unwrap_or(0).max(1);
    let bw = pw() / n as f64;
    let mut body = axis_lines();
    // The labels stay raw counts: log10(0 + 1) is 0 and the top of the axis is
    // the max either way.
    body.push_str(&ylabels(0.0, max as f64));
    body.push_str(&xlabels(&XAxis::Numeric, lo, hi));
    for (i, &c) in counts.iter().enumerate() {
        let bh = hist_len(c, log) / hist_len(max, log) * ph();
        let x = L + i as f64 * bw;
        let y = T + ph() - bh;
        body.push_str(&format!(
            "<rect x=\"{x:.2}\" y=\"{y:.2}\" width=\"{w:.2}\" height=\"{bh:.2}\" \
fill=\"{fill}\" stroke=\"white\"/>\n",
            w = (bw - 1.0).max(0.0),
            // The ramp runs over the raw counts, as in the terminal chart.
            fill = fill_at(ramp, c as f64, 0.0, max as f64),
        ));
    }
    header(title, &body, note, labels)
}

/// Horizontal bar chart: one labelled bar per row, anchored at a zero baseline
/// (a value of 1 on a log axis). `log` draws each bar at its log10, as in the
/// terminal chart, and a value a log axis cannot place gets no bar.
///
/// With several value columns each row is a *group*: one bar per series,
/// sharing the row's height, and a legend naming them — as in the terminal
/// chart, where grouped bars take the series palette (the parser rejects a
/// `-r/--ramp` there). A single series fills each bar by where it sits on the
/// value axis, as the terminal chart paints it.
pub fn bars(
    title: &str,
    b: &BarData,
    log: bool,
    ramp: Option<Ramp>,
    note: &str,
    labels: Labels,
) -> String {
    let (names, rows) = (&b.value_names, &b.rows);
    if rows.is_empty() {
        return header(title, "", note, labels);
    }
    let nseries = names.len().max(1);
    let multi = nseries > 1;
    // Each series keeps its real value (which is what prints) and the value the
    // bar is drawn at.
    let value_at = |row: &BarRow, i: usize| row.1.get(i).copied().flatten();
    let at_of = |row: &BarRow, i: usize| value_at(row, i).and_then(|v| bar_value(v, log));
    // The model works out the value axis — an explicit `-y` one, else a
    // baseline at 0 over every series — so these are the bounds the terminal
    // chart draws against too.
    let (lo, hi) = b.axis_bounds(log);
    let span = (hi - lo).max(f64::MIN_POSITIVE);
    let rh = ph() / rows.len() as f64;
    // A group's rows share the label row's height.
    let sh = rh / nseries as f64;
    let zero = L + (0.0_f64.clamp(lo, hi) - lo) / span * pw();
    let mut body = axis_lines();
    for (ri, row) in rows.iter().enumerate() {
        for i in 0..nseries {
            let at = at_of(row, i);
            // The drawn length is clamped to the axis; the printed value is real.
            let (x, w) = match at {
                Some(at) => {
                    let vx = L + (at.clamp(lo, hi) - lo) / span * pw();
                    (zero.min(vx), (vx - zero).abs())
                }
                None => (zero, 0.0),
            };
            let y = T + ri as f64 * rh + i as f64 * sh;
            // The label heads its group; every row of it prints its own value.
            let text = match (i == 0, value_at(row, i)) {
                (true, Some(v)) => format!("{} ({})", esc(&row.0), format_num(v)),
                (true, None) => esc(&row.0),
                (false, Some(v)) => format!("({})", format_num(v)),
                (false, None) => String::new(),
            };
            let label = if text.is_empty() {
                String::new()
            } else {
                format!(
                    "<text x=\"{lx}\" y=\"{ly:.2}\">{text}</text>\n",
                    lx = L + 4.0,
                    ly = y + sh / 2.0 + 4.0,
                )
            };
            body.push_str(&format!(
                "<rect x=\"{x:.2}\" y=\"{y:.2}\" width=\"{w:.2}\" height=\"{h:.2}\" \
fill=\"{fill}\"/>\n{label}",
                h = (sh - 2.0).max(0.0),
                // A row a log axis cannot place has no bar; it keeps the low end
                // of the ramp, which is what its zero-width rect would show
                // anyway.
                fill = if multi {
                    series_hex(i)
                } else {
                    fill_at(ramp, at.unwrap_or(lo), lo, hi)
                },
            ));
        }
    }
    if multi {
        body.push_str(&legend(names));
    }
    header(title, &body, note, labels)
}

/// The series legend of a multi-series chart, top right of the plot area: a
/// round marker in the series colour and its name — the same `● name` the
/// terminal charts print, so the two cannot drift. It is the one legend for
/// every chart that has series: grouped bars and the xy (scatter/line) charts
/// alike.
fn legend(names: &[String]) -> String {
    names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let y = T + 4.0 + i as f64 * 16.0;
            format!(
                "<circle cx=\"{cx}\" cy=\"{cy}\" r=\"5\" fill=\"{color}\"/>\n\
<text x=\"{tx}\" y=\"{ty}\">{n}</text>\n",
                cx = L + pw() - 85.0,
                cy = y + 5.0,
                tx = L + pw() - 76.0,
                ty = y + 9.0,
                color = series_hex(i),
                n = esc(name),
            )
        })
        .collect()
}

/// Sparkline as a single polyline across the plot width. `values` and `range`
/// are real; `log` maps them onto the value axis as they are drawn, and the
/// labels stay the real bounds.
///
/// `-r/--ramp` deliberately does not reach here: the terminal sparkline paints
/// per cell because it *is* a row of cells, while this is one continuous
/// stroke, so it stays in the default chart colour.
pub fn spark(title: &str, s: &SparkData, log: bool, note: &str, labels: Labels) -> String {
    let values = &s.values;
    if values.is_empty() {
        return header(title, "", note, labels);
    }
    // An explicit range (`-y`) is the axis, as in the terminal chart — the
    // model works it out for both.
    let (lo, hi) = s.bounds();
    let (plo, phi) = (value_pos(lo, log), value_pos(hi, log));
    let span = (phi - plo).max(f64::MIN_POSITIVE);
    let n = values.len().max(2);
    let pts: Vec<String> = values
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = L + i as f64 / (n - 1) as f64 * pw();
            let y = T + ph() - (value_pos(v, log) - plo) / span * ph();
            format!("{x:.2},{y:.2}")
        })
        .collect();
    let mut body = ylabels(lo, hi);
    body.push_str(&format!(
        "<polyline points=\"{}\" fill=\"none\" stroke=\"{stroke}\" stroke-width=\"1.5\"/>\n",
        pts.join(" "),
        stroke = series_hex(0),
    ));
    header(title, &body, note, labels)
}

/// Scatter, or line where `xy.connect` says so, of one or more y-series against
/// a shared x. Series get distinct colours and a legend. The ys and the y range
/// are real; `log` maps them onto the value axis as they are drawn, and the
/// labels stay the real bounds.
///
/// A `-c/--color-by` chart fills each circle with its own value's ramp colour.
/// The terminal renderer also ramps by point *density*, which has no meaning
/// here — density is a count of points per braille cell, and an SVG has no
/// cells — so `-r` alone leaves an SVG's points in the plain series colour.
pub fn xy_chart(
    title: &str,
    xy: &XyData,
    log: bool,
    ramp: Option<Ramp>,
    note: &str,
    labels: Labels,
) -> String {
    let series = xy.series();
    if series.iter().all(Vec::is_empty) {
        return header(title, "", note, labels);
    }
    // The model frames the chart: the points' own extent, or an explicit
    // `-x`/`-y` range where one was given (the points outside it are already
    // clipped away). The terminal chart asks it the same question.
    let (xlo, xhi, ylo, yhi) = xy.bounds();
    let (pylo, pyhi) = (value_pos(ylo, log), value_pos(yhi, log));
    let xspan = (xhi - xlo).max(f64::MIN_POSITIVE);
    let yspan = (pyhi - pylo).max(f64::MIN_POSITIVE);
    let map = |x: f64, y: f64| {
        let px = L + (x - xlo) / xspan * pw();
        let py = T + ph() - (value_pos(y, log) - pylo) / yspan * ph();
        (px, py)
    };

    // Only a `-c/--color-by` chart colours its points, and that is the plan's
    // answer, not the data's — the terminal renderer branches on the same flag,
    // so the two cannot disagree. There is no density counterpart here: density
    // counts points per *braille cell*, which only the terminal has. The ramp
    // spans the column's own range, as the model derives it; with no `-r` it is
    // the default one, so `-c` alone still colours.
    let ramp = ramp.unwrap_or_default();
    let by = xy.color_by.as_ref().map(|_| {
        let (clo, chi) = xy.color_bounds().unwrap_or((0.0, 0.0));
        (xy.color_values(), clo, chi)
    });

    let mut body = axis_lines();
    body.push_str(&ylabels(ylo, yhi));
    body.push_str(&xlabels(&xy.xaxis, xlo, xhi));
    for (si, pts) in series.iter().enumerate() {
        let color = series_hex(si);
        // The colour-by values describe the single plotted series.
        let by = by.as_ref().filter(|_| si == 0);
        if xy.connect {
            let line: Vec<String> = pts
                .iter()
                .map(|&(x, y)| {
                    let (px, py) = map(x, y);
                    format!("{px:.2},{py:.2}")
                })
                .collect();
            body.push_str(&format!(
                "<polyline points=\"{}\" fill=\"none\" stroke=\"{color}\" stroke-width=\"1.5\"/>\n",
                line.join(" ")
            ));
        } else {
            for (pi, &(x, y)) in pts.iter().enumerate() {
                let (px, py) = map(x, y);
                let fill = by
                    .and_then(|(vals, clo, chi)| {
                        let v = vals.get(pi).copied().flatten()?;
                        Some(fill_at(Some(ramp), v, *clo, *chi))
                    })
                    .unwrap_or_else(|| color.clone());
                body.push_str(&format!(
                    "<circle cx=\"{px:.2}\" cy=\"{py:.2}\" r=\"2\" fill=\"{fill}\"/>\n"
                ));
            }
        }
    }
    if series.len() > 1 {
        body.push_str(&legend(&xy.names));
    }
    header(title, &body, note, labels)
}

/// Heatmap: one filled rect per non-empty cell of the grid, on the same axes
/// and labels as [`xy_chart`]. The fill runs along `ramp` (`-r/--ramp`, else
/// the default one) from a count of one to the busiest cell, as the terminal
/// chart paints it; `log` puts that count axis on a log10 scale. An empty cell
/// draws nothing at all, so the page carries only the density that is there.
pub fn heat(
    title: &str,
    h: &HeatData,
    log: bool,
    ramp: Option<Ramp>,
    note: &str,
    labels: Labels,
) -> String {
    let mut body = axis_lines();
    body.push_str(&ylabels(h.ylo, h.yhi));
    body.push_str(&xlabels(&h.xaxis, h.xlo, h.xhi));
    let (lo, hi) = h.count_bounds(log);
    let (cw, ch) = (pw() / h.cols as f64, ph() / h.rows as f64);
    // A heatmap has no other use for a colour, so it always has a ramp.
    let ramp = Some(ramp.unwrap_or_default());
    for (i, &c) in h.counts.iter().enumerate() {
        if c == 0 {
            continue;
        }
        // Row 0 of the grid is the lowest y band, which is the bottom of the
        // plot area.
        let (cx, cy) = (i % h.cols, i / h.cols);
        let x = L + cx as f64 * cw;
        let y = T + ph() - (cy + 1) as f64 * ch;
        body.push_str(&format!(
            "<rect x=\"{x:.2}\" y=\"{y:.2}\" width=\"{cw:.2}\" height=\"{ch:.2}\" \
fill=\"{fill}\"/>\n",
            fill = fill_at(ramp, hist_len(c, log), lo, hi),
        ));
    }
    header(title, &body, note, labels)
}

/// Emit `data` as a standalone SVG document, using `frame`'s title and notes.
/// The per-kind emitters above carry the drawing; this picks the one that fits.
pub fn render(frame: &Frame, data: &ChartData) -> String {
    let note = frame.notes_line();
    let labels = Labels {
        x: frame.xlabel.as_deref(),
        y: frame.ylabel.as_deref(),
    };
    match data {
        // Keep the --svg contract even with nothing to plot: an empty chart.
        ChartData::Hist(None) => hist(
            &frame.title,
            0.0,
            0.0,
            &[],
            frame.log,
            frame.ramp,
            &note,
            labels,
        ),
        ChartData::Hist(Some(h)) => hist(
            &frame.title,
            h.lo,
            h.hi,
            &h.counts,
            frame.log,
            frame.ramp,
            &note,
            labels,
        ),
        ChartData::Bar(b) => bars(&frame.title, b, frame.log, frame.ramp, &note, labels),
        ChartData::Spark(s) => spark(&frame.title, s, frame.log, &note, labels),
        ChartData::Heat(h) => heat(&frame.title, h, frame.log, frame.ramp, &note, labels),
        ChartData::Xy(xy) => xy_chart(&frame.title, xy, frame.log, frame.ramp, &note, labels),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::AxisRange;

    /// A sparkline over `values`, with an optional `-y` range.
    fn spark_data(values: &[f64], range: AxisRange) -> SparkData {
        SparkData {
            name: "v".to_string(),
            values: values.to_vec(),
            range,
        }
    }

    /// A single-series bar chart, as `collect` builds it for
    /// `graph bar LABEL VALUE`.
    fn one_series(rows: &[(&str, f64)]) -> BarData {
        bar_data(
            &["v"],
            &rows
                .iter()
                .map(|(l, v)| (l.to_string(), vec![Some(*v)]))
                .collect::<Vec<BarRow>>(),
            None,
        )
    }

    /// A bar chart of `names` over `rows`, with an optional `-y` axis.
    fn bar_data(names: &[&str], rows: &[BarRow], axis: AxisRange) -> BarData {
        BarData {
            label_name: "k".to_string(),
            value_names: names.iter().map(|n| n.to_string()).collect(),
            rows: rows.to_vec(),
            axis,
        }
    }

    /// An xy chart of one point list per series, as `collect` builds it: each
    /// point is a row that only its own series has a value in.
    fn xy_data(names: &[&str], series: &[&[(f64, f64)]], connect: bool, xaxis: XAxis) -> XyData {
        let mut rows = Vec::new();
        for (si, pts) in series.iter().enumerate() {
            for &(x, y) in *pts {
                let mut ys = vec![None; names.len()];
                ys[si] = Some(y);
                rows.push(crate::chart::XyRow {
                    xcell: format_num(x),
                    x,
                    ys,
                    color_by: None,
                });
            }
        }
        XyData {
            xname: "x".to_string(),
            names: names.iter().map(|n| n.to_string()).collect(),
            rows,
            xaxis,
            connect,
            xrange: None,
            yrange: None,
            color_by: None,
        }
    }

    #[test]
    fn hist_emits_a_rect_per_bin() {
        let s = hist(
            "h",
            0.0,
            10.0,
            &[3, 1, 4],
            false,
            None,
            "",
            Labels::default(),
        );
        assert!(s.starts_with("<svg"));
        assert!(s.trim_end().ends_with("</svg>"));
        assert_eq!(s.matches("<rect").count(), 1 /*background*/ + 3);
    }

    #[test]
    fn axis_labels_render_as_text_elements() {
        let s = hist(
            "h",
            0.0,
            10.0,
            &[3, 1, 4],
            false,
            None,
            "",
            Labels {
                x: Some("v"),
                y: Some("n"),
            },
        );
        assert!(s.contains(">v</text>") && s.contains(">n</text>"), "{s}");
        // No labels given: neither caption appears.
        let bare = hist(
            "h",
            0.0,
            10.0,
            &[3, 1, 4],
            false,
            None,
            "",
            Labels::default(),
        );
        assert!(
            !bare.contains(">v</text>") && !bare.contains(">n</text>"),
            "{bare}"
        );
    }

    #[test]
    fn xy_scatter_emits_circles_and_line_emits_polyline() {
        let pts: &[(f64, f64)] = &[(0.0, 0.0), (1.0, 1.0)];
        let scatter = xy_data(&["y"], &[pts], false, XAxis::Numeric);
        let line = xy_data(&["y"], &[pts], true, XAxis::Numeric);
        assert!(xy_chart("s", &scatter, false, None, "", Labels::default()).contains("<circle"));
        assert!(xy_chart("s", &line, false, None, "", Labels::default()).contains("<polyline"));
    }

    #[test]
    fn xy_multi_series_uses_distinct_colours_and_a_legend() {
        let series: [&[(f64, f64)]; 2] = [&[(0.0, 0.0)], &[(1.0, 1.0)]];
        let names = ["a", "b"];
        let scatter = xy_data(&names, &series, false, XAxis::Numeric);
        let s = xy_chart("m", &scatter, false, None, "", Labels::default());
        assert!(s.contains("#4fc3f7") && s.contains("#ff8a65"));
        assert!(s.contains(">a</text>") && s.contains(">b</text>"));
        // The legend marker is the round one the terminal charts print, and it
        // is the same helper the bar chart uses. A line chart draws its data as
        // polylines, so its only circles are the legend's — one per series.
        let line = xy_chart(
            "m",
            &xy_data(&names, &series, true, XAxis::Numeric),
            false,
            None,
            "",
            Labels::default(),
        );
        assert_eq!(line.matches("<circle").count(), names.len(), "{line}");
        // ...and nothing but the background is a rect.
        assert_eq!(line.matches("<rect").count(), 1, "{line}");
    }

    #[test]
    fn xy_category_axis_shows_end_labels() {
        let ends = XAxis::Ends("t0".to_string(), "t9".to_string());
        let xy = xy_data(&["y"], &[&[(1.0, 0.0), (2.0, 1.0)]], true, ends);
        let s = xy_chart("y vs t", &xy, false, None, "", Labels::default());
        assert!(s.contains(">t0</text>") && s.contains(">t9</text>"), "{s}");
    }

    #[test]
    fn explicit_ranges_set_the_svg_axes() {
        // The y labels come from the range, not the values' own min/max.
        let s = spark(
            "v",
            &spark_data(&[1.0, 2.0], Some((0.0, 100.0))),
            false,
            "",
            Labels::default(),
        );
        assert!(s.contains(">100</text>"), "{s}");
        let mut xy = xy_data(&["y"], &[&[(1.0, 1.0)]], false, XAxis::Numeric);
        xy.yrange = Some((0.0, 100.0));
        let s = xy_chart("y", &xy, false, None, "", Labels::default());
        assert!(s.contains(">100</text>"), "{s}");
    }

    #[test]
    fn log_labels_show_the_real_value() {
        // The spark values are real; the log axis is applied as they are drawn,
        // so the y labels are the real bounds.
        let s = spark(
            "v",
            &spark_data(&[1.0, 100.0], None),
            true,
            "",
            Labels::default(),
        );
        assert!(s.contains(">100</text>") && s.contains(">1</text>"), "{s}");
    }

    #[test]
    fn log_bars_leave_the_non_positive_rows_empty() {
        // As in the terminal chart: 0.5 puts the baseline (a value of 1) inside
        // the plot, so a wrongly placed 0 or -5 would draw a visible bar.
        let b = one_series(&[("a", 100.0), ("b", 0.5), ("c", 0.0), ("d", -5.0)]);
        let s = bars("v", &b, true, None, "", Labels::default());
        // The two rows a log axis cannot place draw nothing but still print
        // their label and real value.
        assert_eq!(s.matches("width=\"0.00\"").count(), 2, "{s}");
        assert!(
            s.contains(">c (0)</text>") && s.contains(">d (-5)</text>"),
            "{s}"
        );
        assert!(
            s.contains(">a (100)</text>") && s.contains(">b (0.5)</text>"),
            "{s}"
        );
    }

    #[test]
    fn ramp_fills_hist_bars_by_count() {
        let mut frame = Frame::new("h".into(), 80, 15, true);
        frame.ramp = Some(crate::color::parse_ramp("blue:red").unwrap());
        let h = crate::chart::HistData {
            lo: 0.0,
            hi: 1.0,
            counts: vec![0, 4],
            total: 4,
        };
        let s = render(&frame, &ChartData::Hist(Some(h)));
        assert!(
            s.contains("fill=\"#0000ee\"") && s.contains("fill=\"#cd0000\""),
            "{s}"
        );
    }

    #[test]
    fn ramp_fills_bar_rows_and_leaves_the_spark_polyline_alone() {
        let ramp = Some(crate::color::parse_ramp("blue:red").unwrap());
        let b = one_series(&[("a", 0.0), ("b", 4.0)]);
        let s = bars("v", &b, false, ramp, "", Labels::default());
        assert!(
            s.contains("fill=\"#0000ee\"") && s.contains("fill=\"#cd0000\""),
            "{s}"
        );
        // A sparkline is one polyline, so there is nothing to fill by value.
        let sp = spark(
            "v",
            &spark_data(&[1.0, 2.0], None),
            false,
            "",
            Labels::default(),
        );
        assert_eq!(sp.matches("<polyline").count(), 1, "{sp}");
        assert!(sp.contains("#4fc3f7"), "{sp}");
    }

    #[test]
    fn bars_group_the_series_of_a_row_with_a_legend() {
        // Two labels x two series: a rect per sub-row, and a legend naming both
        // series (the background rect is the only other one).
        let rows = [
            ("a".to_string(), vec![Some(1.0), Some(2.0)]),
            ("b".to_string(), vec![Some(3.0), None]),
        ];
        let b = bar_data(&["n", "m"], &rows, None);
        let s = bars("v", &b, false, None, "", Labels::default());
        assert_eq!(s.matches("<rect").count(), 1 /*background*/ + 4, "{s}");
        // The series palette colours the sub-rows, and both names are legended.
        assert!(s.contains("#4fc3f7") && s.contains("#ff8a65"), "{s}");
        assert!(s.contains(">n</text>") && s.contains(">m</text>"), "{s}");
        // The label prints on the first series' row, the value on each; a row
        // with no value prints neither a bar nor a number.
        assert!(
            s.contains(">a (1)</text>") && s.contains(">(2)</text>"),
            "{s}"
        );
        assert!(s.contains(">b (3)</text>"), "{s}");
        assert_eq!(s.matches("width=\"0.00\"").count(), 1, "{s}");
    }

    #[test]
    fn heat_emits_a_rect_per_non_empty_cell() {
        let h = crate::chart::HeatData {
            xname: "x".to_string(),
            yname: "y".to_string(),
            xlo: 0.0,
            xhi: 1.0,
            ylo: 0.0,
            yhi: 1.0,
            cols: 2,
            rows: 2,
            counts: vec![0, 1, 0, 4],
            total: 5,
            xaxis: XAxis::Numeric,
        };
        let ramp = crate::color::parse_ramp("blue:red").unwrap();
        let s = heat("h", &h, false, Some(ramp), "", Labels::default());
        // The background plus one rect per non-empty cell — an empty cell is
        // not drawn at all.
        assert_eq!(s.matches("<rect").count(), 1 + 2, "{s}");
        // The ramp runs from a count of 1 to the busiest cell.
        assert!(
            s.contains("fill=\"#0000ee\"") && s.contains("fill=\"#cd0000\""),
            "{s}"
        );
    }

    #[test]
    fn titles_are_xml_escaped() {
        let s = spark(
            "a & b <x>",
            &spark_data(&[1.0, 2.0], None),
            false,
            "",
            Labels::default(),
        );
        assert!(s.contains("a &amp; b &lt;x&gt;"));
    }

    #[test]
    fn the_x_caption_clears_the_footer_note() {
        // Both sit under the plot area, so they must not land on the same line.
        let s = hist(
            "h",
            0.0,
            10.0,
            &[3, 1, 4],
            false,
            None,
            "skipped 2 non-numeric",
            Labels {
                x: Some("v"),
                y: None,
            },
        );
        let y_of = |text: &str| {
            let at = s.find(text).expect(text);
            let line = s[..at].rsplit_once("<text").expect("text element").1;
            let y = line.split("y=\"").nth(1).expect("y");
            y.split('"').next().unwrap().to_string()
        };
        assert_ne!(
            y_of(">v</text>"),
            y_of(">skipped 2 non-numeric</text>"),
            "{s}"
        );
    }

    #[test]
    fn note_is_rendered_as_a_footer() {
        let s = hist(
            "h",
            0.0,
            1.0,
            &[1],
            false,
            None,
            "skipped 2 non-numeric",
            Labels::default(),
        );
        assert!(s.contains("skipped 2 non-numeric"));
        // An empty note adds no footer text element beyond title/labels.
        let bare = spark(
            "v",
            &spark_data(&[1.0, 2.0], None),
            false,
            "",
            Labels::default(),
        );
        assert!(!bare.contains("skipped"));
    }
}
