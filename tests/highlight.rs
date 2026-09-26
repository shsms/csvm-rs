//! `csvm --highlight` end to end: the built binary reads requests on its
//! stdin and answers each on its stdout, in order, until its stdin ends.

mod common;
use common::temp_csv;

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// A running `csvm --highlight`. Dropped before it is finished (a failed
/// test), it is killed, so no helper is left behind.
struct Helper {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    id: u64,
}

impl Helper {
    /// Start the helper and read its first line.
    fn start() -> Helper {
        let mut child = Command::new(env!("CARGO_BIN_EXE_csvm"))
            .arg("--highlight")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start csvm --highlight");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut helper = Helper {
            child,
            stdin,
            stdout,
            id: 0,
        };
        let mut first = String::new();
        helper.stdout.read_line(&mut first).unwrap();
        assert_eq!(first, "inkline-highlight 1 indent\n");
        helper
    }

    /// Write `bytes` to the helper's stdin.
    fn send(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        stdin.write_all(bytes).unwrap();
        stdin.flush().unwrap();
    }

    /// Send the command line `args` (the command's name first) from `cwd`,
    /// each argument `final` except the one at `raw`; return the reply's
    /// lines before its `:end`.
    fn ask_with(&mut self, cwd: &str, args: &[&str], raw: Option<usize>) -> Vec<String> {
        self.id += 1;
        let mut request = format!(":request {}\n", self.id).into_bytes();
        request.extend(blocks(cwd, args, raw));
        request.extend(b":done\n");
        self.exchange(&request)
    }

    fn ask(&mut self, cwd: &str, args: &[&str]) -> Vec<String> {
        self.ask_with(cwd, args, None)
    }

    /// Ask where a new line goes when argument `at.0` of the command line
    /// `args` is split at byte `at.1`, the argument at `raw` sent `raw`;
    /// return the reply's lines before its `:end`.
    fn indent(&mut self, args: &[&str], at: (usize, usize), raw: Option<usize>) -> Vec<String> {
        self.id += 1;
        let mut request = format!(":indent {}\n", self.id).into_bytes();
        request.extend(blocks("/", args, raw));
        request.extend(format!(":at {} {}\n:done\n", at.0, at.1).as_bytes());
        self.exchange(&request)
    }

    /// Send `request`, the whole of request `self.id`; return the reply's
    /// lines before its `:end`.
    fn exchange(&mut self, request: &[u8]) -> Vec<String> {
        self.send(request);
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "the helper stopped"
            );
            let line = line.strip_suffix('\n').expect("a whole line").to_string();
            if line == format!(":end {}", self.id) {
                return lines;
            }
            assert!(!line.starts_with(":end"), "a reply out of order: {line}");
            lines.push(line);
        }
    }

    /// Close the helper's stdin and wait, at most ten seconds, for it to
    /// exit; return how it exited and what it wrote on stderr.
    fn finish(mut self) -> (ExitStatus, String) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "the helper did not exit");
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut err = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut err)
            .unwrap();
        (status, err)
    }
}

/// A request's `:cwd` and `:arg` blocks, for the command line `args` from
/// `cwd`, each argument `final` except the one at `raw`.
fn blocks(cwd: &str, args: &[&str], raw: Option<usize>) -> Vec<u8> {
    let mut out = format!(":cwd {}\n{cwd}\n", cwd.len()).into_bytes();
    for (i, arg) in args.iter().enumerate() {
        let kind = if raw == Some(i) { "raw" } else { "final" };
        out.extend(format!(":arg {kind} {}\n{arg}\n", arg.len()).as_bytes());
    }
    out
}

