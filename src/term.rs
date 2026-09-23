//! The terminal's size, for charts that should fill the screen and for the
//! pager, which needs to know whether a table will fit on it; and its
//! background colour, for shading every other row of a table off it.

use crate::color::{Base, Rgb};
use std::time::Duration;

/// The terminal's column count: `$COLUMNS` when set and numeric, else the
/// size of the terminal on stdout, else `None` (not a terminal, or unknown).
pub fn columns() -> Option<usize> {
    count_from(std::env::var("COLUMNS").ok().as_deref())
        .or_else(|| window().map(|(cols, _)| cols).filter(|&n| n > 0))
}

/// The terminal's row count: `$LINES` when set and numeric, else the size of
/// the terminal on stdout, else `None`.
pub fn rows() -> Option<usize> {
    count_from(std::env::var("LINES").ok().as_deref())
        .or_else(|| window().map(|(_, rows)| rows).filter(|&n| n > 0))
}

/// `$COLUMNS` or `$LINES` read as a count: a positive integer, else `None` —
/// unset, empty, zero or not a number all mean "ask the terminal instead". Its
/// own function so the rule can be tested without setting a variable the whole
/// process (and every other test thread) would see.
fn count_from(var: Option<&str>) -> Option<usize> {
    var.and_then(|v| v.parse::<usize>().ok()).filter(|&n| n > 0)
}

/// The window size of the terminal on stdout, as (columns, rows); `None` when
/// stdout is not a terminal. A count the terminal does not know is zero.
#[cfg(unix)]
fn window() -> Option<(usize, usize)> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ reads the window size of the given descriptor into
    // a `winsize` we own; it writes nothing else and cannot fail unsafely.
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0).then_some((usize::from(ws.ws_col), usize::from(ws.ws_row)))
}

#[cfg(not(unix))]
fn window() -> Option<(usize, usize)> {
    None
}

/// The background colour of the terminal stdout is on: asked of the terminal
/// itself, else read from `$COLORFGBG`; `None` when neither says. Asking writes
/// to and reads from that terminal, and can wait up to a second on a terminal
/// that does not answer, so it is done only when the answer is used. While it
/// asks, the signals that end or stop the process are held until the terminal
/// is back as it was; that holds them for the calling thread only, so call it
/// with no other thread running.
pub fn background() -> Option<Rgb> {
    ask_background(Duration::from_secs(1)).or_else(|| {
        std::env::var("COLORFGBG")
            .ok()
            .and_then(|v| colorfgbg_background(&v))
    })
}

/// Ask the terminal for its background (OSC 11), and then for its attributes
/// (DA1), which terminal emulators answer: that reply coming first means no
/// answer about the background is coming. `wait` bounds the wait for a
/// terminal that answers neither, such as a bare pty; a reply later than
/// that reaches whatever reads the terminal next. Asked only of the
/// terminal stdout is on, and not from a background job, nor with keys typed
/// and not yet read, which the reading would take.
#[cfg(unix)]
fn ask_background(wait: Duration) -> Option<Rgb> {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    // Stdout is on the controlling terminal: tcgetsid gives our session
    // only for it, or for the master end of a pty whose other end it is.
    // SAFETY: both only read the session of stdout's terminal and our own.
    if unsafe { libc::tcgetsid(libc::STDOUT_FILENO) != libc::getsid(0) } {
        return None;
    }
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    let fd = tty.as_raw_fd();
    // SAFETY: both only read the process's and the terminal's process group.
    if unsafe { libc::tcgetpgrp(fd) != libc::getpgrp() } {
        return None;
    }
    let raw = RawMode::enter(fd)?;
    // Counted in raw mode, where a line not yet ended counts too.
    let mut typed: libc::c_int = 0;
    // SAFETY: FIONREAD writes the count of bytes waiting into the int we own.
    if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut typed) } != 0 || typed > 0 {
        return None;
    }
    tty.write_all(b"\x1b]11;?\x1b\\\x1b[c").ok()?;
    let deadline = std::time::Instant::now() + wait;
    let mut reply = Vec::new();
    let mut buf = [0u8; 64];
    while !has_da1_reply(&reply) && reply.len() < 512 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        // In short waits, so a signal held meanwhile ends it soon.
        let ms = left.min(Duration::from_millis(50)).as_millis() as libc::c_int;
        if ms == 0 || raw.held.pending() {
            break;
        }
        let mut ready = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll reads the one `pollfd` we own and writes its revents.
        match unsafe { libc::poll(&mut ready, 1, ms) } {
            0 => continue,
            n if n < 0 => break,
            _ => {}
        }
        match tty.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => reply.extend_from_slice(&buf[..n]),
        }
    }
    osc11_background(&reply)
}

