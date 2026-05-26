use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
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
        Err(e) => return Err(format!("{e}\n\n{}", cli::USAGE)),
    };

    // Parse the pipe script into a plan here, once.
    let mut plan = parse::parse(&args.script).map_err(|e| e.to_string())?;
    let opts = exec::RunOpts {
        chunk_size: args.chunk_size,
        threads: args.threads,
        temp_dir: args.temp_dir.clone().unwrap_or_else(std::env::temp_dir),
        sort_buffer: args.sort_buffer,
    };

    let (mut source, header) = open_source(&args, plan.input_header.as_deref())?;
    let out_header = plan.resolve(&header).map_err(|e| e.to_string())?;

    if args.print_engine {
        print!("{}", exec::describe(&plan));
        return Ok(());
    }

    let mut output = open_output(&args)?;
    if plan.output == OutputFormat::Aligned {
        // Run into a buffer, then align it (needs all rows for column widths).
        let mut buf: Vec<u8> = Vec::new();
        run_into(&mut source, &plan, &out_header, &opts, &mut buf)?;
        exec::format_aligned(&buf, &mut output).map_err(|e| e.to_string())?;
    } else {
        run_into(&mut source, &plan, &out_header, &opts, &mut output)?;
    }
    output.flush().map_err(|e| e.to_string())?;
    Ok(())
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
            let (header, data_start, file_len) = match input_header {
                Some(h) => {
                    let len = std::fs::metadata(path)
                        .map_err(|e| format!("cannot stat '{path}': {e}"))?
                        .len();
                    (h.to_vec(), 0, len)
                }
                None => exec::read_header_from_path(Path::new(path)).map_err(|e| e.to_string())?,
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
            let header = match input_header {
                Some(h) => h.to_vec(),
                None => exec::read_header(&mut reader).map_err(|e| e.to_string())?,
            };
            Ok((Source::Stream(reader), header))
        }
    }
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
