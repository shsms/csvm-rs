//! How far a run has read its input: the readers add what they consume to a
//! shared [`Progress`].

use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A count of input bytes consumed so far. The default counts nothing, so a
/// run with no meter pays one branch per read.
#[derive(Clone, Debug, Default)]
pub struct Progress(Option<Arc<AtomicU64>>);

impl Progress {
    /// A counter that keeps count.
    pub fn counting() -> Progress {
        Progress(Some(Arc::new(AtomicU64::new(0))))
    }

    /// Whether this counter keeps count.
    pub fn is_counting(&self) -> bool {
        self.0.is_some()
    }

    /// Count `n` more bytes.
    pub fn add(&self, n: u64) {
        if let Some(count) = &self.0 {
            count.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// The bytes counted so far.
    pub fn get(&self) -> u64 {
        self.0.as_ref().map_or(0, |c| c.load(Ordering::Relaxed))
    }
}

/// A reader that counts what is read through it.
pub struct Counted<R> {
    inner: R,
    progress: Progress,
}

impl<R> Counted<R> {
    pub fn new(inner: R, progress: Progress) -> Counted<R> {
        Counted { inner, progress }
    }
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.progress.add(n as u64);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_counts_only_when_counting() {
        let p = Progress::counting();
        let q = p.clone();
        p.add(3);
        q.add(4);
        assert_eq!(p.get(), 7);
        let none = Progress::default();
        none.add(5);
        assert_eq!(none.get(), 0);
        assert!(p.is_counting() && !none.is_counting());
    }

    #[test]
    fn counted_reads_add_up() {
        let p = Progress::counting();
        let mut r = Counted::new(&b"hello world"[..], p.clone());
        let mut out = String::new();
        r.read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello world");
        assert_eq!(p.get(), 11);
    }
}
