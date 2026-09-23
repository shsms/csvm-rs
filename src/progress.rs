//! How far a run has read its input, and a one-line meter on stderr that says
//! so while a slow run would otherwise show nothing.
//!
//! The readers add what they consume to a shared [`Progress`]; a [`Meter`]
//! redraws a line from it on a thread of its own. A run that ends within
//! [`Meter::DELAY`] never shows the line at all.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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

/// `n` bytes in binary units, one decimal place from KiB up.
fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// The meter's line: how much of the input is read (a percentage of `total`
/// when it is known, as for a file) and how long the run has taken.
fn status(read: u64, total: Option<u64>, elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64();
    match total {
        Some(total) if total > 0 => {
            let pct = (read.min(total) as f64 / total as f64 * 100.0).floor();
            format!(
                "csvm: {pct:.0}% · {} of {} · {secs:.1}s",
                bytes(read.min(total)),
                bytes(total)
            )
        }
        _ => format!("csvm: {} read · {secs:.1}s", bytes(read)),
    }
}

/// A line on stderr showing a run's [`Progress`], redrawn in place until the
/// meter is dropped, which clears it. Nothing is drawn until the run has taken
/// [`Meter::DELAY`].
pub struct Meter {
    /// The drawing thread, which returns whether it drew a line, and the
    /// channel whose closing tells it to stop (nothing is ever sent).
    running: Option<(mpsc::Sender<()>, JoinHandle<bool>)>,
}

impl Meter {
    /// How long a run goes before the meter shows: a quick run never flashes
    /// a line.
    pub const DELAY: Duration = Duration::from_secs(1);
    /// How often the line is redrawn.
    const TICK: Duration = Duration::from_millis(200);

    /// Start showing `progress` out of `total` bytes (`None` when the input's
    /// size is not known, as for stdin).
    pub fn start(progress: Progress, total: Option<u64>) -> Meter {
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = thread::spawn(move || {
            let began = Instant::now();
            let mut wait = Meter::DELAY;
            let mut drawn = false;
            while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(wait) {
                let line = status(progress.get(), total, began.elapsed());
                // A failed write to stderr has nowhere to be reported.
                let _ = write!(io::stderr(), "\r\x1b[K{line}");
                drawn = true;
                wait = Meter::TICK;
            }
            drawn
        });
        Meter {
            running: Some((stop, thread)),
        }
    }

    /// Stop the drawing thread and wait for it; whether it drew a line.
    fn finish(&mut self) -> bool {
        self.running.take().is_some_and(|(stop, thread)| {
            drop(stop);
            thread.join().unwrap_or(false)
        })
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        // Leave the line empty for whatever is written next.
        if self.finish() {
            let _ = write!(io::stderr(), "\r\x1b[K");
        }
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

    #[test]
    fn bytes_use_binary_units() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(3 << 30), "3.0 GiB");
    }

    #[test]
    fn status_shows_a_share_of_a_known_size() {
        let t = Duration::from_millis(3140);
        assert_eq!(
            status(512 << 20, Some(1 << 30), t),
            "csvm: 50% · 512.0 MiB of 1.0 GiB · 3.1s"
        );
        // A count past the size (a file grown mid-run) stays at 100%.
        assert_eq!(status(20, Some(10), t), "csvm: 100% · 10 B of 10 B · 3.1s");
        assert_eq!(status(2048, None, t), "csvm: 2.0 KiB read · 3.1s");
    }

    #[test]
    fn a_quick_meter_draws_nothing() {
        let mut meter = Meter::start(Progress::counting(), Some(10));
        assert!(!meter.finish());
    }
}
