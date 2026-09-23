//! Paging long output through `$PAGER`, the way git does.
//!
//! The caller decides *whether* to page; this module picks the command and runs
//! it. The pager is started through `sh -c`, so a `$PAGER` with options of its
//! own (`less -i`) works as it does in git. `less` is told what csvm knows
//! about the output: colour is passed through and output that fits on one
//! screen is not paged at all (`LESS=FRX` when the variable is unset), and a
//! table's lines are cut off instead of wrapped, with its header row kept on
//! screen when the table is taller than the window.

use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

/// The pager to run: `$CSVM_PAGER`, else `$PAGER`, else `less`. An empty value
/// or `cat` means no pager, so either variable can turn paging off.
pub fn command(csvm_pager: Option<&str>, pager: Option<&str>) -> Option<String> {
    let cmd = csvm_pager.or(pager).unwrap_or("less").trim();
    (!cmd.is_empty() && cmd != "cat").then(|| cmd.to_string())
}

/// The program `cmd` runs: its first word that is not a variable assignment
/// (`LESS=-i less` runs `less`).
fn program(cmd: &str) -> &str {
    cmd.split_whitespace()
        .find(|word| !is_assignment(word))
        .unwrap_or("")
}

/// Whether `word` sets a variable for the command after it: `NAME=value`.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c == '_' || c.is_ascii_alphabetic())
            && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
    })
}

/// Whether `cmd` runs `less`, by the file name of its program.
fn is_less(cmd: &str) -> bool {
    Path::new(program(cmd))
        .file_name()
        .is_some_and(|n| n == "less")
}

/// The release number in `less --version`'s first line (`less 668 (…)`); a
/// beta's letter suffix (`661x`) is dropped.
fn release_in_banner(banner: &str) -> Option<u32> {
    let word = banner.split_whitespace().nth(1)?;
    let digits = word.split(|c: char| !c.is_ascii_digit()).next()?;
    digits.parse().ok()
}

/// The first general `less` release with `--header` (its NEWS lists the
/// option under the changes from 590 to 608).
const LESS_HEADER_SINCE: u32 = 608;

/// The options a table gets from `less` at `version`: cut long lines off
/// instead of wrapping them (`-S`, so a wide table scrolls sideways and keeps
/// its columns lined up) and, when `tall` (the table does not fit on one
/// screen), keep the header row on screen (`--header=1`, only where the
/// release has it). A short table must not get `--header`: it turns off `-F`,
/// so the table would open in the pager when it could just be printed.
fn table_args(version: Option<u32>, tall: bool) -> Vec<&'static str> {
    let mut args = vec!["-S"];
    if tall && version.is_some_and(|v| v >= LESS_HEADER_SINCE) {
        args.push("--header=1");
    }
    args
}

/// Whether the pager `cmd` can be run, as far as `sh`, which runs it, can find
/// its program: `~` and `$VAR` mean what they mean to the shell. A pager whose
/// program `sh` cannot find is not started, and the output goes to stdout
/// instead. A command that quotes or escapes its program, or anything before
/// it, is left to `sh` whole, since a single word of it may not parse alone.
fn runnable(cmd: &str) -> bool {
    let program = program(cmd);
    if program.is_empty() {
        return false;
    }
    // The words before the program are all assignments (see [`program`]).
    let quoted = cmd
        .split_whitespace()
        .take_while(|word| is_assignment(word))
        .chain([program])
        .any(|word| word.contains(['\'', '"', '\\']));
    if quoted {
        return true;
    }
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v -- {program}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The release of the `less` that `cmd` runs, from `less --version`.
fn less_release(cmd: &str) -> Option<u32> {
    let out = Command::new(program(cmd))
        .arg("--version")
        .stderr(Stdio::null())
        .output()
        .ok()?;
    release_in_banner(&String::from_utf8_lossy(&out.stdout))
}

/// A running pager and the pipe into it. Writing sends it output; dropping it
/// closes the pipe and waits for the pager to exit, so the shell prompt does
/// not come back while the pager still owns the terminal.
pub struct Pager {
    input: Option<BufWriter<ChildStdin>>,
    child: Child,
}

impl Pager {
    /// Start `cmd`. A `table` shown by `less` gets the table options, with its
    /// header pinned when the table is `tall`: as many lines as the window, or
    /// more. `None` when the pager cannot be run, and the caller writes to
    /// stdout instead.
    #[cfg(unix)]
    pub fn start(cmd: &str, table: bool, tall: bool) -> Option<Pager> {
        if !runnable(cmd) {
            return None;
        }
        let mut command = Command::new("sh");
        // `"$@"` hands the pager the options added below, after its own.
        command
            .arg("-c")
            .arg(format!("{cmd} \"$@\""))
            .arg(cmd)
            .stdin(Stdio::piped());
        if table && is_less(cmd) {
            // The release only matters for a tall table's header.
            let version = if tall { less_release(cmd) } else { None };
            command.args(table_args(version, tall));
        }
        if std::env::var_os("LESS").is_none() {
            command.env("LESS", "FRX");
        }
        let mut child = command.spawn().ok()?;
        let input = child.stdin.take().map(BufWriter::new);
        // Ctrl-C belongs to the pager now (less uses it to stop a search). If
        // it also ended csvm, the shell would take the terminal back while the
        // pager was still drawing on it.
        // SAFETY: SIG_IGN installs no handler of ours; it only changes the
        // disposition of SIGINT for this process.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
        }
        Some(Pager { input, child })
    }

    #[cfg(not(unix))]
    pub fn start(_cmd: &str, _table: bool, _tall: bool) -> Option<Pager> {
        None
    }

    fn input(&mut self) -> io::Result<&mut BufWriter<ChildStdin>> {
        self.input
            .as_mut()
            .ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))
    }
}

