//! Parallel external merge sort, modeled on csvm's sort stage.
//!
//! Each worker thread parses an input **block**, applies the pre-sort
//! statements, **serializes** each surviving row to bytes once, and computes an
//! **order-preserving encoded key** (so comparison is a byte compare). It then
//! sorts an index over those rows. The serial **k-way merge** picks the
//! smallest key across runs and hands the caller the row's already-serialized
//! line bytes — no per-field allocation, and no re-serialization on output.
//!
//! Doing the serialization in the parallel workers (not the serial merge) is
//! what makes the merge cheap: it only compares small encoded keys and copies
//! line bytes. A block is a contiguous input range, so its sequence number
//! keeps the merge stable (the role csvm's `orig_chunk_id` plays).
//!
//! Small inputs never touch disk; larger ones spill sorted runs to temp files.
//! The merge is single-level (multi-level merge is left for a later pass).

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use crate::csv;
use crate::error::Error;
use crate::field::Field;
use crate::plan::{self, SortMode, SortStmt, Stmt, apply_stmts};

/// Default in-memory budget before runs start spilling to disk.
pub const DEFAULT_BUDGET_BYTES: usize = 256 << 20;

/// Max runs merged at once. With more runs, intermediate groups of this many
/// are merged (in parallel) into larger runs first, so the final merge never
/// opens more than this many files and the heap stays small.
const DEFAULT_FANOUT: usize = 32;

/// Process-global run-file counter so concurrent sorters never collide on a
/// temp file name (csvm uses the same trick).
static RUN_SEQ: AtomicU64 = AtomicU64::new(0);

/// A spilled run file; removed from disk when dropped.
struct TempRun {
    path: PathBuf,
}

impl Drop for TempRun {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A sorted run. `seq` is input order, used to keep the merge stable.
struct Run {
    seq: u64,
    data: RunData,
}

enum RunData {
    /// Sorted line bytes (`blob`) with `lines[i]` the byte range of the i-th
    /// row in sorted order, and `keys[i]` its encoded sort key.
    Mem {
        blob: Vec<u8>,
        lines: Vec<(u32, u32)>,
        keys: Vec<Box<[u8]>>,
    },
    /// Sorted rows spilled to a temp file (one CSV line each, already sorted).
    File(TempRun),
}

// --- order-preserving key encoding ------------------------------------------

/// Append an order-preserving encoding of `row`'s sort keys to `out`, so that a
/// plain byte comparison of two encodings reproduces [`SortStmt`] ordering
/// (direction included). Numeric keys are 8 bytes; string keys are the bytes
/// plus a terminator; an auto key is a one-byte tag (numbers before text) and
/// then whichever of the two encodings the cell got. (A `\0` inside a string
/// key — never produced by normal CSV — would compare slightly off; UTF-8
/// never contains `\xFF`, used for the descending terminator.)
fn encode_key(row: &[Field], sort: &SortStmt, out: &mut Vec<u8>) -> Result<(), Error> {
    for key in &sort.keys {
        let field = row.get(key.pos);
        match key.mode {
            SortMode::Numeric => encode_num(plan::cell_num(row, key.pos)?, key.descending, out),
            SortMode::Lexical => encode_str(field, key.descending, out),
            SortMode::Auto => match plan::auto_num(row, key.pos) {
                Some(n) => {
                    out.push(if key.descending { 0xFF } else { 0x00 });
                    encode_num(n, key.descending, out);
                }
                None => {
                    out.push(if key.descending { 0xFE } else { 0x01 });
                    encode_str(field, key.descending, out);
                }
            },
        }
    }
    Ok(())
}

/// Map an f64 to 8 order-preserving bytes: flip the sign bit for positives,
/// all bits for negatives; every bit inverted again for descending.
fn encode_num(n: f64, descending: bool, out: &mut Vec<u8>) {
    let bits = n.to_bits();
    let ordered = if bits >> 63 == 1 {
        !bits
    } else {
        bits ^ (1 << 63)
    };
    let ordered = if descending { !ordered } else { ordered };
    out.extend_from_slice(&ordered.to_be_bytes());
}

/// The cell's bytes plus a `0x00` terminator, every byte inverted for
/// descending (so the terminator becomes `0xFF`). Bulk-copied, then
/// inverted in place: this runs per row.
fn encode_str(field: Option<&Field>, descending: bool, out: &mut Vec<u8>) {
    let start = out.len();
    if let Some(f) = field {
        out.extend_from_slice(f.as_str().as_bytes());
    }
    out.push(0x00);
    if descending {
        for b in &mut out[start..] {
            *b = !*b;
        }
    }
}

// --- the parallel sorter ----------------------------------------------------

/// Shared, immutable per-run context handed to every worker.
struct WorkerCtx {
    sort: Arc<SortStmt>,
    pre: Arc<Vec<Stmt>>,
    in_mem: AtomicUsize,
    budget: usize,
    temp_dir: PathBuf,
}

/// Farms raw input blocks to `N` worker threads, each of which parses, applies
/// the pre-sort statements, serializes + key-encodes, and sorts its block into
/// a run. [`Sorter::finish`] joins the workers and merges.
pub struct Sorter {
    sort: Arc<SortStmt>,
    block_size: usize,
    threads: usize,
    temp_dir: PathBuf,
    fanout: usize,
    seq: u64,
    work_tx: Option<Sender<(u64, String)>>,
    results: Receiver<Result<Run, Error>>,
    workers: Vec<JoinHandle<()>>,
}

impl Sorter {
    pub fn new(
        sort: &SortStmt,
        pre: &[Stmt],
        threads: usize,
        temp_dir: PathBuf,
        budget: usize,
    ) -> Self {
        let threads = threads.max(1);
        let budget = budget.max(1);
        let block_size = (budget / (2 * threads)).clamp(4 << 20, 64 << 20);
        Self::with_params(sort, pre, threads, temp_dir, budget, block_size)
    }