#[cfg(not(unix))]
fn ask_background(_wait: Duration) -> Option<Rgb> {
    None
}

/// The terminal in raw mode (no line editing, no echo), for reading a reply
/// it sends back as it comes and without showing it; back as it was when
/// dropped. Meanwhile the calling thread holds SIGINT, SIGQUIT, SIGTERM and
/// SIGTSTP, so a Ctrl-C cannot end the process with the terminal still raw:
/// it takes effect once the terminal is restored.
#[cfg(unix)]
struct RawMode {
    fd: libc::c_int,
    saved: libc::termios,
    /// Dropped after the terminal is restored, as the last field.
    held: HeldSignals,
}

#[cfg(unix)]
impl RawMode {
    fn enter(fd: libc::c_int) -> Option<RawMode> {
        let held = HeldSignals::hold()?;
        let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr fills the termios we own, and we read it only when
        // it says it did.
        let saved = unsafe {
            if libc::tcgetattr(fd, saved.as_mut_ptr()) != 0 {
                return None;
            }
            saved.assume_init()
        };
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: tcsetattr reads the termios we own.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        Some(RawMode { fd, saved, held })
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: tcsetattr reads the termios we saved from this descriptor.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved);
        }
    }
}

/// The signals [`HeldSignals`] holds.
#[cfg(unix)]
const HELD: [libc::c_int; 4] = [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGTSTP];

/// The signals that end or stop the process, held for this thread; its
/// signal mask as it was when dropped, which delivers any signal held
/// meanwhile.
#[cfg(unix)]
struct HeldSignals {
    /// The thread's signal mask before.
    before: libc::sigset_t,
}

#[cfg(unix)]
impl HeldSignals {
    /// `None` when the mask could not be changed.
    fn hold() -> Option<HeldSignals> {
        let mut held = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        let mut before = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: sigemptyset initializes the set we own before sigaddset and
        // pthread_sigmask read it; pthread_sigmask fills `before`, which we
        // read only when it says it did.
        unsafe {
            libc::sigemptyset(held.as_mut_ptr());
            for signal in HELD {
                libc::sigaddset(held.as_mut_ptr(), signal);
            }
            if libc::pthread_sigmask(libc::SIG_BLOCK, held.as_ptr(), before.as_mut_ptr()) != 0 {
                return None;
            }
            Some(HeldSignals {
                before: before.assume_init(),
            })
        }
    }

    /// Whether one of the held signals has come.
    fn pending(&self) -> bool {
        let mut pending = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: sigpending fills the set we own, which sigismember reads
        // only when it says it did.
        unsafe {
            if libc::sigpending(pending.as_mut_ptr()) != 0 {
                return false;
            }
            let pending = pending.assume_init();
            HELD.iter()
                .any(|&signal| libc::sigismember(&pending, signal) == 1)
        }
    }
}

#[cfg(unix)]
impl Drop for HeldSignals {
    fn drop(&mut self) {
        // SAFETY: pthread_sigmask reads the set we saved and writes no old
        // set.
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.before, std::ptr::null_mut());
        }
    }
}

/// Whether `reply` holds the terminal's answer to DA1: `ESC [ ? … c`.
fn has_da1_reply(reply: &[u8]) -> bool {
    reply
        .windows(3)
        .position(|w| w == b"\x1b[?")
        .is_some_and(|at| reply[at..].contains(&b'c'))
}

