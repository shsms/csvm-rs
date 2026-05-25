//! Parallel external merge sort, modeled on csvm's sort stage.
//!
//! The driving thread reads raw input **blocks**; `N` worker threads each
//! **parse a block, apply the pre-sort statements, and sort the survivors**
//! (the expensive work, done in parallel), keeping the sorted run in memory or
//! spilling it to a temp file when the in-memory budget is exceeded. A block is
//! a contiguous input range, so its sequence number alone keeps the final
//! **single-threaded k-way merge** (a binary heap) stable by breaking key ties
//! in input order — the role csvm's `orig_chunk_id` plays.
//!
//! Small inputs never touch disk (everything stays in memory). The merge is
//! single-level; multi-level merge and a parallel merge (csvm's `merge_tmp`
//! workers) are left for a later pass.

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
use crate::plan::{SortStmt, Stmt, apply_stmts};

/// An owned row, detached from any chunk buffer.
pub type OwnedRow = Vec<Field<'static>>;

/// Default in-memory budget before runs start spilling to disk.
pub const DEFAULT_BUDGET_BYTES: usize = 256 << 20;

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

/// A sorted run: rows in sorted order, in memory or on disk. `seq` is the input
/// batch order, used to keep the merge stable.
struct Run {
    seq: u64,
    data: RunData,
}

enum RunData {
    Mem(Vec<OwnedRow>),
    File(TempRun),
}

// --- shared helpers ---------------------------------------------------------

fn coerce_numeric(row: &mut OwnedRow, numeric: &[usize]) -> Result<(), Error> {
    for &p in numeric {
        if let Some(f) = row.get_mut(p) {
            *f = Field::Num(f.coerce_num()?);
        }
    }
    Ok(())
}

/// Coerce numeric keys and stably sort a batch in place.
fn sort_batch(batch: &mut [OwnedRow], sort: &SortStmt, numeric: &[usize]) -> Result<(), Error> {
    for row in batch.iter_mut() {
        coerce_numeric(row, numeric)?;
    }
    batch.sort_by(|a, b| sort.compare(a, b));
    Ok(())
}

/// Write a sorted batch to a fresh temp file.
fn spill(batch: &[OwnedRow], temp_dir: &Path) -> Result<TempRun, Error> {
    let seq = RUN_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
    let path = temp_dir.join(format!("csvm.{}.{}.tmp", std::process::id(), seq));
    let mut writer = BufWriter::new(File::create(&path)?);
    let mut line = String::new();
    for row in batch {
        line.clear();
        csv::write_row(&mut line, row);
        writer.write_all(line.as_bytes())?;
    }
    writer.flush()?;
    Ok(TempRun { path })
}

/// Rough in-memory size of a row, for the spill budget.
fn row_bytes(row: &OwnedRow) -> usize {
    let mut n = 16;
    for f in row {
        n += match f {
            Field::Str(s) => s.len(),
            Field::Owned(s) => s.len() + 16,
            Field::Num(_) => 8,
        } + 8;
    }
    n
}

// --- the parallel sorter ----------------------------------------------------

/// Shared, immutable per-run context handed to every worker.
struct WorkerCtx {
    sort: Arc<SortStmt>,
    numeric: Vec<usize>,
    pre: Arc<Vec<Stmt>>,
    in_mem: AtomicUsize,
    budget: usize,
    temp_dir: PathBuf,
}

/// Farms raw input **blocks** out to `N` worker threads, each of which parses
/// its block, applies the pre-sort statements, sorts the survivors, and emits a
/// run. A block is a contiguous input range, so its sequence number alone keeps
/// the final merge stable. [`Sorter::finish`] joins the workers and merges.
pub struct Sorter {
    sort: Arc<SortStmt>,
    numeric: Arc<Vec<usize>>,
    block_size: usize,
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
        // Several blocks per thread (so parse+sort parallelizes) while keeping
        // spilled-run files few enough to merge in one level.
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
        let numeric: Arc<Vec<usize>> = Arc::new(sort.numeric_positions().collect());
        let ctx = Arc::new(WorkerCtx {
            sort: Arc::clone(&sort),
            numeric: numeric.as_ref().clone(),
            pre: Arc::new(pre.to_vec()),
            in_mem: AtomicUsize::new(0),
            budget,
            temp_dir,
        });

