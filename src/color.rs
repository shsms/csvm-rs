//! Colour vocabulary and rendering for the `color` command (and `fmt`).
//!
//! [`Style`] is a foreground/background/attribute set parsed from a spec like
//! `bold+bg:red`; [`Ramp`] is a two-colour gradient (`green:red`) that maps a
//! value within a range to an interpolated colour. Both render to ANSI SGR
//! escapes. This module is presentation-only — *what* to colour is the caller's
//! decision (see `plan::ColorRule`).

/// An RGB triple. Named colours are kept as RGB so gradients can interpolate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// The 8 ANSI base colours (plus grey), as approximate RGB.
fn named(name: &str) -> Option<Rgb> {
    Some(match name {
        "black" => Rgb(0, 0, 0),
        "red" => Rgb(205, 0, 0),
        "green" => Rgb(0, 205, 0),
        "yellow" => Rgb(205, 205, 0),
        "blue" => Rgb(0, 0, 238),
        "magenta" => Rgb(205, 0, 205),
        "cyan" => Rgb(0, 205, 205),
        "white" => Rgb(229, 229, 229),
        "gray" | "grey" => Rgb(127, 127, 127),
        _ => return None,
    })
}

/// A foreground/background/attribute set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
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
        if let Some(Rgb(r, g, b)) = self.fg {
            codes.push(format!("38;2;{r};{g};{b}"));
        }
        if let Some(Rgb(r, g, b)) = self.bg {
            codes.push(format!("48;2;{r};{g};{b}"));
        }
        format!("\x1b[{}m{text}\x1b[0m", codes.join(";"))
    }
}

/// Parse a colour spec: `+`-separated parts, each an attribute (`bold`/`dim`/
/// `underline`), a `bg:NAME` background, or a `NAME` foreground.
pub fn parse_style(spec: &str) -> Result<Style, String> {
    let mut style = Style::default();
    for part in spec.split('+') {
        match part {
            "bold" => style.bold = true,
            "dim" => style.dim = true,
            "underline" => style.underline = true,
            _ if part.is_empty() => return Err(format!("empty colour part in '{spec}'")),
            _ => {
                if let Some(name) = part.strip_prefix("bg:") {
                    style.bg = Some(named(name).ok_or_else(|| format!("unknown colour '{name}'"))?);
                } else {
                    style.fg = Some(named(part).ok_or_else(|| format!("unknown colour '{part}'"))?);
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
    /// The foreground style for `v` placed in `[lo, hi]` (clamped, so values
    /// outside the range take the solid endpoint colours).
    pub fn at(&self, v: f64, lo: f64, hi: f64) -> Style {
        let t = if hi > lo {
            ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let lerp = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8;
        Style {
            fg: Some(Rgb(
                lerp(self.lo.0, self.hi.0),
                lerp(self.lo.1, self.hi.1),
                lerp(self.lo.2, self.hi.2),
            )),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_paint() {
        let s = parse_style("bold+red").unwrap();
        assert!(s.bold);
        assert_eq!(s.fg, named("red"));
        let painted = s.paint("hi");
        assert!(painted.starts_with("\x1b["));
        assert!(painted.ends_with("\x1b[0m"));
        assert!(painted.contains("hi"));
        // An empty style adds no escapes.
        assert_eq!(Style::default().paint("x"), "x");
    }

    #[test]
    fn background_and_unknown() {
        assert_eq!(parse_style("bg:blue").unwrap().bg, named("blue"));
        assert!(parse_style("chartreuse").is_err());
        assert!(parse_style("bg:nope").is_err());
        assert!(parse_style("bold+").is_err());
    }

    #[test]
    fn ramp_endpoints_and_clamp() {
        let r = parse_ramp("green:red").unwrap();
        assert_eq!(r.at(0.0, 0.0, 10.0).fg, named("green"));
        assert_eq!(r.at(10.0, 0.0, 10.0).fg, named("red"));
        assert_eq!(r.at(-5.0, 0.0, 10.0).fg, named("green")); // below clamps to lo
        assert_eq!(r.at(99.0, 0.0, 10.0).fg, named("red")); // above clamps to hi
        assert_eq!(r.at(5.0, 3.0, 3.0).fg, named("green")); // degenerate range
        assert!(parse_ramp("green").is_err());
    }

    #[test]
    fn layering_last_wins_per_attribute() {
        let red = parse_style("red").unwrap();
        let bg = parse_style("bg:white").unwrap();
        let both = red.over(bg);
        assert_eq!(both.fg, named("red"));
        assert_eq!(both.bg, named("white"));
    }
}
