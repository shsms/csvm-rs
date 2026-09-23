//! How output is shown, decided from facts about the terminal and the
//! command line that are gathered once into a [`Console`]. Each rule is a
//! method on it, so it can be tested without a terminal.

use crate::cli::{Args, ColorWhen};
use crate::term;
use std::io::{self, IsTerminal};

/// Where stdout goes, as far as the way output is shown cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sink {
    /// The terminal the user is looking at.
    Terminal,
    /// A pipe or a socket into another program, which may itself be showing
    /// the output on the terminal (a `less`).
    Pipe,
    /// Anything else: `-o FILE`, or the shell's `>` to a file or a device
    /// such as `/dev/null`.
    File,
}

/// The terminal csvm runs in, and what the user asked for about it: the facts
/// every decision about how output is shown starts from.
#[derive(Clone, Debug)]
pub struct Console {
    /// `--color`.
    pub color: ColorWhen,
    /// `NO_COLOR` is set and not empty.
    pub no_color: bool,
    /// `CLICOLOR_FORCE` is set and not `0`.
    pub force_color: bool,
    /// Where stdout goes.
    pub stdout: Sink,
    /// The terminal's column count, when it is known (see [`term::columns`]).
    pub columns: Option<usize>,
}

impl Console {
    /// The facts for a run of `args`: its flags, the environment, and what
    /// stdout is connected to.
    pub fn read(args: &Args) -> Console {
        Console {
            color: args.color,
            no_color: std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()),
            force_color: std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| v != "0"),
            stdout: if args.out_path().is_some() {
                Sink::File
            } else {
                stdout_sink()
            },
            columns: term::columns(),
        }
    }

    /// Whether stdout is coloured.
    pub fn color(&self) -> bool {
        self.colors(self.stdout == Sink::Terminal)
    }

    /// Whether to colour a stream: an explicit `--color always`/`never` wins.
    /// Under `auto`, `NO_COLOR` turns colour off and `CLICOLOR_FORCE` on, else
    /// the stream must go to a `terminal`.
    fn colors(&self, terminal: bool) -> bool {
        match self.color {
            ColorWhen::Always => true,
            ColorWhen::Never => false,
            ColorWhen::Auto if self.no_color => false,
            ColorWhen::Auto if self.force_color => true,
            ColorWhen::Auto => terminal,
        }
    }

    /// The terminal's width, when stdout is the terminal: a chart's default width.
    pub fn width(&self) -> Option<usize> {
        self.columns.filter(|_| self.stdout == Sink::Terminal)
    }
}

/// Where stdout itself goes.
fn stdout_sink() -> Sink {
    if io::stdout().is_terminal() {
        Sink::Terminal
    } else if stdout_is_pipe() {
        Sink::Pipe
    } else {
        Sink::File
    }
}

/// Whether stdout is a pipe or a socket.
#[cfg(unix)]
fn stdout_is_pipe() -> bool {
    use std::os::fd::AsFd;
    use std::os::unix::fs::FileTypeExt;
    io::stdout()
        .as_fd()
        .try_clone_to_owned()
        .ok()
        .and_then(|fd| std::fs::File::from(fd).metadata().ok())
        .is_some_and(|m| m.file_type().is_fifo() || m.file_type().is_socket())
}

#[cfg(not(unix))]
fn stdout_is_pipe() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A console on an 80-column terminal, with no flags and no colour
    /// variables set.
    fn terminal() -> Console {
        Console {
            color: ColorWhen::Auto,
            no_color: false,
            force_color: false,
            stdout: Sink::Terminal,
            columns: Some(80),
        }
    }

    /// [`terminal`], with stdout going to `stdout`.
    fn to(stdout: Sink) -> Console {
        Console {
            stdout,
            ..terminal()
        }
    }

    #[test]
    fn colour_follows_the_flag_then_the_variables_then_the_terminal() {
        let on = |c: Console| c.color().then_some(());
        assert_eq!(on(terminal()), Some(()));
        assert_eq!(on(to(Sink::File)), None);
        assert_eq!(
            on(Console {
                color: ColorWhen::Always,
                ..to(Sink::File)
            }),
            Some(())
        );
        assert_eq!(
            on(Console {
                force_color: true,
                ..to(Sink::File)
            }),
            Some(())
        );
        assert_eq!(
            on(Console {
                no_color: true,
                force_color: true,
                ..terminal()
            }),
            None
        );
        assert_eq!(
            on(Console {
                color: ColorWhen::Never,
                ..terminal()
            }),
            None
        );
    }

    #[test]
    fn only_a_terminal_has_a_width() {
        assert_eq!(terminal().width(), Some(80));
        assert_eq!(to(Sink::File).width(), None);
        assert_eq!(to(Sink::Pipe).width(), None);
    }
}
