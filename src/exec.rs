//! Executing a compiled [`Plan`] over CSV input.
//!
//! This is the single-threaded baseline. A plan that is a single transform
//! stage streams chunk-by-chunk with borrowed rows (zero-copy); a plan with a
//! `sort` (or otherwise multiple stages) materializes rows as owned values and
//! runs stage by stage. Parallelism and external-merge sort are layered on in
//! later modules.

use std::collections::{HashMap, HashSet};
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
use crate::graph::Histogram;
use crate::plan::{
    AggFunc, AggSpec, BoolExpr, CmpOp, ColorRule, ColorScope, EvalCtx, GraphKind, GraphSpec,
    GroupStmt, JoinStmt, Operand, OutputFormat, Plan, SortStmt, Stage, StatsStmt, Stmt, ValExpr,
    apply_stmts,
};
use crate::sort::Sorter;
use crate::stats::ColStats;
use unicode_width::UnicodeWidthStr;

/// Visible (terminal) width of `s` in columns: CJK/wide glyphs count as 2,
/// zero-width/combining marks as 0. Used so `fmt` aligns by what's displayed,
/// not by `chars().count()`. (ANSI escapes never reach here — alignment is
/// computed on the uncoloured text.)
#[inline]
fn vis_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// An owned row, detached from any chunk buffer (used by the in-memory
/// multi-sort fallback).
type OwnedRow = Vec<Field<'static>>;

