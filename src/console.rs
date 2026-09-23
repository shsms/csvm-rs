//! How output is shown, decided from facts about the terminal and the
//! command line that are gathered once into a [`Console`]. Each rule is a
//! method on it, so it can be tested without a terminal.

use crate::cli::{Args, ColorWhen};
use crate::color::Depth;
use crate::exec::Screen;
use crate::pager::{self, Pager};
use crate::term;
use std::io::{self, IsTerminal, Write};

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
    /// The depth `$COLORTERM` announces (see [`Depth::from_colorterm`]).
    pub depth: Depth,
    /// `TERM=dumb`: the terminal prints text and nothing else.
    pub dumb: bool,
    /// Where stdout goes.
    pub stdout: Sink,
    /// The terminal's column count, when it is known (see [`term::columns`]).
    pub columns: Option<usize>,
    /// The terminal's row count, when it is known (see [`term::rows`]).
    pub rows: Option<usize>,
    /// stderr is a terminal.
    pub stderr_terminal: bool,
    /// The input is stdin, and stdin is a terminal: someone is typing it.
    pub typed_input: bool,
    /// The pager to run, from `$CSVM_PAGER` and `$PAGER` (see
    /// [`pager::command`]); `None` when they turn paging off.
    pub pager_command: Option<String>,
    /// `--no-pager`.
    pub no_pager: bool,
    /// `--no-progress`.
    pub no_progress: bool,
}

