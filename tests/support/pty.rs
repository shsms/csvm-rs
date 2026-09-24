//! A terminal a test plays: csvm runs with a pty as its controlling terminal,
//! and the test reads what it draws there, and types keys or answers the
//! terminal's questions back.

#![allow(dead_code, reason = "each test file uses its own part of it")]

use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Long enough for any step of a test to happen, short of a hang.
pub const PATIENCE: Duration = Duration::from_secs(10);

/// The shell words that run csvm with `args`, each quoted.
pub fn csvm(args: &[&str]) -> String {
    std::iter::once(env!("CARGO_BIN_EXE_csvm"))
        .chain(args.iter().copied())
        .map(|word| format!("'{}'", word.replace('\'', r"'\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A program running on a terminal the test plays.
pub struct Terminal {
    master: Box<dyn MasterPty + Send>,
    keys: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// Everything the terminal has been sent, filled as it comes.
    shown: Arc<Mutex<Vec<u8>>>,
    /// Reads `shown` until the terminal's other end is closed.
    reader: Option<JoinHandle<()>>,
}

impl Terminal {
    /// Run `sh -c line` on a new 80x24 terminal with `TERM=xterm`, none of
    /// the caller's colour, pager or size variables, and `env` added.
    pub fn shell(line: &str, env: &[(&str, &str)]) -> Terminal {
        Terminal::sized(line, 24, env)
    }

    /// [`Terminal::shell`] on a terminal `rows` lines tall.
    pub fn sized(line: &str, rows: u16, env: &[(&str, &str)]) -> Terminal {
        let size = PtySize {
            rows,
            ..PtySize::default()
        };
        let pair = native_pty_system().openpty(size).expect("a pty");
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", line]);
        command.cwd(std::env::current_dir().expect("a working directory"));
        command.env("TERM", "xterm");
        for var in [
            "NO_COLOR",
            "CLICOLOR_FORCE",
            "COLORTERM",
            "COLORFGBG",
            "CSVM_PAGER",
            "PAGER",
            "LESS",
            "COLUMNS",
            "LINES",
        ] {
            command.env_remove(var);
        }
        for (key, value) in env {
            command.env(key, value);
        }
        let child = pair.slave.spawn_command(command).expect("a shell");
        // The program's end must be the last one open, for the reader to
        // see it close.
        drop(pair.slave);
        let mut from = pair.master.try_clone_reader().expect("a reader");
        let keys = pair.master.take_writer().expect("a writer");
        let shown = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&shown);
        let reader = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = from.read(&mut buf) {
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Terminal {
            master: pair.master,
            keys,
            child,
            shown,
            reader: Some(reader),
        }
    }

    /// What the terminal has been sent so far.
    pub fn shown(&self) -> Vec<u8> {
        self.shown.lock().unwrap().clone()
    }

    /// [`Terminal::shown`] as text.
    pub fn text(&self) -> String {
        self.text_since(0)
    }

    /// What the terminal has been sent after its first `from` bytes, as
    /// text.
    pub fn text_since(&self, from: usize) -> String {
        String::from_utf8_lossy(&self.shown()[from..]).into_owned()
    }

    /// Wait for the terminal to be sent `text`; panics, showing what it was
    /// sent, after [`PATIENCE`].
    pub fn wait_for(&self, text: &str) {
        let deadline = Instant::now() + PATIENCE;
        while !self.text().contains(text) {
            assert!(
                Instant::now() < deadline,
                "{text:?} not shown: {:?}",
                self.text()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Send `bytes` to the program, as keys typed or as the terminal's own
    /// replies.
    pub fn send(&mut self, bytes: &[u8]) {
        self.keys
            .write_all(bytes)
            .expect("the terminal takes input");
        self.keys.flush().expect("the terminal takes input");
    }

    /// Wait for the program to end, and for everything it drew; panics after
    /// [`PATIENCE`].
    pub fn finish(&mut self) -> ExitStatus {
        let deadline = Instant::now() + PATIENCE;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("a child to wait on") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "still running: {:?}",
                self.text()
            );
            thread::sleep(Duration::from_millis(5));
        };
        if let Some(reader) = self.reader.take() {
            // The terminal closes once nothing the program started holds it.
            while !reader.is_finished() {
                assert!(
                    Instant::now() < deadline,
                    "still drawing: {:?}",
                    self.text()
                );
                thread::sleep(Duration::from_millis(5));
            }
            reader.join().expect("the reader");
        }
        status
    }

    /// Whether the terminal is back in its usual mode: line editing on,
    /// keys echoed.
    pub fn cooked(&self) -> bool {
        let fd = self.master.as_raw_fd().expect("a pty descriptor");
        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr fills the termios we own, read only on success.
        let termios = unsafe {
            assert_eq!(libc::tcgetattr(fd, termios.as_mut_ptr()), 0);
            termios.assume_init()
        };
        let cooked = libc::ICANON | libc::ECHO;
        termios.c_lflag & cooked == cooked
    }

    /// The path of the program's end of the terminal, such as `/dev/pts/3`.
    pub fn path(&self) -> PathBuf {
        self.master.tty_name().expect("a pty path")
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // A test that failed early leaves nothing running.
        let _ = self.child.kill();
    }
}
