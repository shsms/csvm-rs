//! SVG rendering for the `graph` sink (`--svg`). Hand-written XML — no plotting
//! dependency, so it ships in the default build; PNG (which would need a raster
//! dependency) stays a feature-gated follow-up. Each function takes the same
//! data the terminal renderers do (see `crate::graph`) and emits a standalone
//! `<svg>` document.

use crate::color::Rgb;
use crate::field::format_num;
use crate::graph::{XAxis, series_rgb};

const W: f64 = 720.0;
const H: f64 = 440.0;
// Plot area inset for axes and labels.
const L: f64 = 64.0;
const R: f64 = 16.0;
const T: f64 = 32.0;
const B: f64 = 44.0;

/// The series colour for index `i` as an SVG hex string, from the shared
/// terminal palette ([`crate::graph::series_rgb`]) so the two can't drift.
fn series_hex(i: usize) -> String {
    let Rgb(r, g, b) = series_rgb(i);
    format!("#{r:02x}{g:02x}{b:02x}")
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

fn header(title: &str, body: &str, note: &str) -> String {
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
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{W}\" height=\"{H}\" \
viewBox=\"0 0 {W} {H}\" font-family=\"sans-serif\" font-size=\"12\">\n\
<rect width=\"{W}\" height=\"{H}\" fill=\"white\"/>\n\
<text x=\"{x}\" y=\"20\" font-size=\"15\" font-weight=\"bold\">{t}</text>\n\
{body}{footer}</svg>\n",
        x = L,
        t = esc(title),
    )
}

/// The L-shaped x/y axis lines bounding the plot area.
fn axes() -> String {
    let (x0, y0, x1, y1) = (L, T, L, T + ph());
    format!(
        "<line x1=\"{x0}\" y1=\"{y0}\" x2=\"{x1}\" y2=\"{y1}\" stroke=\"#888\"/>\n\
<line x1=\"{L}\" y1=\"{by}\" x2=\"{rx}\" y2=\"{by}\" stroke=\"#888\"/>\n",
        by = T + ph(),
        rx = L + pw(),
    )
}

/// y-axis labels at the top (hi) and bottom (lo) of the plot area.
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

/// Histogram: one filled bar per bin spanning the plot width.
pub fn hist(title: &str, lo: f64, hi: f64, counts: &[u64], note: &str) -> String {
    let n = counts.len().max(1);
    let max = counts.iter().copied().max().unwrap_or(0).max(1) as f64;
    let bw = pw() / n as f64;
    let mut body = axes();
    body.push_str(&ylabels(0.0, max));
    body.push_str(&xlabels(&XAxis::Numeric, lo, hi));
    for (i, &c) in counts.iter().enumerate() {
        let bh = c as f64 / max * ph();
        let x = L + i as f64 * bw;
        let y = T + ph() - bh;
        body.push_str(&format!(
            "<rect x=\"{x:.2}\" y=\"{y:.2}\" width=\"{w:.2}\" height=\"{bh:.2}\" \
fill=\"#4fc3f7\" stroke=\"white\"/>\n",
            w = (bw - 1.0).max(0.0),
        ));
    }
    header(title, &body, note)
}

/// Horizontal bar chart: one labelled bar per row, anchored at a zero baseline.
pub fn bars(title: &str, rows: &[(String, f64)], note: &str) -> String {
    if rows.is_empty() {
        return header(title, "", note);
    }
    let lo = rows.iter().map(|(_, v)| *v).fold(0.0_f64, f64::min);
    let hi = rows.iter().map(|(_, v)| *v).fold(0.0_f64, f64::max);
    let span = (hi - lo).max(f64::MIN_POSITIVE);
    let n = rows.len();
    let rh = ph() / n as f64;
    let zero = L + (0.0 - lo) / span * pw();
    let mut body = axes();
    for (i, (label, v)) in rows.iter().enumerate() {
        let vx = L + (*v - lo) / span * pw();
        let (x, w) = (zero.min(vx), (vx - zero).abs());
        let y = T + i as f64 * rh;
        body.push_str(&format!(
            "<rect x=\"{x:.2}\" y=\"{y:.2}\" width=\"{w:.2}\" height=\"{h:.2}\" fill=\"#4fc3f7\"/>\n\
<text x=\"{lx}\" y=\"{ly:.2}\">{label} ({val})</text>\n",
            h = (rh - 2.0).max(0.0),
            lx = L + 4.0,
            ly = y + rh / 2.0 + 4.0,
            label = esc(label),
            val = format_num(*v),
        ));
    }
    header(title, &body, note)
}