impl Console {
    /// The facts for a run with no flags: the environment, and what stdin,
    /// stdout and stderr are connected to.
    pub fn read_env() -> Console {
        Console {
            color: ColorWhen::Auto,
            no_color: std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()),
            force_color: std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| v != "0"),
            depth: Depth::from_colorterm(std::env::var("COLORTERM").ok().as_deref()),
            dumb: std::env::var_os("TERM").is_some_and(|t| t == "dumb"),
            stdout: stdout_sink(),
            columns: term::columns(),
            rows: term::rows(),
            stderr_terminal: io::stderr().is_terminal(),
            typed_input: io::stdin().is_terminal(),
            pager_command: pager::command(
                std::env::var("CSVM_PAGER").ok().as_deref(),
                std::env::var("PAGER").ok().as_deref(),
            ),
            no_pager: false,
            no_progress: false,
        }
    }

    /// The facts for a run of `args`: [`Console::read_env`], with its flags,
    /// its output file and its input file.
    pub fn read(args: &Args) -> Console {
        let env = Console::read_env();
        Console {
            color: args.color,
            stdout: if args.out_path().is_some() {
                Sink::File
            } else {
                env.stdout
            },
            typed_input: env.typed_input && args.in_path().is_none(),
            no_pager: args.no_pager,
            no_progress: args.no_progress,
            ..env
        }
    }

    /// The depth colour is drawn at on stdout, or `None` with colour off.
    pub fn color(&self) -> Option<Depth> {
        self.colors(self.stdout == Sink::Terminal)
            .then_some(self.depth)
    }

    /// Whether an error on stderr is coloured, by the rules stdout's colour
    /// follows.
    pub fn error_color(&self) -> bool {
        self.colors(self.stderr_terminal)
    }

    /// Whether to colour a stream: an explicit `--color always`/`never` wins.
    /// Under `auto`, `NO_COLOR` turns colour off and `CLICOLOR_FORCE` on, else
    /// the stream must go to a `terminal`, and not `TERM=dumb`.
    fn colors(&self, terminal: bool) -> bool {
        match self.color {
            ColorWhen::Always => true,
            ColorWhen::Never => false,
            ColorWhen::Auto if self.no_color => false,
            ColorWhen::Auto if self.force_color => true,
            ColorWhen::Auto => terminal && !self.dumb,
        }
    }

    /// The terminal's width, when stdout is the terminal: a chart's default
    /// width, and the width a table is fitted to.
    pub fn width(&self) -> Option<usize> {
        self.columns.filter(|_| self.stdout == Sink::Terminal)
    }

    /// Whether output shown on the terminal goes through a pager.
    pub fn pages(&self) -> bool {
        self.stdout == Sink::Terminal
            && !self.dumb
            && !self.no_pager
            && self.pager_command.is_some()
    }

    /// Whether `output`, one line on screen per line of it, has as many lines
    /// as the window, so a pager would scroll it.
    pub fn fills(&self, output: &[u8]) -> bool {
        self.rows
            .is_some_and(|rows| memchr::memchr_iter(b'\n', output).nth(rows - 1).is_some())
    }

    /// The pager for output shown on the terminal, when [`Console::pages`];
    /// `table` asks for the options a wide table wants, and `tall` says it
    /// fills the window. `None` means write to stdout directly.
    pub fn pager(&self, table: bool, tall: bool) -> Option<Pager> {
        if !self.pages() {
            return None;
        }
        Pager::start(self.pager_command.as_deref()?, table, tall)
    }

    /// Stdout for help and `--explain` text: through the pager when there is
    /// one.
    pub fn paged_stdout(&self) -> Box<dyn Write> {
        match self.pager(false, false) {
            Some(p) => Box::new(p),
            None => Box::new(io::stdout()),
        }
    }

    /// Whether a table's web addresses become links. A link is an escape like a
    /// colour, and only a terminal opens it, so it needs colour on and stdout
    /// the terminal.
    pub fn links(&self) -> bool {
        self.color().is_some() && self.stdout == Sink::Terminal
    }

    /// What rendering needs to know about where its output is shown, through
    /// `pager` when there is one: a paged table is not fitted, since the pager
    /// scrolls it sideways, and links only reach a pager that shows them.
    pub fn screen(&self, pager: Option<&Pager>) -> Screen {
        Screen {
            color: self.color(),
            width: self.width(),
            fit: pager.is_none(),
            // Asking a pager costs a run of `less --version`, so only when
            // links are on.
            links: self.links() && pager.is_none_or(Pager::shows_links),
            stripe: None,
        }
    }

    /// Whether to draw the progress meter on stderr. It needs a terminal on
    /// stderr that nothing else is drawing on while the run reads: nobody
    /// typing the input into it, and stdout not writing rows to it (`buffered`
    /// output waits for the run) nor feeding a program that may be showing
    /// them there (a pipe into `less`).
    pub fn meter(&self, buffered: bool) -> bool {
        let quiet_stdout = match self.stdout {
            Sink::Terminal => buffered,
            Sink::File => true,
            Sink::Pipe => false,
        };
        !self.no_progress && self.stderr_terminal && !self.dumb && !self.typed_input && quiet_stdout
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
            depth: Depth::Ansi256,
            dumb: false,
            stdout: Sink::Terminal,
            columns: Some(80),
            rows: Some(24),
            stderr_terminal: true,
            typed_input: false,
            pager_command: Some("less".into()),
            no_pager: false,
            no_progress: false,
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
        let on = |c: Console| c.color();
        assert_eq!(on(terminal()), Some(Depth::Ansi256));
        assert_eq!(on(to(Sink::File)), None);
        assert_eq!(
            on(Console {
                color: ColorWhen::Always,
                ..to(Sink::File)
            }),
            Some(Depth::Ansi256)
        );
        assert_eq!(
            on(Console {
                force_color: true,
                ..to(Sink::File)
            }),
            Some(Depth::Ansi256)
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

    #[test]
    fn colour_is_drawn_at_the_announced_depth() {
        let truecolor = Console {
            depth: Depth::Truecolor,
            ..terminal()
        };
        assert_eq!(truecolor.color(), Some(Depth::Truecolor));
        assert_eq!(terminal().color(), Some(Depth::Ansi256));
    }

    #[test]
    fn a_dumb_terminal_gets_no_colour_unless_forced() {
        let dumb = Console {
            dumb: true,
            ..terminal()
        };
        assert_eq!(dumb.color(), None);
        assert_eq!(
            Console {
                color: ColorWhen::Always,
                ..dumb
            }
            .color(),
            Some(Depth::Ansi256)
        );
    }

    #[test]
    fn only_a_terminal_that_is_not_dumb_is_paged() {
        assert!(terminal().pages());
        for quiet in [
            Console {
                no_pager: true,
                ..terminal()
            },
            Console {
                pager_command: None,
                ..terminal()
            },
            Console {
                dumb: true,
                ..terminal()
            },
            to(Sink::File),
            to(Sink::Pipe),
        ] {
            assert!(!quiet.pages(), "{quiet:?}");
        }
    }

    #[test]
    fn output_fills_the_window_from_as_many_lines_as_it_has_rows() {
        let three = Console {
            rows: Some(3),
            ..terminal()
        };
        // Two lines leave a row for the pager's prompt; three do not.
        assert!(!three.fills(b"a\nb\n"));
        assert!(three.fills(b"a\nb\nc\n"));
        let unknown = Console {
            rows: None,
            ..terminal()
        };
        assert!(!unknown.fills(b"a\nb\nc\n"));
    }

    #[test]
    fn a_table_is_fitted_unless_a_pager_scrolls_it() {
        assert!(terminal().screen(None).fit);
    }

    #[test]
    fn links_need_colour_and_the_terminal() {
        assert!(terminal().links());
        assert!(terminal().screen(None).links);
        // Forced colour into a file or a pipe writes no links.
        for stdout in [Sink::File, Sink::Pipe] {
            let forced = Console {
                color: ColorWhen::Always,
                ..to(stdout)
            };
            assert!(forced.color().is_some() && !forced.links(), "{forced:?}");
        }
        let plain = Console {
            color: ColorWhen::Never,
            ..terminal()
        };
        assert!(!plain.links());
    }

    #[test]
    fn the_meter_draws_only_on_a_terminal_nothing_else_draws_on() {
        // Rows to a file, or a table the run is still building: shown.
        assert!(to(Sink::File).meter(false));
        assert!(terminal().meter(true));
        // Rows streaming to the terminal, or into a pipe: not shown.
        assert!(!terminal().meter(false));
        assert!(!to(Sink::Pipe).meter(true));
        for quiet in [
            Console {
                typed_input: true,
                ..to(Sink::File)
            },
            Console {
                stderr_terminal: false,
                ..to(Sink::File)
            },
            Console {
                dumb: true,
                ..to(Sink::File)
            },
            Console {
                no_progress: true,
                ..to(Sink::File)
            },
        ] {
            assert!(!quiet.meter(true), "{quiet:?}");
        }
    }

    #[test]
    fn errors_are_coloured_by_stderr_under_the_same_rules() {
        // stderr on the terminal colours errors, whatever stdout is.
        assert!(to(Sink::File).error_color());
        let piped = Console {
            stderr_terminal: false,
            ..terminal()
        };
        assert!(!piped.error_color());
        let forced = Console {
            force_color: true,
            ..piped
        };
        assert!(forced.error_color());
    }
}
