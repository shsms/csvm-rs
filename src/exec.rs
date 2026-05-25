//! Executing a compiled [`Plan`] over CSV input.
//!
//! This is the single-threaded baseline. A plan that is a single transform
//! stage streams chunk-by-chunk with borrowed rows (zero-copy); a plan with a
//! `sort` (or otherwise multiple stages) materializes rows as owned values and
//! runs stage by stage. Parallelism and external-merge sort are layered on in
//! later modules.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::thread;

use crossbeam_channel::bounded;

use crate::csv;
use crate::error::Error;
use crate::field::Field;
use crate::plan::{BoolExpr, CmpOp, Operand, Plan, SortStmt, Stage, Stmt};
use crate::sort::{ExternalSorter, OwnedRow};

/// Knobs for a run: chunk size, worker count, and where sort spills its temp
/// files.
#[derive(Clone, Debug)]
pub struct RunOpts {
    pub chunk_size: usize,
    pub threads: usize,
    pub temp_dir: PathBuf,
    pub sort_buffer: usize,
}

/// Apply a transform stage's statements to a row, returning whether it
/// survives. The hot inner call — no interpreter involved.
#[inline]
pub fn apply_transform(stmts: &[Stmt], row: &mut Vec<Field>) -> Result<bool, Error> {
    for s in stmts {
        if !s.apply(row)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Read and parse the header line from `input`.
pub fn read_header<R: BufRead>(input: &mut R) -> Result<Vec<String>, Error> {
    let mut line = Vec::new();
    input.read_until(b'\n', &mut line)?;
    if line.is_empty() {
        return Err(Error::Other("could not read header (empty input)".into()));
    }
    let text = std::str::from_utf8(&line)
        .map_err(|e| Error::Other(format!("header is not valid UTF-8: {e}")))?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    Ok(csv::parse_header(text))
}

/// Read the next chunk: up to `size` bytes, extended to the end of the current
/// line so a chunk never splits a row. Returns `None` at end of input.
fn next_chunk<R: BufRead>(input: &mut R, size: usize) -> Result<Option<String>, Error> {
    let mut buf = vec![0u8; size];
    let n = read_fully(input, &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    buf.truncate(n);
    if n == size {
        input.read_until(b'\n', &mut buf)?;
    }
    let chunk = String::from_utf8(buf)
        .map_err(|e| Error::Other(format!("input is not valid UTF-8: {e}")))?;
    Ok(Some(chunk))
}

fn read_fully<R: io::Read>(input: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match input.read(&mut buf[total..])? {
            0 => break,
            n => total += n,
        }
    }
    Ok(total)
}

/// Run a compiled plan, writing the output header and rows to `output`.
///
/// A lone transform stage streams with `threads` workers (or single-threaded
/// when `threads <= 1`); anything with a `sort` runs the staged path.
pub fn run<R: BufRead, W: Write + Send>(
    plan: &Plan,
    out_header: &[String],
    opts: &RunOpts,
    input: &mut R,
    output: &mut W,
) -> Result<(), Error> {
    write_header(output, out_header)?;

    match plan.stages.as_slice() {
        [Stage::Transform(stmts)] if opts.threads > 1 => {
            stream_transform_parallel(stmts, opts.threads, opts.chunk_size, input, output)
        }
        [Stage::Transform(stmts)] => stream_transform(stmts, opts.chunk_size, input, output),
        _ => run_staged(plan, opts, input, output),
    }
}

fn write_header<W: Write>(output: &mut W, header: &[String]) -> Result<(), Error> {
    let row: Vec<Field> = header.iter().map(|s| Field::Str(s.as_str())).collect();
    let mut buf = String::new();
    csv::write_row(&mut buf, &row);
    output.write_all(buf.as_bytes())?;
    Ok(())
}

/// Stream a single transform stage: parse a chunk, apply, serialize, write.
fn stream_transform<R: BufRead, W: Write>(
    stmts: &[Stmt],
    chunk_size: usize,
    input: &mut R,
    output: &mut W,
) -> Result<(), Error> {
    let mut out_buf = String::new();
    while let Some(chunk) = next_chunk(input, chunk_size)? {
        out_buf.clear();
        let mut err: Option<Error> = None;
        csv::parse_chunk(&chunk, |row| {
            if err.is_some() {
                return;
            }
            match apply_transform(stmts, row) {
                Ok(true) => csv::write_row(&mut out_buf, row),
                Ok(false) => {}
                Err(e) => err = Some(e),
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        output.write_all(out_buf.as_bytes())?;
    }
    Ok(())
}

/// Parallel transform: the calling thread reads chunks and tags them with a
/// sequence id, `threads` workers parse + apply + serialize, and a writer
/// thread reassembles output in id order. Each worker owns its chunk, so the
/// borrowed rows never cross a thread boundary.
fn stream_transform_parallel<R: BufRead, W: Write + Send>(
    stmts: &[Stmt],
    threads: usize,
    chunk_size: usize,
    input: &mut R,
    output: &mut W,
) -> Result<(), Error> {
    let cap = threads * 2 + 1;
    let (chunk_tx, chunk_rx) = bounded::<(u64, String)>(cap);
    let (out_tx, out_rx) = bounded::<(u64, Result<String, Error>)>(cap);

    thread::scope(|scope| {
        for _ in 0..threads {
            let chunk_rx = chunk_rx.clone();
            let out_tx = out_tx.clone();
            scope.spawn(move || {
                while let Ok((id, chunk)) = chunk_rx.recv() {
                    let mut out_buf = String::new();
                    let mut err: Option<Error> = None;
                    csv::parse_chunk(&chunk, |row| {
                        if err.is_some() {
                            return;
                        }
                        match apply_transform(stmts, row) {
                            Ok(true) => csv::write_row(&mut out_buf, row),
                            Ok(false) => {}
                            Err(e) => err = Some(e),
                        }
                    });
                    let stop = err.is_some();
                    let msg = err.map_or(Ok(out_buf), Err);
                    if out_tx.send((id, msg)).is_err() || stop {
                        break;
                    }
                }
            });
        }
        // Drop the spare handles so the channels close once the spawned
        // workers (and the reader below) are done.
        drop(chunk_rx);
        drop(out_tx);

        let writer = scope.spawn(move || -> Result<(), Error> {
            let mut next = 0u64;
            let mut pending: HashMap<u64, String> = HashMap::new();
            let mut first_err: Option<Error> = None;
            while let Ok((id, res)) = out_rx.recv() {
                match res {
                    Ok(buf) => {
                        pending.insert(id, buf);
                        while let Some(buf) = pending.remove(&next) {
                            output.write_all(buf.as_bytes())?;
                            next += 1;
                        }
                    }
                    Err(e) if first_err.is_none() => first_err = Some(e),
                    Err(_) => {}
                }
            }
            first_err.map_or(Ok(()), Err)
        });

        // Reader: this thread feeds chunks until input is exhausted or errors.
        let mut id = 0u64;
        let mut read_err = None;
        loop {
            match next_chunk(input, chunk_size) {
                Ok(Some(chunk)) => {
                    if chunk_tx.send((id, chunk)).is_err() {
                        break;
                    }
                    id += 1;
                }
                Ok(None) => break,
                Err(e) => {
                    read_err = Some(e);
                    break;
                }
            }
        }
        drop(chunk_tx);

        let writer_result = writer.join().expect("writer thread panicked");
        read_err.map_or(Ok(()), Err).and(writer_result)
    })
}

/// Run a plan that contains a `sort`. The common case — exactly one sort —
/// streams input through the pre-sort transforms into an [`ExternalSorter`]
/// (which spills to temp files when large), then streams the merged output
/// through the post-sort transforms. Plans with more than one sort fall back to
/// the simpler in-memory path.
fn run_staged<R: BufRead, W: Write>(
    plan: &Plan,
    opts: &RunOpts,
    input: &mut R,
    output: &mut W,
) -> Result<(), Error> {
    let sort_count = plan
        .stages
        .iter()
        .filter(|s| matches!(s, Stage::Sort(_)))
        .count();
    if sort_count != 1 {
        return run_staged_in_memory(plan, opts.chunk_size, input, output);
    }

    let sort_idx = plan
        .stages
        .iter()
        .position(|s| matches!(s, Stage::Sort(_)))
        .unwrap();
    let pre = transform_stmts(&plan.stages[..sort_idx]);
    let Stage::Sort(sort) = &plan.stages[sort_idx] else {
        unreachable!()
    };
    let post = transform_stmts(&plan.stages[sort_idx + 1..]);

    // Feed the pre-sort survivors into the sorter.
    let mut sorter = ExternalSorter::with_budget(sort, opts.temp_dir.clone(), opts.sort_buffer);
    while let Some(chunk) = next_chunk(input, opts.chunk_size)? {
        let mut feed_err: Option<Error> = None;
        csv::parse_chunk(&chunk, |row| {
            if feed_err.is_some() {
                return;
            }
            match apply_transform(pre, row) {
                Ok(true) => {
                    let owned: OwnedRow = row.iter().map(|f| f.clone().into_owned()).collect();
                    if let Err(e) = sorter.push(owned) {
                        feed_err = Some(e);
                    }
                }
                Ok(false) => {}
                Err(e) => feed_err = Some(e),
            }
        });
        if let Some(e) = feed_err {
            return Err(e);
        }
    }

    // Stream the sorted rows through the post-sort transforms.
    let mut out_buf = String::new();
    for row in sorter.finish()? {
        let mut row = row?;
        if apply_transform(post, &mut row)? {
            csv::write_row(&mut out_buf, &row);
            if out_buf.len() >= 1 << 16 {
                output.write_all(out_buf.as_bytes())?;
                out_buf.clear();
            }
        }
    }
    output.write_all(out_buf.as_bytes())?;
    Ok(())
}

/// The statements of an optional single transform stage (empty otherwise).
fn transform_stmts(stages: &[Stage]) -> &[Stmt] {
    match stages {
        [Stage::Transform(stmts)] => stmts,
        _ => &[],
    }
}

/// Materialize all rows, run each stage in turn, then serialize. The fallback
/// for plans with more than one `sort`.
fn run_staged_in_memory<R: BufRead, W: Write>(
    plan: &Plan,
    chunk_size: usize,
    input: &mut R,
    output: &mut W,
) -> Result<(), Error> {
    let mut rows = materialize(chunk_size, input)?;
    for stage in &plan.stages {
        match stage {
            Stage::Transform(stmts) => {
                let mut kept = Vec::with_capacity(rows.len());
                for mut row in rows.drain(..) {
                    if apply_transform(stmts, &mut row)? {
                        kept.push(row);
                    }
                }
                rows = kept;
            }
            Stage::Sort(sort) => sort_rows(sort, &mut rows)?,
        }
    }
    write_rows(output, &rows)
}

/// Read all input rows as owned values.
fn materialize<R: BufRead>(chunk_size: usize, input: &mut R) -> Result<Vec<OwnedRow>, Error> {
    let mut rows: Vec<OwnedRow> = Vec::new();
    while let Some(chunk) = next_chunk(input, chunk_size)? {
        csv::parse_chunk(&chunk, |row| {
            rows.push(row.iter().map(|f| f.clone().into_owned()).collect());
        });
    }
    Ok(rows)
}

/// Convert the numeric sort-key columns once, then stable-sort.
fn sort_rows(sort: &SortStmt, rows: &mut [OwnedRow]) -> Result<(), Error> {
    let numeric: Vec<usize> = sort.numeric_positions().collect();
    for row in rows.iter_mut() {
        for &p in &numeric {
            if let Some(f) = row.get_mut(p) {
                *f = Field::Num(f.coerce_num()?);
            }
        }
    }
    rows.sort_by(|a, b| sort.compare(a, b));
    Ok(())
}

fn write_rows<W: Write>(output: &mut W, rows: &[OwnedRow]) -> Result<(), Error> {
    let mut buf = String::new();
    for row in rows {
        csv::write_row(&mut buf, row);
        if buf.len() >= 1 << 16 {
            output.write_all(buf.as_bytes())?;
            buf.clear();
        }
    }
    output.write_all(buf.as_bytes())?;
    Ok(())
}

/// Render a resolved plan for `--print-engine`: stages, their statements, and
/// the column positions each one resolved to.
pub fn describe(plan: &Plan) -> String {
    let mut out = String::new();
    for (i, stage) in plan.stages.iter().enumerate() {
        let n = i + 1;
        match stage {
            Stage::Transform(stmts) => {
                out.push_str(&format!("stage {n} (transform):\n"));
                for (j, stmt) in stmts.iter().enumerate() {
                    out.push_str(&format!("  {n}.{} {}\n", j + 1, describe_stmt(stmt)));
                }
            }
            Stage::Sort(sort) => {
                out.push_str(&format!("stage {n} (sort):\n"));
                out.push_str(&format!("  {n}.1 {}\n", describe_sort(sort)));
            }
        }
    }
    out
}

fn describe_stmt(stmt: &Stmt) -> String {
    match stmt {
        Stmt::Cols(p) => {
            let verb = if p.exclude { "drop-cols" } else { "cols" };
            format!("{verb} -> keep positions {:?}", p.positions)
        }
        Stmt::Select(e) => format!("select {}", fmt_expr(e)),
        Stmt::ToNum(c) => format!("to-num {:?} (positions {:?})", c.names, c.positions),
        Stmt::ToStr(c) => format!("to-str {:?} (positions {:?})", c.names, c.positions),
    }
}

fn describe_sort(sort: &SortStmt) -> String {
    let keys: Vec<String> = sort
        .keys
        .iter()
        .map(|k| {
            let mut s = format!("{}[{}]", k.name, k.pos);
            if k.descending {
                s.push_str(" reverse");
            }
            if k.numeric {
                s.push_str(" numeric");
            }
            s
        })
        .collect();
    format!("sort {}", keys.join(", "))
}

fn fmt_expr(e: &BoolExpr) -> String {
    match e {
        BoolExpr::And(es) => format!("(and {})", fmt_list(es)),
        BoolExpr::Or(es) => format!("(or {})", fmt_list(es)),
        BoolExpr::Not(e) => format!("(not {})", fmt_expr(e)),
        BoolExpr::Cmp(c) => format!(
            "({} {} {})",
            cmp_symbol(c.op),
            fmt_operand(&c.lhs),
            fmt_operand(&c.rhs)
        ),
        BoolExpr::Match { col, negate, .. } => {
            let op = if *negate { "!~" } else { "=~" };
            format!("({op} {}[{}] /regex/)", col.name, col.pos)
        }
    }
}

fn fmt_list(es: &[BoolExpr]) -> String {
    es.iter().map(fmt_expr).collect::<Vec<_>>().join(" ")
}

fn fmt_operand(op: &Operand) -> String {
    match op {
        Operand::Col(c) => format!("{}[{}]", c.name, c.pos),
        Operand::Str(s) => format!("{s:?}"),
        Operand::Num(n) => n.to_string(),
    }
}

fn cmp_symbol(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "==",
        CmpOp::Ne => "!=",
        CmpOp::Lt => "<",
        CmpOp::Gt => ">",
        CmpOp::Le => "<=",
        CmpOp::Ge => ">=",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compile;

    /// Compile, resolve against the input's header, and run end to end.
    fn run_str(script: &str, input: &str) -> Result<String, Error> {
        run_with(script, input, 1, 1_000_000)
    }

    fn run_with(script: &str, input: &str, threads: usize, chunk: usize) -> Result<String, Error> {
        let mut plan = compile(script)?;
        let mut reader = io::BufReader::new(input.as_bytes());
        let header = read_header(&mut reader)?;
        let out_header = plan.resolve(&header)?;
        let mut out = Vec::new();
        let opts = RunOpts {
            chunk_size: chunk,
            threads,
            temp_dir: std::env::temp_dir(),
            sort_buffer: crate::sort::DEFAULT_BUDGET_BYTES,
        };
        run(&plan, &out_header, &opts, &mut reader, &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    /// Run a sort plan with a tiny sort buffer so the external merge (temp-file
    /// spilling) path is exercised end to end.
    fn run_spilled(script: &str, input: &str) -> Result<String, Error> {
        let mut plan = compile(script)?;
        let mut reader = io::BufReader::new(input.as_bytes());
        let header = read_header(&mut reader)?;
        let out_header = plan.resolve(&header)?;
        let mut out = Vec::new();
        let opts = RunOpts {
            chunk_size: 64,
            threads: 1,
            temp_dir: std::env::temp_dir(),
            sort_buffer: 1, // spill after every row
        };
        run(&plan, &out_header, &opts, &mut reader, &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    const INPUT: &str = "id,fieldA,countZ\n1,t,5\n2,f,0\n3,t,0\n4,t,9\n";

    #[test]
    fn cols_reorder_and_drop() {
        assert_eq!(
            run_str("(cols countZ id)", INPUT).unwrap(),
            "countZ,id\n5,1\n0,2\n0,3\n9,4\n"
        );
        assert_eq!(
            run_str("(drop-cols fieldA)", INPUT).unwrap(),
            "id,countZ\n1,5\n2,0\n3,0\n4,9\n"
        );
    }

    #[test]
    fn select_string_and_implicit_numeric() {
        assert_eq!(
            run_str(r#"(select (== fieldA "t"))"#, INPUT).unwrap(),
            "id,fieldA,countZ\n1,t,5\n3,t,0\n4,t,9\n"
        );
        // Implicit numeric: no to-num needed.
        assert_eq!(
            run_str(r#"(select (and (== fieldA "t") (> countZ 0)))"#, INPUT).unwrap(),
            "id,fieldA,countZ\n1,t,5\n4,t,9\n"
        );
    }

    #[test]
    fn numeric_sort_then_filter() {
        // sort by countZ descending, numerically; output numbers print cleanly.
        assert_eq!(
            run_str("(sort-by (countZ :reverse :numeric))", INPUT).unwrap(),
            "id,fieldA,countZ\n4,t,9\n1,t,5\n2,f,0\n3,t,0\n"
        );
    }

    #[test]
    fn pipeline_with_sort_stage_split() {
        // filter, sort, then drop a column — three stages.
        let out = run_str(
            "(select (== fieldA \"t\")) (sort-by (countZ :n)) (drop-cols fieldA)",
            INPUT,
        )
        .unwrap();
        assert_eq!(out, "id,countZ\n3,0\n1,5\n4,9\n");
    }

    #[test]
    fn lexical_vs_numeric_sort() {
        let input = "n\n10\n9\n100\n";
        // numeric: 9 < 10 < 100
        assert_eq!(
            run_str("(sort-by (n :numeric))", input).unwrap(),
            "n\n9\n10\n100\n"
        );
        // lexical (default): "10" < "100" < "9"
        assert_eq!(run_str("(sort-by n)", input).unwrap(), "n\n10\n100\n9\n");
    }

    #[test]
    fn external_sort_spilling_matches_in_memory() {
        let mut input = String::from("id,val\n");
        for i in 0..1000 {
            input.push_str(&format!("{i},{}\n", (i * 37) % 1000));
        }
        let script = "(sort-by (val :numeric) id)";
        let in_memory = run_str(script, &input).unwrap();
        let spilled = run_spilled(script, &input).unwrap();
        assert_eq!(in_memory, spilled);
        // Spot-check: smallest val (0) sorts first after the header.
        assert!(spilled.starts_with("id,val\n"));
        let first_data = spilled.lines().nth(1).unwrap();
        assert!(first_data.ends_with(",0"));
    }

    #[test]
    fn parallel_preserves_order_and_matches_serial() {
        // Many rows + a tiny chunk size forces multiple chunks across workers;
        // output must come back in input order regardless of thread count.
        let mut input = String::from("id,keep\n");
        for i in 0..5000 {
            input.push_str(&format!("{i},{}\n", i % 2));
        }
        let script = r#"(select (== keep "1"))"#;
        let serial = run_with(script, &input, 1, 1_000_000).unwrap();
        let parallel = run_with(script, &input, 8, 64).unwrap();
        assert_eq!(serial, parallel);
        // Spot-check ordering: first data rows are 1, 3, 5, ...
        assert!(parallel.starts_with("id,keep\n1,1\n3,1\n5,1\n"));
    }

    #[test]
    fn missing_column_is_an_error() {
        assert!(matches!(
            run_str("(cols nope)", INPUT),
            Err(Error::Column(_))
        ));
    }
}
