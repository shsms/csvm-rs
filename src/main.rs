use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process;

use csvm::cli::{self, Parsed};
use csvm::console::Console;
use csvm::plan::OutputFormat;
use csvm::{exec, parse};

fn main() {
    match run() {
        Ok(()) | Err(Failure::Closed) => {}
        Err(Failure::Message(msg)) => {
            eprintln!("csvm: {msg}");
            process::exit(1);
        }
    }
}

/// Why a run stopped short.
enum Failure {
    /// Report this on stderr and exit 1.
    Message(String),
    /// The output's reader stopped reading (`csvm … | head`). Nothing is
    /// wrong with the run, so it ends quietly and successfully, the way
    /// `cat` and `grep` do.
    Closed,
}

impl From<String> for Failure {
    fn from(msg: String) -> Self {
        Failure::Message(msg)
    }
}

impl From<csvm::error::Error> for Failure {
    fn from(e: csvm::error::Error) -> Self {
        match e {
            csvm::error::Error::Io(e) => e.into(),
            e => Failure::Message(e.to_string()),
        }
    }
}

impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::BrokenPipe {
            Failure::Closed
        } else {
            Failure::Message(e.to_string())
        }
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

fn run() -> Result<(), Failure> {
    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(Parsed::Run(args)) => *args,
        Ok(Parsed::Help { topic, no_pager }) => {
            let text = csvm::help::render(topic.as_deref())?;
            let console = Console {
                no_pager,
                ..Console::read_env()
            };
            let mut out = console.paged_stdout();
            writeln!(out, "{text}")?;
            out.flush()?;
            return Ok(());
        }
        Ok(Parsed::Version) => {
            writeln!(io::stdout(), "csvm {}", csvm::VERSION)?;
            return Ok(());
        }
        // On a usage error show the brief synopsis, not the whole manual.
        Err(e) => {
            return Err(Failure::Message(format!(
                "{e}\n\n{}\nrun `csvm --help` for options, `csvm help CMD` for a command",
                csvm::help::usage_line()
            )));
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
    exec::prepare_joins(&mut plan)?;
    let out_header = plan.resolve(&header)?;

    let console = Console::read(&args);
    if args.explain {
        // The plan goes to stdout even with -o, so it pages by stdout.
        let explain = Console {
            no_pager: args.no_pager,
            ..Console::read_env()
        };
        let mut out = explain.paged_stdout();
        write!(out, "{}", exec::describe(&plan))?;
        out.flush()?;
        return Ok(());
    }

    // Colour, when on, is drawn at the depth the terminal announces.
    let color = console.color();
    let mut output = open_output(&args)?;
    // Aligning needs all rows (for column widths), colouring needs all rows (for
    // gradient ranges), and a graph draws from the whole output — so each of
    // these buffers the run first, then renders.
    if plan.output == OutputFormat::Aligned
        || plan.graph.is_some()
        || (color.is_some() && !plan.colors.is_empty())
    {
        let mut buf: Vec<u8> = Vec::new();
        run_into(&mut source, &plan, &out_header, &opts, &mut buf)?;
        // A table or a chart is read on screen, so a long one is paged. The
        // pager starts only now, with the run done, so it never sits waiting
        // on a slow pipeline. A table prints a line for each line of the
        // run's output, so that says whether it fills the window.
        let table = plan.output == OutputFormat::Aligned;
        let pager = if table || plan.graph.is_some() {
            console.pager(table, table && console.fills(&buf))
        } else {
            None
        };
        let screen = console.screen(pager.as_ref());
        if let Some(p) = pager {
            output = Box::new(p);
        }
        exec::render(&buf, &plan, &screen, &mut output)?;
    } else {
        run_into(&mut source, &plan, &out_header, &opts, &mut output)?;
    }
    output.flush()?;
    Ok(())
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
    match args.in_path() {
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
        .in_path()
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
) -> Result<(), csvm::error::Error> {
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
}

fn open_output(args: &cli::Args) -> Result<Box<dyn Write + Send>, String> {
    Ok(match args.out_path() {
        Some(path) => Box::new(BufWriter::new(
            File::create(path).map_err(|e| format!("cannot open output '{path}': {e}"))?,
        )),
        _ => Box::new(BufWriter::new(io::stdout())),
    })
}
