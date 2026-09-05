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
//! Small inputs never touch disk; larger ones spill sorted runs to temp files
//! as key + line records, so a spilled key is exactly the one computed from
//! the live row (re-deriving it from the serialized line would round numbers
//! to six decimals). With many runs the merge is multi-level.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
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

/// Smallest raw input block a sort worker is handed; the worker count is
/// capped so `2 * workers` of them fit the budget. A higher floor keeps the
/// run count (temp files, merge levels) down.
const MIN_BLOCK: usize = 1 << 20;

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
    /// Sorted rows spilled to a temp file, one [`write_record`] each, so the
    /// merge reads the key back instead of re-deriving it from the line.
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
        let budget = budget.max(1);
        // Up to `2 * workers` raw blocks are in flight, so with the block-size
        // floor a large thread count would exceed the budget: cap the workers
        // at what the budget holds. The merge width is not bound by it.
        let workers = threads.clamp(1, (budget / (2 * MIN_BLOCK)).max(1));
        let block_size = (budget / (2 * workers)).clamp(MIN_BLOCK, 64 << 20);
        let mut sorter = Self::with_params(sort, pre, workers, temp_dir, budget, block_size);
        sorter.threads = threads.max(1);
        sorter
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

    /// Run-generation workers (the merge width is `threads`).
    #[cfg(test)]
    fn workers(&self) -> usize {
        self.workers.len()
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
        let runs = consolidate(runs, self.threads, &self.temp_dir, self.fanout)?;
        Merge::new(runs)
    }
}

/// Reduce `runs` to at most `fanout` runs by repeatedly merging groups of
/// `fanout` (in input/`seq` order) into larger spilled runs. Groups within a
/// level are merged in parallel across `threads` workers.
fn consolidate(
    mut runs: Vec<Run>,
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
        runs = merge_groups(groups, threads, temp_dir)?;
    }
    runs.sort_by_key(|r| r.seq);
    Ok(runs)
}

