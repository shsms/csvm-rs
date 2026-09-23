//! Colour vocabulary and rendering for the `color` command (and `fmt`).
//!
//! [`Style`] is a foreground/background/attribute set parsed from a spec like
//! `bold+bg:red`; [`Ramp`] is a two-colour gradient (`green:red`) that maps a
//! value within a range to an interpolated colour. Both render to ANSI SGR
//! escapes at a [`Depth`]: 24-bit where the terminal says it has it, the
//! 256-colour palette otherwise. This module is presentation-only — *what* to
//! colour is the caller's decision (see `plan::ColorRule`).

use std::fmt;

/// An RGB triple. Named colours are kept as RGB so gradients can interpolate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// One of the terminal's base colours: the eight ANSI colours and grey (bright
/// black). It is painted with its own SGR code, so it shows in the shade the
/// terminal's theme gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    Gray,
}

impl Base {
    /// Every base colour, in the order of the terminal's colours 0 to 8.
    pub(crate) const ALL: [Base; 9] = [
        Base::Black,
        Base::Red,
        Base::Green,
        Base::Yellow,
        Base::Blue,
        Base::Magenta,
        Base::Cyan,
        Base::White,
        Base::Gray,
    ];

    /// The base colour `name` names (`grey` spells `gray` too).
    pub fn named(name: &str) -> Option<Base> {
        let name = if name == "grey" { "gray" } else { name };
        Base::ALL.into_iter().find(|base| base.name() == name)
    }

    /// Its name in a colour spec.
    pub fn name(self) -> &'static str {
        match self {
            Base::Black => "black",
            Base::Red => "red",
            Base::Green => "green",
            Base::Yellow => "yellow",
            Base::Blue => "blue",
            Base::Magenta => "magenta",
            Base::Cyan => "cyan",
            Base::White => "white",
            Base::Gray => "gray",
        }
    }

    /// The RGB a gradient between base colours goes through: xterm's shade,
    /// a stand-in for arithmetic, since the terminal's own is not known.
    pub fn rgb(self) -> Rgb {
        match self {
            Base::Black => Rgb(0, 0, 0),
            Base::Red => Rgb(205, 0, 0),
            Base::Green => Rgb(0, 205, 0),
            Base::Yellow => Rgb(205, 205, 0),
            Base::Blue => Rgb(0, 0, 238),
            Base::Magenta => Rgb(205, 0, 205),
            Base::Cyan => Rgb(0, 205, 205),
            Base::White => Rgb(229, 229, 229),
            Base::Gray => Rgb(127, 127, 127),
        }
    }

    /// Its SGR code less the foreground's 30 or the background's 40: grey is
    /// bright black, whose codes are 90 and 100.
    fn code(self) -> u8 {
        match self {
            Base::Black => 0,
            Base::Red => 1,
            Base::Green => 2,
            Base::Yellow => 3,
            Base::Blue => 4,
            Base::Magenta => 5,
            Base::Cyan => 6,
            Base::White => 7,
            Base::Gray => 60,
        }
    }
}

/// A base colour's name as RGB, for a gradient's ends.
fn named(name: &str) -> Option<Rgb> {
    Base::named(name).map(Base::rgb)
}

/// `c` as a `#rrggbb` literal — the hex form SVG and CSS want.
pub fn rgb_hex(c: &Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

/// The base-colour name for `c`, or a `#rrggbb` literal when it isn't one of the
/// named colours (e.g. a value interpolated along a ramp). Inverse of [`named`];
/// used for the terse `--explain` rendering.
fn rgb_name(c: Rgb) -> String {
    match Base::ALL.into_iter().find(|base| base.rgb() == c) {
        Some(base) => base.name().into(),
        None => rgb_hex(&c),
    }
}

/// A background a little off `bg`, for every other row of a table: 18 steps
/// a channel lighter on a dark background, darker on a light one. That is
/// enough for the 256-colour palette to tell a grey or near-grey background
/// from its stripe; a strongly coloured one may map both to one entry.
pub fn stripe(bg: Rgb) -> Rgb {
    let luma = 0.2126 * f64::from(bg.0) + 0.7152 * f64::from(bg.1) + 0.0722 * f64::from(bg.2);
    let shift = |c: u8| {
        if luma < 128.0 {
            c.saturating_add(18)
        } else {
            c.saturating_sub(18)
        }
    };
    Rgb(shift(bg.0), shift(bg.1), shift(bg.2))
}

/// How many colours the terminal can show, which decides how an RGB colour is
/// written: exactly, or as the nearest entry of the 256-colour palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    /// The xterm 256-colour palette (`38;5;N`), which nearly every terminal
    /// has.
    Ansi256,
    /// 24-bit colour (`38;2;R;G;B`).
    Truecolor,
}

