//! Command-line argument parsing.
//!
//! Mirrors csvm's flags: `-f`/`-o` for input/output files (default
//! stdin/stdout), `-n` for the worker count, `-t`/`--temp-dir` for sort spill
//! files, `--chunk-size` for the input chunk size, and `--print-engine` to dump
//! the compiled plan and exit. The script is the one required positional.

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
}

const DEFAULT_CHUNK_SIZE: usize = 1_000_000;
/// In-memory budget before `sort` spills a run to a temp file.
pub const DEFAULT_SORT_BUFFER: usize = 256 << 20;

pub const USAGE: &str = "\
usage: csvm [-f IN] [-o OUT] [-n THREADS] [-t TEMPDIR] [--chunk-size BYTES]
            [--print-engine] SCRIPT

  -f IN              input CSV file (default: stdin)
  -o OUT             output CSV file (default: stdout)
  -n THREADS         worker threads (default: 1; <=0 means 1)
  -t, --temp-dir DIR directory for sort spill files (default: system temp)
  --chunk-size BYTES input chunk size in bytes (default: 1000000)
  --sort-buffer BYTES in-memory budget before sort spills to temp files
                     (default: 256 MiB)
  --print-engine     print the compiled execution plan and exit
  -h, --help         show this help

SCRIPT is a pipe-syntax pipeline, e.g.:
  'select fieldA == \"t\" && countZ > 0 | cols -v fieldA'";

/// Outcome of parsing: either run with `Args`, or print help/usage and exit.
pub enum Parsed {
    Run(Box<Args>),
    Help,
}

/// Parse arguments (excluding argv[0]). Returns an error message suitable for
/// printing to stderr.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Parsed, String> {
    let mut script: Option<String> = None;
    let mut in_file = None;
    let mut out_file = None;
    let mut threads: Option<i64> = None;
    let mut temp_dir = None;
    let mut chunk_size = DEFAULT_CHUNK_SIZE;
    let mut sort_buffer = DEFAULT_SORT_BUFFER;
    let mut print_engine = false;

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
            "-f" => in_file = Some(value!()),
            "-o" => out_file = Some(value!()),
            "-n" => {
                let v = value!();
                threads = Some(v.parse().map_err(|_| format!("invalid -n value: {v}"))?);
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
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option: {other}"));
            }
            _ => {
                if script.replace(arg).is_some() {
                    return Err("more than one script given".to_string());
                }
            }
        }
    }

    let Some(script) = script else {
        return Err("no script given".to_string());
    };

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
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Result<Args, String> {
        match parse(parts.iter().map(|s| s.to_string()))? {
            Parsed::Run(a) => Ok(*a),
            Parsed::Help => Err("help".into()),
        }
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
        let a = args(&[
            "-f",
            "in.csv",
            "-o",
            "out.csv",
            "-n",
            "4",
            "--print-engine",
            "(sort-by x)",
        ])
        .unwrap();
        assert_eq!(a.in_file.as_deref(), Some("in.csv"));
        assert_eq!(a.out_file.as_deref(), Some("out.csv"));
        assert_eq!(a.threads, 4);
        assert!(a.print_engine);
        assert_eq!(a.script, "(sort-by x)");
    }

    #[test]
    fn non_positive_threads_clamp_to_one() {
        assert_eq!(args(&["-n", "0", "(cols a)"]).unwrap().threads, 1);
        assert_eq!(args(&["-n", "-3", "(cols a)"]).unwrap().threads, 1);
    }

    #[test]
    fn errors() {
        assert!(parse(["-f".to_string()]).is_err()); // missing value
        assert!(parse(["--nope".to_string(), "(cols a)".to_string()]).is_err());
        assert!(parse(std::iter::empty()).is_err()); // no script
    }
}