/// `(x, y)` points per y-series, for `graph scatter`/`line` (see `collect_xy`).
type XySeries = Vec<Vec<(f64, f64)>>;

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
        return Err(Error::Other("input is empty (no header line)".into()));
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
    // A stateful `add` (`prev()`/`rownum()`) is order-dependent: it can't shard
    // or stream chunk-parallel. Materialize and run the ordered in-memory path
    // (the same fallback `tail`/`uniq`/`join` use).
    if plan_has_stateful_add(plan) {
        return run_staged_in_memory(plan, opts, input, output);
    }
    // `head` with no sort streams single-threaded and stops early.
    if let Some((pre, n, post)) = head_only_shape(plan) {
        return stream_head(pre, n, post, opts.chunk_size, input, output);
    }
    // `stats` reduces the stream to a tiny profile; stream the input through it
    // (O(columns) memory) and run any following stages over that profile.
    if let Some((pre, stats, post)) = stats_shape(plan) {
        return run_stats_streaming(pre, stats, post, opts, input, output);
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
    if plan.stages.iter().any(|s| {
        matches!(
            s,
            Stage::Sort(_)
                | Stage::Stats(_)
                | Stage::Tail(_)
                | Stage::DropLast(_)
                | Stage::Uniq(_)
                | Stage::Group(_)
                | Stage::Join(_)
        )
    }) {
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
            match apply_stmts(pre, row, &mut scratch, &EvalCtx::default()) {
                Ok(true) => {
                    taken += 1;
                    match apply_stmts(post, row, &mut scratch, &EvalCtx::default()) {
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
    opts: &RunOpts,
    input: &mut R,
    output: &mut W,
) -> Result<(), Error> {
    let mut accs: Vec<ColStats> = stats.positions.iter().map(|_| ColStats::new()).collect();
    while let Some(chunk) = next_chunk(input, opts.chunk_size)? {
        let mut scratch: Vec<Field> = Vec::new();
        let mut err: Option<Error> = None;
        csv::parse_chunk(&chunk, |row| {
            if err.is_some() {
                return;
            }
            match apply_stmts(pre, row, &mut scratch, &EvalCtx::default()) {
                Ok(true) => accumulate(&mut accs, &stats.positions, row),
                Ok(false) => {}
                Err(e) => err = Some(e),
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
    }
    let rows = apply_stages_over_rows(post, profile_rows(stats, &accs), opts)?;
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

/// Per-group state: the key cells (emitted verbatim, in first-seen order) and a
/// [`ColStats`] per *distinct* aggregated column plus a row counter (for a bare
/// `count`). One per distinct key — O(groups × aggregated-cols) memory.
struct GroupAcc {
    key: OwnedRow,
    rows: u64,
    stats: Vec<ColStats>,
}

/// `group … | agg …` reducer. The per-key sibling of [`build_colstats`]:
/// folds rows into per-group accumulators keyed by the CSV-encoded key cells,
/// preserving first-seen order. Aggregates sharing a column share one
/// `ColStats` (deduped by position).
struct Grouper<'a> {
    g: &'a GroupStmt,
    /// Distinct aggregated column positions (a bare `count` contributes none).
    stat_positions: Vec<usize>,
    /// For each agg, the slot into a group's `stats`, or `None` for a bare `count`.
    agg_slot: Vec<Option<usize>>,
    index: HashMap<String, usize>,
    groups: Vec<GroupAcc>,
}

impl<'a> Grouper<'a> {
    fn new(g: &'a GroupStmt) -> Self {
        let mut stat_positions: Vec<usize> = Vec::new();
        let agg_slot = g
            .aggs
            .iter()
            .map(|a| {
                a.pos.map(|p| {
                    stat_positions
                        .iter()
                        .position(|&q| q == p)
                        .unwrap_or_else(|| {
                            stat_positions.push(p);
                            stat_positions.len() - 1
                        })
                })
            })
            .collect();
        Grouper {
            g,
            stat_positions,
            agg_slot,
            index: HashMap::new(),
            groups: Vec::new(),
        }
    }

    fn update(&mut self, row: &[Field]) {
        let key: OwnedRow = self
            .g
            .key_positions
            .iter()
            .map(|&p| row.get(p).cloned().unwrap_or(Field::Str("")).into_owned())
            .collect();
        let mut keybuf = String::new();
        csv::write_row(&mut keybuf, &key);
        let idx = match self.index.get(&keybuf) {
            Some(&i) => i,
            None => {
                let i = self.groups.len();
                self.index.insert(keybuf, i);
                self.groups.push(GroupAcc {
                    key,
                    rows: 0,
                    stats: self
                        .stat_positions
                        .iter()
                        .map(|_| ColStats::new())
                        .collect(),
                });
                i
            }
        };
        let acc = &mut self.groups[idx];
        acc.rows += 1;
        for (slot, &p) in self.stat_positions.iter().enumerate() {
            match row.get(p) {
                Some(f) => acc.stats[slot].update(f),
                None => acc.stats[slot].update(&Field::Str("")),
            }
        }
    }

    /// Emit one row per group: key cells followed by one cell per aggregate.
    fn into_rows(self) -> Vec<OwnedRow> {
        let Grouper {
            g,
            agg_slot,
            groups,
            ..
        } = self;
        groups
            .into_iter()
            .map(|acc| {
                let mut row = acc.key;
                for (a, slot) in g.aggs.iter().zip(&agg_slot) {
                    let stats = slot.map(|s| &acc.stats[s]);
                    row.push(agg_value(a, stats, acc.rows));
                }
                row
            })
            .collect()
    }
}

/// One aggregate's output cell. Numeric aggregates over a text/empty column are
/// blank (the same policy `stats` uses).
fn agg_value(a: &AggSpec, stats: Option<&ColStats>, rows: u64) -> Field<'static> {
    match a.func {
        // Bare `count` counts rows; `count(col)` counts non-empty cells.
        AggFunc::Count => match stats {
            Some(s) => Field::Num(s.count() as f64),
            None => Field::Num(rows as f64),
        },
        AggFunc::Min => stats.map_or(Field::Str(""), ColStats::min_field),
        AggFunc::Max => stats.map_or(Field::Str(""), ColStats::max_field),
        AggFunc::Sum => num_or_blank(stats, ColStats::sum),
        AggFunc::Mean => num_or_blank(stats, ColStats::mean),
        AggFunc::Stddev => stats.map_or(Field::Str(""), ColStats::stddev_field),
    }
}

fn num_or_blank(stats: Option<&ColStats>, f: fn(&ColStats) -> f64) -> Field<'static> {
    match stats {
        Some(s) if s.has_numeric() => Field::Num(f(s)),
        _ => Field::Str(""),
    }
}

/// Reduce already-materialized rows by `group … | agg …` (the in-memory path).
fn group_rows(g: &GroupStmt, rows: &[OwnedRow]) -> Vec<OwnedRow> {
    let mut grouper = Grouper::new(g);
    for row in rows {
        grouper.update(row);
    }
    grouper.into_rows()
}

/// Read and parse the header of a file, returning the header, the byte offset
/// where the data rows begin, and the file length.
pub fn read_header_from_path(path: &Path) -> Result<(Vec<String>, u64, u64), Error> {
    let file_len = std::fs::metadata(path)?.len();
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line)?;
    if line.is_empty() {
        return Err(Error::Other("input is empty (no header line)".into()));
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

    // A stateful `add` forces the ordered in-memory path (see `run_body`); skip
    // all sharded fast paths and let the reader path route it there.
    let stateful = plan_has_stateful_add(plan);

    if let [Stage::Transform(stmts)] = plan.stages.as_slice()
        && opts.threads > 1
        && !stateful
    {
        return run_sharded(stmts, opts.threads, path, data_start, file_len, output);
    }

    // `stats` reduces associatively, so shard it over the file too.
    if opts.threads > 1
        && !stateful
        && let Some((pre, stats, post)) = stats_shape(plan)
    {
        let merged = run_stats_sharded(
            pre,
            &stats.positions,
            opts.threads,
            path,
            data_start,
            file_len,
        )?;
        let rows = apply_stages_over_rows(post, profile_rows(stats, &merged), opts)?;
        return write_rows(output, &rows);
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
            match apply_stmts(stmts, row, &mut scratch, &EvalCtx::default()) {
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
                        match apply_stmts(stmts, row, &mut scratch, &EvalCtx::default()) {
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
        match apply_stmts(stmts, row, &mut scratch, &EvalCtx::default()) {
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

/// Accumulate partial per-column [`ColStats`] over one shard's byte range,
/// applying the pre-stats statements. The partials merge associatively, so the
/// profile is identical regardless of how the file was split.
fn stats_over_range(
    pre: &[Stmt],
    positions: &[usize],
    path: &Path,
    start: u64,
    end: u64,
) -> Result<Vec<ColStats>, Error> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((end - start) as usize);
    file.take(end - start).read_to_end(&mut bytes)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| Error::Other(format!("input is not valid UTF-8: {e}")))?;

    let mut accs: Vec<ColStats> = positions.iter().map(|_| ColStats::new()).collect();
    let mut scratch: Vec<Field> = Vec::new();
    let mut err: Option<Error> = None;
    csv::parse_chunk(text, |row| {
        if err.is_some() {
            return;
        }
        match apply_stmts(pre, row, &mut scratch, &EvalCtx::default()) {
            Ok(true) => accumulate(&mut accs, positions, row),
            Ok(false) => {}
            Err(e) => err = Some(e),
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(accs),
    }
}

/// Sharded `stats` over a seekable file: each shard computes partial `ColStats`
/// over its line-aligned byte range, then the partials merge. Returns the merged
/// per-column accumulators; the caller renders the profile and runs any
/// post-stats stages. The reduce mirror of [`run_sharded`].
///
/// Counts, `min`, and `max` are order-independent and so identical to the
/// single-threaded result; floating `sum`/`mean`/`stddev` may differ by ~1 ULP,
/// since a sharded (pairwise) reduction sums in a different order — inherent to
/// any parallel reduction.
fn run_stats_sharded(
    pre: &[Stmt],
    positions: &[usize],
    threads: usize,
    path: &Path,
    data_start: u64,
    file_len: u64,
) -> Result<Vec<ColStats>, Error> {
    let mut merged: Vec<ColStats> = positions.iter().map(|_| ColStats::new()).collect();
    let ranges = shard_ranges(path, data_start, file_len, threads)?;
    if !ranges.is_empty() {
        let partials: Vec<Result<Vec<ColStats>, Error>> = thread::scope(|scope| {
            let handles: Vec<_> = ranges
                .into_iter()
                .map(|(start, end)| {
                    scope.spawn(move || stats_over_range(pre, positions, path, start, end))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(Error::Other("stats shard worker panicked".into())))
                })
                .collect()
        });
        for part in partials {
            for (m, p) in merged.iter_mut().zip(&part?) {
                m.merge(p);
            }
        }
    }
    Ok(merged)
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
    let has_buffered = plan.stages.iter().any(|s| {
        matches!(
            s,
            Stage::Tail(_) | Stage::DropLast(_) | Stage::Uniq(_) | Stage::Group(_) | Stage::Join(_)
        )
    });
    // The streaming sort path handles exactly one sort and no head/stats/
    // tail/drop-last/uniq/join; anything else materializes and runs stage by stage.
    if sort_count != 1 || has_head || has_stats || has_buffered {
        return run_staged_in_memory(plan, opts, input, output);
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
        match apply_stmts(post, row, &mut scratch, &EvalCtx::default()) {
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

/// Whether any transform stage holds a stateful `add` (`prev()`/`rownum()`),
/// which is order-dependent and so can't shard or stream chunk-parallel.
fn plan_has_stateful_add(plan: &Plan) -> bool {
    plan.stages.iter().any(|s| match s {
        Stage::Transform(stmts) => stmts.iter().any(Stmt::is_stateful),
        _ => false,
    })
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
    opts: &RunOpts,
    input: &mut R,
    output: &mut W,
) -> Result<(), Error> {
    let rows = materialize(opts.chunk_size, input)?;
    let rows = apply_stages_over_rows(&plan.stages, rows, opts)?;
    write_rows(output, &rows)
}

/// Run a sequence of stages over already-materialized rows, returning the final
/// rows. The in-memory fallback uses it for the whole plan; the streaming stats
/// path uses it for the (tiny) post-stats stages.
fn apply_stages_over_rows(
    stages: &[Stage],
    mut rows: Vec<OwnedRow>,
    opts: &RunOpts,
) -> Result<Vec<OwnedRow>, Error> {
    for stage in stages {
        match stage {
            Stage::Transform(stmts) => {
                let mut kept = Vec::with_capacity(rows.len());
                let mut scratch: Vec<Field> = Vec::new();
                if stmts.iter().any(Stmt::is_stateful) {
                    // A stateful `add` (`prev()`/`rownum()`) needs rows in input
                    // order with the previous row available. `prev(C)` resolves
                    // C's position against the header *as that add sees it* —
                    // after any earlier `cols`/`rename`/`add` in the same stage,
                    // but before any later one. So the previous row must be
                    // snapshotted at each stateful add's own point in the stage;
                    // `prev_rows[k]` holds it for the add at statement index `k`.
                    // `prev` thus tracks the previous row that reached that add
                    // (independent of a later `select`), and `rownum` counts
                    // every entering row, 1-based.
                    let mut prev_rows: Vec<Option<OwnedRow>> = vec![None; stmts.len()];
                    for (i, row) in rows.drain(..).enumerate() {
                        let mut row = row;
                        let mut keep = true;
                        for (k, stmt) in stmts.iter().enumerate() {
                            let survived = if stmt.is_stateful() {
                                let snapshot = row.clone(); // this add's input layout
                                let r = {
                                    let ctx = EvalCtx {
                                        prev_row: prev_rows[k].as_deref(),
                                        rownum: i as u64 + 1,
                                    };
                                    stmt.apply(&mut row, &mut scratch, &ctx)?
                                };
                                prev_rows[k] = Some(snapshot); // for the next row
                                r
                            } else {
                                stmt.apply(&mut row, &mut scratch, &EvalCtx::default())?
                            };
                            if !survived {
                                keep = false;
                                break;
                            }
                        }
                        if keep {
                            kept.push(row);
                        }
                    }
                } else {
                    for mut row in rows.drain(..) {
                        if apply_stmts(stmts, &mut row, &mut scratch, &EvalCtx::default())? {
                            kept.push(row);
                        }
                    }
                }
                rows = kept;
            }
            Stage::Sort(sort) => sort_rows(sort, &mut rows)?,
            Stage::Head(n) => rows.truncate(*n),
            Stage::Tail(n) => {
                let drop = rows.len().saturating_sub(*n);
                rows.drain(..drop);
            }
            Stage::DropLast(n) => rows.truncate(rows.len().saturating_sub(*n)),
            Stage::Uniq(u) => dedup_rows(&mut rows, &u.positions),
            Stage::Stats(s) => {
                let accs = build_colstats(&s.positions, &rows);
                rows = profile_rows(s, &accs);
            }
            Stage::Group(g) => rows = group_rows(g, &rows),
            Stage::Join(j) => rows = join_rows(j, rows, opts)?,
        }
    }
    Ok(rows)
}

/// Read each `join`'s right-side file header and resolve its sub-plan, recording
/// the right output header on the `JoinStmt`. Must run before [`Plan::resolve`]
/// (which needs the right header) — it's separate because it does IO. Recurses
/// so a join nested inside a right sub-pipeline is prepared too.
pub fn prepare_joins(plan: &mut Plan) -> Result<(), Error> {
    for stage in &mut plan.stages {
        if let Stage::Join(j) = stage {
            prepare_joins(&mut j.right_plan)?;
            let in_header = match &j.right_plan.input_header {
                Some(h) => h.clone(),
                None => read_header_from_path(Path::new(&j.file))?.0,
            };
            j.right_header = j.right_plan.resolve(&in_header)?;
        }
    }
    Ok(())
}

/// Run a join's right sub-plan over its file and collect the result as owned
/// rows (the build side). The sub-plan is run into a buffer via the normal
/// executor, then its body re-parsed — join is blocking and the right side is
/// the smaller one, so the extra serialize/parse is cheap and reuses all paths.
fn materialize_join_right(j: &JoinStmt, opts: &RunOpts) -> Result<Vec<OwnedRow>, Error> {
    let path = Path::new(&j.file);
    let (data_start, file_len) = match &j.right_plan.input_header {
        Some(_) => (0, std::fs::metadata(path)?.len()),
        None => {
            let (_, ds, fl) = read_header_from_path(path)?;
            (ds, fl)
        }
    };
    let mut buf: Vec<u8> = Vec::new();
    run_file(
        &j.right_plan,
        &j.right_header,
        opts,
        path,
        data_start,
        file_len,
        &mut buf,
    )?;
    // Drop the header line the executor wrote; parse the rest into owned rows.
    let body = memchr(b'\n', &buf).map_or(buf.len(), |i| i + 1);
    let text = std::str::from_utf8(&buf[body..])
        .map_err(|e| Error::Other(format!("join right side is not valid UTF-8: {e}")))?;
    let mut rows: Vec<OwnedRow> = Vec::new();
    csv::parse_chunk(text, |row| {
        rows.push(row.iter().map(|f| f.clone().into_owned()).collect());
    });
    Ok(rows)
}

/// Probe the left rows against a hash table built from the right side, emitting
/// joined rows per the join type. One-to-many fan-out is honored (a left row
/// matching N right rows yields N rows); unmatched rows are padded with empty
/// cells for left/right/full as appropriate.
fn join_rows(j: &JoinStmt, left: Vec<OwnedRow>, opts: &RunOpts) -> Result<Vec<OwnedRow>, Error> {
    let right = materialize_join_right(j, opts)?;
    let mut table: HashMap<String, Vec<usize>> = HashMap::new();
    let mut key = String::new();
    let mut sel: Vec<Field<'static>> = Vec::new();
    for (ri, row) in right.iter().enumerate() {
        encode_key(&mut key, &mut sel, row, &j.right_key_pos);
        table.entry(std::mem::take(&mut key)).or_default().push(ri);
    }

    let mut matched = vec![false; right.len()];
    let mut out: Vec<OwnedRow> = Vec::new();
    for lrow in &left {
        encode_key(&mut key, &mut sel, lrow, &j.left_key_pos);
        match table.get(&key) {
            Some(idxs) => {
                for &ri in idxs {
                    matched[ri] = true;
                    out.push(combine(j, lrow, Some(&right[ri])));
                }
            }
            None if j.join_type.keeps_left_unmatched() => out.push(combine(j, lrow, None)),
            None => {}
        }
    }
    if j.join_type.keeps_right_unmatched() {
        for (ri, &m) in matched.iter().enumerate() {
            if !m {
                out.push(combine_right_only(j, &right[ri]));
            }
        }
    }
    Ok(out)
}

/// Build a CSV-encoded key from `positions` of `row` into `key` (reusing `sel`
/// as scratch), so commas/quotes in cells can't collide between keys.
fn encode_key(
    key: &mut String,
    sel: &mut Vec<Field<'static>>,
    row: &[Field<'static>],
    positions: &[usize],
) {
    key.clear();
    sel.clear();
    sel.extend(
        positions
            .iter()
            .map(|&p| row.get(p).cloned().unwrap_or(Field::Str(""))),
    );
    csv::write_row(key, sel);
}

/// A joined output row: the left columns, then the right's emitted columns
/// (empty cells when `rrow` is `None`, i.e. an unmatched left row).
fn combine(j: &JoinStmt, lrow: &[Field<'static>], rrow: Option<&[Field<'static>]>) -> OwnedRow {
    let mut out: OwnedRow = Vec::with_capacity(j.left_ncols + j.right_emit_pos.len());
    for i in 0..j.left_ncols {
        out.push(lrow.get(i).cloned().unwrap_or(Field::Str("")));
    }
    for &p in &j.right_emit_pos {
        out.push(match rrow {
            Some(r) => r.get(p).cloned().unwrap_or(Field::Str("")),
            None => Field::Str(""),
        });
    }
    out
}

/// An unmatched right row (right/full join): left columns empty, except the left
/// key columns carry the equal right key value (coalesce), then the right cells.
fn combine_right_only(j: &JoinStmt, rrow: &[Field<'static>]) -> OwnedRow {
    let mut out: OwnedRow = vec![Field::Str(""); j.left_ncols];
    for (&lp, &rp) in j.left_key_pos.iter().zip(&j.right_key_pos) {
        if let Some(slot) = out.get_mut(lp) {
            *slot = rrow.get(rp).cloned().unwrap_or(Field::Str(""));
        }
    }
    for &p in &j.right_emit_pos {
        out.push(rrow.get(p).cloned().unwrap_or(Field::Str("")));
    }
    out
}

/// Drop duplicate rows in place, keeping the first occurrence. The dedup key is
/// the whole row when `positions` is empty, else those columns' cells; either
/// way it is built by CSV-encoding the cells so commas/quotes can't collide.
fn dedup_rows(rows: &mut Vec<OwnedRow>, positions: &[usize]) {
    let mut seen: HashSet<String> = HashSet::new();
    let mut key = String::new();
    let mut sel: Vec<Field> = Vec::new();
    rows.retain(|row| {
        key.clear();
        if positions.is_empty() {
            csv::write_row(&mut key, row);
        } else {
            sel.clear();
            sel.extend(
                positions
                    .iter()
                    .map(|&p| row.get(p).cloned().unwrap_or(Field::Str(""))),
            );
            csv::write_row(&mut key, &sel);
        }
        seen.insert(std::mem::take(&mut key))
    });
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
    // The graph sink draws a chart from the buffered output instead of emitting
    // rows; it takes precedence over alignment/colour (which become no-ops).
    if let Some(g) = &plan.graph {
        return render_graph(bytes, g, color, output);
    }
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
        Some(compute_styles(&plan.colors, &rows))
    } else {
        None
    };
    if aligned {
        align_and_write(&rows, styles.as_deref(), output)
    } else {
        write_csv_colored(&rows, styles.as_deref(), output)
    }
}

/// Draw the `graph` sink's chart from the buffered CSV output. The plan ran
/// normally to produce rows (header + data); this pulls the charted columns,
/// dropping non-numeric/empty cells loudly (counted and reported), and renders
/// the chart text. The header (row 0) is skipped.
fn render_graph<W: Write>(
    bytes: &[u8],
    g: &GraphSpec,
    color: bool,
    output: &mut W,
) -> Result<(), Error> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| Error::Other(format!("output is not valid UTF-8: {e}")))?;
    // Default title: the value column, or "y vs x" for a single-series xy chart.
    let title = g.opts.title.clone().unwrap_or_else(|| match g.kind {
        GraphKind::Bar => g.cols[1].name.clone(),
        GraphKind::Scatter | GraphKind::Line if g.cols.len() == 2 => {
            format!("{} vs {}", g.cols[1].name, g.cols[0].name)
        }
        GraphKind::Scatter | GraphKind::Line => format!("vs {}", g.cols[0].name),
        GraphKind::Hist | GraphKind::Spark => g.cols[0].name.clone(),
    });

    // One scale factor sets both chart dimensions.
    let (width, height) = crate::graph::chart_size(g.opts.scale);
    let chart = match g.kind {
        GraphKind::Hist => {
            let (values, skipped) = collect_numeric(text, g.cols[0].pos);
            let note = skipped_note(skipped, 0);
            match Histogram::build(&values, g.opts.bins, skipped) {
                Some(h) if g.opts.svg => crate::svg::hist(&title, h.lo, h.hi, &h.counts, &note),
                Some(h) => h.render(&title, width),
                // Keep the --svg contract even with nothing to plot: an empty SVG.
                None if g.opts.svg => crate::svg::hist(&title, 0.0, 0.0, &[], &note),
                None => {
                    format!("{title}: no numeric values to plot (skipped {skipped} non-numeric)\n")
                }
            }
        }
        GraphKind::Spark => {
            let (values, skipped) = collect_numeric(text, g.cols[0].pos);
            if g.opts.svg {
                crate::svg::spark(&title, &values, &skipped_note(skipped, 0))
            } else {
                crate::graph::render_spark(&title, &values, width, skipped)
            }
        }
        GraphKind::Bar => {
            let (rows, skipped, truncated) =
                collect_label_value(text, g.cols[0].pos, g.cols[1].pos, crate::graph::MAX_BARS);
            if g.opts.svg {
                crate::svg::bars(&title, &rows, &skipped_note(skipped, truncated))
            } else {
                crate::graph::render_bars(&title, &rows, width, skipped, truncated)
            }
        }
        GraphKind::Scatter | GraphKind::Line => {
            let ypos: Vec<usize> = g.cols[1..].iter().map(|c| c.pos).collect();
            let ynames: Vec<String> = g.cols[1..].iter().map(|c| c.name.clone()).collect();
            let XyData {
                series,
                skipped,
                xends,
                even_spacing,
            } = collect_xy(text, g.cols[0].pos, &ypos);
            let connect = g.kind == GraphKind::Line;
            // Only the row-index fallback distorts spacing; a temporal axis is true.
            let xnote = if even_spacing { "even row spacing" } else { "" };
            if g.opts.svg {
                let note = [skipped_note(skipped, 0), xnote.to_string()]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("  ");
                crate::svg::xy(&title, &ynames, &series, connect, &note, xends)
            } else {
                let title = if xnote.is_empty() {
                    title
                } else {
                    format!("{title}  ({xnote})")
                };
                crate::graph::render_xy(
                    &title, &ynames, &series, width, height, color, connect, skipped, xends,
                )
            }
        }
    };
    output.write_all(chart.as_bytes())?;
    Ok(())
}

/// The result of collecting `graph scatter`/`line` data: the points per
/// y-series, the dropped-point count, an optional axis-end label override, and
/// whether positions are by row (the even-spacing caveat).
struct XyData {
    series: XySeries,
    skipped: u64,
    /// Override for the bottom-axis end labels (`None` ⇒ format the numeric range).
    xends: Option<(String, String)>,
    /// True only for the row-index fallback, where spacing isn't the real x.
    even_spacing: bool,
}

/// Collect `(x, y)` points per y-series from the buffered output (header
/// skipped). A non-numeric y drops just that series' point (counted in
/// `skipped`). The x column is handled in three modes:
/// - **numeric** — plotted as-is, numeric axis labels;
/// - **temporal** — if no value is numeric but every one parses as a timestamp,
///   plot at true epoch positions and label the axis with formatted dates;
/// - **row-index fallback** — otherwise (e.g. categories) plot against the
///   1-based row ordinal, label the axis with the first/last raw cells, and flag
///   even spacing.
fn collect_xy(text: &str, xpos: usize, ypos: &[usize]) -> XyData {
    let num = |r: &[Field], p: usize| -> Option<f64> {
        r.get(p)
            .and_then(|f| f.as_str().trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
    };
    // (numeric x, raw x text, y values) per data row.
    let mut rows: Vec<(Option<f64>, String, Vec<Option<f64>>)> = Vec::new();
    let mut first = true;
    csv::parse_chunk(text, |r| {
        if first {
            first = false;
            return;
        }
        let xcell = r
            .get(xpos)
            .map(|f| f.as_str().into_owned())
            .unwrap_or_default();
        let x = num(r, xpos);
        let ys = ypos.iter().map(|&p| num(r, p)).collect();
        rows.push((x, xcell, ys));
    });

    let mut series: Vec<Vec<(f64, f64)>> = vec![Vec::new(); ypos.len()];
    let mut skipped = 0u64;
    let push_row = |series: &mut XySeries, skipped: &mut u64, xv, ys: Vec<Option<f64>>| {
        for (i, y) in ys.into_iter().enumerate() {
            match y {
                Some(y) => series[i].push((xv, y)),
                None => *skipped += 1,
            }
        }
    };

    // Numeric x: plot as-is; rows with a non-numeric x are dropped.
    if rows.iter().any(|(x, _, _)| x.is_some()) {
        for (x, _, ys) in rows {
            match x {
                Some(xv) => push_row(&mut series, &mut skipped, xv, ys),
                None => skipped += ys.len() as u64,
            }
        }
        return XyData {
            series,
            skipped,
            xends: None,
            even_spacing: false,
        };
    }

    // No numeric x — does every cell parse as a timestamp? Then use a true time
    // axis (real spacing, formatted date labels).
    let epochs: Vec<Option<f64>> = rows
        .iter()
        .map(|(_, raw, _)| crate::datetime::parse_epoch(raw))
        .collect();
    if !rows.is_empty() && epochs.iter().all(Option::is_some) {
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        for ((_, _, ys), e) in rows.into_iter().zip(&epochs) {
            let xv = e.unwrap();
            lo = lo.min(xv);
            hi = hi.max(xv);
            push_row(&mut series, &mut skipped, xv, ys);
        }
        return XyData {
            series,
            skipped,
            xends: Some((
                crate::datetime::format_epoch(lo),
                crate::datetime::format_epoch(hi),
            )),
            even_spacing: false,
        };
    }

    // Row-index fallback: plot against the 1-based ordinal, label with the raw
    // first/last cells.
    let xfirst = rows.first().map(|(_, r, _)| r.clone()).unwrap_or_default();
    let xlast = rows.last().map(|(_, r, _)| r.clone()).unwrap_or_default();
    for (idx, (_, _, ys)) in rows.into_iter().enumerate() {
        push_row(&mut series, &mut skipped, (idx + 1) as f64, ys);
    }
    XyData {
        series,
        skipped,
        xends: Some((xfirst, xlast)),
        even_spacing: true,
    }
}

/// The dropped-data footnote for an SVG chart (the "strict and loud" policy the
/// terminal renderers already print); empty when nothing was dropped.
fn skipped_note(skipped: u64, truncated: usize) -> String {
    let mut parts = Vec::new();
    if truncated > 0 {
        parts.push(format!("+{truncated} more not shown"));
    }
    if skipped > 0 {
        parts.push(format!("skipped {skipped} non-numeric"));
    }
    parts.join("  ")
}

/// Collect one column's finite numeric values from the buffered output (header
/// skipped), counting non-numeric/empty cells as `skipped`.
fn collect_numeric(text: &str, pos: usize) -> (Vec<f64>, u64) {
    let mut values = Vec::new();
    let mut skipped = 0u64;
    let mut first = true;
    csv::parse_chunk(text, |r| {
        if first {
            first = false;
            return;
        }
        match r.get(pos).map(|f| f.as_str()) {
            Some(s) => match s.trim().parse::<f64>() {
                Ok(v) if v.is_finite() => values.push(v),
                _ => skipped += 1,
            },
            None => skipped += 1,
        }
    });
    (values, skipped)
}

/// Collect `(label, value)` pairs for a bar chart (header skipped). Rows past
/// `cap` numeric pairs are counted as `truncated` rather than drawn; non-numeric
/// values are counted as `skipped`.
fn collect_label_value(
    text: &str,
    label_pos: usize,
    value_pos: usize,
    cap: usize,
) -> (Vec<(String, f64)>, u64, usize) {
    let mut rows = Vec::new();
    let mut skipped = 0u64;
    let mut truncated = 0usize;
    let mut first = true;
    csv::parse_chunk(text, |r| {
        if first {
            first = false;
            return;
        }
        match r.get(value_pos).map(|f| f.as_str()) {
            Some(s) => match s.trim().parse::<f64>() {
                Ok(v) if v.is_finite() => {
                    if rows.len() < cap {
                        let label = r.get(label_pos).map(|f| f.as_str().into_owned());
                        rows.push((label.unwrap_or_default(), v));
                    } else {
                        truncated += 1;
                    }
                }
                _ => skipped += 1,
            },
            None => skipped += 1,
        }
    });
    (rows, skipped, truncated)
}

/// The per-cell [`Style`] grid (`[row][col]`) for the colour rules over `rows`.
/// The header row (index 0) is never styled. Rules apply in order; each layers
/// onto what earlier rules set (last wins per attribute).
///
/// Best-effort: a predicate that errors on a row (e.g. a non-numeric cell in a
/// numeric comparison) leaves that row unpainted rather than aborting — colour
/// is a cosmetic overlay, so one bad cell shouldn't kill the whole output.
fn compute_styles(rules: &[ColorRule], rows: &[Vec<String>]) -> Vec<Vec<Style>> {
    let ncols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut styles = vec![vec![Style::default(); ncols]; rows.len()];
    for rule in rules {
        match rule {
            ColorRule::Predicate { scope, style, expr } => {
                // Reuse one row buffer across all rows (cells borrow from `rows`,
                // so it just holds &str views — one alloc per rule, not per row).
                let mut frow: Vec<Field> = Vec::new();
                for (ri, row) in rows.iter().enumerate().skip(1) {
                    frow.clear();
                    frow.extend(row.iter().map(|s| Field::Str(s)));
                    if !matches!(expr.eval(&frow), Ok(true)) {
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
    styles
}

/// Default gradient bounds: the min/max over the column's *parseable* cells —
/// exactly the cells the gradient will paint (same `trim().parse` test as
/// `compute_styles`). Computed directly rather than via `ColStats`, whose
/// numeric range is `None` for a column with any non-numeric cell; that would
/// collapse the bounds to `0..1` and clamp every real value to the hi colour.
/// Falls back to `0..1` when no cell parses (then nothing is painted anyway).
fn column_minmax(rows: &[Vec<String>], pos: usize) -> (f64, f64) {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for row in rows.iter().skip(1) {
        if let Some(v) = row.get(pos).and_then(|c| c.trim().parse::<f64>().ok()) {
            lo = lo.min(v);
            hi = hi.max(v);
        }
    }
    if lo <= hi { (lo, hi) } else { (0.0, 1.0) }
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
            widths[i] = widths[i].max(vis_width(field));
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
            let pad = widths[i].saturating_sub(vis_width(field));
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
            Stage::Tail(limit) => {
                out.push_str(&format!("stage {n} (tail):\n  {n}.1 tail {limit}\n"));
            }
            Stage::DropLast(limit) => {
                out.push_str(&format!(
                    "stage {n} (drop-last):\n  {n}.1 drop last {limit} (head -n -{limit})\n"
                ));
            }
            Stage::Uniq(u) => {
                out.push_str(&format!("stage {n} (uniq):\n"));
                let by = if u.positions.is_empty() {
                    "whole row".to_string()
                } else {
                    format!("{:?} (positions {:?})", u.cols, u.positions)
                };
                out.push_str(&format!("  {n}.1 uniq by {by}\n"));
            }
            Stage::Stats(s) => {
                out.push_str(&format!("stage {n} (stats):\n"));
                out.push_str(&format!(
                    "  {n}.1 stats {:?} (positions {:?})\n",
                    s.names, s.positions
                ));
            }
            Stage::Group(g) => {
                out.push_str(&format!("stage {n} (group):\n"));
                out.push_str(&format!(
                    "  {n}.1 keys {:?} (positions {:?})\n",
                    g.keys, g.key_positions
                ));
                let aggs: Vec<String> = g
                    .aggs
                    .iter()
                    .map(|a| match &a.col {
                        Some(c) => format!("{}={}({c})", a.name, a.func.name()),
                        None => format!("{}={}", a.name, a.func.name()),
                    })
                    .collect();
                out.push_str(&format!("  {n}.2 agg {aggs:?}\n"));
            }
            Stage::Join(j) => {
                out.push_str(&format!("stage {n} (join {}):\n", j.join_type.label()));
                let keys: Vec<String> = j
                    .keys
                    .iter()
                    .map(|(l, r)| {
                        if l == r {
                            l.clone()
                        } else {
                            format!("{l}={r}")
                        }
                    })
                    .collect();
                out.push_str(&format!(
                    "  {n}.1 join {} on {} (left keys {:?}, right keys {:?})\n",
                    j.file,
                    keys.join(","),
                    j.left_key_pos,
                    j.right_key_pos
                ));
                out.push_str(&format!(
                    "  {n}.1 emit right positions {:?}\n",
                    j.right_emit_pos
                ));
                if !j.right_plan.stages.is_empty() {
                    out.push_str("  right sub-pipeline:\n");
                    for line in describe(&j.right_plan).lines() {
                        out.push_str(&format!("    {line}\n"));
                    }
                }
            }
        }
    }
    if plan.output == OutputFormat::Aligned {
        out.push_str("output: aligned\n");
    }
    for rule in &plan.colors {
        out.push_str(&describe_color(rule));
    }
    if let Some(g) = &plan.graph {
        let kind = match g.kind {
            GraphKind::Hist => "hist",
            GraphKind::Bar => "bar",
            GraphKind::Spark => "spark",
            GraphKind::Scatter => "scatter",
            GraphKind::Line => "line",
        };
        let cols: Vec<&str> = g.cols.iter().map(|c| c.name.as_str()).collect();
        out.push_str(&format!("graph: {kind} {cols:?}"));
        if let Some(b) = g.opts.bins {
            out.push_str(&format!(" bins={b}"));
        }
        out.push('\n');
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
            format!("color {tgt} {style} when {}\n", fmt_expr(expr))
        }
        ColorRule::Gradient { col, ramp, bounds } => {
            let b = match bounds {
                Some((lo, hi)) => format!("{lo}..{hi}"),
                None => "auto".to_string(),
            };
            format!(
                "color gradient {}[{}] {ramp} bounds {b}\n",
                col.name, col.pos
            )
        }
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
        Stmt::Add(a) => {
            let target = match a.pos {
                Some(i) => format!("{}[{i}]", a.name),
                None => format!("{} (append)", a.name),
            };
            format!("add {target} = {}", fmt_valexpr(&a.expr))
        }
    }
}

fn fmt_valexpr(e: &ValExpr) -> String {
    match e {
        ValExpr::Col(c) => format!("{}[{}]", c.name, c.pos),
        ValExpr::Num(n) => n.to_string(),
        ValExpr::Str(s) => format!("{s:?}"),
        ValExpr::Neg(e) => format!("(neg {})", fmt_valexpr(e)),
        ValExpr::Arith { op, lhs, rhs } => {
            format!(
                "({} {} {})",
                op.symbol(),
                fmt_valexpr(lhs),
                fmt_valexpr(rhs)
            )
        }
        ValExpr::Concat(parts) => format!("(++ {})", fmt_valexpr_list(parts)),
        ValExpr::Func(f, args) => format!("({} {})", f.name(), fmt_valexpr_list(args)),
        ValExpr::Bool(b) => format!("(bool {})", fmt_expr(b)),
        ValExpr::Cond { test, then_, else_ } => format!(
            "(if {} {} {})",
            fmt_expr(test),
            fmt_valexpr(then_),
            fmt_valexpr(else_)
        ),
        ValExpr::Prev(c) => format!("(prev {}[{}])", c.name, c.pos),
        ValExpr::Rownum => "(rownum)".to_string(),
    }
}

fn fmt_valexpr_list(es: &[ValExpr]) -> String {
    es.iter().map(fmt_valexpr).collect::<Vec<_>>().join(" ")
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
        // The `:num`/`:str` tag exposes whether the comparison coerces to a
        // number or compares text — so a surprise lexical compare (e.g. two bare
        // columns, with no numeric literal to trigger numeric mode) is visible.
        BoolExpr::Cmp(c) => format!(
            "({} {} {} :{})",
            cmp_symbol(c.op),
            fmt_operand(&c.lhs),
            fmt_operand(&c.rhs),
            if c.numeric { "num" } else { "str" }
        ),
        BoolExpr::Match { col, negate, .. } => {
            let op = if *negate { "!~" } else { "=~" };
            format!("({op} {}[{}] /regex/)", col.name, col.pos)
        }
        BoolExpr::Affix { col, needle, kind } => {
            format!("({} {}[{}] {needle:?})", kind.symbol(), col.name, col.pos)
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
    fn head_negative_keeps_all_but_last() {
        // INPUT has ids 1..4; `head -n -1` drops the last row.
        assert_eq!(
            run_str("head -n -1", INPUT).unwrap(),
            "id,fieldA,countZ\n1,t,5\n2,f,0\n3,t,0\n"
        );
        // Dropping more than present yields just the header.
        assert_eq!(run_str("head -n -9", INPUT).unwrap(), "id,fieldA,countZ\n");
    }

    #[test]
    fn uniq_dedups_keeping_first() {
        let input = "g,v\na,1\nb,2\na,3\na,1\nb,9\n";
        // Whole-row dedup keeps the first of each distinct row (a,1 appears twice).
        assert_eq!(run_str("uniq", input).unwrap(), "g,v\na,1\nb,2\na,3\nb,9\n");
        // By a key column: first row per distinct g.
        assert_eq!(run_str("uniq g", input).unwrap(), "g,v\na,1\nb,2\n");
    }

    #[test]
    fn tail_keeps_last_rows() {
        // INPUT has 4 data rows (ids 1..4); tail 2 keeps the last two.
        assert_eq!(
            run_str("tail 2", INPUT).unwrap(),
            "id,fieldA,countZ\n3,t,0\n4,t,9\n"
        );
        // Composes after a filter (fieldA == 't' -> ids 1,3,4; last one).
        assert_eq!(
            run_str("select fieldA == 't' | tail 1", INPUT).unwrap(),
            "id,fieldA,countZ\n4,t,9\n"
        );
        // Asking for more than present keeps them all.
        assert_eq!(run_str("tail 99", INPUT).unwrap(), INPUT);
    }

    #[test]
    fn missing_column_is_an_error() {
        assert!(matches!(
            run_str("cols nope", INPUT),
            Err(Error::Column { .. })
        ));
    }

    #[test]
    fn describe_annotates_compare_mode_and_affix() {
        // Two bare columns compare as text (the footgun, now visible as :str);
        // a numeric literal forces :num; affix renders with its operator.
        let mut plan = parse("select fieldA > id && countZ > 0 && fieldA ^= 't'").unwrap();
        let _ = plan.resolve(&["id".into(), "fieldA".into(), "countZ".into()]);
        let d = describe(&plan);
        assert!(d.contains("(> fieldA[1] id[0] :str)"), "{d}");
        assert!(d.contains("(> countZ[2] 0 :num)"), "{d}");
        assert!(d.contains("(^= fieldA[1] \"t\")"), "{d}");
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
    fn stats_sharded_matches_serial() {
        // A CSV with fractional numbers + text + empties, enough rows to split
        // into several shards. Profile it single-threaded (streaming path) and
        // multi-threaded (sharded path). Counts/min/max are order-independent and
        // must match exactly; floating sum/mean/stddev may differ by ~1 ULP (the
        // sharded reduce sums in a different order), so compare those within a
        // tiny relative tolerance.
        let mut csv = String::from("price,region\n");
        for i in 0..600 {
            let price = if i % 7 == 0 {
                String::new()
            } else {
                format!("{:.2}", (i % 250) as f64 * 1.13)
            };
            let region = ["EU", "US", "APAC"][i % 3];
            csv.push_str(&format!("{price},{region}\n"));
        }
        let path =
            std::env::temp_dir().join(format!("csvm-stats-shard-{}.csv", std::process::id()));
        std::fs::write(&path, &csv).unwrap();

        let run_threads = |threads: usize| -> String {
            let mut plan = parse("stats").unwrap();
            let (header, data_start, file_len) = read_header_from_path(&path).unwrap();
            let out_header = plan.resolve(&header).unwrap();
            let opts = RunOpts {
                chunk_size: 256,
                threads,
                temp_dir: std::env::temp_dir(),
                sort_buffer: crate::sort::DEFAULT_BUDGET_BYTES,
            };
            let mut out = Vec::new();
            run_file(
                &plan,
                &out_header,
                &opts,
                &path,
                data_start,
                file_len,
                &mut out,
            )
            .unwrap();
            String::from_utf8(out).unwrap()
        };

        let serial = run_threads(1); // reader/streaming path
        let parallel = run_threads(8); // sharded path
        std::fs::remove_file(&path).ok();

        // Compare cell by cell: numbers within tolerance, text exactly. (The test
        // data has no quoted/comma cells, so a plain split is fine here.)
        let rows = |s: &str| -> Vec<Vec<String>> {
            s.lines()
                .map(|l| l.split(',').map(str::to_string).collect())
                .collect()
        };
        let (sr, pr) = (rows(&serial), rows(&parallel));
        assert_eq!(sr.len(), pr.len(), "row count differs");
        for (a, b) in sr.iter().zip(&pr) {
            assert_eq!(a.len(), b.len());
            for (x, y) in a.iter().zip(b) {
                match (x.parse::<f64>(), y.parse::<f64>()) {
                    (Ok(xv), Ok(yv)) => {
                        assert!(
                            (xv - yv).abs() <= 1e-9 * xv.abs().max(1.0),
                            "numeric cell differs: {x} vs {y}"
                        );
                    }
                    _ => assert_eq!(x, y, "text cell differs"),
                }
            }
        }
        assert!(serial.contains("\nprice,") && serial.contains("\nregion,"));
    }

    #[test]
    fn stats_after_sort_uses_in_memory_path() {
        // A blocking stage before stats falls back to the materializing path.
        let out = run_str("sort id=nr | stats id", INPUT).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[1], "id,4,0,1,4,10,2.5,1.290994");
    }

    #[test]
    fn group_default_count_in_first_seen_order() {
        // fieldA: 't' first (row 1), then 'f' (row 2). count = rows per group.
        let out = run_str("group fieldA", INPUT).unwrap();
        assert_eq!(out, "fieldA,count\nt,3\nf,1\n");
    }

    #[test]
    fn group_agg_numeric_reductions() {
        // 't' rows have countZ 5,0,9; 'f' has 0.
        let out = run_str(
            "group fieldA | agg sum(countZ),mean(countZ),min(countZ),max(countZ)",
            INPUT,
        )
        .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            "fieldA,countZ_sum,countZ_mean,countZ_min,countZ_max"
        );
        assert_eq!(lines[1], "t,14,4.666667,0,9");
        assert_eq!(lines[2], "f,0,0,0,0");
    }

    #[test]
    fn agg_global_with_no_keys_is_one_row() {
        let out = run_str("agg count, sum(countZ)", INPUT).unwrap();
        assert_eq!(out, "count,countZ_sum\n4,14\n");
    }

    #[test]
    fn agg_by_clause_supplies_keys() {
        let out = run_str("agg sum(countZ) by fieldA", INPUT).unwrap();
        assert_eq!(out, "fieldA,countZ_sum\nt,14\nf,0\n");
    }

    #[test]
    fn agg_sum_of_text_column_is_blank() {
        // fieldA is text, so sum/mean are blank, but min/max stay lexical.
        let out = run_str("group id | agg sum(fieldA),min(fieldA)", INPUT).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "id,fieldA_sum,fieldA_min");
        assert_eq!(lines[1], "1,,t");
    }

    #[test]
    fn count_rows_differs_from_count_of_column() {
        // A bare count counts rows; count(col) counts non-empty cells.
        let input = "g,v\nx,1\nx,\nx,3\n";
        let out = run_str("group g | agg count, count(v)", input).unwrap();
        assert_eq!(out, "g,count,v_count\nx,3,2\n");
    }

    #[test]
    fn group_composes_with_sort_and_fmt() {
        let out = run_str("group fieldA | agg sum(countZ) | sort countZ_sum=nr", INPUT).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        // f,0 sorts after t,14 when descending by the aggregate.
        assert_eq!(lines[1], "t,14");
        assert_eq!(lines[2], "f,0");
    }

    #[test]
    fn graph_hist_renders_chart_from_a_column() {
        // countZ is 5,0,0,9 over 4 rows; a histogram bins those values.
        let out = render_str("graph hist countZ --bins 2", INPUT, false);
        assert!(out.starts_with("countZ\n"), "{out}");
        assert!(out.contains("n=4"));
        assert!(out.contains("min=0"));
        assert!(out.contains("max=9"));
        assert!(out.contains("bins=2"));
        assert!(out.contains('█')); // something was drawn
    }

    #[test]
    fn graph_hist_composes_after_a_filter() {
        // Only fieldA == 't' rows reach the graph: countZ 5,0,9.
        let out = render_str("select fieldA == 't' | graph hist countZ", INPUT, false);
        assert!(out.contains("n=3"), "{out}");
    }

    #[test]
    fn graph_hist_reports_non_numeric_cells() {
        // fieldA is text, so every value is skipped and nothing is plotted.
        let out = render_str("graph hist fieldA", INPUT, false);
        assert!(out.contains("no numeric values to plot"), "{out}");
        assert!(out.contains("skipped 4 non-numeric"), "{out}");
    }

    #[test]
    fn graph_title_overrides_the_column_name() {
        let out = render_str("graph hist countZ --title Counts", INPUT, false);
        assert!(out.starts_with("Counts\n"), "{out}");
    }

    #[test]
    fn graph_bar_draws_one_bar_per_row_after_group() {
        // group fieldA: t (3 rows), f (1 row); bar the counts.
        let out = render_str("group fieldA | graph bar fieldA count", INPUT, false);
        // Title defaults to the value column.
        assert!(out.starts_with("count\n"), "{out}");
        assert!(out.contains("t │"), "{out}");
        assert!(out.contains("f │"), "{out}");
        assert!(out.contains("bars=2"), "{out}");
    }

    #[test]
    fn graph_spark_is_a_single_line() {
        let out = render_str("graph spark countZ", INPUT, false);
        assert!(out.starts_with("countZ\n"), "{out}");
        assert!(out.contains("min=0") && out.contains("max=9"), "{out}");
    }

    #[test]
    fn graph_scatter_plots_xy_on_a_braille_frame() {
        let out = render_str("graph scatter id countZ", INPUT, false);
        assert!(out.starts_with("countZ vs id\n"), "{out}");
        assert!(out.contains('┤') && out.contains('└'), "{out}");
        assert!(out.contains("points=4"), "{out}");
    }

    #[test]
    fn graph_svg_emits_an_svg_document() {
        let out = render_str("graph hist countZ --bins 3 --svg", INPUT, false);
        assert!(out.starts_with("<svg"), "{out}");
        assert!(out.trim_end().ends_with("</svg>"));
        assert!(out.contains("<rect")); // bars
        // --svg short-circuits the terminal renderer (no block glyphs).
        assert!(!out.contains('█'));
    }

    #[test]
    fn graph_svg_reports_skipped_and_stays_svg_with_no_data() {
        // fieldA is all text: every value skipped, but the output is still SVG
        // (not a plain-text diagnostic) and reports the dropped count.
        let out = render_str("graph hist fieldA --svg", INPUT, false);
        assert!(out.starts_with("<svg"), "{out}");
        assert!(out.trim_end().ends_with("</svg>"), "{out}");
        assert!(out.contains("skipped 4 non-numeric"), "{out}");
    }

    #[test]
    fn graph_xy_falls_back_to_row_index_for_non_numeric_x() {
        // fieldA (x) is text, so x plots against the 1-based row order; countZ
        // (5,0,0,9) all chart. The fallback flags even spacing and the axis
        // shows the real first/last x values (fieldA is t,f,t,t → "t" … "t").
        let out = render_str("graph line fieldA countZ", INPUT, false);
        assert!(out.contains("even row spacing"), "{out}");
        assert!(out.contains("points=4"), "{out}");
        // Axis ends are the real x cells, not synthetic 1/4 indices.
        let axis = out.lines().rev().nth(1).unwrap_or("");
        assert!(axis.trim_start().starts_with('t'), "{out}");
    }

    #[test]
    fn graph_xy_uses_a_true_time_axis_for_timestamps() {
        // A string timestamp x is parsed to epoch: real spacing (no even-row
        // caveat) and the axis shows formatted dates.
        let input = "t,v\n2024-01-01T00:00:00Z,1\n2024-01-01T01:00:00Z,5\n2024-01-01T02:00:00Z,3\n";
        let out = render_str("graph line t v", input, false);
        assert!(out.contains("points=3"), "{out}");
        assert!(!out.contains("even row spacing"), "{out}"); // true axis, not fallback
        assert!(
            out.contains("2024-01-01 00:00:00") && out.contains("2024-01-01 02:00:00"),
            "{out}"
        );
    }

    #[test]
    fn graph_xy_partial_numeric_x_stays_strict() {
        // id is fully numeric, so no fallback and no rows dropped.
        let out = render_str("graph scatter id countZ", INPUT, false);
        assert!(!out.contains("row order"), "{out}");
        assert!(out.contains("points=4"), "{out}");
    }

    #[test]
    fn graph_line_multi_series_is_coloured_with_a_legend() {
        let out = render_str("graph line id fieldA,countZ", INPUT, true);
        // fieldA is text ⇒ its points are skipped; countZ contributes 4.
        assert!(out.contains("points=4"), "{out}");
        assert!(out.contains('\x1b'), "expected colour escapes");
        assert!(out.contains('●'), "expected a legend");
    }

    #[test]
    fn color_rule_for_a_dropped_column_is_skipped_not_fatal() {
        // A colour rule on countZ, then countZ dropped by `cols`: the rule is
        // inert (its column isn't in the output), so the run succeeds.
        let out = render_str("color red countZ == '0' | cols id,fieldA", INPUT, true);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "id,fieldA");
        assert_eq!(lines.len(), 5); // header + 4 rows, no error
        assert!(!out.contains('\x1b')); // nothing painted (the column is gone)
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
    fn fmt_aligns_by_display_width() {
        // A wide (CJK) glyph occupies two terminal columns; alignment must count
        // it as 2, not as one char. With char-count the first column would be 1
        // wide and the 袋 row would gain a stray trailing space.
        let out = render_str("fmt", "v,tag\n袋,x\nab,y\n", false);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines, ["v   tag", "袋  x", "ab  y"]);
    }

    #[test]
    fn color_predicate_is_best_effort_on_non_numeric() {
        // A non-numeric cell in a numeric predicate must not abort the render;
        // the offending row is just left unpainted. (Before the fix this errored
        // with "non-numeric value 'NA'".)
        let input = "amount\n10\nNA\n30\n";
        let out = render_str("color red amount > 20 | fmt", input, true);
        let line = |needle: &str| out.lines().find(|l| l.contains(needle)).unwrap();
        assert!(
            line("30").contains("38;2;205;0;0"),
            "30 should be red: {out:?}"
        );
        assert!(!line("10").contains('\u{1b}'), "10 unpainted: {out:?}");
        assert!(!line("NA").contains('\u{1b}'), "NA unpainted: {out:?}");
    }

    #[test]
    fn color_gradient_paints_numeric_column() {
        let out = render_str("color -g countZ green:red | fmt", INPUT, true);
        assert!(out.contains("\u{1b}[38;2;")); // truecolour foreground
    }

    #[test]
    fn color_gradient_bounds_ignore_non_numeric_cells() {
        // A lone non-numeric cell must not collapse the default bounds: the
        // numeric values still span green(min)..red(max). (Before the fix, the
        // bounds fell back to 0..1 and clamped every value to the hi colour.)
        let input = "amount\n10\nNA\n30\n";
        let out = render_str("color -g amount green:red | fmt", input, true);
        let line = |needle: &str| out.lines().find(|l| l.contains(needle)).unwrap();
        assert!(
            line("10").contains("38;2;0;205;0"), // min -> green
            "min should be green, got: {out:?}"
        );
        assert!(
            line("30").contains("38;2;205;0;0"), // max -> red
            "max should be red, got: {out:?}"
        );
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
