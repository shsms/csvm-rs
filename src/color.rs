//! Colour vocabulary and rendering for the `color` command (and `fmt`).
//!
//! [`Style`] is a foreground/background/attribute set parsed from a spec like
//! `bold+bg:red`; [`Ramp`] is a two-colour gradient (`green:red`) that maps a
//! value within a range to an interpolated colour. Both render to ANSI SGR
//! escapes. This module is presentation-only — *what* to colour is the caller's
//! decision (see `plan::ColorRule`).

use std::fmt;

/// An RGB triple. Named colours are kept as RGB so gradients can interpolate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// The terminal's base colours: the 8 ANSI colours and grey (bright black),
/// each with the RGB a gradient between them interpolates through. A base
/// colour is painted with its own SGR code, so it shows in the shade the
/// terminal's theme gives it; the RGB is only the stand-in for arithmetic.
const BASE: [(&str, Rgb); 9] = [
    ("black", Rgb(0, 0, 0)),
    ("red", Rgb(205, 0, 0)),
    ("green", Rgb(0, 205, 0)),
    ("yellow", Rgb(205, 205, 0)),
    ("blue", Rgb(0, 0, 238)),
    ("magenta", Rgb(205, 0, 205)),
    ("cyan", Rgb(0, 205, 205)),
    ("white", Rgb(229, 229, 229)),
    ("gray", Rgb(127, 127, 127)),
];

/// The index into [`BASE`] for a colour name (`grey` spells `gray` too).
fn base_index(name: &str) -> Option<usize> {
    let name = if name == "grey" { "gray" } else { name };
    BASE.iter().position(|(n, _)| *n == name)
}

/// A base colour's name as RGB, for a gradient's ends.
fn named(name: &str) -> Option<Rgb> {
    base_index(name).map(|i| BASE[i].1)
}

/// `c` as a `#rrggbb` literal — the hex form SVG and CSS want.
pub fn rgb_hex(c: &Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

/// The base-colour name for `c`, or a `#rrggbb` literal when it isn't one of the
/// named colours (e.g. a value interpolated along a ramp). Inverse of [`named`];
/// used for the terse `--explain` rendering.
fn rgb_name(c: Rgb) -> String {
    match BASE.iter().find(|(_, rgb)| *rgb == c) {
        Some((name, _)) => (*name).into(),
        None => rgb_hex(&c),
    }
}

/// A colour as the terminal is asked for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    /// One of the terminal's base colours, by its index into the table of
    /// them (black, red, …, gray), drawn in the shade the terminal's theme
    /// gives it.
    Base(u8),
    /// An exact colour: a point along a ramp, or a chart's series colour.
    Rgb(Rgb),
}

impl Color {
    /// The SGR parameters for this colour, as a foreground (`bg` false) or a
    /// background.
    fn sgr(self, bg: bool) -> String {
        match self {
            // Grey is bright black, which has its own code range (90/100).
            Color::Base(8) => (if bg { "100" } else { "90" }).into(),
            Color::Base(i) => format!("{}", u32::from(i) + if bg { 40 } else { 30 }),
            Color::Rgb(c) => {
                let lead = if bg { 48 } else { 38 };
                format!("{lead};2;{};{};{}", c.0, c.1, c.2)
            }
        }
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Color::Base(i) => write!(f, "{}", BASE[usize::from(*i)].0),
            Color::Rgb(c) => write!(f, "{}", rgb_name(*c)),
        }
    }
}

/// A foreground/background/attribute set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub underline: bool,
}

impl Style {
    pub fn is_empty(&self) -> bool {
        self.fg.is_none() && self.bg.is_none() && !self.bold && !self.dim && !self.underline
    }

    /// Layer `other` on top of `self`: a colour `other` sets wins, attributes
    /// accumulate. This is the rule-stacking semantics (last wins per attribute).
    pub fn over(self, other: Style) -> Style {
        Style {
            fg: other.fg.or(self.fg),
            bg: other.bg.or(self.bg),
            bold: self.bold || other.bold,
            dim: self.dim || other.dim,
            underline: self.underline || other.underline,
        }
    }