impl Write for Pager {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.input()?.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.input()?.flush()
    }
}

impl Drop for Pager {
    fn drop(&mut self) {
        // Closing the pipe is the pager's end of input; then it is the user's
        // until they quit it.
        drop(self.input.take());
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_prefers_csvm_pager_then_pager_then_less() {
        assert_eq!(command(Some("most"), Some("more")).as_deref(), Some("most"));
        assert_eq!(command(None, Some("less -i")).as_deref(), Some("less -i"));
        assert_eq!(command(None, None).as_deref(), Some("less"));
        // Either variable turns paging off, the csvm one even when PAGER is
        // set.
        assert_eq!(command(Some(""), Some("less")), None);
        assert_eq!(command(Some("cat"), None), None);
        assert_eq!(command(None, Some(" ")), None);
    }

    #[test]
    fn less_is_known_by_its_program_name() {
        assert!(is_less("less"));
        assert!(is_less("/usr/bin/less -i"));
        assert!(!is_less("lesspipe"));
        assert!(is_less("LESS=-i less"));
        assert!(!is_less("most less"));
    }

    #[test]
    fn the_shell_says_whether_a_pager_can_run() {
        for can in [
            "sh",
            "sh -c true",
            "LESS=-i sh",
            "LESS='-R -i' sh",
            "\"/bin/sh\"",
            "/bin/sh",
            "exec sh",
            // Quoted or escaped, before the program or in it: left to sh.
            "A='x' csvm-no-such-pager",
            "\"csvm-no-such-pager\"",
            "csvm\\-no-such-pager",
        ] {
            assert!(runnable(can), "{can}");
        }
        for cannot in [
            "",
            "csvm-no-such-pager",
            "/csvm/no/such/pager",
            "~/csvm-no-such-pager",
            "$HOME/csvm-no-such-pager",
            // A quote after the program does not hide a missing one.
            "csvm-no-such-pager 'x'",
        ] {
            assert!(!runnable(cannot), "{cannot}");
        }
    }

    #[test]
    fn release_in_banner_reads_the_banner() {
        assert_eq!(
            release_in_banner("less 668 (GNU regular expressions)"),
            Some(668)
        );
        assert_eq!(
            release_in_banner("less 661x (PCRE2 regular expressions)"),
            Some(661)
        );
        assert_eq!(release_in_banner("less"), None);
        assert_eq!(release_in_banner(""), None);
    }

    #[test]
    fn table_args_pin_the_header_of_a_tall_table_where_less_has_it() {
        assert_eq!(table_args(Some(668), true), ["-S", "--header=1"]);
        assert_eq!(table_args(Some(608), true), ["-S", "--header=1"]);
        assert_eq!(table_args(Some(607), true), ["-S"]);
        assert_eq!(table_args(Some(590), true), ["-S"]);
        assert_eq!(table_args(None, true), ["-S"]);
        // A table that fits keeps -F working, so it gets no header.
        assert_eq!(table_args(Some(668), false), ["-S"]);
    }
}
