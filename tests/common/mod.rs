//! Fixtures shared by the integration tests.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static SEQ: AtomicU32 = AtomicU32::new(0);

/// A CSV file in the system temp directory, removed when dropped. Derefs to
/// its path, so it goes wherever a `&Path` does.
pub struct TempCsv(PathBuf);

impl Deref for TempCsv {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempCsv {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Write `content` to a fresh temp file (unique per process and call).
pub fn temp_csv(content: &str) -> TempCsv {
    let path = std::env::temp_dir().join(format!(
        "csvm_test_{}_{}.csv",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, content).unwrap();
    TempCsv(path)
}