/// Merge each group of runs into one spilled run, across `threads` workers.
fn merge_groups(groups: Vec<Vec<Run>>, threads: usize, temp_dir: &Path) -> Result<Vec<Run>, Error> {
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
            scope.spawn(move || {
                while let Ok(group) = work_rx.recv() {
                    if res_tx.send(merge_group(group, temp_dir)).is_err() {
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
fn merge_group(group: Vec<Run>, temp_dir: &Path) -> Result<Run, Error> {
    let seq = group.iter().map(|r| r.seq).min().unwrap_or(0);
    let file = write_run(temp_dir, |w| {
        Merge::new(group)?.for_each_record(|key, line| write_record(w, key, line))
    })?;
    Ok(Run {
        seq,
        data: RunData::File(file),
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
    let keys_sorted: Vec<Box<[u8]>> = order
        .iter()
        .map(|&i| std::mem::take(&mut keys[i as usize]))
        .collect();
    let bytes = blob.len() + keys_sorted.iter().map(|k| k.len() + 16).sum::<usize>();

    let prev = ctx.in_mem.fetch_add(bytes, AtomicOrdering::Relaxed);
    if prev + bytes <= ctx.budget {
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
        let file = spill(&blob, &lines_sorted, &keys_sorted, &ctx.temp_dir)?;
        Ok(Run {
            seq,
            data: RunData::File(file),
        })
    }
}

/// Write a run's rows (already in sorted order) to a fresh temp file.
fn spill(
    blob: &[u8],
    lines: &[(u32, u32)],
    keys: &[Box<[u8]>],
    temp_dir: &Path,
) -> Result<TempRun, Error> {
    write_run(temp_dir, |w| {
        for (&(start, end), key) in lines.iter().zip(keys) {
            write_record(w, key, &blob[start as usize..end as usize])?;
        }
        Ok(())
    })
}

/// Create a fresh run file under `temp_dir`, let `write` fill it, and return
/// it. The file is owned from creation, so it is removed on an error too.
fn write_run<F>(temp_dir: &Path, write: F) -> Result<TempRun, Error>
where
    F: FnOnce(&mut BufWriter<File>) -> Result<(), Error>,
{
    let seq = RUN_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
    let path = temp_dir.join(format!("csvm.{}.{}.tmp", std::process::id(), seq));
    let mut writer = BufWriter::new(File::create(&path)?);
    let run = TempRun { path };
    write(&mut writer)?;
    writer.flush()?;
    Ok(run)
}

/// Append one spilled row: the two lengths (u32, little-endian), the encoded
/// key, then the CSV line. Length-prefixed because a key can hold any byte.
fn write_record<W: Write>(writer: &mut W, key: &[u8], line: &[u8]) -> Result<(), Error> {
    writer.write_all(&(key.len() as u32).to_le_bytes())?;
    writer.write_all(&(line.len() as u32).to_le_bytes())?;
    writer.write_all(key)?;
    writer.write_all(line)?;
    Ok(())
}

/// Read one spilled row written by [`write_record`] into `key` and `line`;
/// `false` at end of file, with both left empty.
fn read_record(
    reader: &mut BufReader<File>,
    key: &mut Vec<u8>,
    line: &mut Vec<u8>,
) -> Result<bool, Error> {
    if reader.fill_buf()?.is_empty() {
        key.clear();
        line.clear();
        return Ok(false);
    }
    let mut lens = [0u8; 8];
    reader.read_exact(&mut lens)?;
    let key_len = u32::from_le_bytes(lens[..4].try_into().unwrap()) as usize;
    let line_len = u32::from_le_bytes(lens[4..].try_into().unwrap()) as usize;
    key.resize(key_len, 0);
    reader.read_exact(key)?;
    line.resize(line_len, 0);
    reader.read_exact(line)?;
    Ok(true)
}

// --- the merge --------------------------------------------------------------

/// k-way merge over sorted runs. Single-threaded; emits already-serialized line
/// bytes via a callback so there is no per-row allocation on output.
pub struct Merge {
    sources: Vec<Source>,
    heap: BinaryHeap<Reverse<HeapKey>>,
}

impl Merge {
    fn new(runs: Vec<Run>) -> Result<Self, Error> {
        let mut sources: Vec<Source> = runs
            .into_iter()
            .map(Source::new)
            .collect::<Result<_, _>>()?;
        let mut heap = BinaryHeap::with_capacity(sources.len());
        for (idx, source) in sources.iter_mut().enumerate() {
            if let Some(key) = source.next_key(Vec::new())? {
                heap.push(Reverse(HeapKey {
                    key,
                    seq: source.seq,
                    idx,
                }));
            }
        }
        Ok(Merge { sources, heap })
    }

    /// Drive the merge, calling `emit` with each row's line bytes in order.
    pub fn for_each_line<F>(self, mut emit: F) -> Result<(), Error>
    where
        F: FnMut(&[u8]) -> Result<(), Error>,
    {
        self.for_each_record(|_, line| emit(line))
    }

    /// Drive the merge, calling `emit` with each row's encoded key and line
    /// bytes in order (an intermediate merge writes both back out).
    fn for_each_record<F>(mut self, mut emit: F) -> Result<(), Error>
    where
        F: FnMut(&[u8], &[u8]) -> Result<(), Error>,
    {
        while let Some(Reverse(item)) = self.heap.pop() {
            emit(&item.key, self.sources[item.idx].current_line())?;
            if let Some(key) = self.sources[item.idx].next_key(item.key)? {
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
    key: Vec<u8>,
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

/// One run being consumed by the merge. [`Source::next_key`] steps to a row
/// and hands out its key; [`Source::current_line`] is that row's bytes.
struct Source {
    seq: u64,
    kind: SourceKind,
}

enum SourceKind {
    Mem {
        blob: Vec<u8>,
        lines: Vec<(u32, u32)>,
        keys: Vec<Box<[u8]>>,
        /// Index of the next row to step to; the current row is `next - 1`.
        next: usize,
    },
    File {
        reader: BufReader<File>,
        _run: TempRun,
        /// The current row's line; empty once the run is exhausted.
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
                next: 0,
            },
            RunData::File(temp) => SourceKind::File {
                reader: BufReader::new(File::open(&temp.path)?),
                _run: temp,
                line: Vec::new(),
            },
        };
        Ok(Source { seq: run.seq, kind })
    }

    /// Step to the next row and return its encoded key, or `None` once the
    /// run is exhausted. `spent` is the key buffer the merge just emitted; a
    /// file run reads the next key into it, so the merge allocates nothing
    /// per row.
    fn next_key(&mut self, mut spent: Vec<u8>) -> Result<Option<Vec<u8>>, Error> {
        match &mut self.kind {
            SourceKind::Mem {
                keys, next, lines, ..
            } => {
                let key = keys.get_mut(*next).map(|k| std::mem::take(k).into_vec());
                *next = (*next + 1).min(lines.len());
                Ok(key)
            }
            SourceKind::File { reader, line, .. } => {
                Ok(read_record(reader, &mut spent, line)?.then_some(spent))
            }
        }
    }

    /// The current row's line bytes (valid until the next [`Source::next_key`]).
    fn current_line(&self) -> &[u8] {
        match &self.kind {
            SourceKind::Mem {
                blob, lines, next, ..
            } => {
                let (start, end) = lines[*next - 1];
                &blob[start as usize..end as usize]
            }
            SourceKind::File { line, .. } => line,
        }
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
        sort_lines_with(sort, &[], lines, threads, budget)
    }

    fn sort_lines_with(
        sort: &SortStmt,
        pre: &[Stmt],
        lines: &[&str],
        threads: usize,
        budget: usize,
    ) -> Vec<String> {
        let mut s = Sorter::with_params(sort, pre, threads, std::env::temp_dir(), budget, 1 << 20);
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
    fn block_size_follows_the_budget_and_thread_count() {
        let s = SortStmt {
            keys: vec![key(0, false, SortMode::Numeric)],
        };
        let block = |threads, budget| {
            Sorter::new(&s, &[], threads, std::env::temp_dir(), budget).block_size()
        };
        // Two blocks per worker in flight share the budget.
        assert_eq!(block(8, 256 << 20), 16 << 20);
        assert_eq!(block(64, 256 << 20), 2 << 20);
        // Floor and ceiling.
        assert_eq!(block(4, 1 << 20), MIN_BLOCK);
        assert_eq!(block(64, 1 << 20), MIN_BLOCK);
        assert_eq!(block(1, 1 << 30), 64 << 20);
        // Workers are capped so their in-flight blocks fit the budget; the
        // merge width stays as requested.
        let sorter = Sorter::new(&s, &[], 64, std::env::temp_dir(), 4 << 20);
        assert_eq!((sorter.workers(), sorter.threads), (2, 64));
        let sorter = Sorter::new(&s, &[], 4, std::env::temp_dir(), 1 << 20);
        assert_eq!((sorter.workers(), sorter.threads), (1, 4));
        let sorter = Sorter::new(&s, &[], 8, std::env::temp_dir(), 256 << 20);
        assert_eq!((sorter.workers(), sorter.threads), (8, 8));
    }

    #[test]
    fn spilled_runs_keep_full_key_precision() {
        // `add v num(v)` makes the sort key a `Field::Num`, which serializes
        // with six decimals. The spilled order must still be the in-memory
        // order, so values that differ past the sixth decimal (and the
        // stability of ties) cannot depend on `--sort-buffer`.
        let mut plan = crate::parse::parse("add v num(v) | sort v=nr").unwrap();
        plan.resolve(&["v".to_string(), "tag".to_string()]).unwrap();
        let [
            crate::plan::Stage::Transform(pre),
            crate::plan::Stage::Sort(sort),
        ] = plan.stages.as_slice()
        else {
            panic!("unexpected plan shape");
        };
        let lines = [
            "1.00000010,a",
            "1.00000030,b",
            "1.00000020,c",
            "1.00000025,d",
        ];
        let expected = ["1,b", "1,d", "1,c", "1,a"];
        assert_eq!(sort_lines_with(sort, pre, &lines, 4, 1 << 30), expected);
        assert_eq!(sort_lines_with(sort, pre, &lines, 4, 1), expected);
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