impl Drop for Helper {
    fn drop(&mut self) {
        // Already exited when finished; else stopped here.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn csvm_highlight_answers_each_request_in_order() {
    let data = temp_csv("amount,region\n1,x\n");
    let dir = data.parent().unwrap().to_str().unwrap();
    let name = data.file_name().unwrap().to_str().unwrap();
    let mut helper = Helper::start();

    // A clean script: its colours and no error.
    assert_eq!(
        helper.ask(dir, &["csvm", "cols amount | select amount > 1", name]),
        [
            ":span 1 0 4 command",
            ":span 1 5 11 variable",
            ":span 1 12 13 operator",
            ":span 1 14 20 command",
            ":span 1 21 27 variable",
            ":span 1 28 29 operator",
            ":span 1 30 31 number",
        ]
    );
    // A script that does not parse.
    assert_eq!(
        helper.ask(dir, &["csvm", "select a >> 1"]),
        [
            ":span 1 0 6 command",
            ":span 1 7 8 variable",
            ":span 1 9 10 operator",
            ":span 1 10 11 operator",
            ":span 1 12 13 number",
            ":error 1 10 11 expected a column, number, string, or function, found '>'",
        ]
    );
    // A column the input file does not have, found from `:cwd`.
    assert_eq!(
        helper.ask(dir, &["csvm", "select amont > 1", name]),
        [
            ":span 1 0 6 command",
            ":span 1 7 12 variable",
            ":span 1 13 14 operator",
            ":span 1 15 16 number",
            ":error 1 7 12 column not found: amont (did you mean `amount`?) — have: amount, region",
        ]
    );
    // The same input as bash will still change it: no column check.
    assert_eq!(
        helper.ask_with(dir, &["csvm", "select amont > 1", name], Some(2)),
        [
            ":span 1 0 6 command",
            ":span 1 7 12 variable",
            ":span 1 13 14 operator",
            ":span 1 15 16 number",
        ]
    );
    // A script on two lines of one argument.
    assert_eq!(
        helper.ask(dir, &["csvm", "cols amount\nselect amount > 1", name]),
        [
            ":span 1 0 4 command",
            ":span 1 5 11 variable",
            ":span 1 12 18 command",
            ":span 1 19 25 variable",
            ":span 1 26 27 operator",
            ":span 1 28 29 number",
        ]
    );
    // `-f`: only the options are checked.
    assert_eq!(
        helper.ask(dir, &["csvm", "-f", "prog.csvm", name]),
        Vec::<String>::new()
    );
    // A bad option, on its argument.
    assert_eq!(
        helper.ask(dir, &["csvm", "--colr", "always", "cols a"]),
        [":error 1 0 6 unknown option: --colr"]
    );
    // The line that starts the helper, typed with more after it.
    assert_eq!(
        helper.ask(dir, &["csvm", "--highlight", "cols a"]),
        [":error 1 0 11 --highlight takes no other arguments"]
    );
    let (status, err) = helper.finish();
    assert!(status.success(), "{status}: {err}");
    assert_eq!(err, "");
}

#[test]
fn csvm_highlight_answers_indent_requests_between_colour_ones() {
    let mut helper = Helper::start();
    // Inside a join group, on the line after `join (`.
    let script = "head\n| join (\n  cols a,b";
    assert_eq!(
        helper.indent(&["csvm", script, "x.csv"], (1, script.len()), None),
        [":depth 1 1"]
    );
    // Before the `)` that closes it: the new line moves out.
    let script = "head\n| join (\n  cols a,b) other.csv on a";
    let at = script.find(')').unwrap();
    assert_eq!(
        helper.indent(&["csvm", script], (1, at), None),
        [":depth 0 1"]
    );
    // A colour request in between is answered as before.
    assert_eq!(helper.ask("/", &["csvm", "fmt"]), [":span 1 0 3 command"]);
    // In an fn body, with an option before the script.
    let script = "fn prep(n) {\n  rename value=n";
    assert_eq!(
        helper.indent(&["csvm", "-n", "1", script], (3, script.len()), None),
        [":depth 1 1"]
    );
    // Split outside the script, or in a raw one: only the end.
    assert_eq!(
        helper.indent(&["csvm", "join (", "x.csv"], (2, 1), None),
        Vec::<String>::new()
    );
    assert_eq!(
        helper.indent(&["csvm", "\"join (\n\""], (1, 8), Some(1)),
        Vec::<String>::new()
    );
    let (status, err) = helper.finish();
    assert!(status.success(), "{status}: {err}");
    assert_eq!(err, "");
}

#[test]
fn csvm_highlight_stops_with_an_error_on_a_broken_request() {
    let mut helper = Helper::start();
    helper.send(b"hello\n");
    let mut rest = String::new();
    helper.stdout.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "");
    let (status, err) = helper.finish();
    assert_eq!(status.code(), Some(1));
    assert!(err.starts_with("csvm: not a highlight request"), "{err}");
}

#[test]
fn csvm_highlight_exits_quietly_when_its_output_is_closed() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_csvm"))
        .arg("--highlight")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start csvm --highlight");
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    assert_eq!(first, "inkline-highlight 1 indent\n");
    // Stop reading: the reply then has nowhere to go.
    drop(stdout);
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b":request 1\n:cwd 1\n/\n:arg final 4\ncsvm\n:arg final 6\ncols a\n:done\n")
        .unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{}: {err}", out.status);
    assert_eq!(err, "");
}

/// inkline talks to its helper over a socket pair. Closing a socket with
/// bytes in it still unread resets the connection: the helper's next read
/// fails with ECONNRESET instead of seeing the end of its input. That is a
/// reader gone like any other, so the helper ends quietly then too.
#[cfg(target_os = "linux")]
#[test]
fn csvm_highlight_exits_quietly_when_its_connection_is_reset() {
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;

    let (ours, theirs) = UnixStream::pair().unwrap();
    let theirs_out = theirs.try_clone().unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_csvm"))
        .arg("--highlight")
        .stdin(Stdio::from(OwnedFd::from(theirs)))
        .stdout(Stdio::from(OwnedFd::from(theirs_out)))
        .stderr(Stdio::piped())
        .spawn()
        .expect("start csvm --highlight");
    // Wait for the greeting, but leave it unread.
    let mut byte = 0u8;
    // SAFETY: `byte` is one writable byte for the whole call.
    let n = unsafe { libc::recv(ours.as_raw_fd(), (&raw mut byte).cast(), 1, libc::MSG_PEEK) };
    assert_eq!(n, 1);
    drop(ours);
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{}: {err}", out.status);
    assert_eq!(err, "");
}