    /// Wrap `text` in SGR escapes for this style (returns it unchanged when the
    /// style is empty, so no stray resets are emitted).
    pub fn paint(&self, text: &str) -> String {
        if self.is_empty() {
            return text.to_string();
        }
        let mut codes: Vec<String> = Vec::new();
        if self.bold {
            codes.push("1".into());
        }
        if self.dim {
            codes.push("2".into());
        }
        if self.underline {
            codes.push("4".into());
        }
        if let Some(fg) = self.fg {
            codes.push(fg.sgr(false));
        }
        if let Some(bg) = self.bg {
            codes.push(bg.sgr(true));
        }
        format!("\x1b[{}m{text}\x1b[0m", codes.join(";"))
    }
}

impl fmt::Display for Style {
    /// The `+`-joined spec form (the inverse of [`parse_style`], modulo part
    /// order), e.g. `bold+red` or `white+bg:red`; `default` when empty.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return write!(f, "default");
        }
        let mut parts: Vec<String> = Vec::new();
        if self.bold {
            parts.push("bold".into());
        }
        if self.dim {
            parts.push("dim".into());
        }
        if self.underline {
            parts.push("underline".into());
        }
        if let Some(fg) = self.fg {
            parts.push(fg.to_string());
        }
        if let Some(bg) = self.bg {
            parts.push(format!("bg:{bg}"));
        }
        write!(f, "{}", parts.join("+"))
    }
}

/// Parse a colour spec: `+`-separated parts, each an attribute (`bold`/`dim`/
/// `underline`), a `bg:NAME` background, or a `NAME` foreground. A name is a
/// base colour, so it paints in the terminal theme's shade of it.
pub fn parse_style(spec: &str) -> Result<Style, String> {
    let base = |name: &str| {
        base_index(name)
            .map(|i| Color::Base(i as u8))
            .ok_or_else(|| format!("unknown colour '{name}'"))
    };
    let mut style = Style::default();
    for part in spec.split('+') {
        match part {
            "bold" => style.bold = true,
            "dim" => style.dim = true,
            "underline" => style.underline = true,
            _ if part.is_empty() => return Err(format!("empty colour part in '{spec}'")),
            _ => {
                if let Some(name) = part.strip_prefix("bg:") {
                    style.bg = Some(base(name)?);
                } else {
                    style.fg = Some(base(part)?);
                }
            }
        }
    }
    Ok(style)
}

/// A two-colour gradient between `lo` and `hi`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ramp {
    pub lo: Rgb,
    pub hi: Rgb,
}

impl Default for Ramp {
    /// Green at the low end, red at the high end.
    fn default() -> Self {
        Ramp {
            lo: named("green").unwrap(),
            hi: named("red").unwrap(),
        }
    }
}

impl fmt::Display for Ramp {
    /// The `lo:hi` spec form (the inverse of [`parse_ramp`]).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", rgb_name(self.lo), rgb_name(self.hi))
    }
}

/// Parse a ramp spec `locolour:hicolour`, e.g. `green:red`.
pub fn parse_ramp(spec: &str) -> Result<Ramp, String> {
    let (lo, hi) = spec
        .split_once(':')
        .ok_or_else(|| format!("ramp must be 'lo:hi', got '{spec}'"))?;
    Ok(Ramp {
        lo: named(lo).ok_or_else(|| format!("unknown colour '{lo}'"))?,
        hi: named(hi).ok_or_else(|| format!("unknown colour '{hi}'"))?,
    })
}

impl Ramp {
    /// The foreground style for value `v` mapped onto the ramp between bounds
    /// `lo` and `hi`: `lo` takes the ramp's low colour, `hi` its high colour, and
    /// values outside `[lo, hi]` clamp to the endpoints. `lo > hi` inverts the
    /// gradient (the larger value gets the low colour); `lo == hi` is degenerate
    /// and yields the low colour.
    pub fn at(&self, v: f64, lo: f64, hi: f64) -> Style {
        Style {
            fg: Some(Color::Rgb(self.rgb_at(v, lo, hi))),
            ..Default::default()
        }
    }

