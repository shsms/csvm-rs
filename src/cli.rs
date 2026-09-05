//! Command-line argument parsing.
//!
//! The script is the first positional and the input file an optional second
//! positional (`csvm SCRIPT [INPUT]`, like `awk 'prog' file`); input defaults to
//! stdin, and a bare `-` is also stdin. At most one input is accepted. Options:
//! `-o`/`--output` (default stdout), `-n`/`--threads`, `-f`/`--file` (read the
//! script from a file), `-t`/`--temp-dir`, `--chunk-size`, `--sort-buffer`,
//! `--header`, `--color`, `--format` (csv | parquet), and `--explain`.
//! Long options take their value as `--flag VALUE` or `--flag=VALUE`. See the
//! help registry for the full help.

use std::path::PathBuf;

/// Parsed command-line arguments.
#[derive(Debug, Clone)]
pub struct Args {
    pub script: String,
    /// When set (`-f`), the pipeline is read from this file and `script` is
    /// empty; the resolution (file I/O) happens in `main`.
    pub script_file: Option<String>,
    pub in_file: Option<String>,
    pub out_file: Option<String>,
    pub threads: usize,
    pub temp_dir: Option<PathBuf>,
    pub chunk_size: usize,
    pub sort_buffer: usize,
    pub explain: bool,
    pub color: ColorWhen,
    /// `--header`: the input has no header line; this names its columns.
    pub header: Option<Header>,
    /// `--format`: input format override. `None` auto-detects from the file
    /// extension (`.parquet` ⇒ Parquet, else CSV).
    pub format: Option<InputFormat>,
}

/// `--header`: how a headerless input's columns are named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Header {
    /// These names, in order.
    Named(Vec<String>),
    /// `c1, c2, …`, one per column of the first line.
    Auto,
}

impl Header {
    /// Parse the flag's value: `-` or empty auto-names; else a comma list,
    /// read like a header line (so a quoted name may contain a comma).
    fn parse(value: &str) -> Result<Header, String> {
        let value = value.trim();
        if value.is_empty() || value == "-" {
            return Ok(Header::Auto);
        }
        let names: Vec<String> = crate::csv::parse_header(value)
            .into_iter()
            .map(|n| n.trim().to_string())
            .collect();
        if names.iter().any(String::is_empty) {
            return Err(format!("--header has an empty column name in '{value}'"));
        }
        Ok(Header::Named(names))
    }
}

/// The input format, set by `--format` or auto-detected from the extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    Csv,
    /// Read `.parquet` (only with the `parquet` build feature).
    Parquet,
}

/// When to emit ANSI colour for `color` rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorWhen {
    /// Colour only when stdout is a terminal.
    #[default]
    Auto,
    Always,
    Never,
}

const DEFAULT_CHUNK_SIZE: usize = 1_000_000;
/// In-memory budget before `sort` spills a run to a temp file.
pub const DEFAULT_SORT_BUFFER: usize = 256 << 20;

/// Most worker threads a run gets, whether `-n` asked for more or the
/// machine has more cores.
const MAX_THREADS: usize = 1024;

/// The worker count for `-n`: the request when positive (capped at
/// [`MAX_THREADS`]), else 1; `cores` (the machine's core count) when no `-n`
/// was given.
fn resolve_threads(requested: Option<i64>, cores: usize) -> usize {
    match requested {
        Some(n) if n >= 1 => usize::try_from(n).map_or(MAX_THREADS, |n| n.min(MAX_THREADS)),
        Some(_) => 1,
        None => cores.clamp(1, MAX_THREADS),
    }
}

/// Parse a byte size: a plain integer, or one with a binary `K`/`M`/`G` suffix
/// (case-insensitive, powers of 1024 — matching the `256 MiB` default). Returns
/// `i64` so a non-positive value can fall back to the default at the call site.
fn parse_size(s: &str, what: &str) -> Result<i64, String> {
    let s = s.trim();
    let (digits, mult) = match s.as_bytes().last() {
        Some(b'k' | b'K') => (&s[..s.len() - 1], 1i64 << 10),
        Some(b'm' | b'M') => (&s[..s.len() - 1], 1i64 << 20),
        Some(b'g' | b'G') => (&s[..s.len() - 1], 1i64 << 30),
        _ => (s, 1),
    };
    digits
        .trim()
        .parse::<i64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .ok_or_else(|| format!("invalid {what} value: {s}"))
}

