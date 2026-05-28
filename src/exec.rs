//! Executing a compiled [`Plan`] over CSV input.
//!
//! This is the single-threaded baseline. A plan that is a single transform
//! stage streams chunk-by-chunk with borrowed rows (zero-copy); a plan with a
//! `sort` (or otherwise multiple stages) materializes rows as owned values and
//! runs stage by stage. Parallelism and external-merge sort are layered on in
//! later modules.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;

use crossbeam_channel::bounded;
use memchr::memchr;

use crate::color::Style;
use crate::csv;
use crate::error::Error;
use crate::field::Field;
use crate::plan::{
    BoolExpr, CmpOp, ColorRule, ColorScope, Operand, OutputFormat, Plan, SortStmt, Stage,
    StatsStmt, Stmt, apply_stmts,
};
use crate::sort::Sorter;
use crate::stats::ColStats;

/// An owned row, detached from any chunk buffer (used by the in-memory
/// multi-sort fallback).
type OwnedRow = Vec<Field<'static>>;

/// Knobs for a run: chunk size, worker count, and where sort spills its temp
/// files.
#[derive(Clone, Debug)]
pub struct RunOpts {
    pub chunk_size: usize,
    pub threads: usize,
    pub temp_dir: PathBuf,
    pub sort_buffer: usize,
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

/// Like [`next_chunk`], but returns as soon as the reader yields *any* data
/// (a single `read`, then completed to a line boundary) instead of blocking
/// until `size` bytes arrive. The streaming paths (`head` and a lone transform)
/// read through this so they emit promptly on a slow or unbounded stream rather
/// than waiting to fill a whole chunk that may never come — `head` could stop
/// early but would block (see the head regression test), and a streaming
/// `select`/`cols` would withhold all output until a megabyte accumulated.
/// (`sort` and the in-memory fallback must read all input first, so they stay
/// on the throughput-batched [`next_chunk`].)
fn next_chunk_available<R: BufRead>(input: &mut R, size: usize) -> Result<Option<String>, Error> {
    let mut buf = vec![0u8; size];
    let n = input.read(&mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    buf.truncate(n);
    // A single read may stop mid-line; finish that line so the chunk ends on a
    // row boundary. This blocks only until the current line completes, not for
    // a whole chunk.
    if buf.last() != Some(&b'\n') {
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

/// Run a compiled plan over a reader (e.g. stdin), writing header and rows to
/// `output`. A seekable file should go through [`run_file`], which can shard.
pub fn run<R: BufRead, W: Write + Send>(
    plan: &Plan,
    out_header: &[String],
    opts: &RunOpts,
    input: &mut R,
    output: &mut W,
) -> Result<(), Error> {
    write_header(output, out_header)?;
    run_body(plan, opts, input, output)
}

/// Dispatch the body of a run over a reader (header already written).
fn run_body<R: BufRead, W: Write + Send>(
    plan: &Plan,
    opts: &RunOpts,
    input: &mut R,
    output: &mut W,
) -> Result<(), Error> {
    // `head` with no sort streams single-threaded and stops early.
    if let Some((pre, n, post)) = head_only_shape(plan) {
        return stream_head(pre, n, post, opts.chunk_size, input, output);
    }
    // `stats` reduces the stream to a tiny profile; stream the input through it
    // (O(columns) memory) and run any following stages over that profile.
    if let Some((pre, stats, post)) = stats_shape(plan) {
        return run_stats_streaming(pre, stats, post, opts.chunk_size, input, output);
    }
    match plan.stages.as_slice() {
        [Stage::Transform(stmts)] if opts.threads > 1 => {
            stream_transform_parallel(stmts, opts.threads, opts.chunk_size, input, output)
        }
        [Stage::Transform(stmts)] => stream_transform(stmts, opts.chunk_size, input, output),
        _ => run_staged(plan, opts, input, output),
    }
}

/// If the plan is `[Transform?, Head(n), Transform?]` with no sort, return the
/// pre-head statements, the limit, and the post-head statements.
fn head_only_shape(plan: &Plan) -> Option<(&[Stmt], usize, &[Stmt])> {
    if plan
        .stages
        .iter()
        .any(|s| matches!(s, Stage::Sort(_) | Stage::Stats(_)))
    {
        return None;
    }
    let heads: Vec<usize> = plan
        .stages
        .iter()
        .enumerate()
        .filter(|(_, s)| matches!(s, Stage::Head(_)))
        .map(|(i, _)| i)
        .collect();
    let [hi] = heads[..] else { return None };
    let Stage::Head(n) = plan.stages[hi] else {
        return None;
    };
    Some((
        transform_stmts(&plan.stages[..hi]),
        n,
        transform_stmts(&plan.stages[hi + 1..]),
    ))
}

/// Stream `[pre | head n | post]` single-threaded, stopping once `n` rows have
/// reached the head. Reads only as much input as it needs (via
/// [`next_chunk_available`]) so it emits and stops promptly on a stream rather
/// than blocking for a full chunk.
fn stream_head<R: BufRead, W: Write>(
    pre: &[Stmt],
    n: usize,
    post: &[Stmt],
    chunk_size: usize,
    input: &mut R,
    output: &mut W,
) -> Result<(), Error> {
    let mut taken = 0usize;
    let mut out_buf = String::new();
    while taken < n {
        let Some(chunk) = next_chunk_available(input, chunk_size)? else {
            break;
        };
        out_buf.clear();
        let mut scratch: Vec<Field> = Vec::new();
        let mut err: Option<Error> = None;
        csv::parse_chunk(&chunk, |row| {
            if err.is_some() || taken >= n {
                return;
            }
            match apply_stmts(pre, row, &mut scratch) {
                Ok(true) => {
                    taken += 1;
                    match apply_stmts(post, row, &mut scratch) {
                        Ok(true) => csv::write_row(&mut out_buf, row),
                        Ok(false) => {}
                        Err(e) => err = Some(e),
                    }
                }
                Ok(false) => {}
                Err(e) => err = Some(e),
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        output.write_all(out_buf.as_bytes())?;
        output.flush()?; // don't let a live/slow stream's output sit in the BufWriter
    }
    Ok(())
}

/// If the plan has exactly one `stats` stage with nothing blocking before it (an
/// optional leading transform only), return the pre-stats statements, the stats
/// stage, and the stages that follow it. Anything else (a sort/head before
/// stats, or multiple stats) returns `None` and takes the in-memory path.
fn stats_shape(plan: &Plan) -> Option<(&[Stmt], &StatsStmt, &[Stage])> {
    let idxs: Vec<usize> = plan
        .stages
        .iter()
        .enumerate()
        .filter(|(_, s)| matches!(s, Stage::Stats(_)))
        .map(|(i, _)| i)
        .collect();
    let [si] = idxs[..] else { return None };
    let pre: &[Stmt] = match &plan.stages[..si] {
        [] => &[],
        [Stage::Transform(stmts)] => stmts,
        _ => return None,
    };
    let Stage::Stats(stats) = &plan.stages[si] else {
        return None;
    };
    Some((pre, stats, &plan.stages[si + 1..]))
}

/// Stream `[pre | stats]`, folding each surviving row into per-column
/// accumulators (O(columns) memory, not O(rows)), then run the post-stats
/// stages over the tiny profile and write it. `stats` must read all input, so it
/// uses the throughput-batched [`next_chunk`].
fn run_stats_streaming<R: BufRead, W: Write>(
    pre: &[Stmt],
    stats: &StatsStmt,
    post: &[Stage],
    chunk_size: usize,
    input: &mut R,
    output: &mut W,
) -> Result<(), Error> {
    let mut accs: Vec<ColStats> = stats.positions.iter().map(|_| ColStats::new()).collect();
    while let Some(chunk) = next_chunk(input, chunk_size)? {
        let mut scratch: Vec<Field> = Vec::new();
        let mut err: Option<Error> = None;
        csv::parse_chunk(&chunk, |row| {
            if err.is_some() {
                return;
            }
            match apply_stmts(pre, row, &mut scratch) {
                Ok(true) => accumulate(&mut accs, &stats.positions, row),
                Ok(false) => {}
                Err(e) => err = Some(e),
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
    }
    let rows = apply_stages_over_rows(post, profile_rows(stats, &accs))?;
    write_rows(output, &rows)
}

/// Fold one row's profiled cells into the matching accumulators.
#[inline]
fn accumulate(accs: &mut [ColStats], positions: &[usize], row: &[Field]) {
    for (a, &p) in accs.iter_mut().zip(positions) {
        match row.get(p) {
            Some(f) => a.update(f),
            None => a.update(&Field::Str("")),
        }
    }
}

/// Build accumulators over already-materialized rows (the in-memory path).
fn build_colstats(positions: &[usize], rows: &[OwnedRow]) -> Vec<ColStats> {
    let mut accs: Vec<ColStats> = positions.iter().map(|_| ColStats::new()).collect();
    for row in rows {
        accumulate(&mut accs, positions, row);
    }
    accs
}

/// Turn finalized accumulators into the profile rows (one per profiled column).
fn profile_rows(stats: &StatsStmt, accs: &[ColStats]) -> Vec<OwnedRow> {
    stats
        .names
        .iter()
        .zip(accs)
        .map(|(name, a)| a.to_row(name))
        .collect()
}

/// Read and parse the header of a file, returning the header, the byte offset
/// where the data rows begin, and the file length.
pub fn read_header_from_path(path: &Path) -> Result<(Vec<String>, u64, u64), Error> {
    let file_len = std::fs::metadata(path)?.len();
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line)?;
    if line.is_empty() {
        return Err(Error::Other("could not read header (empty input)".into()));
    }
    let data_start = line.len() as u64; // bytes consumed including the newline
    let text = std::str::from_utf8(&line)
        .map_err(|e| Error::Other(format!("header is not valid UTF-8: {e}")))?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    Ok((csv::parse_header(text), data_start, file_len))
}

/// Run a plan over a seekable file. A lone transform stage with `threads > 1`
/// is **sharded**: each worker reads its own byte range, with no central reader
/// or channel. Everything else falls back to the reader-based path.
pub fn run_file<W: Write + Send>(
    plan: &Plan,
    out_header: &[String],
    opts: &RunOpts,
    path: &Path,
    data_start: u64,
    file_len: u64,
    output: &mut W,
) -> Result<(), Error> {
    write_header(output, out_header)?;

    if let [Stage::Transform(stmts)] = plan.stages.as_slice()
        && opts.threads > 1
    {
        return run_sharded(stmts, opts.threads, path, data_start, file_len, output);
    }

    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(data_start))?;
    let mut reader = BufReader::new(file);
    run_body(plan, opts, &mut reader, output)
}

fn write_header<W: Write>(output: &mut W, header: &[String]) -> Result<(), Error> {
    let row: Vec<Field> = header.iter().map(|s| Field::Str(s.as_str())).collect();
    let mut buf = String::new();
    csv::write_row(&mut buf, &row);
    output.write_all(buf.as_bytes())?;
    Ok(())
}

/// Stream a single transform stage: parse a chunk, apply, serialize, write.
/// Reads via [`next_chunk_available`] so output flows as input arrives rather
/// than only after a full chunk buffers (matters on a live or slow stream).
fn stream_transform<R: BufRead, W: Write>(
    stmts: &[Stmt],
    chunk_size: usize,
    input: &mut R,
    output: &mut W,
) -> Result<(), Error> {
    let mut out_buf = String::new();
    while let Some(chunk) = next_chunk_available(input, chunk_size)? {
        out_buf.clear();
        let mut scratch: Vec<Field> = Vec::new();
        let mut err: Option<Error> = None;
        csv::parse_chunk(&chunk, |row| {
            if err.is_some() {
                return;
            }
            match apply_stmts(stmts, row, &mut scratch) {
                Ok(true) => csv::write_row(&mut out_buf, row),
                Ok(false) => {}
                Err(e) => err = Some(e),
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        output.write_all(out_buf.as_bytes())?;
        output.flush()?; // don't let a live/slow stream's output sit in the BufWriter
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
                    let mut scratch: Vec<Field> = Vec::new();
                    let mut err: Option<Error> = None;
                    csv::parse_chunk(&chunk, |row| {
                        if err.is_some() {
                            return;
                        }
                        match apply_stmts(stmts, row, &mut scratch) {
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
                        let mut wrote = false;
                        while let Some(buf) = pending.remove(&next) {
                            output.write_all(buf.as_bytes())?;
                            next += 1;
                            wrote = true;
                        }
                        // Flush once the in-order prefix advanced, so a live
                        // stream sees output without waiting for the BufWriter.
                        if wrote {
                            output.flush()?;
                        }
                    }
                    Err(e) if first_err.is_none() => first_err = Some(e),
                    Err(_) => {}
                }
            }
            first_err.map_or(Ok(()), Err)
        });

        // Reader: feed chunks as input arrives (a single read each, not a full
        // buffer) until input is exhausted or errors, so the workers and writer
        // make progress on a live stream instead of waiting for a 1 MB fill.
        let mut id = 0u64;
        let mut read_err = None;
        loop {
            match next_chunk_available(input, chunk_size) {
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

/// Sharded transform: split the file's data region into `threads` line-aligned
/// byte ranges and process each on its own thread (no central reader, no
/// channel). Shard outputs are concatenated in file order, preserving row
/// order.
fn run_sharded<W: Write>(
    stmts: &[Stmt],
    threads: usize,
    path: &Path,
    data_start: u64,
    file_len: u64,
    output: &mut W,
) -> Result<(), Error> {
    let ranges = shard_ranges(path, data_start, file_len, threads)?;
    if ranges.is_empty() {
        return Ok(()); // header only, no data rows
    }
    let results: Vec<Result<String, Error>> = thread::scope(|scope| {
        let handles: Vec<_> = ranges
            .into_iter()
            .map(|(start, end)| scope.spawn(move || process_range(stmts, path, start, end)))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(Error::Other("shard worker panicked".into())))
            })
            .collect()
    });
    for result in results {
        output.write_all(result?.as_bytes())?;
    }
    Ok(())
}

/// Divide `[data_start, file_len)` into up to `n` contiguous ranges, each
/// starting and ending on a line boundary, so every row falls in exactly one
/// shard. Empty ranges (more threads than lines) are dropped.
fn shard_ranges(
    path: &Path,
    data_start: u64,
    file_len: u64,
    n: usize,
) -> Result<Vec<(u64, u64)>, Error> {
    if data_start >= file_len {
        return Ok(Vec::new());
    }
    let mut file = File::open(path)?;
    let total = file_len - data_start;
    let mut bounds = Vec::with_capacity(n + 1);
    bounds.push(data_start);
    for i in 1..n {
        let nominal = data_start + total * i as u64 / n as u64;
        bounds.push(snap_to_newline(&mut file, nominal, file_len)?);
    }
    bounds.push(file_len);

    let mut ranges = Vec::with_capacity(n);
    for w in bounds.windows(2) {
        if w[0] < w[1] {
            ranges.push((w[0], w[1]));
        }
    }
    Ok(ranges)
}

/// The byte offset of the start of the line following `pos` (i.e. just past the
/// next `\n` at or after `pos`). Returns `file_len` if no newline follows.
fn snap_to_newline(file: &mut File, pos: u64, file_len: u64) -> Result<u64, Error> {
    if pos >= file_len {
        return Ok(file_len);
    }
    file.seek(SeekFrom::Start(pos))?;
    let mut buf = [0u8; 64 * 1024];
    let mut scanned = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(file_len);
        }
        if let Some(rel) = memchr(b'\n', &buf[..n]) {
            return Ok(pos + scanned + rel as u64 + 1);
        }
        scanned += n as u64;
    }
}

/// Read one shard's byte range, parse it, apply the statements, and return the
/// serialized survivors.
fn process_range(stmts: &[Stmt], path: &Path, start: u64, end: u64) -> Result<String, Error> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((end - start) as usize);
    file.take(end - start).read_to_end(&mut bytes)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| Error::Other(format!("input is not valid UTF-8: {e}")))?;

    let mut out = String::with_capacity(bytes.len() / 2 + 64);
    let mut scratch: Vec<Field> = Vec::new();
    let mut err: Option<Error> = None;
    csv::parse_chunk(text, |row| {
        if err.is_some() {
            return;
        }
        match apply_stmts(stmts, row, &mut scratch) {
            Ok(true) => csv::write_row(&mut out, row),
            Ok(false) => {}
            Err(e) => err = Some(e),
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(out),
    }
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
    let has_head = plan.stages.iter().any(|s| matches!(s, Stage::Head(_)));
    let has_stats = plan.stages.iter().any(|s| matches!(s, Stage::Stats(_)));
    // The streaming sort path handles exactly one sort and no head/stats;
    // anything else (head, stats, or multiple sorts) materializes.
    if sort_count != 1 || has_head || has_stats {
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

    // Feed raw input blocks to the sorter; its workers parse, apply the
    // pre-sort statements, and sort each block in parallel.
    let mut sorter = Sorter::new(
        sort,
        pre,
        opts.threads,
        opts.temp_dir.clone(),
        opts.sort_buffer,
    );
    let block_size = sorter.block_size();
    while let Some(block) = next_chunk(input, block_size)? {
        sorter.push_block(block);
    }

    // The merge hands us already-serialized line bytes (workers serialized them
    // in parallel). With no post-sort statements we write them straight out;
    // otherwise we re-parse each line, apply, and re-serialize.
    let mut out_buf: Vec<u8> = Vec::new();
    sorter.finish()?.for_each_line(|line| {
        if post.is_empty() {
            out_buf.extend_from_slice(line);
        } else {
            apply_post_to_line(post, line, &mut out_buf)?;
        }
        if out_buf.len() >= 1 << 16 {
            output.write_all(&out_buf)?;
            out_buf.clear();
        }
        Ok(())
    })?;
    output.write_all(&out_buf)?;
    Ok(())
}

/// Re-parse one merged line, apply the post-sort statements, and append the
/// re-serialized survivors to `out`. Only used when a `sort` has trailing
/// transforms; the common pure-sort path writes line bytes directly.
fn apply_post_to_line(post: &[Stmt], line: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
    let text = std::str::from_utf8(line)
        .map_err(|e| Error::Other(format!("input is not valid UTF-8: {e}")))?;
    let mut err: Option<Error> = None;
    let mut buf = String::new();
    let mut scratch: Vec<Field> = Vec::new();
    csv::parse_chunk(text, |row| {
        if err.is_some() {
            return;
        }
        match apply_stmts(post, row, &mut scratch) {
            Ok(true) => {
                buf.clear();
                csv::write_row(&mut buf, row);
                out.extend_from_slice(buf.as_bytes());
            }
            Ok(false) => {}
            Err(e) => err = Some(e),
        }
    });
    err.map_or(Ok(()), Err)
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
    let rows = materialize(chunk_size, input)?;
    let rows = apply_stages_over_rows(&plan.stages, rows)?;
    write_rows(output, &rows)
}

/// Run a sequence of stages over already-materialized rows, returning the final
/// rows. The in-memory fallback uses it for the whole plan; the streaming stats
/// path uses it for the (tiny) post-stats stages.
fn apply_stages_over_rows(
    stages: &[Stage],
    mut rows: Vec<OwnedRow>,
) -> Result<Vec<OwnedRow>, Error> {
    for stage in stages {
        match stage {
            Stage::Transform(stmts) => {
                let mut kept = Vec::with_capacity(rows.len());
                let mut scratch: Vec<Field> = Vec::new();
                for mut row in rows.drain(..) {
                    if apply_stmts(stmts, &mut row, &mut scratch)? {
                        kept.push(row);
                    }
                }
                rows = kept;
            }
            Stage::Sort(sort) => sort_rows(sort, &mut rows)?,
            Stage::Head(n) => rows.truncate(*n),
            Stage::Stats(s) => {
                let accs = build_colstats(&s.positions, &rows);
                rows = profile_rows(s, &accs);
            }
        }
    }
    Ok(rows)
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

/// Render buffered output `bytes` to `output`, applying the plan's colour rules
/// (when `color` is on) and aligning columns when the plan's output is
/// `Aligned`. Width is measured by *visible* characters, so ANSI escapes never
/// throw off alignment.
pub fn render<W: Write>(
    bytes: &[u8],
    plan: &Plan,
    color: bool,
    output: &mut W,
) -> Result<(), Error> {
    let aligned = plan.output == OutputFormat::Aligned;
    let want_color = color && !plan.colors.is_empty();
    if !aligned && !want_color {
        // Nothing to do but copy the bytes through (the caller only buffers when
        // there is something to render, so this is just a safety net).
        output.write_all(bytes)?;
        return Ok(());
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|e| Error::Other(format!("output is not valid UTF-8: {e}")))?;
    let mut rows: Vec<Vec<String>> = Vec::new();
    csv::parse_chunk(text, |r| {
        rows.push(r.iter().map(|f| f.as_str().into_owned()).collect());
    });

    let styles = if want_color {
        Some(compute_styles(&plan.colors, &rows)?)
    } else {
        None
    };
    if aligned {
        align_and_write(&rows, styles.as_deref(), output)
    } else {
        write_csv_colored(&rows, styles.as_deref(), output)
    }
}

/// The per-cell [`Style`] grid (`[row][col]`) for the colour rules over `rows`.
/// The header row (index 0) is never styled. Rules apply in order; each layers
/// onto what earlier rules set (last wins per attribute).
fn compute_styles(rules: &[ColorRule], rows: &[Vec<String>]) -> Result<Vec<Vec<Style>>, Error> {
    let ncols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut styles = vec![vec![Style::default(); ncols]; rows.len()];
    for rule in rules {
        match rule {
            ColorRule::Predicate { scope, style, expr } => {
                for (ri, row) in rows.iter().enumerate().skip(1) {
                    let frow: Vec<Field> = row.iter().map(|s| Field::Str(s)).collect();
                    if !expr.eval(&frow)? {
                        continue;
                    }
                    match scope {
                        ColorScope::Row => {
                            for cell in &mut styles[ri] {
                                *cell = cell.over(*style);
                            }
                        }
                        ColorScope::Cell(c) => {
                            if let Some(cell) = styles[ri].get_mut(c.pos) {
                                *cell = cell.over(*style);
                            }
                        }
                    }
                }
            }
            ColorRule::Gradient { col, ramp, bounds } => {
                let (lo, hi) = (*bounds).unwrap_or_else(|| column_minmax(rows, col.pos));
                for (ri, row) in rows.iter().enumerate().skip(1) {
                    if let Some(v) = row.get(col.pos).and_then(|c| c.trim().parse::<f64>().ok())
                        && let Some(cell) = styles[ri].get_mut(col.pos)
                    {
                        *cell = cell.over(ramp.at(v, lo, hi));
                    }
                }
            }
        }
    }
    Ok(styles)
}

/// Default gradient bounds: a column's numeric min/max, computed by reusing the
/// `stats` accumulator. Falls back to `0..1` for a non-numeric column (whose
/// cells won't parse, so nothing is painted anyway).
fn column_minmax(rows: &[Vec<String>], pos: usize) -> (f64, f64) {
    let mut acc = ColStats::new();
    for row in rows.iter().skip(1) {
        if let Some(cell) = row.get(pos) {
            acc.update(&Field::Str(cell));
        }
    }
    acc.num_range().unwrap_or((0.0, 1.0))
}

/// Look up the style for cell `[ri][ci]`, defaulting to empty.
#[inline]
fn style_at(styles: Option<&[Vec<Style>]>, ri: usize, ci: usize) -> Style {
    styles
        .and_then(|s| s.get(ri))
        .and_then(|r| r.get(ci))
        .copied()
        .unwrap_or_default()
}

/// Whitespace-align columns (`fmt` / `column -t`): each column padded to its
/// widest cell, two spaces between. A numeric column (every data cell reads as a
/// number) is right-justified; text columns left-justified, trailing column
/// unpadded. Padding is by visible width; the painted text carries the colour.
fn align_and_write<W: Write>(
    rows: &[Vec<String>],
    styles: Option<&[Vec<Style>]>,
    output: &mut W,
) -> Result<(), Error> {
    let ncols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; ncols];
    for row in rows {
        for (i, field) in row.iter().enumerate() {
            widths[i] = widths[i].max(field.chars().count());
        }
    }

    // Right-justify a column when every data cell reads as a number (blanks
    // allowed, but at least one must be a real number).
    let numeric: Vec<bool> = (0..ncols)
        .map(|i| {
            let mut saw_number = false;
            for row in rows.iter().skip(1) {
                let cell = row.get(i).map_or("", String::as_str);
                if Field::Str(cell).coerce_num().is_err() {
                    return false;
                }
                saw_number |= !cell.trim().is_empty();
            }
            saw_number
        })
        .collect();

    let mut line = String::new();
    for (ri, row) in rows.iter().enumerate() {
        line.clear();
        let last = row.len().saturating_sub(1);
        for (i, field) in row.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            let pad = widths[i].saturating_sub(field.chars().count());
            let painted = style_at(styles, ri, i).paint(field);
            if numeric[i] {
                // Right-justify: pad on the left (so never a trailing space).
                for _ in 0..pad {
                    line.push(' ');
                }
                line.push_str(&painted);
            } else {
                line.push_str(&painted);
                // Left-justify: pad on the right, except the last column.
                if i != last {
                    for _ in 0..pad {
                        line.push(' ');
                    }
                }
            }
        }
        line.push('\n');
        output.write_all(line.as_bytes())?;
    }
    Ok(())
}

/// Write CSV rows with each cell painted by its style (for colouring plain,
/// non-aligned output).
fn write_csv_colored<W: Write>(
    rows: &[Vec<String>],
    styles: Option<&[Vec<Style>]>,
    output: &mut W,
) -> Result<(), Error> {
    let mut line = String::new();
    for (ri, row) in rows.iter().enumerate() {
        line.clear();
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                line.push(',');
            }
            let encoded = csv::encode_field(cell);
            line.push_str(&style_at(styles, ri, i).paint(&encoded));
        }
        line.push('\n');
        output.write_all(line.as_bytes())?;
    }
    Ok(())
}

/// Render a resolved plan for `--print-engine`: stages, their statements, and
/// the column positions each one resolved to.
pub fn describe(plan: &Plan) -> String {
    let mut out = String::new();
    if let Some(h) = &plan.input_header {
        out.push_str(&format!("input header (hdr): {h:?}\n"));
    }
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
            Stage::Head(limit) => {
                out.push_str(&format!("stage {n} (head):\n  {n}.1 head {limit}\n"));
            }
            Stage::Stats(s) => {
                out.push_str(&format!("stage {n} (stats):\n"));
                out.push_str(&format!(
                    "  {n}.1 stats {:?} (positions {:?})\n",
                    s.names, s.positions
                ));
            }
        }
    }
    if plan.output == OutputFormat::Aligned {
        out.push_str("output: aligned\n");
    }
    for rule in &plan.colors {
        out.push_str(&describe_color(rule));
    }
    out
}

fn describe_color(rule: &ColorRule) -> String {
    match rule {
        ColorRule::Predicate { scope, style, expr } => {
            let tgt = match scope {
                ColorScope::Row => "row".to_string(),
                ColorScope::Cell(c) => format!("cell {}[{}]", c.name, c.pos),
            };
            format!("color {tgt} {style:?} when {}\n", fmt_expr(expr))
        }
        ColorRule::Gradient { col, ramp, bounds } => format!(
            "color gradient {}[{}] {ramp:?} bounds {bounds:?}\n",
            col.name, col.pos
        ),
    }
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
        Stmt::Rename(r) => format!("rename {:?}", r.pairs),
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
    use crate::parse::parse;

    /// Parse, resolve against the input's header, and run end to end.
    fn run_str(script: &str, input: &str) -> Result<String, Error> {
        run_with(script, input, 1, 1_000_000)
    }

    fn run_with(script: &str, input: &str, threads: usize, chunk: usize) -> Result<String, Error> {
        let mut plan = parse(script)?;
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
        let mut plan = parse(script)?;
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

    /// Run a script into a buffer, then render it (optionally with colour).
    fn render_str(script: &str, input: &str, color: bool) -> String {
        let mut plan = parse(script).unwrap();
        let mut reader = io::BufReader::new(input.as_bytes());
        let header = read_header(&mut reader).unwrap();
        let out_header = plan.resolve(&header).unwrap();
        let opts = RunOpts {
            chunk_size: 1_000_000,
            threads: 1,
            temp_dir: std::env::temp_dir(),
            sort_buffer: crate::sort::DEFAULT_BUDGET_BYTES,
        };
        let mut buf = Vec::new();
        run(&plan, &out_header, &opts, &mut reader, &mut buf).unwrap();
        let mut out = Vec::new();
        render(&buf, &plan, color, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    const INPUT: &str = "id,fieldA,countZ\n1,t,5\n2,f,0\n3,t,0\n4,t,9\n";

    #[test]
    fn cols_reorder_and_drop() {
        assert_eq!(
            run_str("cols countZ,id", INPUT).unwrap(),
            "countZ,id\n5,1\n0,2\n0,3\n9,4\n"
        );
        assert_eq!(
            run_str("cols -v fieldA", INPUT).unwrap(),
            "id,countZ\n1,5\n2,0\n3,0\n4,9\n"
        );
    }

    #[test]
    fn select_string_and_implicit_numeric() {
        assert_eq!(
            run_str("select fieldA == 't'", INPUT).unwrap(),
            "id,fieldA,countZ\n1,t,5\n3,t,0\n4,t,9\n"
        );
        // Implicit numeric: no to-num needed.
        assert_eq!(
            run_str("select fieldA == 't' && countZ > 0", INPUT).unwrap(),
            "id,fieldA,countZ\n1,t,5\n4,t,9\n"
        );
    }

    #[test]
    fn numeric_sort_then_filter() {
        // sort by countZ descending, numerically; output numbers print cleanly.
        assert_eq!(
            run_str("sort countZ=nr", INPUT).unwrap(),
            "id,fieldA,countZ\n4,t,9\n1,t,5\n2,f,0\n3,t,0\n"
        );
    }

    #[test]
    fn pipeline_with_sort_stage_split() {
        // filter, sort, then drop a column — three stages.
        let out = run_str(
            "select fieldA == 't' | sort countZ=n | cols -v fieldA",
            INPUT,
        )
        .unwrap();
        assert_eq!(out, "id,countZ\n3,0\n1,5\n4,9\n");
    }

    #[test]
    fn lexical_vs_numeric_sort() {
        let input = "n\n10\n9\n100\n";
        // numeric: 9 < 10 < 100
        assert_eq!(run_str("sort n=n", input).unwrap(), "n\n9\n10\n100\n");
        // lexical (default): "10" < "100" < "9"
        assert_eq!(run_str("sort n", input).unwrap(), "n\n10\n100\n9\n");
    }

    #[test]
    fn external_sort_spilling_matches_in_memory() {
        let mut input = String::from("id,val\n");
        for i in 0..1000 {
            input.push_str(&format!("{i},{}\n", (i * 37) % 1000));
        }
        let script = "sort val=n id";
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
        let script = "select keep == '1'";
        let serial = run_with(script, &input, 1, 1_000_000).unwrap();
        let parallel = run_with(script, &input, 8, 64).unwrap();
        assert_eq!(serial, parallel);
        // Spot-check ordering: first data rows are 1, 3, 5, ...
        assert!(parallel.starts_with("id,keep\n1,1\n3,1\n5,1\n"));
    }

    #[test]
    fn missing_column_is_an_error() {
        assert!(matches!(run_str("cols nope", INPUT), Err(Error::Column(_))));
    }

    #[test]
    fn stats_profiles_every_column() {
        let out = run_str("stats", INPUT).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "field,count,empty,min,max,sum,mean,stddev");
        assert_eq!(lines[1], "id,4,0,1,4,10,2.5,1.290994");
        // text column: lexical min/max, numeric stats blank
        assert_eq!(lines[2], "fieldA,4,0,f,t,,,");
        assert_eq!(lines[3], "countZ,4,0,0,9,14,3.5,4.358899");
    }

    #[test]
    fn stats_after_filter_streams_named_column() {
        // fieldA == 't' keeps countZ values 5,0,9 (streaming path with a pre-filter)
        let out = run_str("select fieldA == 't' | stats countZ", INPUT).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2); // header + countZ
        assert!(lines[1].starts_with("countZ,3,0,0,9,14,4.666667,"));
    }

    #[test]
    fn stats_composes_with_sort_and_head() {
        // largest mean first: countZ(3.5) > id(2.5) > fieldA(blank => 0)
        let out = run_str("stats | sort mean=nr | head 1", INPUT).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].starts_with("countZ,"));
    }

    #[test]
    fn stats_after_sort_uses_in_memory_path() {
        // A blocking stage before stats falls back to the materializing path.
        let out = run_str("sort id=nr | stats id", INPUT).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[1], "id,4,0,1,4,10,2.5,1.290994");
    }

    #[test]
    fn color_predicate_paints_matching_rows() {
        // countZ is 5,0,0,9 — paint rows where countZ == 0.
        let out = render_str("color red countZ == '0' | fmt", INPUT, true);
        assert!(out.contains('\x1b')); // some cells coloured
        // Colour off ⇒ no escapes (aligned, but plain).
        let plain = render_str("color red countZ == '0' | fmt", INPUT, false);
        assert!(!plain.contains('\x1b'));
    }

    #[test]
    fn color_gradient_paints_numeric_column() {
        let out = render_str("color -g countZ green:red | fmt", INPUT, true);
        assert!(out.contains("\u{1b}[38;2;")); // truecolour foreground
    }

    #[test]
    fn color_alignment_unaffected_by_escapes() {
        // The painted and plain aligned output have the same visible layout:
        // stripping ANSI from the coloured render reproduces the plain render.
        let colored = render_str("color red countZ == '0' | fmt", INPUT, true);
        let plain = render_str("color red countZ == '0' | fmt", INPUT, false);
        assert_eq!(strip_ansi(&colored), plain);
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                // skip until the SGR terminator 'm'
                for d in chars.by_ref() {
                    if d == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn head_stops_early_without_reading_to_eof() {
        // A reader that yields the header and a few rows, then *errors* instead
        // of signaling EOF — standing in for a stream that has produced some
        // rows but has not ended (an interactive pipe, `tail -f`, etc.). `head`
        // must emit its N rows and stop *before* demanding more input. With the
        // bug it tried to fill a whole 1 MB chunk first, so it would block on a
        // real stream; here that shows up as the read error propagating.
        use std::collections::VecDeque;

        struct PausingReader(VecDeque<Vec<u8>>);
        impl io::Read for PausingReader {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                match self.0.pop_front() {
                    Some(chunk) => {
                        assert!(chunk.len() <= buf.len());
                        buf[..chunk.len()].copy_from_slice(&chunk);
                        Ok(chunk.len())
                    }
                    None => Err(io::Error::new(io::ErrorKind::WouldBlock, "stream paused")),
                }
            }
        }

        let mut chunks: VecDeque<Vec<u8>> = VecDeque::new();
        chunks.push_back(b"id,val\n".to_vec());
        for i in 0..5 {
            chunks.push_back(format!("{i},x\n").into_bytes());
        }
        let mut reader = io::BufReader::new(PausingReader(chunks));

        let mut plan = parse("head 2").unwrap();
        let header = read_header(&mut reader).unwrap();
        let out_header = plan.resolve(&header).unwrap();
        let opts = RunOpts {
            chunk_size: 1_000_000,
            threads: 1,
            temp_dir: std::env::temp_dir(),
            sort_buffer: crate::sort::DEFAULT_BUDGET_BYTES,
        };
        let mut out = Vec::new();
        run(&plan, &out_header, &opts, &mut reader, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "id,val\n0,x\n1,x\n");
    }
}
