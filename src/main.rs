use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Cursor, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process;

use csvm::cli::{self, Parsed};
use csvm::plan::OutputFormat;
use csvm::{exec, parse};

fn main() {
    if let Err(msg) = run() {
        eprintln!("csvm: {msg}");
        process::exit(1);
    }
}

/// Where input rows come from. A seekable file can be sharded; stdin streams.
enum Source {
    File {
        path: PathBuf,
        data_start: u64,
        file_len: u64,
    },
    Stream(Box<dyn BufRead>),
    /// A `.parquet` file (feature `parquet`): typed, columnar, read in batches.
    #[cfg(feature = "parquet")]
    Parquet {
        path: PathBuf,
    },
}

fn run() -> Result<(), String> {
    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(Parsed::Run(args)) => *args,
        Ok(Parsed::Help(topic)) => {
            println!("{}", csvm::help::render(topic.as_deref())?);
            return Ok(());
        }
        Ok(Parsed::Version) => {
            println!("csvm {}", csvm::VERSION);
            return Ok(());
        }
        // On a usage error show the brief synopsis, not the whole manual.
        Err(e) => {
            return Err(format!(
                "{e}\n\n{}\nrun `csvm --help` for options, `csvm help CMD` for a command",
                csvm::help::usage_line()
            ));
        }
    };

    // The pipeline is either the SCRIPT positional or, with `-f`, a file.
    let script = match &args.script_file {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read script file '{path}': {e}"))?,
        None => args.script.clone(),
    };
    // Parse the pipe script into a plan here, once.
    let mut plan = parse::parse(&script).map_err(|e| e.to_string())?;
    let opts = exec::RunOpts {
        chunk_size: args.chunk_size,
        threads: args.threads,
        temp_dir: args.temp_dir.clone().unwrap_or_else(std::env::temp_dir),
        sort_buffer: args.sort_buffer,
    };

    let (mut source, header) = open_source(&args)?;
    // Joins need each right file's header to resolve; read them (IO) first.
    exec::prepare_joins(&mut plan).map_err(|e| e.to_string())?;
    let out_header = plan.resolve(&header).map_err(|e| e.to_string())?;

    if args.explain {
        print!("{}", exec::describe(&plan));
        return Ok(());
    }

    let color_on = color_enabled(args.color, args.out_file.as_deref());
    // The graph sink's default width: the terminal's columns, but only when
    // stdout is actually a terminal (matches `color_enabled`'s auto check).
    let term_width = if stdout_is_tty(args.out_file.as_deref()) {
        csvm::term::columns()
    } else {
        None
    };
    let mut output = open_output(&args)?;
    // Aligning needs all rows (for column widths), colouring needs all rows (for
    // gradient ranges), and a graph draws from the whole output — so each of
    // these buffers the run first, then renders.
    if plan.output == OutputFormat::Aligned
        || plan.graph.is_some()
        || (color_on && !plan.colors.is_empty())
    {
        let mut buf: Vec<u8> = Vec::new();
        run_into(&mut source, &plan, &out_header, &opts, &mut buf)?;
        exec::render(&buf, &plan, color_on, term_width, &mut output).map_err(|e| e.to_string())?;
    } else {
        run_into(&mut source, &plan, &out_header, &opts, &mut output)?;
    }
    output.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// Whether to emit ANSI colour. An explicit `--color always`/`never` wins. For
/// `auto`, honor the de-facto env conventions — `NO_COLOR` (set & non-empty)
/// disables, `CLICOLOR_FORCE` (set & not `0`) forces — then fall back to: write
/// to a terminal (stdout, not a `-o` file).
fn color_enabled(when: cli::ColorWhen, out_file: Option<&str>) -> bool {
    match when {
        cli::ColorWhen::Always => true,
        cli::ColorWhen::Never => false,
        cli::ColorWhen::Auto => {
            if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
                return false;
            }
            if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| v != "0") {
                return true;
            }
            stdout_is_tty(out_file)
        }
    }
}

/// Whether the output goes to a terminal: stdout (no `-o`, or `-o -`) and that
/// stdout is one. Both the colour default and the chart's default width ask
/// this, so they cannot disagree about where the output is going.
fn stdout_is_tty(out_file: Option<&str>) -> bool {
    out_file.is_none_or(|p| p == "-") && io::stdout().is_terminal()
}