        let (work_tx, work_rx) = bounded::<(u64, String)>(threads);
        // Unbounded so workers never block returning a run; outstanding runs are
        // bounded by the in-memory budget (Mem runs) plus small file handles.
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
            numeric,
            block_size,
            seq: 0,
            work_tx: Some(work_tx),
            results: res_rx,
            workers,
        }
    }

    /// The block size the caller should read input in.
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Hand one input block to a worker (one block becomes one run).
    pub fn push_block(&mut self, block: String) {
        let seq = self.seq;
        self.seq += 1;
        // A send error means the workers are gone; it surfaces from `finish`.
        let _ = self.work_tx.as_ref().unwrap().send((seq, block));
    }

    /// Join the workers and return the merged stream.
    pub fn finish(mut self) -> Result<Merge, Error> {
        drop(self.work_tx.take()); // workers exit once the work channel drains

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
        Merge::new(runs, self.sort, self.numeric)
    }
}

/// Parse a block, apply the pre-sort statements, sort the survivors, and return
/// one run (kept in memory, or spilled if over budget).
fn make_run(ctx: &WorkerCtx, seq: u64, block: &str) -> Result<Run, Error> {
    let mut batch: Vec<OwnedRow> = Vec::new();
    let mut err: Option<Error> = None;
    csv::parse_chunk(block, |row| {
        if err.is_some() {
            return;
        }
        match apply_stmts(&ctx.pre, row) {
            Ok(true) => batch.push(row.iter().map(|f| f.clone().into_owned()).collect()),
            Ok(false) => {}
            Err(e) => err = Some(e),
        }
    });
    if let Some(e) = err {
        return Err(e);
    }

    sort_batch(&mut batch, &ctx.sort, &ctx.numeric)?;
    let bytes: usize = batch.iter().map(row_bytes).sum();

    // Keep the run in memory if doing so stays within budget, otherwise spill.
    // Racy by a few blocks across threads, which is fine for a soft budget.
    let prev = ctx.in_mem.fetch_add(bytes, AtomicOrdering::Relaxed);
    if prev + bytes <= ctx.budget {
        Ok(Run {
            seq,
            data: RunData::Mem(batch),
        })
    } else {
        ctx.in_mem.fetch_sub(bytes, AtomicOrdering::Relaxed);
        Ok(Run {
            seq,
            data: RunData::File(spill(&batch, &ctx.temp_dir)?),
        })
    }
}

// --- the merge --------------------------------------------------------------

/// k-way merge over sorted runs, yielding rows in sorted order. Single-threaded
/// and single-level.
pub struct Merge {
    sources: Vec<Source>,
    heap: BinaryHeap<Reverse<HeapKey>>,
    sort: Arc<SortStmt>,
    numeric: Arc<Vec<usize>>,
}

impl Merge {
    fn new(runs: Vec<Run>, sort: Arc<SortStmt>, numeric: Arc<Vec<usize>>) -> Result<Self, Error> {
        let mut sources: Vec<Source> = runs
            .into_iter()
            .map(Source::new)
            .collect::<Result<_, _>>()?;
        let mut heap = BinaryHeap::with_capacity(sources.len());
        for (idx, source) in sources.iter_mut().enumerate() {
            if let Some(row) = source.next(&numeric)? {
                heap.push(Reverse(HeapKey {
                    row,
                    idx,
                    seq: source.seq,
                    sort: Arc::clone(&sort),
                }));
            }
        }
        Ok(Merge {
            sources,
            heap,
            sort,
            numeric,
        })
    }

    fn next_row(&mut self) -> Option<Result<OwnedRow, Error>> {
        let Reverse(item) = self.heap.pop()?;
        match self.sources[item.idx].next(&self.numeric) {
            Ok(Some(row)) => self.heap.push(Reverse(HeapKey {
                row,
                idx: item.idx,
                seq: item.seq,
                sort: Arc::clone(&self.sort),
            })),
            Ok(None) => {}
            Err(e) => return Some(Err(e)),
        }
        Some(Ok(item.row))
    }
}

impl Iterator for Merge {
    type Item = Result<OwnedRow, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        self.next_row()
    }
}

/// A heap entry: the current row of one run. Ordered by the sort keys, then by
/// run input sequence (and run index) so equal keys keep input order.
struct HeapKey {
    row: OwnedRow,
    idx: usize,
    seq: u64,
    sort: Arc<SortStmt>,
}

impl Ord for HeapKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sort
            .compare(&self.row, &other.row)
            .then(self.seq.cmp(&other.seq))
            .then(self.idx.cmp(&other.idx))
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

/// One run being consumed by the merge.
struct Source {
    seq: u64,
    kind: SourceKind,
}

enum SourceKind {
    Mem(std::vec::IntoIter<OwnedRow>),
    File {
        reader: BufReader<File>,
        _run: TempRun,
        line: String,
    },
}