/// Sparkline as a single polyline across the plot width.
pub fn spark(title: &str, values: &[f64], note: &str) -> String {
    if values.is_empty() {
        return header(title, "", note);
    }
    let (lo, hi) = crate::graph::minmax(values).unwrap_or((0.0, 0.0));
    let span = (hi - lo).max(f64::MIN_POSITIVE);
    let n = values.len().max(2);
    let pts: Vec<String> = values
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = L + i as f64 / (n - 1) as f64 * pw();
            let y = T + ph() - (v - lo) / span * ph();
            format!("{x:.2},{y:.2}")
        })
        .collect();
    let mut body = ylabels(lo, hi);
    body.push_str(&format!(
        "<polyline points=\"{}\" fill=\"none\" stroke=\"#4fc3f7\" stroke-width=\"1.5\"/>\n",
        pts.join(" ")
    ));
    header(title, &body, note)
}

/// Scatter (`connect=false`) or line (`connect=true`) of one or more y-series
/// against a shared x. Series get distinct colours and a legend.
pub fn xy(
    title: &str,
    names: &[String],
    series: &[Vec<(f64, f64)>],
    connect: bool,
    note: &str,
    xaxis: XAxis,
) -> String {
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
    if !xlo.is_finite() {
        return header(title, "", note);
    }
    let xspan = (xhi - xlo).max(f64::MIN_POSITIVE);
    let yspan = (yhi - ylo).max(f64::MIN_POSITIVE);
    let map = |x: f64, y: f64| {
        let px = L + (x - xlo) / xspan * pw();
        let py = T + ph() - (y - ylo) / yspan * ph();
        (px, py)
    };

    let mut body = axes();
    body.push_str(&ylabels(ylo, yhi));
    body.push_str(&xlabels(&xaxis, xlo, xhi));
    for (si, pts) in series.iter().enumerate() {
        let color = series_hex(si);
        if connect {
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
            for &(x, y) in pts {
                let (px, py) = map(x, y);
                body.push_str(&format!(
                    "<circle cx=\"{px:.2}\" cy=\"{py:.2}\" r=\"2\" fill=\"{color}\"/>\n"
                ));
            }
        }
    }
    if series.len() > 1 {
        for (i, name) in names.iter().enumerate() {
            let color = series_hex(i);
            let y = T + 4.0 + i as f64 * 16.0;
            body.push_str(&format!(
                "<rect x=\"{lx}\" y=\"{ry}\" width=\"10\" height=\"10\" fill=\"{color}\"/>\n\
<text x=\"{tx}\" y=\"{ty}\">{n}</text>\n",
                lx = L + pw() - 90.0,
                ry = y,
                tx = L + pw() - 76.0,
                ty = y + 9.0,
                n = esc(name),
            ));
        }
    }
    header(title, &body, note)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hist_emits_a_rect_per_bin() {
        let s = hist("h", 0.0, 10.0, &[3, 1, 4], "");
        assert!(s.starts_with("<svg"));
        assert!(s.trim_end().ends_with("</svg>"));
        assert_eq!(s.matches("<rect").count(), 1 /*background*/ + 3);
    }

    #[test]
    fn xy_scatter_emits_circles_and_line_emits_polyline() {
        let series = vec![vec![(0.0, 0.0), (1.0, 1.0)]];
        let names = ["y".to_string()];
        assert!(xy("s", &names, &series, false, "", XAxis::Numeric).contains("<circle"));
        assert!(xy("s", &names, &series, true, "", XAxis::Numeric).contains("<polyline"));
    }

    #[test]
    fn xy_multi_series_uses_distinct_colours_and_a_legend() {
        let series = vec![vec![(0.0, 0.0)], vec![(1.0, 1.0)]];
        let names = ["a".to_string(), "b".to_string()];
        let s = xy("m", &names, &series, false, "", XAxis::Numeric);
        assert!(s.contains("#4fc3f7") && s.contains("#ff8a65"));
        assert!(s.contains(">a</text>") && s.contains(">b</text>"));
    }

    #[test]
    fn xy_category_axis_shows_end_labels() {
        let series = vec![vec![(1.0, 0.0), (2.0, 1.0)]];
        let names = ["y".to_string()];
        let ends = XAxis::Ends("t0".to_string(), "t9".to_string());
        let s = xy("y vs t", &names, &series, true, "", ends);
        assert!(s.contains(">t0</text>") && s.contains(">t9</text>"), "{s}");
    }

    #[test]
    fn titles_are_xml_escaped() {
        let s = spark("a & b <x>", &[1.0, 2.0], "");
        assert!(s.contains("a &amp; b &lt;x&gt;"));
    }

    #[test]
    fn note_is_rendered_as_a_footer() {
        let s = hist("h", 0.0, 1.0, &[1], "skipped 2 non-numeric");
        assert!(s.contains("skipped 2 non-numeric"));
        // An empty note adds no footer text element beyond title/labels.
        let bare = spark("v", &[1.0, 2.0], "");
        assert!(!bare.contains("skipped"));
    }
}