/// Outcome of parsing: run with `Args`, or print help / version and exit. `Help`
/// carries an optional topic (`csvm help CMD`); `None` is the overview.
pub enum Parsed {
    Run(Box<Args>),
    Help(Option<String>),
    Version,
}

/// Parse arguments (excluding argv[0]). Returns an error message suitable for
/// printing to stderr.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Parsed, String> {
    let mut positionals: Vec<String> = Vec::new();
    let mut out_file = None;
    let mut threads: Option<i64> = None;
    let mut temp_dir = None;
    let mut chunk_size = DEFAULT_CHUNK_SIZE;
    let mut sort_buffer = DEFAULT_SORT_BUFFER;
    let mut explain = false;
    let mut color = ColorWhen::default();
    let mut script_file = None;
    let mut header = None;
    let mut format = None;

    let mut it = args.into_iter();
    while let Some(raw) = it.next() {
        // Accept the GNU `--flag=value` form alongside `--flag value`: split a
        // long option on its first `=` and use the right side as the value.
        let (arg, inline) = match raw.split_once('=') {
            Some((flag, val)) if flag.starts_with("--") => {
                (flag.to_string(), Some(val.to_string()))
            }
            _ => (raw, None),
        };
        macro_rules! value {
            () => {
                match inline {
                    Some(v) => v,
                    None => it
                        .next()
                        .ok_or_else(|| format!("missing value for {arg}"))?,
                }
            };
        }
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help(None)),
            "-V" | "--version" => return Ok(Parsed::Version),
            "-o" | "--output" => out_file = Some(value!()),
            "-n" | "--threads" => {
                let v = value!();
                threads = Some(
                    v.parse()
                        .map_err(|_| format!("invalid threads value: {v}"))?,
                );
            }
            "-f" | "--file" => script_file = Some(value!()),
            "-t" | "--temp-dir" => temp_dir = Some(PathBuf::from(value!())),
            "--chunk-size" => {
                let n = parse_size(&value!(), "--chunk-size")?;
                chunk_size = if n <= 0 {
                    DEFAULT_CHUNK_SIZE
                } else {
                    n as usize
                };
            }
            "--sort-buffer" => {
                let n = parse_size(&value!(), "--sort-buffer")?;
                sort_buffer = if n <= 0 {
                    DEFAULT_SORT_BUFFER
                } else {
                    n as usize
                };
            }
            "--explain" => explain = true,
            "--header" => header = Some(Header::parse(&value!())?),
            "--format" => {
                let v = value!();
                format = Some(match v.as_str() {
                    "csv" => InputFormat::Csv,
                    "parquet" => InputFormat::Parquet,
                    _ => return Err(format!("invalid --format value: {v} (csv|parquet)")),
                });
            }
            "--color" => {
                let v = value!();
                color = match v.as_str() {
                    "auto" => ColorWhen::Auto,
                    "always" => ColorWhen::Always,
                    "never" => ColorWhen::Never,
                    _ => return Err(format!("invalid --color value: {v} (auto|always|never)")),
                };
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option: {other}"));
            }
            _ => positionals.push(arg),
        }
    }

    // `csvm help [TOPIC]` (without -f) prints help and exits, like a subcommand.
    // A bare `help` isn't a valid script, so this can't shadow a real pipeline.
    if script_file.is_none() && positionals.first().is_some_and(|p| p == "help") {
        if positionals.len() > 2 {
            return Err("usage: csvm help [COMMAND|TOPIC]".to_string());
        }
        return Ok(Parsed::Help(positionals.into_iter().nth(1)));
    }

    // Positionals are `SCRIPT [INPUT]` (awk-style): the script is the first
    // positional and required, the input file optional (default stdin); a bare
    // `-` input is stdin. With `-f FILE` the script comes from the file, so the
    // single positional is the input instead. (At most one input either way; a
    // second positional is an error.)
    let mut positionals = positionals.into_iter();
    let script = if script_file.is_some() {
        String::new()
    } else {
        positionals
            .next()
            .ok_or_else(|| "no script given".to_string())?
    };
    let in_file = positionals.next();
    if positionals.next().is_some() {
        return Err("too many arguments (expected [SCRIPT] [INPUT])".to_string());
    }

    // Default to the core count; `-n 1` is the explicit single-threaded
    // escape hatch. A non-positive count clamps to 1.
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let threads = resolve_threads(threads, cores);

    Ok(Parsed::Run(Box::new(Args {
        script,
        script_file,
        in_file,
        out_file,
        threads,
        temp_dir,
        chunk_size,
        sort_buffer,
        explain,
        color,
        header,
        format,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Result<Args, String> {
        match parse(parts.iter().map(|s| s.to_string()))? {
            Parsed::Run(a) => Ok(*a),
            Parsed::Help(_) => Err("help".into()),
            Parsed::Version => Err("version".into()),
        }
    }

    #[test]
    fn version_flag() {
        assert!(matches!(parse(["-V".to_string()]), Ok(Parsed::Version)));
        assert!(matches!(
            parse(["--version".to_string()]),
            Ok(Parsed::Version)
        ));
    }

    #[test]
    fn defaults_and_script() {
        let a = args(&["cols a"]).unwrap();
        assert_eq!(a.script, "cols a");
        assert_eq!(a.in_file, None);
        assert!(a.threads >= 1);
        assert_eq!(a.chunk_size, DEFAULT_CHUNK_SIZE);
        assert!(!a.explain);
    }

    #[test]
    fn flags_parse() {
        let a = args(&["-o", "out.csv", "-n", "4", "--explain", "sort x"]).unwrap();
        // The old spelling is gone.
        assert!(parse(["--print-engine".to_string(), "sort x".to_string()]).is_err());
        assert_eq!(a.out_file.as_deref(), Some("out.csv"));
        assert_eq!(a.threads, 4);
        assert!(a.explain);
        assert_eq!(a.script, "sort x");
        assert_eq!(a.in_file, None); // no input positional -> stdin
    }

    #[test]
    fn long_flag_equals_value() {
        // GNU `--flag=value` form, alongside `--flag value`.
        let a = args(&["--threads=3", "--color=always", "--output=o.csv", "cols a"]).unwrap();
        assert_eq!(a.threads, 3);
        assert_eq!(a.color, ColorWhen::Always);
        assert_eq!(a.out_file.as_deref(), Some("o.csv"));
        // The value keeps any further `=` (split on the first only).
        assert_eq!(
            args(&["--output=a=b.csv", "cols a"])
                .unwrap()
                .out_file
                .as_deref(),
            Some("a=b.csv")
        );
        // A bad value still errors through the same path.
        assert!(parse(["--color=bogus".to_string(), "cols a".to_string()]).is_err());
    }

    #[test]
    fn positional_input_file() {
        // `csvm SCRIPT INPUT` (awk-style): script first, input second.
        let a = args(&["select x > 1", "data.csv"]).unwrap();
        assert_eq!(a.script, "select x > 1");
        assert_eq!(a.in_file.as_deref(), Some("data.csv"));
        // Flags may surround the positionals.
        let a = args(&["-n", "4", "cols a", "data.csv"]).unwrap();
        assert_eq!(a.script, "cols a");
        assert_eq!(a.in_file.as_deref(), Some("data.csv"));
        assert_eq!(a.threads, 4);
        // A bare '-' input means stdin.
        assert_eq!(
            args(&["cols a", "-"]).unwrap().in_file.as_deref(),
            Some("-")
        );
    }

    #[test]
    fn size_suffixes() {
        assert_eq!(
            args(&["--sort-buffer", "256M", "cols a"])
                .unwrap()
                .sort_buffer,
            256 << 20
        );
        assert_eq!(
            args(&["--chunk-size", "2g", "cols a"]).unwrap().chunk_size,
            2 << 30
        );
        assert_eq!(
            args(&["--chunk-size", "4096", "cols a"])
                .unwrap()
                .chunk_size,
            4096
        );
        // non-positive falls back to the default; garbage errors.
        assert_eq!(
            args(&["--chunk-size", "0", "cols a"]).unwrap().chunk_size,
            DEFAULT_CHUNK_SIZE
        );
        assert!(
            parse([
                "--chunk-size".to_string(),
                "5x".to_string(),
                "cols a".to_string()
            ])
            .is_err()
        );
    }

    #[test]
    fn header_flag() {
        assert_eq!(args(&["cols a"]).unwrap().header, None);
        assert_eq!(
            args(&["--header", "a, b", "cols a"]).unwrap().header,
            Some(Header::Named(vec!["a".into(), "b".into()]))
        );
        assert_eq!(
            args(&["--header=a,b", "cols a"]).unwrap().header,
            Some(Header::Named(vec!["a".into(), "b".into()]))
        );
        // `-` (or nothing) auto-names the columns c1, c2, …
        assert_eq!(
            args(&["--header", "-", "cols c1"]).unwrap().header,
            Some(Header::Auto)
        );
        assert_eq!(
            args(&["--header", "", "cols c1"]).unwrap().header,
            Some(Header::Auto)
        );
        assert!(args(&["--header", "a,,b", "cols a"]).is_err());
        // Quoted like a header line, so a name may hold a comma.
        assert_eq!(
            args(&["--header", "\"last, first\",n", "cols n"])
                .unwrap()
                .header,
            Some(Header::Named(vec!["last, first".into(), "n".into()]))
        );
        // The old flag is gone.
        assert!(args(&["--no-header", "cols c1"]).is_err());
    }

    #[test]
    fn long_form_flags() {
        let a = args(&["--threads", "3", "--output", "o.csv", "cols a", "in.csv"]).unwrap();
        assert_eq!(a.threads, 3);
        assert_eq!(a.out_file.as_deref(), Some("o.csv"));
        assert_eq!(a.script, "cols a");
        assert_eq!(a.in_file.as_deref(), Some("in.csv"));
    }

    #[test]
    fn threads_default_to_the_core_count_and_clamp_to_one() {
        assert_eq!(resolve_threads(None, 8), 8);
        assert_eq!(resolve_threads(None, 1), 1);
        assert_eq!(resolve_threads(Some(4), 1), 4);
        assert_eq!(resolve_threads(Some(0), 8), 1);
        assert_eq!(resolve_threads(Some(-3), 8), 1);
        assert_eq!(resolve_threads(Some(1 << 40), 8), MAX_THREADS);
        assert_eq!(resolve_threads(Some(i64::MAX), 8), MAX_THREADS);
        assert_eq!(args(&["-n", "0", "cols a"]).unwrap().threads, 1);
        assert_eq!(args(&["-n", "-3", "cols a"]).unwrap().threads, 1);
    }

    #[test]
    fn errors() {
        assert!(parse(["--nope".to_string(), "cols a".to_string()]).is_err()); // unknown option
        assert!(parse(std::iter::empty()).is_err()); // no script
        // A third positional beyond SCRIPT and INPUT is an error.
        assert!(parse(["s".to_string(), "in.csv".to_string(), "extra".to_string()]).is_err());
    }

    #[test]
    fn script_file_flag() {
        // With -f, the script comes from the file and the positional is the input.
        let a = args(&["-f", "prog.csvm", "data.csv"]).unwrap();
        assert_eq!(a.script_file.as_deref(), Some("prog.csvm"));
        assert_eq!(a.in_file.as_deref(), Some("data.csv"));
        assert_eq!(a.script, "");
        // -f with no input -> stdin.
        let a = args(&["--file", "prog.csvm"]).unwrap();
        assert_eq!(a.in_file, None);
        // Without -f, a script is still required.
        assert!(parse(std::iter::empty()).is_err());
    }
}
