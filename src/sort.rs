//! External merge sort with temp-file spilling.
//!
//! Rows are buffered until a memory budget is hit, then each batch is stably
//! sorted and written to a temp run file. [`ExternalSorter::finish`] either
//! returns the single in-memory run (small inputs never touch disk) or a k-way
//! merge over the run files. This is how csvm sorts files larger than memory.
//!
//! The merge is single-threaded and single-level (all runs merged at once);
//! parallelising it is a later refinement.

use std::cmp::Ordering;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

/// Process-global run-file counter so concurrent sorters never collide on a
/// temp file name (csvm uses the same trick).
static RUN_SEQ: AtomicU64 = AtomicU64::new(0);

use crate::csv;
use crate::error::Error;
use crate::field::Field;
use crate::plan::SortStmt;

/// An owned row, detached from any chunk buffer.
pub type OwnedRow = Vec<Field<'static>>;

/// Default in-memory budget before a run is spilled to disk.
pub const DEFAULT_BUDGET_BYTES: usize = 256 << 20;

/// A spilled run file; removed from disk when dropped.
struct TempRun {
    path: PathBuf,
}

impl Drop for TempRun {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Accumulates rows, spilling sorted runs once the budget is exceeded.
pub struct ExternalSorter<'a> {
    sort: &'a SortStmt,
    numeric: Vec<usize>,
    budget_bytes: usize,
    buffer: Vec<OwnedRow>,
    buffer_bytes: usize,
    runs: Vec<TempRun>,
    temp_dir: PathBuf,
}

impl<'a> ExternalSorter<'a> {
    pub fn new(sort: &'a SortStmt, temp_dir: PathBuf) -> Self {
        Self::with_budget(sort, temp_dir, DEFAULT_BUDGET_BYTES)
    }

    pub fn with_budget(sort: &'a SortStmt, temp_dir: PathBuf, budget_bytes: usize) -> Self {
        ExternalSorter {
            numeric: sort.numeric_positions().collect(),
            sort,
            budget_bytes: budget_bytes.max(1),
            buffer: Vec::new(),
            buffer_bytes: 0,
            runs: Vec::new(),
            temp_dir,
        }
    }

    /// Add a row. Numeric sort-key columns are converted up front so the
    /// comparator never reparses.
    pub fn push(&mut self, mut row: OwnedRow) -> Result<(), Error> {
        coerce_numeric(&mut row, &self.numeric)?;
        self.buffer_bytes += row_bytes(&row);
        self.buffer.push(row);
        if self.buffer_bytes >= self.budget_bytes {
            self.spill()?;
        }
        Ok(())
    }

    fn spill(&mut self) -> Result<(), Error> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.sort_buffer();
        let seq = RUN_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
        let path = self
            .temp_dir
            .join(format!("csvm.{}.{}.tmp", std::process::id(), seq));
        let mut writer = BufWriter::new(File::create(&path)?);
        let mut line = String::new();
        for row in &self.buffer {
            line.clear();
            csv::write_row(&mut line, row);
            writer.write_all(line.as_bytes())?;
        }
        writer.flush()?;
        self.runs.push(TempRun { path });
        self.buffer.clear();
        self.buffer_bytes = 0;
        Ok(())
    }

    fn sort_buffer(&mut self) {
        let sort = self.sort;
        self.buffer.sort_by(|a, b| sort.compare(a, b));
    }

    /// Finish accumulating and return the sorted rows. Small inputs stay in
    /// memory; otherwise the remaining buffer is spilled and a k-way merge over
    /// all runs is returned.
    pub fn finish(mut self) -> Result<Sorted<'a>, Error> {
        if self.runs.is_empty() {
            self.sort_buffer();
            return Ok(Sorted::Memory(std::mem::take(&mut self.buffer).into_iter()));
        }
        self.spill()?;
        let mut cursors = Vec::with_capacity(self.runs.len());
        for run in std::mem::take(&mut self.runs) {
            cursors.push(RunCursor::open(run, &self.numeric)?);
        }
        Ok(Sorted::Merge(KMerge {
            cursors,
            sort: self.sort,
            numeric: self.numeric.clone(),
        }))
    }
}

/// Sorted output: an iterator of rows (each fallible for the merge case).
pub enum Sorted<'a> {
    Memory(std::vec::IntoIter<OwnedRow>),
    Merge(KMerge<'a>),
}

impl Iterator for Sorted<'_> {
    type Item = Result<OwnedRow, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Sorted::Memory(it) => it.next().map(Ok),
            Sorted::Merge(m) => m.next_row(),
        }
    }
}

/// k-way merge across run files, picking the smallest current row each step.
pub struct KMerge<'a> {
    cursors: Vec<RunCursor>,
    sort: &'a SortStmt,
    numeric: Vec<usize>,
}