impl Source {
    fn new(run: Run) -> Result<Self, Error> {
        let seq = run.seq;
        let kind = match run.data {
            RunData::Mem(rows) => SourceKind::Mem(rows.into_iter()),
            RunData::File(temp) => SourceKind::File {
                reader: BufReader::new(File::open(&temp.path)?),
                _run: temp,
                line: String::new(),
            },
        };
        Ok(Source { seq, kind })
    }

    fn next(&mut self, numeric: &[usize]) -> Result<Option<OwnedRow>, Error> {
        match &mut self.kind {
            SourceKind::Mem(it) => Ok(it.next()),
            SourceKind::File { reader, line, .. } => {
                line.clear();
                if reader.read_line(line)? == 0 {
                    return Ok(None);
                }
                // Each spilled row is one line (input fields never contain a
                // newline), so a single parse yields exactly one row.
                let mut row: Option<OwnedRow> = None;
                csv::parse_chunk(line, |r| {
                    if row.is_none() {
                        row = Some(r.iter().map(|f| f.clone().into_owned()).collect());
                    }
                });
                let mut row = row.unwrap_or_default();
                coerce_numeric(&mut row, numeric)?;
                Ok(Some(row))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{SortKey, SortStmt};

    fn numeric_key(pos: usize, descending: bool) -> SortStmt {
        SortStmt {
            keys: vec![SortKey {
                name: "k".into(),
                pos,
                descending,
                numeric: true,
            }],
        }
    }

    /// Feed each CSV line as its own block (so it becomes its own run) and
    /// collect the fully merged rows.
    fn sort_lines(
        sort: &SortStmt,
        lines: &[&str],
        threads: usize,
        budget: usize,
    ) -> Vec<Vec<String>> {
        let mut s = Sorter::with_params(sort, &[], threads, std::env::temp_dir(), budget, 1 << 20);
        for line in lines {
            s.push_block(format!("{line}\n"));
        }
        s.finish()
            .unwrap()
            .map(|r| r.unwrap().iter().map(|f| f.as_str().into_owned()).collect())
            .collect()
    }

    fn col0(rows: Vec<Vec<String>>) -> Vec<String> {
        rows.into_iter().map(|r| r[0].clone()).collect()
    }

    #[test]
    fn in_memory_single_thread() {
        let got = col0(sort_lines(
            &numeric_key(0, false),
            &["10", "9", "100", "2"],
            1,
            1 << 30,
        ));
        assert_eq!(got, ["2", "9", "10", "100"]);
    }

    #[test]
    fn parallel_in_memory_many_runs() {
        // One run per line across 4 threads; the heap merge must still produce a
        // fully sorted result.
        let got = col0(sort_lines(
            &numeric_key(0, false),
            &["5", "1", "9", "3", "7", "2", "8", "4", "6", "0"],
            4,
            1 << 30,
        ));
        assert_eq!(got, ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"]);
    }

    #[test]
    fn parallel_spilling_matches() {
        // budget = 1 forces every run to spill to a temp file.
        let got = col0(sort_lines(
            &numeric_key(0, false),
            &["5", "1", "9", "3", "7", "2"],
            4,
            1,
        ));
        assert_eq!(got, ["1", "2", "3", "5", "7", "9"]);
    }

    #[test]
    fn parallel_merge_is_stable() {
        // Equal keys must keep input order even with one run per line across many
        // threads (out-of-order completion) — stability comes from the seq tie.
        let out = sort_lines(
            &numeric_key(0, false),
            &["1,a", "1,b", "0,c", "1,d"],
            4,
            1 << 30,
        );
        assert_eq!(
            out,
            vec![
                vec!["0", "c"],
                vec!["1", "a"],
                vec!["1", "b"],
                vec!["1", "d"],
            ]
        );
    }

    #[test]
    fn stable_within_a_spilled_block() {
        // A single multi-row block: stability within it must survive a spill.
        let mut s = Sorter::with_params(
            &numeric_key(0, false),
            &[],
            2,
            std::env::temp_dir(),
            1,
            1 << 20,
        );
        s.push_block("1,a\n1,b\n0,c\n1,d\n".to_string());
        let out: Vec<Vec<String>> = s
            .finish()
            .unwrap()
            .map(|r| r.unwrap().iter().map(|f| f.as_str().into_owned()).collect())
            .collect();
        assert_eq!(
            out,
            vec![
                vec!["0", "c"],
                vec!["1", "a"],
                vec!["1", "b"],
                vec!["1", "d"]
            ]
        );
    }

    #[test]
    fn descending() {
        let got = col0(sort_lines(&numeric_key(0, true), &["3", "1", "2"], 4, 1));
        assert_eq!(got, ["3", "2", "1"]);
    }
}
