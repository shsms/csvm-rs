use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::process;

use csvm::cli::{self, Parsed};
use csvm::{compile, exec};

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

    // tulisp compiles the script into a plan here, once.
    let mut plan = compile::compile(&args.script).map_err(|e| e.to_string())?;

    let mut input: Box<dyn BufRead> = match args.in_file.as_deref() {
        Some(path) if path != "-" => Box::new(BufReader::new(
            File::open(path).map_err(|e| format!("cannot open input '{path}': {e}"))?,
        )),
        _ => Box::new(BufReader::new(io::stdin())),
    };

    let header = exec::read_header(&mut input).map_err(|e| e.to_string())?;
    let out_header = plan.resolve(&header).map_err(|e| e.to_string())?;

    if args.print_engine {
        print!("{}", exec::describe(&plan));
        return Ok(());
    }

    let mut output: Box<dyn Write + Send> = match args.out_file.as_deref() {
        Some(path) if path != "-" => Box::new(BufWriter::new(
            File::create(path).map_err(|e| format!("cannot open output '{path}': {e}"))?,
        )),
        _ => Box::new(BufWriter::new(io::stdout())),
    };

    exec::run(
        &plan,
        &out_header,
        args.chunk_size,
        args.threads,
        &mut input,
        &mut output,
    )
    .map_err(|e| e.to_string())?;
    output.flush().map_err(|e| e.to_string())?;
    Ok(())
}