impl Depth {
    /// The depth `$COLORTERM` announces: `truecolor` or `24bit` is 24-bit
    /// colour, anything else (or nothing) the 256-colour palette. Terminals
    /// with 24-bit colour set the variable, and one without it shows 24-bit
    /// escapes wrongly or not at all, so the palette is the safe default.
    pub fn from_colorterm(colorterm: Option<&str>) -> Depth {
        match colorterm {
            Some(v) if v.eq_ignore_ascii_case("truecolor") || v.eq_ignore_ascii_case("24bit") => {
                Depth::Truecolor
            }
            _ => Depth::Ansi256,
        }
    }
}

/// A colour as the terminal is asked for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    /// One of the terminal's base colours, drawn in its theme's shade.
    Base(Base),
    /// An exact colour: a point along a ramp, or a chart's series colour.
    Rgb(Rgb),
}

impl Color {
    /// The SGR parameters for this colour, as a foreground (`bg` false) or a
    /// background, at `depth`.
    fn sgr(self, bg: bool, depth: Depth) -> String {
        match self {
            Color::Base(base) => (base.code() + if bg { 40 } else { 30 }).to_string(),
            Color::Rgb(c) => {
                let lead = if bg { 48 } else { 38 };
                match depth {
                    Depth::Truecolor => format!("{lead};2;{};{};{}", c.0, c.1, c.2),
                    Depth::Ansi256 => format!("{lead};5;{}", ansi256(c)),
                }
            }
        }
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Color::Base(base) => write!(f, "{}", base.name()),
            Color::Rgb(c) => write!(f, "{}", rgb_name(*c)),
        }
    }
}

