use std::fs::File;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::Path;
use std::process;

use csvm::cli::{self, Parsed};
use csvm::{exec, parse};

fn main() {
    if let Err(msg) = run() {
        eprintln!("csvm: {msg}");
        process::exit(1);
    }
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

    // A real file goes through the sharding-capable path; stdin/`-` streams.
    match args.in_file.as_deref().filter(|p| *p != "-") {
        Some(path) => {
            let path = Path::new(path);
            let (header, data_start, file_len) =
                exec::read_header_from_path(path).map_err(|e| e.to_string())?;
            let out_header = plan.resolve(&header).map_err(|e| e.to_string())?;
            if args.print_engine {
                print!("{}", exec::describe(&plan));
                return Ok(());
            }
            let mut output = open_output(&args)?;
            exec::run_file(
                &plan,
                &out_header,
                &opts,
                path,
                data_start,
                file_len,
                &mut output,
            )
            .map_err(|e| e.to_string())?;
            output.flush().map_err(|e| e.to_string())?;
        }
        None => {
            let mut input = BufReader::new(io::stdin());
            let header = exec::read_header(&mut input).map_err(|e| e.to_string())?;
            let out_header = plan.resolve(&header).map_err(|e| e.to_string())?;
            if args.print_engine {
                print!("{}", exec::describe(&plan));
                return Ok(());
            }
            let mut output = open_output(&args)?;
            exec::run(&plan, &out_header, &opts, &mut input, &mut output)
                .map_err(|e| e.to_string())?;
            output.flush().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn open_output(args: &cli::Args) -> Result<Box<dyn Write + Send>, String> {
    Ok(match args.out_file.as_deref() {
        Some(path) if path != "-" => Box::new(BufWriter::new(
            File::create(path).map_err(|e| format!("cannot open output '{path}': {e}"))?,
        )),
        _ => Box::new(BufWriter::new(io::stdout())),
    })
}
