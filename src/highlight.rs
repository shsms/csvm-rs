//! `csvm --highlight`: the helper inkline asks to colour a csvm command
//! line while it is typed.
//!
//! inkline writes requests to stdin:
//!
//! ```text
//! :request ID
//! :cwd LEN
//! BYTES
//! :arg final|raw LEN
//! BYTES
//! :done
//! ```
//!
//! with one `:arg` per argument, the command's name first. csvm answers
//! each on stdout, in order:
//!
//! ```text
//! :span ARG START END KIND
//! :error ARG START END MESSAGE      (or :error - - - MESSAGE)
//! :end ID
//! ```
//!
//! Lengths and offsets count bytes. inkline's `docs/highlight-protocol.md`
//! describes the whole protocol.

use std::io::{self, BufRead, Read};

/// The longest `:` line a request may have, newline included.
const MAX_LINE: u64 = 256;

/// The most bytes one `:cwd` or `:arg` may hold. A command line is far
/// smaller; the limit keeps a corrupt length from asking for any amount of
/// memory.
const MAX_BYTES: usize = 16 << 20;

/// One request: the shell's directory and the command's arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub id: u64,
    pub cwd: Vec<u8>,
    /// Argument 0 is the command's name.
    pub args: Vec<Arg>,
}

/// One argument of the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arg {
    /// Bash will still expand it, so `bytes` is the text as typed, not what
    /// csvm will get.
    pub raw: bool,
    pub bytes: Vec<u8>,
}

/// Read the next request; `None` when the input ends before one starts.
/// Input that is not a request is an [`io::ErrorKind::InvalidData`] error:
/// after it, the stream cannot be followed.
pub fn read_request(input: &mut impl BufRead) -> io::Result<Option<Request>> {
    let Some(line) = read_line(input)? else {
        return Ok(None);
    };
    let id = number(field(&line, ":request ")?)?;
    let line = need_line(input)?;
    let cwd = read_bytes(input, number(field(&line, ":cwd ")?)?)?;
    let mut args = Vec::new();
    loop {
        let line = need_line(input)?;
        if line == ":done" {
            return Ok(Some(Request { id, cwd, args }));
        }
        let (kind, len) = field(&line, ":arg ")?
            .split_once(' ')
            .ok_or_else(|| bad(&line))?;
        let raw = match kind {
            "final" => false,
            "raw" => true,
            _ => return Err(bad(&line)),
        };
        args.push(Arg {
            raw,
            bytes: read_bytes(input, number(len)?)?,
        });
    }
}

/// The rest of `line` after `keyword`, which it must start with.
fn field<'l>(line: &'l str, keyword: &str) -> io::Result<&'l str> {
    line.strip_prefix(keyword).ok_or_else(|| bad(line))
}

/// A decimal number: ASCII digits only.
fn number<T: std::str::FromStr>(text: &str) -> io::Result<T> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad(text));
    }
    text.parse().map_err(|_| bad(text))
}

/// One `:` line, without its newline; `None` at the end of the input.
fn read_line(input: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    input.by_ref().take(MAX_LINE).read_until(b'\n', &mut line)?;
    if line.is_empty() {
        return Ok(None);
    }
    if line.pop() != Some(b'\n') {
        return Err(bad(&String::from_utf8_lossy(&line)));
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|e| bad(&String::from_utf8_lossy(e.as_bytes())))
}

/// [`read_line`], where the end of the input is an error.
fn need_line(input: &mut impl BufRead) -> io::Result<String> {
    read_line(input)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the input ended inside a request",
        )
    })
}

/// `len` bytes, then the newline after them.
fn read_bytes(input: &mut impl BufRead, len: usize) -> io::Result<Vec<u8>> {
    if len > MAX_BYTES {
        return Err(bad(&format!("{len} bytes")));
    }
    let mut bytes = vec![0; len + 1];
    input.read_exact(&mut bytes)?;
    if bytes.pop() != Some(b'\n') {
        return Err(bad("no newline after the bytes"));
    }
    Ok(bytes)
}

/// The error for input that is not a request.
fn bad(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("not a highlight request: {what:?}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_read_with_its_bytes_as_given() {
        let input = b":request 7\n:cwd 4\n/tmp\n:arg final 4\ncsvm\n:arg final 5\na\n\0|b\n\
                      :arg raw 7\n$HOME/x\n:arg final 0\n\n:done\n";
        let mut r = &input[..];
        assert_eq!(
            read_request(&mut r).unwrap(),
            Some(Request {
                id: 7,
                cwd: b"/tmp".to_vec(),
                args: vec![
                    Arg {
                        raw: false,
                        bytes: b"csvm".to_vec()
                    },
                    Arg {
                        raw: false,
                        bytes: b"a\n\0|b".to_vec()
                    },
                    Arg {
                        raw: true,
                        bytes: b"$HOME/x".to_vec()
                    },
                    Arg {
                        raw: false,
                        bytes: Vec::new()
                    },
                ],
            })
        );
        // Then the input ends: no more requests.
        assert_eq!(read_request(&mut r).unwrap(), None);
    }

    #[test]
    fn a_request_that_breaks_the_protocol_is_an_error() {
        use io::ErrorKind::{InvalidData, UnexpectedEof};
        let bad = |input: &[u8]| {
            let mut r = input;
            read_request(&mut r).unwrap_err().kind()
        };
        assert_eq!(bad(b"hello\n"), InvalidData);
        assert_eq!(bad(b":request x\n"), InvalidData);
        assert_eq!(bad(b":request +1\n"), InvalidData);
        // A line cut off, or too long to be a `:` line.
        assert_eq!(bad(b":request 1"), InvalidData);
        assert_eq!(
            bad(format!(":request {}\n", "1".repeat(300)).as_bytes()),
            InvalidData
        );
        // No `:cwd`, or a kind that is neither final nor raw.
        assert_eq!(bad(b":request 1\n:arg final 1\na\n"), InvalidData);
        assert_eq!(
            bad(b":request 1\n:cwd 1\n/\n:arg odd 1\na\n:done\n"),
            InvalidData
        );
        // Fewer bytes than the length says, or no newline after them.
        assert_eq!(bad(b":request 1\n:cwd 2\n/\n"), UnexpectedEof);
        assert_eq!(bad(b":request 1\n:cwd 1\n/x:done\n"), InvalidData);
        // A length too large to be a command line.
        assert_eq!(bad(b":request 1\n:cwd 99999999999\n"), InvalidData);
        // The input ends before `:done`.
        assert_eq!(bad(b":request 1\n:cwd 1\n/\n"), UnexpectedEof);
    }
}