/// The 256-colour palette entry nearest to `c`: the closer of the 6×6×6 colour
/// cube (entries 16–231) and the 24-step grey ramp (232–255). The 16 entries
/// below the cube are left out; terminals theme them, so what they show is not
/// known.
fn ansi256(c: Rgb) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let dist = |a: Rgb, b: Rgb| {
        let d = |x: u8, y: u8| (i32::from(x) - i32::from(y)).pow(2);
        d(a.0, b.0) + d(a.1, b.1) + d(a.2, b.2)
    };
    let level = |v: u8| {
        (0..LEVELS.len())
            .min_by_key(|&i| (i32::from(LEVELS[i]) - i32::from(v)).abs())
            .unwrap()
    };
    let (r, g, b) = (level(c.0), level(c.1), level(c.2));
    let cube = Rgb(LEVELS[r], LEVELS[g], LEVELS[b]);
    // Grey step i is the level 8 + 10i.
    let mean = (u32::from(c.0) + u32::from(c.1) + u32::from(c.2)) / 3;
    let step = ((mean.saturating_sub(3)) / 10).min(23) as u8;
    let grey = 8 + 10 * step;
    if dist(c, Rgb(grey, grey, grey)) < dist(c, cube) {
        232 + step
    } else {
        16 + 36 * r as u8 + 6 * g as u8 + b as u8
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

    /// Wrap `text` in SGR escapes for this style at `depth` (returns it
    /// unchanged when the style is empty, so no stray resets are emitted).
    pub fn paint(&self, text: &str, depth: Depth) -> String {
        match self.start(depth) {
            Some(start) => format!("{start}{text}\x1b[0m"),
            None => text.to_string(),
        }
    }

    /// The SGR escape that turns this style on at `depth` (`\x1b[0m` turns it
    /// off), or `None` when the style is empty.
    pub fn start(&self, depth: Depth) -> Option<String> {
        if self.is_empty() {
            return None;
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
            codes.push(fg.sgr(false, depth));
        }
        if let Some(bg) = self.bg {
            codes.push(bg.sgr(true, depth));
        }
        Some(format!("\x1b[{}m", codes.join(";")))
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
        Base::named(name)
            .map(Color::Base)
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
        Base::named(name).map(Color::Base)
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
        let painted = s.paint("hi", Depth::Truecolor);
        assert!(painted.starts_with("\x1b["));
        assert!(painted.ends_with("\x1b[0m"));
        assert!(painted.contains("hi"));
        // An empty style adds no escapes.
        assert_eq!(Style::default().paint("x", Depth::Truecolor), "x");
    }

    #[test]
    fn base_colours_use_the_terminal_codes_at_any_depth() {
        for depth in [Depth::Truecolor, Depth::Ansi256] {
            let s = parse_style("red+bg:blue").unwrap();
            assert_eq!(s.paint("x", depth), "\x1b[31;44mx\x1b[0m");
            // Grey is bright black, from the 90/100 range.
            let s = parse_style("grey+bg:gray").unwrap();
            assert_eq!(s.paint("x", depth), "\x1b[90;100mx\x1b[0m");
        }
    }

    #[test]
    fn exact_colours_follow_the_depth() {
        let s = Style {
            fg: Some(Color::Rgb(Rgb(255, 0, 0))),
            bg: Some(Color::Rgb(Rgb(0, 0, 0))),
            ..Style::default()
        };
        assert_eq!(
            s.paint("x", Depth::Truecolor),
            "\x1b[38;2;255;0;0;48;2;0;0;0mx\x1b[0m"
        );
        assert_eq!(
            s.paint("x", Depth::Ansi256),
            "\x1b[38;5;196;48;5;16mx\x1b[0m"
        );
    }

    #[test]
    fn a_stripe_is_a_little_off_the_background() {
        assert_eq!(stripe(Rgb(0, 0, 0)), Rgb(18, 18, 18));
        assert_eq!(stripe(Rgb(255, 255, 255)), Rgb(237, 237, 237));
        assert_eq!(stripe(Rgb(0, 43, 54)), Rgb(18, 61, 72)); // a dark blue
        // The 256-colour palette still shows it as a different entry.
        for bg in [
            Rgb(0, 0, 0),
            Rgb(30, 30, 30),
            Rgb(40, 40, 40),
            Rgb(255, 255, 255),
        ] {
            assert_ne!(ansi256(stripe(bg)), ansi256(bg), "{bg:?}");
        }
    }

    #[test]
    fn ansi256_picks_the_nearest_cube_or_grey_entry() {
        assert_eq!(ansi256(Rgb(0, 0, 0)), 16);
        assert_eq!(ansi256(Rgb(255, 255, 255)), 231);
        assert_eq!(ansi256(Rgb(255, 0, 0)), 196);
        assert_eq!(ansi256(Rgb(0, 205, 0)), 16 + 6 * 4); // green, level 215
        // A mid grey sits on the grey ramp, not the cube's coarse 95/135 steps.
        assert_eq!(ansi256(Rgb(128, 128, 128)), 232 + 12);
        assert_eq!(ansi256(Rgb(8, 8, 8)), 232);
        assert_eq!(ansi256(Rgb(238, 238, 238)), 255);
        // A colour off the grey axis stays in the cube.
        assert_eq!(ansi256(Rgb(0x4f, 0xc3, 0xf7)), 16 + 36 + 6 * 3 + 5);
    }

    #[test]
    fn colorterm_names_the_depth() {
        assert_eq!(Depth::from_colorterm(Some("truecolor")), Depth::Truecolor);
        assert_eq!(Depth::from_colorterm(Some("24bit")), Depth::Truecolor);
        assert_eq!(Depth::from_colorterm(Some("TrueColor")), Depth::Truecolor);
        assert_eq!(Depth::from_colorterm(Some("")), Depth::Ansi256);
        assert_eq!(Depth::from_colorterm(Some("yes")), Depth::Ansi256);
        assert_eq!(Depth::from_colorterm(None), Depth::Ansi256);
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
