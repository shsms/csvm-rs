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
}

fn run() -> Result<(), String> {
    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(Parsed::Run(args)) => *args,
        Ok(Parsed::Help) => {
            println!("{}", cli::USAGE);
            return Ok(());
        }
        Ok(Parsed::Version) => {
            println!("csvm {}", csvm::VERSION);
            return Ok(());
        }
        Err(e) => return Err(format!("{e}\n\n{}", cli::USAGE)),
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

    let (mut source, header) = open_source(&args, plan.input_header.as_deref())?;
    // Joins need each right file's header to resolve; read them (IO) first.
    exec::prepare_joins(&mut plan).map_err(|e| e.to_string())?;
    let out_header = plan.resolve(&header).map_err(|e| e.to_string())?;

    if args.print_engine {
        print!("{}", exec::describe(&plan));
        return Ok(());
    }

    let color_on = color_enabled(args.color, args.out_file.as_deref());
    let mut output = open_output(&args)?;
    // Aligning needs all rows (for column widths) and colouring needs all rows
    // (for gradient ranges), so either one buffers first, then renders.
    if plan.output == OutputFormat::Aligned || (color_on && !plan.colors.is_empty()) {
        let mut buf: Vec<u8> = Vec::new();
        run_into(&mut source, &plan, &out_header, &opts, &mut buf)?;
        exec::render(&buf, &plan, color_on, &mut output).map_err(|e| e.to_string())?;
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
            out_file.is_none_or(|p| p == "-") && io::stdout().is_terminal()
        }
    }
}

/// Open the input and determine its header. With `input_header` (from a `hdr`
/// command) the input has no header line: the supplied names are the header and
/// the whole input is data. Otherwise the first line is read as the header.
fn open_source(
    args: &cli::Args,
    input_header: Option<&[String]>,
) -> Result<(Source, Vec<String>), String> {
    match args.in_file.as_deref().filter(|p| *p != "-") {
        Some(path) => {
            let (header, data_start, file_len) = if let Some(h) = input_header {
                let len = std::fs::metadata(path)
                    .map_err(|e| format!("cannot stat '{path}': {e}"))?
                    .len();
                (h.to_vec(), 0, len)
            } else if args.no_header {
                // Peek the first line only to count columns; it is data, so the
                // data region starts at byte 0.
                let (first, _data_start, file_len) =
                    exec::read_header_from_path(Path::new(path)).map_err(|e| e.to_string())?;
                (auto_header(first.len()), 0, file_len)
            } else {
                exec::read_header_from_path(Path::new(path)).map_err(|e| e.to_string())?
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
            if let Some(h) = input_header {
                return Ok((Source::Stream(reader), h.to_vec()));
            }
            if args.no_header {
                // Read the first line to count columns, then chain it back in
                // front of the rest so the whole stream is processed as data.
                let mut first = Vec::new();
                reader
                    .read_until(b'\n', &mut first)
                    .map_err(|e| e.to_string())?;
                let line = std::str::from_utf8(&first)
                    .map_err(|e| format!("input is not valid UTF-8: {e}"))?;
                let n = csvm::csv::parse_header(line.strip_suffix('\n').unwrap_or(line)).len();
                let chained: Box<dyn BufRead> =
                    Box::new(BufReader::new(Cursor::new(first).chain(reader)));
                return Ok((Source::Stream(chained), auto_header(n)));
            }
            let header = exec::read_header(&mut reader).map_err(|e| e.to_string())?;
            Ok((Source::Stream(reader), header))
        }
    }
}

/// Auto-generated column names for `--no-header`: `c1, c2, …, cn`.
fn auto_header(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("c{i}")).collect()
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