    /// Like [`Sorter::new`] but with an explicit block size (tests push blocks
    /// directly, so they use this only to set thread count and budget).
    pub fn with_params(
        sort: &SortStmt,
        pre: &[Stmt],
        threads: usize,
        temp_dir: PathBuf,
        budget: usize,
        block_size: usize,
    ) -> Self {
        let threads = threads.max(1);
        let sort = Arc::new(sort.clone());
        let ctx = Arc::new(WorkerCtx {
            sort: Arc::clone(&sort),
            pre: Arc::new(pre.to_vec()),
            in_mem: AtomicUsize::new(0),
            budget,
            temp_dir: temp_dir.clone(),
        });

        let (work_tx, work_rx) = bounded::<(u64, String)>(threads);
        let (res_tx, res_rx) = unbounded::<Result<Run, Error>>();

        let mut workers = Vec::with_capacity(threads);
        for _ in 0..threads {
            let work_rx = work_rx.clone();
            let res_tx = res_tx.clone();
            let ctx = Arc::clone(&ctx);
            workers.push(thread::spawn(move || {
                while let Ok((seq, block)) = work_rx.recv() {
                    if res_tx.send(make_run(&ctx, seq, &block)).is_err() {
                        break;
                    }
                }
            }));
        }

        Sorter {
            sort,
            block_size,
            threads,
            temp_dir,
            fanout: DEFAULT_FANOUT,
            seq: 0,
            work_tx: Some(work_tx),
            results: res_rx,
            workers,
        }
    }

    /// Override the merge fan-out (tests use a small value to force multi-level
    /// merging with few runs).
    #[cfg(test)]
    fn set_fanout(&mut self, fanout: usize) {
        self.fanout = fanout.max(2);
    }

    /// The block size the caller should read input in.
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Hand one input block to a worker (one block becomes one run).
    pub fn push_block(&mut self, block: String) {
        let seq = self.seq;
        self.seq += 1;
        let _ = self.work_tx.as_ref().unwrap().send((seq, block));
    }