impl KMerge<'_> {
    fn next_row(&mut self) -> Option<Result<OwnedRow, Error>> {
        // Pick the run with the smallest front row; ties resolve to the lower
        // run index, which preserves the original input order (runs are written
        // in input order) and keeps the sort stable.
        let mut best: Option<usize> = None;
        for i in 0..self.cursors.len() {
            let Some(front) = &self.cursors[i].front else {
                continue;
            };
            match best {
                None => best = Some(i),
                Some(b) => {
                    let other = self.cursors[b].front.as_ref().unwrap();
                    if self.sort.compare(front, other) == Ordering::Less {
                        best = Some(i);
                    }
                }
            }
        }
        let bi = best?;
        let row = self.cursors[bi].front.take().unwrap();
        if let Err(e) = self.cursors[bi].advance(&self.numeric) {
            return Some(Err(e));
        }
        Some(Ok(row))
    }
}

/// A buffered reader over one run file, holding the next row to merge.
struct RunCursor {
    reader: BufReader<File>,
    _run: TempRun,
    front: Option<OwnedRow>,
    line: String,
}

impl RunCursor {
    fn open(run: TempRun, numeric: &[usize]) -> Result<Self, Error> {
        let reader = BufReader::new(File::open(&run.path)?);
        let mut cursor = RunCursor {
            reader,
            _run: run,
            front: None,
            line: String::new(),
        };
        cursor.advance(numeric)?;
        Ok(cursor)
    }

    fn advance(&mut self, numeric: &[usize]) -> Result<(), Error> {
        self.line.clear();
        if self.reader.read_line(&mut self.line)? == 0 {
            self.front = None;
            return Ok(());
        }
        // Each spilled row is exactly one line (input fields never contain a
        // newline), so a single parse yields one row.
        let mut row: Option<OwnedRow> = None;
        csv::parse_chunk(&self.line, |r| {
            if row.is_none() {
                row = Some(r.iter().map(|f| f.clone().into_owned()).collect());
            }
        });
        let mut row = row.unwrap_or_default();
        coerce_numeric(&mut row, numeric)?;
        self.front = Some(row);
        Ok(())
    }
}

fn coerce_numeric(row: &mut OwnedRow, numeric: &[usize]) -> Result<(), Error> {
    for &p in numeric {
        if let Some(f) = row.get_mut(p) {
            *f = Field::Num(f.coerce_num()?);
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{SortKey, SortStmt};

    fn row(vals: &[&str]) -> OwnedRow {
        vals.iter().map(|s| Field::Owned((*s).into())).collect()
    }

    fn collect(sorted: Sorted) -> Vec<Vec<String>> {
        sorted
            .map(|r| r.unwrap().iter().map(|f| f.as_str().into_owned()).collect())
            .collect()
    }

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

    #[test]
    fn in_memory_when_under_budget() {
        let sort = numeric_key(0, false);
        let mut s = ExternalSorter::new(&sort, std::env::temp_dir());
        for v in ["10", "9", "100", "2"] {
            s.push(row(&[v])).unwrap();
        }
        let out = collect(s.finish().unwrap());
        assert_eq!(out, vec![vec!["2"], vec!["9"], vec!["10"], vec!["100"]]);
    }

    #[test]
    fn spills_and_merges_when_over_budget() {
        // A 1-byte budget forces a spill after every row, exercising the k-way
        // merge across many run files.
        let sort = numeric_key(0, false);
        let mut s = ExternalSorter::with_budget(&sort, std::env::temp_dir(), 1);
        for v in ["5", "1", "9", "3", "7", "2", "8", "4", "6", "0"] {
            s.push(row(&[v])).unwrap();
        }
        let out = collect(s.finish().unwrap());
        let got: Vec<String> = out.into_iter().map(|r| r[0].clone()).collect();
        assert_eq!(got, vec!["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"]);
    }

    #[test]
    fn merge_is_stable_on_ties() {
        // Sort by column 0 (numeric); column 1 is a tag carried along. Equal
        // keys must keep input order even after spilling each row to its own run.
        let sort = numeric_key(0, false);
        let mut s = ExternalSorter::with_budget(&sort, std::env::temp_dir(), 1);
        for (k, tag) in [("1", "a"), ("1", "b"), ("0", "c"), ("1", "d")] {
            s.push(row(&[k, tag])).unwrap();
        }
        let out = collect(s.finish().unwrap());
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
    fn descending_merge() {
        let sort = numeric_key(0, true);
        let mut s = ExternalSorter::with_budget(&sort, std::env::temp_dir(), 1);
        for v in ["3", "1", "2"] {
            s.push(row(&[v])).unwrap();
        }
        let got: Vec<String> = collect(s.finish().unwrap())
            .into_iter()
            .map(|r| r[0].clone())
            .collect();
        assert_eq!(got, vec!["3", "2", "1"]);
    }
}