/// Open the input and determine its header. With `--header` the input has no
/// header line: the given (or auto) names are the header and the whole input
/// is data. Otherwise the first line is read as the header.
fn open_source(args: &cli::Args) -> Result<(Source, Vec<String>), String> {
    // Parquet carries its own typed schema, so it bypasses the CSV header logic
    // (and, without the feature, reports the build hint before anything else).
    if input_format(args) == cli::InputFormat::Parquet {
        return open_parquet(args);
    }
    // A named header needs no look at the input (so an empty input is a
    // legal zero-row table, and `--explain` never waits on stdin); otherwise
    // the first line is read and `Header::resolve` decides whether it was the
    // header or the first data row.
    let named = match &args.header {
        Some(cli::Header::Named(h)) => Some(h.clone()),
        _ => None,
    };
    match args.in_file.as_deref().filter(|p| *p != "-") {
        Some(path) => {
            let (header, data_start, file_len) = match named {
                Some(h) => {
                    let len = std::fs::metadata(path)
                        .map_err(|e| format!("cannot stat '{path}': {e}"))?
                        .len();
                    (h, 0, len)
                }
                None => {
                    let (first, after_first, file_len) =
                        exec::read_header_from_path(Path::new(path)).map_err(|e| e.to_string())?;
                    let (header, data_start) =
                        cli::Header::resolve(args.header.as_ref(), first, after_first);
                    (header, data_start, file_len)
                }
            };
            let source = Source::File {
                path: PathBuf::from(path),
                data_start,
                file_len,
            };
            Ok((source, header))
        }
        None => {
            let mut reader: Box<dyn BufRead> = Box::new(BufReader::new(io::stdin()));
            if let Some(h) = named {
                return Ok((Source::Stream(reader), h));
            }
            // Read the first line: the header, or the row `--header -` counts
            // and then chains back in front of the rest as data.
            let mut first = Vec::new();
            reader
                .read_until(b'\n', &mut first)
                .map_err(|e| e.to_string())?;
            if first.is_empty() {
                return Err("input is empty (no header line)".to_string());
            }
            let line = std::str::from_utf8(&first)
                .map_err(|e| format!("input is not valid UTF-8: {e}"))?;
            let columns = csvm::csv::parse_header(line.strip_suffix('\n').unwrap_or(line));
            let (header, data_start) =
                cli::Header::resolve(args.header.as_ref(), columns, first.len() as u64);
            if data_start == 0 {
                let chained: Box<dyn BufRead> =
                    Box::new(BufReader::new(Cursor::new(first).chain(reader)));
                return Ok((Source::Stream(chained), header));
            }
            Ok((Source::Stream(reader), header))
        }
    }
}

/// The input format: an explicit `--format` wins, else auto-detect from the
/// input file's extension (`.parquet` ⇒ Parquet, everything else CSV).
fn input_format(args: &cli::Args) -> cli::InputFormat {
    if let Some(f) = args.format {
        return f;
    }
    match args.in_file.as_deref() {
        Some(p)
            if Path::new(p)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("parquet")) =>
        {
            cli::InputFormat::Parquet
        }
        _ => cli::InputFormat::Csv,
    }
}

/// Resolve a parquet input to a `Source` and its schema header. Parquet needs a
/// seekable file (footer metadata) and rejects `--header` (the schema is the
/// header). Without the feature this returns the build hint immediately, so a
/// missing build is reported ahead of any other argument problem.
#[cfg(feature = "parquet")]
fn open_parquet(args: &cli::Args) -> Result<(Source, Vec<String>), String> {
    let path = args
        .in_file
        .as_deref()
        .filter(|p| *p != "-")
        .ok_or_else(|| "parquet input must be a seekable file, not stdin".to_string())?;
    if args.header.is_some() {
        return Err(
            "--header doesn't apply to parquet input (it carries a typed schema)".to_string(),
        );
    }
    let header = csvm::parquet::read_header(Path::new(path)).map_err(|e| e.to_string())?;
    Ok((
        Source::Parquet {
            path: PathBuf::from(path),
        },
        header,
    ))
}

#[cfg(not(feature = "parquet"))]
fn open_parquet(_args: &cli::Args) -> Result<(Source, Vec<String>), String> {
    Err("parquet input requires building csvm with --features parquet".to_string())
}

fn run_into<W: Write + Send>(
    source: &mut Source,
    plan: &csvm::plan::Plan,
    out_header: &[String],
    opts: &exec::RunOpts,
    output: &mut W,
) -> Result<(), String> {
    match source {
        Source::File {
            path,
            data_start,
            file_len,
        } => exec::run_file(plan, out_header, opts, path, *data_start, *file_len, output),
        Source::Stream(reader) => exec::run(plan, out_header, opts, reader, output),
        #[cfg(feature = "parquet")]
        Source::Parquet { path } => exec::run_parquet(plan, out_header, opts, path, output),
    }
    .map_err(|e| e.to_string())
}

fn open_output(args: &cli::Args) -> Result<Box<dyn Write + Send>, String> {
    Ok(match args.out_file.as_deref() {
        Some(path) if path != "-" => Box::new(BufWriter::new(
            File::create(path).map_err(|e| format!("cannot open output '{path}': {e}"))?,
        )),
        _ => Box::new(BufWriter::new(io::stdout())),
    })
}