    /// Join the workers and return the merge over their runs.
    pub fn finish(mut self) -> Result<Merge, Error> {
        drop(self.work_tx.take());
        let mut runs = Vec::new();
        let mut first_err = None;
        for result in self.results.iter() {
            match result {
                Ok(run) => runs.push(run),
                Err(e) if first_err.is_none() => first_err = Some(e),
                Err(_) => {}
            }
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        // Multi-level merge: with more than `fanout` runs, merge groups of them
        // into larger runs first (in parallel) so the final merge is bounded.
        let runs = consolidate(runs, &self.sort, self.threads, &self.temp_dir, self.fanout)?;
        Merge::new(runs, Arc::clone(&self.sort))
    }
}

/// Reduce `runs` to at most `fanout` runs by repeatedly merging groups of
/// `fanout` (in input/`seq` order) into larger spilled runs. Groups within a
/// level are merged in parallel across `threads` workers.
fn consolidate(
    mut runs: Vec<Run>,
    sort: &Arc<SortStmt>,
    threads: usize,
    temp_dir: &Path,
    fanout: usize,
) -> Result<Vec<Run>, Error> {
    let fanout = fanout.max(2);
    while runs.len() > fanout {
        runs.sort_by_key(|r| r.seq);
        let mut groups: Vec<Vec<Run>> = Vec::new();
        let mut iter = runs.into_iter();
        loop {
            let group: Vec<Run> = iter.by_ref().take(fanout).collect();
            if group.is_empty() {
                break;
            }
            groups.push(group);
        }
        runs = merge_groups(groups, sort, threads, temp_dir)?;
    }
    runs.sort_by_key(|r| r.seq);
    Ok(runs)
}

/// Merge each group of runs into one spilled run, across `threads` workers.
fn merge_groups(
    groups: Vec<Vec<Run>>,
    sort: &Arc<SortStmt>,
    threads: usize,
    temp_dir: &Path,
) -> Result<Vec<Run>, Error> {
    let (work_tx, work_rx) = unbounded::<Vec<Run>>();
    for group in groups {
        let _ = work_tx.send(group);
    }
    drop(work_tx);
    let (res_tx, res_rx) = unbounded::<Result<Run, Error>>();

    thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            let work_rx = work_rx.clone();
            let res_tx = res_tx.clone();
            let sort = Arc::clone(sort);
            scope.spawn(move || {
                while let Ok(group) = work_rx.recv() {
                    if res_tx.send(merge_group(group, &sort, temp_dir)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(work_rx);
        drop(res_tx);

        let mut out = Vec::new();
        let mut first_err = None;
        for result in res_rx.iter() {
            match result {
                Ok(run) => out.push(run),
                Err(e) if first_err.is_none() => first_err = Some(e),
                Err(_) => {}
            }
        }
        first_err.map_or(Ok(out), Err)
    })
}

/// Merge one group of runs into a single spilled run. Its `seq` is the group's
/// minimum, so consolidated runs stay in input order (keeping the sort stable).
fn merge_group(group: Vec<Run>, sort: &Arc<SortStmt>, temp_dir: &Path) -> Result<Run, Error> {
    let seq = group.iter().map(|r| r.seq).min().unwrap_or(0);
    let file_seq = RUN_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
    let path = temp_dir.join(format!("csvm.{}.{}.tmp", std::process::id(), file_seq));
    let mut writer = BufWriter::new(File::create(&path)?);
    Merge::new(group, Arc::clone(sort))?.for_each_line(|line| {
        writer.write_all(line)?;
        Ok(())
    })?;
    writer.flush()?;
    Ok(Run {
        seq,
        data: RunData::File(TempRun { path }),
    })
}

/// Parse a block, apply the pre-sort statements, serialize + key-encode each
/// survivor, sort by key, and return the run (in memory, or spilled).
fn make_run(ctx: &WorkerCtx, seq: u64, block: &str) -> Result<Run, Error> {
    let mut blob: Vec<u8> = Vec::with_capacity(block.len());
    let mut lines: Vec<(u32, u32)> = Vec::new();
    let mut keys: Vec<Box<[u8]>> = Vec::new();
    let mut scratch: Vec<Field> = Vec::new();
    let mut line = String::new();
    let mut key_buf: Vec<u8> = Vec::new();
    let mut err: Option<Error> = None;

    csv::parse_chunk(block, |row| {
        if err.is_some() {
            return;
        }
        match apply_stmts(
            &ctx.pre,
            row,
            &mut scratch,
            &crate::plan::EvalCtx::default(),
        ) {
            Ok(true) => {
                key_buf.clear();
                if let Err(e) = encode_key(row, &ctx.sort, &mut key_buf) {
                    err = Some(e);
                    return;
                }
                let start = blob.len() as u32;
                line.clear();
                csv::write_row(&mut line, row);
                blob.extend_from_slice(line.as_bytes());
                lines.push((start, blob.len() as u32));
                keys.push(key_buf.as_slice().into());
            }
            Ok(false) => {}
            Err(e) => err = Some(e),
        }
    });
    if let Some(e) = err {
        return Err(e);
    }

    // Stable sort an index by key (ties keep input order within the block).
    let mut order: Vec<u32> = (0..lines.len() as u32).collect();
    order.sort_by(|&a, &b| keys[a as usize].cmp(&keys[b as usize]));

    // Materialize the sorted order so the merge can stream sequentially.
    let lines_sorted: Vec<(u32, u32)> = order.iter().map(|&i| lines[i as usize]).collect();
    let bytes = blob.len() + keys.iter().map(|k| k.len() + 16).sum::<usize>();

    let prev = ctx.in_mem.fetch_add(bytes, AtomicOrdering::Relaxed);
    if prev + bytes <= ctx.budget {
        let keys_sorted: Vec<Box<[u8]>> = order.iter().map(|&i| keys[i as usize].clone()).collect();
        Ok(Run {
            seq,
            data: RunData::Mem {
                blob,
                lines: lines_sorted,
                keys: keys_sorted,
            },
        })
    } else {
        ctx.in_mem.fetch_sub(bytes, AtomicOrdering::Relaxed);
        let file = spill(&blob, &lines_sorted, &ctx.temp_dir)?;
        Ok(Run {
            seq,
            data: RunData::File(file),
        })
    }
}

/// Write a run's lines (already in sorted order) to a fresh temp file.
fn spill(blob: &[u8], lines: &[(u32, u32)], temp_dir: &Path) -> Result<TempRun, Error> {
    let seq = RUN_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
    let path = temp_dir.join(format!("csvm.{}.{}.tmp", std::process::id(), seq));
    let mut writer = BufWriter::new(File::create(&path)?);
    for &(start, end) in lines {
        writer.write_all(&blob[start as usize..end as usize])?;
    }
    writer.flush()?;
    Ok(TempRun { path })
}

// --- the merge --------------------------------------------------------------

/// k-way merge over sorted runs. Single-threaded; emits already-serialized line
/// bytes via a callback so there is no per-row allocation on output.
pub struct Merge {
    sources: Vec<Source>,
    heap: BinaryHeap<Reverse<HeapKey>>,
    sort: Arc<SortStmt>,
}

impl Merge {
    fn new(runs: Vec<Run>, sort: Arc<SortStmt>) -> Result<Self, Error> {
        let mut sources: Vec<Source> = runs
            .into_iter()
            .map(Source::new)
            .collect::<Result<_, _>>()?;
        let mut heap = BinaryHeap::with_capacity(sources.len());
        for (idx, source) in sources.iter_mut().enumerate() {
            if let Some(key) = source.key(&sort)? {
                heap.push(Reverse(HeapKey {
                    key,
                    seq: source.seq,
                    idx,
                }));
            }
        }
        Ok(Merge {
            sources,
            heap,
            sort,
        })
    }

    /// Drive the merge, calling `emit` with each row's line bytes in order.
    pub fn for_each_line<F>(mut self, mut emit: F) -> Result<(), Error>
    where
        F: FnMut(&[u8]) -> Result<(), Error>,
    {
        while let Some(Reverse(item)) = self.heap.pop() {
            emit(self.sources[item.idx].current_line())?;
            self.sources[item.idx].advance()?;
            if let Some(key) = self.sources[item.idx].key(&self.sort)? {
                self.heap.push(Reverse(HeapKey {
                    key,
                    seq: item.seq,
                    idx: item.idx,
                }));
            }
        }
        Ok(())
    }
}

/// Heap entry: a run's current encoded key. Ordered by key bytes, then by run
/// sequence so equal keys keep input order (stability).
struct HeapKey {
    key: Box<[u8]>,
    seq: u64,
    idx: usize,
}

impl Ord for HeapKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key).then(self.seq.cmp(&other.seq))
    }
}
impl PartialOrd for HeapKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for HeapKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for HeapKey {}

/// One run being consumed by the merge. Holds the current line's bytes and key.
struct Source {
    seq: u64,
    kind: SourceKind,
}

enum SourceKind {
    Mem {
        blob: Vec<u8>,
        lines: Vec<(u32, u32)>,
        keys: Vec<Box<[u8]>>,
        pos: usize,
    },
    File {
        reader: BufReader<File>,
        _run: TempRun,
        line: Vec<u8>,
    },
}

impl Source {
    fn new(run: Run) -> Result<Self, Error> {
        let kind = match run.data {
            RunData::Mem { blob, lines, keys } => SourceKind::Mem {
                blob,
                lines,
                keys,
                pos: 0,
            },
            RunData::File(temp) => {
                let mut reader = BufReader::new(File::open(&temp.path)?);
                let mut line = Vec::new();
                read_line(&mut reader, &mut line)?; // prime the first line
                SourceKind::File {
                    reader,
                    _run: temp,
                    line,
                }
            }
        };
        Ok(Source { seq: run.seq, kind })
    }

