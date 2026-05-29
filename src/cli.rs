//! Command-line argument parsing.
//!
//! The script is the first positional and the input file an optional second
//! positional (`csvm SCRIPT [INPUT]`, like `awk 'prog' file`); input defaults to
//! stdin, and a bare `-` is also stdin. `-o` sets the output file (default
//! stdout), `-n` the worker count, `-t`/`--temp-dir` the sort spill directory,
//! `--chunk-size` the input chunk size, and `--print-engine` dumps the compiled
//! plan and exits.

use std::path::PathBuf;

/// Parsed command-line arguments.
#[derive(Debug, Clone)]
pub struct Args {
    pub script: String,
    pub in_file: Option<String>,
    pub out_file: Option<String>,
    pub threads: usize,
    pub temp_dir: Option<PathBuf>,
    pub chunk_size: usize,
    pub sort_buffer: usize,
    pub print_engine: bool,
    pub color: ColorWhen,
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

pub const USAGE: &str = "\
usage: csvm [-o OUT] [-n THREADS] [-t TEMPDIR] [--chunk-size BYTES]
            [--print-engine] SCRIPT [INPUT]

  SCRIPT             pipe-syntax pipeline (required)
  INPUT              input CSV file (default: stdin; '-' is stdin)
  -o, --output OUT   output CSV file (default: stdout)
  -n, --threads N    worker threads (default: 1; <=0 means 1)
  -t, --temp-dir DIR directory for sort spill files (default: system temp)
  --chunk-size BYTES input chunk size in bytes (default: 1000000)
  --sort-buffer BYTES in-memory budget before sort spills to temp files
                     (default: 256 MiB)
  --print-engine     print the compiled execution plan and exit
  --color WHEN       colour `color` rules: auto (TTY only), always, never
  -h, --help         show this help
  -V, --version      print version and exit

SCRIPT is a pipe-syntax pipeline, e.g.:
  csvm 'select fieldA == \"t\" && countZ > 0 | cols -v fieldA' input.csv";

/// Outcome of parsing: run with `Args`, or print help / version and exit.
pub enum Parsed {
    Run(Box<Args>),
    Help,
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
    let mut print_engine = false;
    let mut color = ColorWhen::default();

    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        macro_rules! value {
            () => {
                it.next()
                    .ok_or_else(|| format!("missing value for {arg}"))?
            };
        }
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "-o" | "--output" => out_file = Some(value!()),
            "-n" | "--threads" => {
                let v = value!();
                threads = Some(
                    v.parse()
                        .map_err(|_| format!("invalid threads value: {v}"))?,
                );
            }
            "-t" | "--temp-dir" => temp_dir = Some(PathBuf::from(value!())),
            "--chunk-size" => {
                let v = value!();
                let n: i64 = v
                    .parse()
                    .map_err(|_| format!("invalid --chunk-size value: {v}"))?;
                chunk_size = if n <= 0 {
                    DEFAULT_CHUNK_SIZE
                } else {
                    n as usize
                };
            }
            "--sort-buffer" => {
                let v = value!();
                let n: i64 = v
                    .parse()
                    .map_err(|_| format!("invalid --sort-buffer value: {v}"))?;
                sort_buffer = if n <= 0 {
                    DEFAULT_SORT_BUFFER
                } else {
                    n as usize
                };
            }
            "--print-engine" => print_engine = true,
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

    // Positionals are `SCRIPT [INPUT]` (awk-style): the script is required, the
    // input file optional (default stdin). A bare `-` falls through here too, so
    // `csvm SCRIPT -` is an explicit stdin.
    let mut positionals = positionals.into_iter();
    let Some(script) = positionals.next() else {
        return Err("no script given".to_string());
    };
    let in_file = positionals.next();
    if positionals.next().is_some() {
        return Err("too many arguments (expected SCRIPT [INPUT])".to_string());
    }

    // Default to single-threaded (like csvm): the parallel path only pays off
    // for heavier per-row work, and stdin can't be sharded. Users opt in with
    // `-n`. A non-positive count clamps to 1.
    let threads = match threads {
        Some(n) if n >= 1 => n as usize,
        _ => 1,
    };

    Ok(Parsed::Run(Box::new(Args {
        script,
        in_file,
        out_file,
        threads,
        temp_dir,
        chunk_size,
        sort_buffer,
        print_engine,
        color,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Result<Args, String> {
        match parse(parts.iter().map(|s| s.to_string()))? {
            Parsed::Run(a) => Ok(*a),
            Parsed::Help => Err("help".into()),
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
        let a = args(&["(cols a)"]).unwrap();
        assert_eq!(a.script, "(cols a)");
        assert_eq!(a.in_file, None);
        assert_eq!(a.threads, 1);
        assert_eq!(a.chunk_size, DEFAULT_CHUNK_SIZE);
        assert!(!a.print_engine);
    }

    #[test]
    fn flags_parse() {
        let a = args(&["-o", "out.csv", "-n", "4", "--print-engine", "(sort-by x)"]).unwrap();
        assert_eq!(a.out_file.as_deref(), Some("out.csv"));
        assert_eq!(a.threads, 4);
        assert!(a.print_engine);
        assert_eq!(a.script, "(sort-by x)");
        assert_eq!(a.in_file, None); // no input positional -> stdin
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
    fn long_form_flags() {
        let a = args(&["--threads", "3", "--output", "o.csv", "cols a", "in.csv"]).unwrap();
        assert_eq!(a.threads, 3);
        assert_eq!(a.out_file.as_deref(), Some("o.csv"));
        assert_eq!(a.script, "cols a");
        assert_eq!(a.in_file.as_deref(), Some("in.csv"));
    }

    #[test]
    fn non_positive_threads_clamp_to_one() {
        assert_eq!(args(&["-n", "0", "(cols a)"]).unwrap().threads, 1);
        assert_eq!(args(&["-n", "-3", "(cols a)"]).unwrap().threads, 1);
    }

    #[test]
    fn errors() {
        assert!(parse(["--nope".to_string(), "(cols a)".to_string()]).is_err()); // unknown option
        assert!(parse(std::iter::empty()).is_err()); // no script
        // A third positional beyond SCRIPT and INPUT is an error.
        assert!(parse(["s".to_string(), "in.csv".to_string(), "extra".to_string()]).is_err());
        // `-f` is no longer a flag; input is positional now.
        assert!(parse(["-f".to_string(), "in.csv".to_string(), "s".to_string()]).is_err());
    }
}