    /// The colour [`Ramp::at`] paints `v` with, as RGB. Every point of a ramp
    /// is an exact colour, its ends included, so a gradient runs smoothly
    /// whatever the terminal's theme makes of the base colours it is named by.
    pub fn rgb_at(&self, v: f64, lo: f64, hi: f64) -> Rgb {
        let t = if hi == lo {
            0.0
        } else {
            ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
        };
        let lerp = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8;
        Rgb(
            lerp(self.lo.0, self.hi.0),
            lerp(self.lo.1, self.hi.1),
            lerp(self.lo.2, self.hi.2),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A base colour by name, as a parsed spec holds it.
    fn base(name: &str) -> Option<Color> {
        base_index(name).map(|i| Color::Base(i as u8))
    }

    /// A base colour's stand-in RGB, as a ramp paints it.
    fn rgb(name: &str) -> Option<Color> {
        named(name).map(Color::Rgb)
    }

    #[test]
    fn parse_and_paint() {
        let s = parse_style("bold+red").unwrap();
        assert!(s.bold);
        assert_eq!(s.fg, base("red"));
        let painted = s.paint("hi");
        assert!(painted.starts_with("\x1b["));
        assert!(painted.ends_with("\x1b[0m"));
        assert!(painted.contains("hi"));
        // An empty style adds no escapes.
        assert_eq!(Style::default().paint("x"), "x");
    }

    #[test]
    fn base_colours_use_the_terminal_codes() {
        let s = parse_style("red+bg:blue").unwrap();
        assert_eq!(s.paint("x"), "\x1b[31;44mx\x1b[0m");
        // Grey is bright black, from the 90/100 range.
        let s = parse_style("grey+bg:gray").unwrap();
        assert_eq!(s.paint("x"), "\x1b[90;100mx\x1b[0m");
    }

    #[test]
    fn exact_colours_are_24_bit() {
        let s = Style {
            fg: Some(Color::Rgb(Rgb(255, 0, 0))),
            bg: Some(Color::Rgb(Rgb(0, 0, 0))),
            ..Style::default()
        };
        assert_eq!(s.paint("x"), "\x1b[38;2;255;0;0;48;2;0;0;0mx\x1b[0m");
    }

    #[test]
    fn background_and_unknown() {
        assert_eq!(parse_style("bg:blue").unwrap().bg, base("blue"));
        assert!(parse_style("chartreuse").is_err());
        assert!(parse_style("bg:nope").is_err());
        assert!(parse_style("bold+").is_err());
    }

    #[test]
    fn ramp_endpoints_and_clamp() {
        let r = parse_ramp("green:red").unwrap();
        assert_eq!(r.at(0.0, 0.0, 10.0).fg, rgb("green"));
        assert_eq!(r.at(10.0, 0.0, 10.0).fg, rgb("red"));
        assert_eq!(r.at(-5.0, 0.0, 10.0).fg, rgb("green")); // below clamps to lo
        assert_eq!(r.at(99.0, 0.0, 10.0).fg, rgb("red")); // above clamps to hi
        assert_eq!(r.at(5.0, 3.0, 3.0).fg, rgb("green")); // degenerate range
        // Reversed bounds (lo > hi) invert: the larger value takes the lo colour.
        assert_eq!(r.at(10.0, 10.0, 0.0).fg, rgb("green"));
        assert_eq!(r.at(0.0, 10.0, 0.0).fg, rgb("red"));
        assert_eq!(r.at(99.0, 10.0, 0.0).fg, rgb("green")); // above lo clamps to lo
        assert!(parse_ramp("green").is_err());
        assert_eq!(Ramp::default(), parse_ramp("green:red").unwrap()); // default ramp
    }

    #[test]
    fn display_round_trips_specs() {
        for spec in ["red", "bold+red", "white+bg:red", "bg:blue", "gray"] {
            assert_eq!(parse_style(spec).unwrap().to_string(), spec);
        }
        assert_eq!(parse_style("grey").unwrap().to_string(), "gray");
        assert_eq!(Style::default().to_string(), "default");
        assert_eq!(Ramp::default().to_string(), "green:red");
        assert_eq!(
            parse_ramp("blue:yellow").unwrap().to_string(),
            "blue:yellow"
        );
        // A non-named colour (e.g. a ramp-interpolated value) shows as hex.
        assert_eq!(rgb_name(Rgb(0x12, 0x34, 0x56)), "#123456");
    }

    #[test]
    fn layering_last_wins_per_attribute() {
        let red = parse_style("red").unwrap();
        let bg = parse_style("bg:white").unwrap();
        let both = red.over(bg);
        assert_eq!(both.fg, base("red"));
        assert_eq!(both.bg, base("white"));
    }
}