/// The colour in a reply to OSC 11: `ESC ] 11 ; rgb:R/G/B`, each part one to
/// four hex digits, ended by BEL or ST.
fn osc11_background(reply: &[u8]) -> Option<Rgb> {
    let text = std::str::from_utf8(reply).ok()?;
    let start = text.find("\x1b]11;rgb:")? + "\x1b]11;rgb:".len();
    let spec = text[start..].split(['\x07', '\x1b']).next()?;
    let mut parts = spec.split('/').map(|hex| {
        if !(1..=4).contains(&hex.len()) {
            return None;
        }
        let max = (1u32 << (4 * hex.len())) - 1;
        let v = u32::from_str_radix(hex, 16).ok()?;
        Some((v * 255 / max) as u8)
    });
    let rgb = Rgb(parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(rgb)
}

/// The background `$COLORFGBG` names: its last `;` field, an index into the
/// terminal's 16 colours, taken at xterm's shades of them. `None` for
/// `default` or anything else.
fn colorfgbg_background(value: &str) -> Option<Rgb> {
    // Colours 9 to 15; 0 to 8 are the base colours.
    const BRIGHT: [Rgb; 7] = [
        Rgb(255, 0, 0),
        Rgb(0, 255, 0),
        Rgb(255, 255, 0),
        Rgb(92, 92, 255),
        Rgb(255, 0, 255),
        Rgb(0, 255, 255),
        Rgb(255, 255, 255),
    ];
    let index: usize = value.rsplit(';').next()?.trim().parse().ok()?;
    match index.checked_sub(Base::ALL.len()) {
        None => Some(Base::ALL[index].rgb()),
        Some(bright) => BRIGHT.get(bright).copied(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc11_replies_are_read_at_any_hex_width() {
        let reply = |s: &str| osc11_background(s.as_bytes());
        assert_eq!(
            reply("\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\\x1b[?62;22c"),
            Some(Rgb(28, 28, 28))
        );
        assert_eq!(reply("\x1b]11;rgb:ff/80/00\x07"), Some(Rgb(255, 128, 0)));
        assert_eq!(reply("\x1b]11;rgb:f/0/f\x07"), Some(Rgb(255, 0, 255)));
        // Only the attributes came back: no answer about the background.
        assert_eq!(reply("\x1b[?62;22c"), None);
        assert_eq!(reply("\x1b]11;rgb:ff/80\x07"), None);
        assert_eq!(reply("\x1b]11;rgb:ff/80/zz\x07"), None);
    }

    #[test]
    fn the_attributes_reply_ends_the_wait() {
        assert!(has_da1_reply(b"\x1b]11;rgb:0/0/0\x07\x1b[?62;22c"));
        assert!(!has_da1_reply(b"\x1b]11;rgb:0/0/0\x07\x1b[?62;2"));
        assert!(!has_da1_reply(b""));
    }

    #[test]
    fn colorfgbg_names_the_background_last() {
        assert_eq!(colorfgbg_background("15;0"), Some(Rgb(0, 0, 0)));
        assert_eq!(colorfgbg_background("0;15"), Some(Rgb(255, 255, 255)));
        assert_eq!(
            colorfgbg_background("0;default;15"),
            Some(Rgb(255, 255, 255))
        );
        assert_eq!(colorfgbg_background("15;default"), None);
        assert_eq!(colorfgbg_background("15;16"), None);
        assert_eq!(colorfgbg_background(""), None);
    }

    #[test]
    fn count_from_takes_a_positive_integer_and_nothing_else() {
        assert_eq!(count_from(Some("120")), Some(120));
        // Zero is a size no chart or table could use, so it is not an answer.
        assert_eq!(count_from(Some("0")), None);
        assert_eq!(count_from(Some("x")), None);
        assert_eq!(count_from(None), None);
    }

    #[test]
    fn the_size_is_a_positive_count_or_none() {
        // Under `cargo test` stdout is usually a pipe; either answer is fine,
        // but never zero.
        assert!(columns().is_none_or(|n| n > 0));
        assert!(rows().is_none_or(|n| n > 0));
    }
}