    /// The encoded key of the current row, or `None` if the run is exhausted.
    /// Must be called exactly once per row (Mem keys are moved out).
    fn key(&mut self, sort: &SortStmt) -> Result<Option<Box<[u8]>>, Error> {
        match &mut self.kind {
            SourceKind::Mem {
                keys, pos, lines, ..
            } => Ok((*pos < lines.len()).then(|| std::mem::take(&mut keys[*pos]))),
            SourceKind::File { line, .. } => {
                if line.is_empty() {
                    Ok(None)
                } else {
                    encode_file_line(line, sort).map(Some)
                }
            }
        }
    }

    /// The current row's line bytes (valid until the next [`Source::advance`]).
    fn current_line(&self) -> &[u8] {
        match &self.kind {
            SourceKind::Mem {
                blob, lines, pos, ..
            } => {
                let (start, end) = lines[*pos];
                &blob[start as usize..end as usize]
            }
            SourceKind::File { line, .. } => line,
        }
    }

    /// Advance to the next row (call [`Source::key`] afterwards for its key).
    fn advance(&mut self) -> Result<(), Error> {
        match &mut self.kind {
            SourceKind::Mem { pos, .. } => {
                *pos += 1;
                Ok(())
            }
            SourceKind::File { reader, line, .. } => read_line(reader, line),
        }
    }
}

/// Read one line (including its newline) into `buf`; `buf` is empty at EOF.
fn read_line(reader: &mut BufReader<File>, buf: &mut Vec<u8>) -> Result<(), Error> {
    buf.clear();
    reader.read_until(b'\n', buf)?;
    Ok(())
}

/// Parse a spilled line and encode its sort key.
fn encode_file_line(line: &[u8], sort: &SortStmt) -> Result<Box<[u8]>, Error> {
    let text = std::str::from_utf8(line)
        .map_err(|e| Error::Other(format!("input is not valid UTF-8: {e}")))?;
    let mut key = Vec::new();
    let mut err = None;
    csv::parse_chunk(text, |row| {
        if err.is_none()
            && let Err(e) = encode_key(row, sort, &mut key)
        {
            err = Some(e);
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(key.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{SortKey, SortMode, SortStmt};

    fn key(pos: usize, descending: bool, mode: SortMode) -> SortKey {
        SortKey {
            name: "k".into(),
            pos,
            descending,
            mode,
        }
    }

    fn sort_lines(sort: &SortStmt, lines: &[&str], threads: usize, budget: usize) -> Vec<String> {
        let mut s = Sorter::with_params(sort, &[], threads, std::env::temp_dir(), budget, 1 << 20);
        for line in lines {
            s.push_block(format!("{line}\n"));
        }
        let mut out = Vec::new();
        s.finish()
            .unwrap()
            .for_each_line(|line| {
                out.push(
                    String::from_utf8(line.to_vec())
                        .unwrap()
                        .trim_end()
                        .to_string(),
                );
                Ok(())
            })
            .unwrap();
        out
    }

    #[test]
    fn numeric_ascending() {
        let s = SortStmt {
            keys: vec![key(0, false, SortMode::Numeric)],
        };
        assert_eq!(
            sort_lines(&s, &["10", "9", "100", "2", "-5"], 4, 1 << 30),
            ["-5", "2", "9", "10", "100"]
        );
    }

    #[test]
    fn numeric_descending_and_spilling() {
        let s = SortStmt {
            keys: vec![key(0, true, SortMode::Numeric)],
        };
        // budget = 1 forces spilling so the file path is exercised too.
        assert_eq!(
            sort_lines(&s, &["3", "1", "2", "10"], 4, 1),
            ["10", "3", "2", "1"]
        );
    }

    #[test]
    fn string_keys() {
        let s = SortStmt {
            keys: vec![key(0, false, SortMode::Lexical)],
        };
        assert_eq!(
            sort_lines(&s, &["banana", "apple", "cherry", "ab"], 4, 1 << 30),
            ["ab", "apple", "banana", "cherry"]
        );
        let s = SortStmt {
            keys: vec![key(0, true, SortMode::Lexical)],
        };
        assert_eq!(
            sort_lines(&s, &["banana", "apple", "cherry"], 4, 1),
            ["cherry", "banana", "apple"]
        );
    }

    #[test]
    fn auto_keys_numbers_first_then_text() {
        // Auto: cells that parse as numbers order numerically and come first
        // (a blank reads as 0, as in select); text follows in lexical order.
        // Spilled runs (budget 1) must agree with the in-memory encoding.
        let s = SortStmt {
            keys: vec![key(0, false, SortMode::Auto)],
        };
        let lines = ["10", "b", "9", "", "100", "a", "-1.5"];
        for budget in [1usize << 30, 1] {
            assert_eq!(
                sort_lines(&s, &lines, 4, budget),
                ["-1.5", "", "9", "10", "100", "a", "b"]
            );
        }
        // Descending reverses the whole order, tag included.
        let s = SortStmt {
            keys: vec![key(0, true, SortMode::Auto)],
        };
        assert_eq!(
            sort_lines(&s, &lines, 4, 1),
            ["b", "a", "100", "10", "9", "", "-1.5"]
        );
    }

    #[test]
    fn stable_on_ties() {
        // Sort by col 0; equal keys keep input order even one-row-per-block.
        let s = SortStmt {
            keys: vec![key(0, false, SortMode::Numeric)],
        };
        assert_eq!(
            sort_lines(&s, &["1,a", "1,b", "0,c", "1,d"], 4, 1 << 30),
            ["0,c", "1,a", "1,b", "1,d"]
        );
    }

    #[test]
    fn multi_key() {
        // grp ascending (string), then val descending (numeric).
        let s = SortStmt {
            keys: vec![
                key(0, false, SortMode::Lexical),
                key(1, true, SortMode::Numeric),
            ],
        };
        assert_eq!(
            sort_lines(&s, &["a,1", "b,5", "a,9", "b,2"], 4, 1),
            ["a,9", "a,1", "b,5", "b,2"]
        );
    }

    /// Sort with a forced fan-out so groups of runs are merged in several
    /// levels before the final merge.
    fn sort_multilevel(lines: &[&str], threads: usize, budget: usize, fanout: usize) -> Vec<i64> {
        let sort = SortStmt {
            keys: vec![key(0, false, SortMode::Numeric)],
        };
        let mut s = Sorter::with_params(&sort, &[], threads, std::env::temp_dir(), budget, 1 << 20);
        s.set_fanout(fanout);
        for line in lines {
            s.push_block(format!("{line}\n"));
        }
        let mut out = Vec::new();
        s.finish()
            .unwrap()
            .for_each_line(|line| {
                let n: i64 = std::str::from_utf8(line)
                    .unwrap()
                    .trim_end()
                    .parse()
                    .unwrap();
                out.push(n);
                Ok(())
            })
            .unwrap();
        out
    }

    #[test]
    fn multi_level_merge_spilled_and_in_memory() {
        let lines: Vec<String> = (0..40).map(|i| ((i * 17) % 40).to_string()).collect();
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let mut expected: Vec<i64> = refs.iter().map(|s| s.parse().unwrap()).collect();
        expected.sort();

        // fanout = 3 with 40 one-row blocks forces multiple merge levels.
        // budget = 1 spills every run; a large budget keeps them in memory and
        // consolidation merges the in-memory runs into files.
        assert_eq!(sort_multilevel(&refs, 4, 1, 3), expected, "spilled");
        assert_eq!(sort_multilevel(&refs, 4, 1 << 30, 3), expected, "in-memory");
        assert_eq!(sort_multilevel(&refs, 1, 1, 2), expected, "single thread");
    }

    #[test]
    fn multi_level_merge_is_stable() {
        // Tag column carried along; equal keys must keep input order across all
        // the merge levels.
        let sort = SortStmt {
            keys: vec![key(0, false, SortMode::Numeric)],
        };
        let lines: Vec<String> = (0..30).map(|i| format!("{},{i}", i % 3)).collect();
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let mut s = Sorter::with_params(&sort, &[], 4, std::env::temp_dir(), 1, 1 << 20);
        s.set_fanout(2);
        for line in &refs {
            s.push_block(format!("{line}\n"));
        }
        let mut out = Vec::new();
        s.finish()
            .unwrap()
            .for_each_line(|line| {
                out.push(std::str::from_utf8(line).unwrap().trim_end().to_string());
                Ok(())
            })
            .unwrap();
        // Within each key group, tags ascend (input order preserved).
        for grp in 0..3 {
            let tags: Vec<i64> = out
                .iter()
                .filter(|r| r.starts_with(&format!("{grp},")))
                .map(|r| r.split(',').nth(1).unwrap().parse().unwrap())
                .collect();
            let mut sorted = tags.clone();
            sorted.sort();
            assert_eq!(tags, sorted, "group {grp} not stable");
        }
    }
}
