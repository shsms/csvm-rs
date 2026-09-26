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

use crate::cli::{self, InputFormat, Parsed};
use crate::error::Error;
use crate::exec;
use crate::parse::{self, Recorder, SpanKind};
use crate::plan::{Plan, Stage};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The line csvm writes first: the protocol's name and version.
pub const GREETING: &str = "inkline-highlight 1";

/// The longest `:` line a request may have, newline included.
const MAX_LINE: u64 = 256;

/// The most bytes one `:cwd` or `:arg` may hold. A command line is far
/// smaller; the limit keeps a corrupt length from asking for any amount of
/// memory.
const MAX_BYTES: usize = 16 << 20;

/// The most bytes of `:span` lines one reply may hold. inkline turns a
/// helper off when a reply passes 1 MiB, so the spans past this are not
/// sent (the end of a very long script is then not coloured), which leaves
/// room for the error and `:end`.
const MAX_SPAN_BYTES: usize = 900 << 10;

/// The most bytes of an error's message that are sent, so that a message
/// quoting a very long word cannot push a reply past inkline's limit either.
const MAX_MESSAGE: usize = 4 << 10;

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

/// The answer to one request.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reply {
    /// In order, none overlapping another.
    pub spans: Vec<Span>,
    pub error: Option<ReplyError>,
}

/// A coloured part of one argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// The argument's index in the request.
    pub arg: usize,
    /// Byte offsets in the argument.
    pub at: Range<usize>,
    pub kind: SpanKind,
}

/// The one error a reply may carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyError {
    /// The argument, and the bytes of it, the error is about; `None` when
    /// it has no place in the arguments.
    pub place: Option<(usize, Range<usize>)>,
    /// One line of text.
    pub message: String,
}

impl ReplyError {
    /// An error with `message` made one line, as [`write_reply`] writes it.
    pub fn new(place: Option<(usize, Range<usize>)>, message: &str) -> ReplyError {
        ReplyError {
            place,
            message: one_line(message),
        }
    }
}

/// What the helper keeps from one request to the next.
#[derive(Default)]
pub struct Session {
    headers: Headers,
}

impl Session {
    /// Answer one request: the script's colours, and the first thing csvm
    /// would reject on this command line.
    pub fn answer(&mut self, request: &Request) -> Reply {
        let mut reply = Reply::default();
        // Argument 0 is the command's name; csvm's own parser reads the rest.
        let mut words = Vec::with_capacity(request.args.len());
        for (i, arg) in request.args.iter().enumerate().skip(1) {
            match std::str::from_utf8(&arg.bytes) {
                Ok(text) => words.push(text.to_string()),
                Err(e) => {
                    // csvm cannot start with such an argument at all.
                    let start = e.valid_up_to();
                    let end = e.error_len().map_or(arg.bytes.len(), |n| start + n);
                    reply.error = Some(ReplyError::new(
                        Some((i, start..end)),
                        "this argument is not valid UTF-8",
                    ));
                    return reply;
                }
            }
        }
        let args = match cli::parse_at(words) {
            Ok(Parsed::Run(args)) => args,
            // Help and the version take no script.
            Ok(Parsed::Help { .. } | Parsed::Version) => return reply,
            Err(usage) => {
                // A line that stops before its script is still being typed,
                // so an error with no argument to point at is not sent. The
                // arguments are read left to right, so an error depends only
                // on the arguments up to it, or on all of them when it is
                // about the whole line; when one of those is `raw`, bash may
                // still turn it into other words (or none), and csvm may not
                // see this error at all.
                if let Some(at) = usage.arg {
                    let read = if usage.whole_line {
                        &request.args[1..]
                    } else {
                        &request.args[1..=at + 1]
                    };
                    if !read.iter().any(|a| a.raw) {
                        let len = request.args[at + 1].bytes.len();
                        reply.error = Some(ReplyError::new(Some((at + 1, 0..len)), &usage.message));
                    }
                }
                return reply;
            }
        };
        // With `-f` the script is in a file, which is not coloured.
        let Some(script_at) = args.script_at else {
            return reply;
        };
        let arg = script_at + 1;
        let mut rec = Recorder::default();
        let parsed = parse::parse_recorded(&args.script, &mut rec);
        reply.spans = rec
            .into_spans()
            .into_iter()
            .map(|(at, kind)| Span { arg, at, kind })
            .collect();
        // A `raw` argument before the script may become other words, or
        // none, so csvm may take another argument as its script; a raw
        // script is not the text csvm gets. Either way, an error found in
        // this text may not be csvm's.
        if request.args[1..=arg].iter().any(|a| a.raw) {
            return reply;
        }
        let error = match parsed {
            Ok(mut plan) => self.check_columns(request, &args, &mut plan),
            Err(e) => Some(e),
        };
        reply.error = error.map(|e| script_error(arg, &args.script, &e));
        reply
    }

    /// The error csvm would find resolving `plan` against the input's
    /// header. `None` when there is none, or when the header cannot be
    /// known here: an argument is `raw`, or the input is stdin, a relative
    /// path with no shell directory to take it from, not a regular file, or
    /// cannot be read; or the same holds for a join's file.
    fn check_columns(
        &mut self,
        request: &Request,
        args: &cli::Args,
        plan: &mut Plan,
    ) -> Option<Error> {
        // A `raw` argument anywhere may become other words, or none, and so
        // change the input, its header, or what csvm makes of the line.
        if request.args[1..].iter().any(|a| a.raw) {
            return None;
        }
        let path = args.in_path()?;
        let cwd = shell_dir(&request.cwd);
        let path = from_dir(cwd, Path::new(path))?;
        let format = args.input_format();
        let header = match (&args.header, format) {
            // csvm rejects `--header` for Parquet, which names its own columns.
            (Some(_), InputFormat::Parquet) => return None,
            (Some(cli::Header::Named(names)), _) => {
                std::fs::metadata(&path).ok().filter(|m| m.is_file())?;
                names.clone()
            }
            (spec, _) => {
                let first = self.headers.first_line(&path, format)?;
                cli::Header::resolve(spec.as_ref(), first, 0).0
            }
        };
        if let Err(e) = resolve_joins(plan, cwd, &mut self.headers) {
            return e;
        }
        plan.resolve(&header).err()
    }
}

/// `e`, found in the script, which is argument `arg`, as a reply's error:
/// on its place in the script when it has one.
fn script_error(arg: usize, script: &str, e: &Error) -> ReplyError {
    let place = e.span().map(|at| {
        let end = at.end.min(script.len());
        (arg, at.start.min(end)..end)
    });
    ReplyError::new(place, &e.to_string())
}

/// The shell's directory, from a request's `:cwd`. `None` when that is not
/// an absolute UTF-8 path: bash sends an empty one when `PWD` is unset.
fn shell_dir(cwd: &[u8]) -> Option<&Path> {
    let dir = Path::new(std::str::from_utf8(cwd).ok()?);
    dir.is_absolute().then_some(dir)
}

/// `path` as the shell sees it from its directory `cwd`. `None` for a
/// relative path when that directory is not known: the helper's own
/// directory is not the shell's, so it is never used.
fn from_dir(cwd: Option<&Path>, path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        cwd.map(|dir| dir.join(path))
    }
}

/// Resolve each join's sub-pipeline in `plan` against its file's header,
/// the sub-pipeline's own joins first, as `exec::prepare_joins` does at
/// start-up, with a relative file taken from the shell's directory `cwd`.
/// Each file's first line comes from `headers`, like the input's. The
/// error is `None` when a file's header cannot be known here: its path is
/// relative and `cwd` is not known, it is not a regular file (a FIFO or a
/// device could block reading it), or its first line cannot be read.
fn resolve_joins(
    plan: &mut Plan,
    cwd: Option<&Path>,
    headers: &mut Headers,
) -> Result<(), Option<Error>> {
    for stage in &mut plan.stages {
        if let Stage::Join(j) = stage {
            resolve_joins(&mut j.right_plan, cwd, headers)?;
            let path = from_dir(cwd, Path::new(&j.file)).ok_or(None)?;
            // A join's right file is always CSV.
            let header = headers.first_line(&path, InputFormat::Csv).ok_or(None)?;
            j.right_header = j.right_plan.resolve(&header).map_err(Some)?;
        }
    }
    Ok(())
}

/// How many files [`Headers`] keeps; past that it starts again from none.
const MAX_FILES: usize = 64;

/// The most bytes read looking for the end of a CSV file's first line.
const MAX_HEADER_BYTES: u64 = 1 << 20;

/// The first line of each file read so far, by path and format. An entry
/// is used again only while the file's size and modification time are
/// what they were when it was read, so an unchanged file costs one look at
/// its size and time per request. A read that failed is kept too, so it is
/// not tried again until the file changes.
#[derive(Default)]
struct Headers {
    files: HashMap<(PathBuf, InputFormat), Seen>,
}

/// One file's first line, or `None` when it could not be read, and the
/// file's size and time when it was read.
struct Seen {
    len: u64,
    modified: Option<SystemTime>,
    columns: Option<Vec<String>>,
}

impl Headers {
    /// The columns of the first line of the file at `path`, read as csvm
    /// reads its input's. `None` when it is not a regular file (a FIFO or a
    /// device could block or never end) or cannot be read as `format`.
    fn first_line(&mut self, path: &Path, format: InputFormat) -> Option<Vec<String>> {
        let meta = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
        let modified = meta.modified().ok();
        let key = (path.to_path_buf(), format);
        if let Some(seen) = self.files.get(&key)
            && seen.len == meta.len()
            && seen.modified == modified
        {
            return seen.columns.clone();
        }
        let columns = read_first_line(path, format, meta.len());
        if self.files.len() >= MAX_FILES {
            self.files.clear();
        }
        self.files.insert(
            key,
            Seen {
                len: meta.len(),
                modified,
                columns: columns.clone(),
            },
        );
        columns
    }
}

/// The columns of the first line of `path`, a file `len` bytes long.
fn read_first_line(path: &Path, format: InputFormat, len: u64) -> Option<Vec<String>> {
    match format {
        InputFormat::Csv => {
            let mut line = Vec::new();
            let file = File::open(path).ok()?;
            BufReader::new(file.take(MAX_HEADER_BYTES))
                .read_until(b'\n', &mut line)
                .ok()?;
            // A line the limit cut off is not the whole header.
            if !line.ends_with(b"\n") && (line.len() as u64) < len {
                return None;
            }
            exec::read_header(&mut line.as_slice()).ok()
        }
        #[cfg(feature = "parquet")]
        InputFormat::Parquet => crate::parquet::read_header(path).ok(),
        #[cfg(not(feature = "parquet"))]
        InputFormat::Parquet => None,
    }
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

/// Write `reply` to request `id`, ending with `:end ID`. The spans stop
/// before they pass `MAX_SPAN_BYTES`; the error, on one line of at most
/// `MAX_MESSAGE` bytes, and the end are always written.
pub fn write_reply(out: &mut impl Write, id: u64, reply: &Reply) -> io::Result<()> {
    let mut written = 0;
    for span in &reply.spans {
        let line = format!(
            ":span {} {} {} {}\n",
            span.arg,
            span.at.start,
            span.at.end,
            span.kind.name()
        );
        written += line.len();
        if written > MAX_SPAN_BYTES {
            break;
        }
        out.write_all(line.as_bytes())?;
    }
    if let Some(error) = &reply.error {
        let message = one_line(&error.message);
        match &error.place {
            Some((arg, at)) => writeln!(out, ":error {arg} {} {} {message}", at.start, at.end)?,
            None => writeln!(out, ":error - - - {message}")?,
        }
    }
    writeln!(out, ":end {id}")
}

/// `message` as one line: up to its first newline, at most [`MAX_MESSAGE`]
/// bytes, cut between two characters, a `\r` as a space, and no spaces at
/// the end.
fn one_line(message: &str) -> String {
    let first = message.split('\n').next().unwrap_or("");
    let first = &first[..first.floor_char_boundary(MAX_MESSAGE)];
    first.replace('\r', " ").trim_end().to_string()
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

    #[test]
    fn a_reply_is_written_line_by_line() {
        let reply = Reply {
            spans: vec![
                Span {
                    arg: 1,
                    at: 0..4,
                    kind: SpanKind::Command,
                },
                Span {
                    arg: 1,
                    at: 5..6,
                    kind: SpanKind::Variable,
                },
            ],
            error: Some(ReplyError::new(Some((1, 7..7)), "bad thing\nsecond line")),
        };
        let mut out = Vec::new();
        write_reply(&mut out, 3, &reply).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            ":span 1 0 4 command\n:span 1 5 6 variable\n:error 1 7 7 bad thing\n:end 3\n"
        );
        // An error with no place; a stray `\r` does not reach the line.
        let reply = Reply {
            spans: Vec::new(),
            error: Some(ReplyError::new(None, "no place\r")),
        };
        let mut out = Vec::new();
        write_reply(&mut out, 4, &reply).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            ":error - - - no place\n:end 4\n"
        );
        // Nothing to say: only the end.
        let mut out = Vec::new();
        write_reply(&mut out, 5, &Reply::default()).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), ":end 5\n");
    }

    /// inkline's reader requires the space before `MESSAGE` even when the
    /// message is empty (`:error ARG START END ` / `:error - - - `), so a
    /// consumer can always split off four fields. This checks the exact
    /// bytes for both an empty message with a place and one without.
    #[test]
    fn an_error_with_an_empty_message_still_gets_its_trailing_space() {
        let reply = Reply {
            spans: Vec::new(),
            error: Some(ReplyError::new(Some((0, 3..3)), "")),
        };
        let mut out = Vec::new();
        write_reply(&mut out, 1, &reply).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), ":error 0 3 3 \n:end 1\n");

        let reply = Reply {
            spans: Vec::new(),
            error: Some(ReplyError::new(None, "")),
        };
        let mut out = Vec::new();
        write_reply(&mut out, 2, &reply).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), ":error - - - \n:end 2\n");
    }

    #[test]
    fn a_message_is_written_on_one_line_however_it_was_built() {
        let reply = Reply {
            spans: Vec::new(),
            error: Some(ReplyError {
                place: None,
                message: "a\nb".to_string(),
            }),
        };
        let mut out = Vec::new();
        write_reply(&mut out, 1, &reply).unwrap();
        assert_eq!(out, b":error - - - a\n:end 1\n");
        // A `\r` is a space; a message too long is cut on a character.
        let reply = Reply {
            spans: Vec::new(),
            error: Some(ReplyError {
                place: Some((1, 0..1)),
                message: format!("a\rb{}", "é".repeat(MAX_MESSAGE)),
            }),
        };
        let mut out = Vec::new();
        write_reply(&mut out, 2, &reply).unwrap();
        let text = String::from_utf8(out).unwrap();
        let message = text
            .strip_prefix(":error 1 0 1 ")
            .and_then(|t| t.strip_suffix("\n:end 2\n"))
            .unwrap();
        assert!(message.starts_with("a b\u{e9}"));
        assert!(message.len() <= MAX_MESSAGE);
        assert!(message.len() > MAX_MESSAGE - 2);
    }

    /// Check that `bytes` is a well-formed reply to request `id`, by the
    /// protocol's shape: every line ends in a newline and is a `:span`,
    /// `:error` or `:end` line; a number field is plain ASCII digits, no
    /// sign; `:span` and `:error` each carry exactly four space-separated
    /// fields (so `MESSAGE`'s leading space is required even when it is
    /// empty); a span has `START < END` and a placed error `START <= END`;
    /// a placeless error is exactly `- - -`; there is at most one `:error`;
    /// and the reply ends with `:end ID`, naming the request's `ID`, and
    /// nothing after it. It does not know the arguments, so it cannot check
    /// an offset against an argument's length, nor that spans do not
    /// overlap, nor a span's `KIND`.
    fn check_reply_is_well_formed(bytes: &[u8], id: u64) {
        fn is_number(s: &str) -> bool {
            !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
        }

        let text = std::str::from_utf8(bytes).expect("reply must be UTF-8");
        let mut lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(
            lines.pop(),
            Some(""),
            "every line, :end included, ends in a newline"
        );
        assert!(!lines.is_empty(), "a reply always has :end");

        let mut error_seen = false;
        for (i, line) in lines.iter().enumerate() {
            assert!(
                line.starts_with(':'),
                "a reply line starts with ':': {line:?}"
            );
            if let Some(rest) = line.strip_prefix(":span ") {
                let fields: Vec<&str> = rest.splitn(4, ' ').collect();
                assert_eq!(fields.len(), 4, ":span needs ARG START END KIND: {line:?}");
                assert!(is_number(fields[0]), "ARG is not plain digits: {line:?}");
                assert!(is_number(fields[1]), "START is not plain digits: {line:?}");
                assert!(is_number(fields[2]), "END is not plain digits: {line:?}");
                let start: u64 = fields[1].parse().unwrap();
                let end: u64 = fields[2].parse().unwrap();
                assert!(start < end, "a span needs START < END: {line:?}");
                assert!(!fields[3].is_empty(), "KIND is missing: {line:?}");
            } else if let Some(rest) = line.strip_prefix(":error ") {
                assert!(!error_seen, "at most one :error line per reply");
                error_seen = true;
                let fields: Vec<&str> = rest.splitn(4, ' ').collect();
                assert_eq!(
                    fields.len(),
                    4,
                    ":error needs ARG START END MESSAGE, MESSAGE's leading \
                     space included even when it is empty: {line:?}"
                );
                if fields[0] == "-" || fields[1] == "-" || fields[2] == "-" {
                    assert_eq!(
                        (fields[0], fields[1], fields[2]),
                        ("-", "-", "-"),
                        "a placeless error is exactly '- - -': {line:?}"
                    );
                } else {
                    assert!(is_number(fields[0]), "ARG is not plain digits: {line:?}");
                    assert!(is_number(fields[1]), "START is not plain digits: {line:?}");
                    assert!(is_number(fields[2]), "END is not plain digits: {line:?}");
                    let start: u64 = fields[1].parse().unwrap();
                    let end: u64 = fields[2].parse().unwrap();
                    assert!(start <= end, "an error needs START <= END: {line:?}");
                }
            } else if let Some(rest) = line.strip_prefix(":end ") {
                assert_eq!(i, lines.len() - 1, ":end must be the reply's last line");
                assert!(is_number(rest), "ID is not plain digits: {line:?}");
                assert_eq!(rest.parse::<u64>().unwrap(), id, "wrong :end ID");
            } else {
                panic!("not a reply line: {line:?}");
            }
        }
        assert!(
            lines.last().unwrap().starts_with(":end "),
            "a reply must end with :end"
        );
    }

    #[test]
    fn a_written_reply_is_well_formed() {
        let reply = Reply {
            spans: vec![
                Span {
                    arg: 1,
                    at: 0..4,
                    kind: SpanKind::Command,
                },
                Span {
                    arg: 1,
                    at: 5..6,
                    kind: SpanKind::Variable,
                },
            ],
            error: Some(ReplyError::new(Some((1, 7..7)), "bad thing")),
        };
        let mut out = Vec::new();
        write_reply(&mut out, 9, &reply).unwrap();
        check_reply_is_well_formed(&out, 9);

        let reply = Reply {
            spans: Vec::new(),
            error: Some(ReplyError::new(None, "")),
        };
        let mut out = Vec::new();
        write_reply(&mut out, 10, &reply).unwrap();
        check_reply_is_well_formed(&out, 10);

        let mut out = Vec::new();
        write_reply(&mut out, 11, &Reply::default()).unwrap();
        check_reply_is_well_formed(&out, 11);
    }

    /// A request for the command line `args` (the command's name first),
    /// every argument `final`, from the directory `cwd`.
    fn request(cwd: &str, args: &[&str]) -> Request {
        Request {
            id: 1,
            cwd: cwd.as_bytes().to_vec(),
            args: args
                .iter()
                .map(|a| Arg {
                    raw: false,
                    bytes: a.as_bytes().to_vec(),
                })
                .collect(),
        }
    }

    /// The reply's lines as [`write_reply`] writes them, without `:end`.
    fn reply_lines(reply: &Reply) -> Vec<String> {
        let mut out = Vec::new();
        write_reply(&mut out, 1, reply).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .filter(|l| !l.starts_with(":end"))
            .map(str::to_string)
            .collect()
    }

    /// The reply's lines for the command line `args`, from `/`.
    fn ask(args: &[&str]) -> Vec<String> {
        reply_lines(&Session::default().answer(&request("/", args)))
    }

    /// [`ask`], with the arguments at `raw` sent as `raw`.
    fn ask_raw(args: &[&str], raw: &[usize]) -> Vec<String> {
        let mut req = request("/", args);
        for &i in raw {
            req.args[i].raw = true;
        }
        reply_lines(&Session::default().answer(&req))
    }

    #[test]
    fn the_script_is_coloured_where_it_is_on_the_line() {
        assert_eq!(
            ask(&["csvm", "-n", "2", "cols a | head 3"]),
            [
                ":span 3 0 4 command",
                ":span 3 5 6 variable",
                ":span 3 7 8 operator",
                ":span 3 9 13 command",
                ":span 3 14 15 number",
            ]
        );
    }

    #[test]
    fn a_multi_line_script_is_coloured_on_every_line() {
        assert_eq!(
            ask(&["csvm", "cols a\nselect a > 1"]),
            [
                ":span 1 0 4 command",
                ":span 1 5 6 variable",
                ":span 1 7 13 command",
                ":span 1 14 15 variable",
                ":span 1 16 17 operator",
                ":span 1 18 19 number",
            ]
        );
    }

    #[test]
    fn a_script_error_is_sent_on_its_place() {
        assert_eq!(
            ask(&["csvm", "select a >> 1"]),
            [
                ":span 1 0 6 command",
                ":span 1 7 8 variable",
                ":span 1 9 10 operator",
                ":span 1 10 11 operator",
                ":span 1 12 13 number",
                ":error 1 10 11 expected a column, number, string, or function, found '>'",
            ]
        );
        // An unknown command, with its hint.
        assert_eq!(
            ask(&["csvm", "selct a"]),
            [":error 1 0 5 unknown command: selct (did you mean `select`?)"]
        );
        // An error with no place in the script.
        assert_eq!(
            ask(&["csvm", "# only a comment"]),
            [":span 1 0 16 comment", ":error - - - empty script"]
        );
    }

    #[test]
    fn an_option_error_is_sent_on_its_argument() {
        assert_eq!(
            ask(&["csvm", "--colr", "always", "cols a"]),
            [":error 1 0 6 unknown option: --colr"]
        );
        assert_eq!(
            ask(&["csvm", "cols a", "-n", "many"]),
            [":error 3 0 4 invalid threads value: many"]
        );
        assert_eq!(
            ask(&["csvm", "cols a", "-f"]),
            [":error 2 0 2 missing value for -f"]
        );
        // A line that stops before its script: nothing to say yet.
        assert_eq!(ask(&["csvm", "-n", "2"]), Vec::<String>::new());
        assert_eq!(ask(&["csvm"]), Vec::<String>::new());
    }

    #[test]
    fn a_usage_error_after_a_raw_argument_is_not_sent() {
        // `$FLAGS` may become any number of words, options among them, so
        // the arguments after it may not be what they look like here.
        assert_eq!(
            ask_raw(&["csvm", "$FLAGS", "select amount > 1", "data.csv"], &[1]),
            Vec::<String>::new()
        );
        assert_eq!(
            ask_raw(&["csvm", "$X", "--colr", "cols a"], &[1]),
            Vec::<String>::new()
        );
        // Nor an error on the raw argument itself.
        assert_eq!(
            ask_raw(&["csvm", "-n", "$N", "cols a"], &[2]),
            Vec::<String>::new()
        );
        // A usage error before any raw argument is still sent.
        assert_eq!(
            ask_raw(&["csvm", "--colr", "$X", "cols a"], &[2]),
            [":error 1 0 6 unknown option: --colr"]
        );
        assert_eq!(
            ask_raw(&["csvm", "cols a", "-n", "many", "$X"], &[4]),
            [":error 3 0 4 invalid threads value: many"]
        );
        // An error found once every argument is read depends on all of
        // them: `$X` may be `--help`.
        assert_eq!(
            ask_raw(&["csvm", "cols a", "in.csv", "extra", "$X"], &[4]),
            Vec::<String>::new()
        );
        assert_eq!(
            ask_raw(&["csvm", "help", "fmt", "extra", "$X"], &[4]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_script_error_after_a_raw_argument_is_not_sent() {
        // `$OUT` may become several words, so csvm may take another
        // argument as its script: the colours stay, the error does not.
        assert_eq!(
            ask_raw(&["csvm", "-o", "$OUT", "cols a | selct"], &[2]),
            [
                ":span 3 0 4 command",
                ":span 3 5 6 variable",
                ":span 3 7 8 operator",
            ]
        );
        // Nor in a raw script, sent as typed, quote marks included. Here
        // the `|` is inside the quote marks, so the text is one stage whose
        // first word, `"cols`, is not a command: nothing is coloured.
        assert_eq!(
            ask_raw(&["csvm", "\"cols a | selct\""], &[1]),
            Vec::<String>::new()
        );
        // A raw argument after the script is taken to stay one argument
        // that is not an option, so the script's error is still sent.
        assert_eq!(
            ask_raw(&["csvm", "cols a | selct", "$f"], &[2])
                .last()
                .unwrap(),
            ":error 1 9 14 unknown command: selct (did you mean `select`?)"
        );
    }

    #[test]
    fn a_script_file_or_help_is_not_coloured() {
        assert_eq!(
            ask(&["csvm", "-f", "prog.csvm", "data.csv"]),
            Vec::<String>::new()
        );
        assert_eq!(ask(&["csvm", "help", "select"]), Vec::<String>::new());
        assert_eq!(ask(&["csvm", "--version"]), Vec::<String>::new());
    }

    #[test]
    fn an_argument_that_is_not_utf8_is_an_error_at_its_first_bad_byte() {
        let mut req = request("/", &["csvm", "cols a"]);
        req.args[1].bytes = b"cols \xff a".to_vec();
        assert_eq!(
            reply_lines(&Session::default().answer(&req)),
            [":error 1 5 6 this argument is not valid UTF-8"]
        );
        // Cut off inside a character: up to the end.
        req.args[1].bytes = b"cols \xc3".to_vec();
        assert_eq!(
            reply_lines(&Session::default().answer(&req)),
            [":error 1 5 6 this argument is not valid UTF-8"]
        );
    }

    #[test]
    fn an_empty_cwd_is_answered_like_any_other() {
        assert_eq!(
            reply_lines(&Session::default().answer(&request("", &["csvm", "cols a"]))),
            [":span 1 0 4 command", ":span 1 5 6 variable"]
        );
    }

    #[test]
    fn a_reply_to_a_very_long_script_stays_under_inklines_limit() {
        // Over 3 MiB of spans in all, and an error at the very end.
        let script = format!("cols {}| selct", "a ".repeat(100_000));
        let reply = Session::default().answer(&request("/", &["csvm", &script]));
        // `cols`, each `a` and the `|`.
        assert_eq!(reply.spans.len(), 100_002);
        let mut out = Vec::new();
        write_reply(&mut out, 1, &reply).unwrap();
        assert!(out.len() < 1 << 20, "{} bytes", out.len());
        check_reply_is_well_formed(&out, 1);
        let lines = reply_lines(&reply);
        let (error, spans) = lines.split_last().unwrap();
        // The spans written are the first ones, as many as fit.
        assert!(spans.len() > 20_000 && spans.len() < reply.spans.len());
        assert_eq!(spans[0], ":span 1 0 4 command");
        assert_eq!(spans[spans.len() - 1], {
            let at = 5 + 2 * (spans.len() - 2);
            format!(":span 1 {at} {} variable", at + 1)
        });
        // The error and the end are always written.
        let at = script.len() - 5;
        assert_eq!(
            *error,
            format!(
                ":error 1 {at} {} unknown command: selct (did you mean `select`?)",
                script.len()
            )
        );
        assert!(out.ends_with(b":end 1\n"));
    }

    use std::time::Duration;

    /// A fresh directory under the system's temp directory, removed when
    /// dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let dir =
                std::env::temp_dir().join(format!("csvm_highlight_{}_{name}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        /// Write `content` to `file` in the directory; returns its path.
        fn write(&self, file: &str, content: &str) -> PathBuf {
            let path = self.0.join(file);
            std::fs::write(&path, content).unwrap();
            path
        }

        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Whether `lines` hold no error.
    fn no_error(lines: &[String]) -> bool {
        lines.iter().all(|l| l.starts_with(":span"))
    }

    #[test]
    fn an_unknown_column_in_the_input_is_the_error() {
        let dir = TempDir::new("columns");
        dir.write("data.csv", "amount,region\n1,x\n");
        let mut session = Session::default();
        let mut ask_in = |args: &[&str]| reply_lines(&session.answer(&request(dir.path(), args)));
        assert_eq!(
            ask_in(&["csvm", "select amont > 1", "data.csv"])
                .last()
                .unwrap(),
            ":error 1 7 12 column not found: amont (did you mean `amount`?) — have: amount, region"
        );
        assert!(no_error(&ask_in(&[
            "csvm",
            "select amount > 1",
            "data.csv"
        ])));
        // `--header` names the columns instead of the first line.
        assert_eq!(
            ask_in(&["csvm", "--header", "p,q", "cols amount", "data.csv"])
                .last()
                .unwrap(),
            ":error 3 5 11 column not found: amount — have: p, q"
        );
        assert!(no_error(&ask_in(&[
            "csvm", "--header", "-", "cols c2", "data.csv"
        ])));
    }

    #[test]
    fn no_column_check_without_a_readable_regular_file() {
        let dir = TempDir::new("nocheck");
        dir.write("data.csv", "a\n1\n");
        dir.write("empty.csv", "");
        let ask_in =
            |args: &[&str]| reply_lines(&Session::default().answer(&request(dir.path(), args)));
        // stdin
        assert!(no_error(&ask_in(&["csvm", "cols zz"])));
        assert!(no_error(&ask_in(&["csvm", "cols zz", "-"])));
        // no such file, not a regular file, no header line
        assert!(no_error(&ask_in(&["csvm", "cols zz", "missing.csv"])));
        assert!(no_error(&ask_in(&["csvm", "cols zz", "/dev/null"])));
        assert!(no_error(&ask_in(&["csvm", "cols zz", "empty.csv"])));
        // The input as bash will still change it.
        let mut req = request(dir.path(), &["csvm", "cols zz", "data.csv"]);
        req.args[2].raw = true;
        assert!(no_error(&reply_lines(&Session::default().answer(&req))));
        // Any other raw argument: before the script, csvm may take another
        // argument as its script or its input; after it, `$H` may not be
        // the header, and `$OUT` may add an argument.
        for (args, raw) in [
            (&["csvm", "-o", "$OUT", "cols zz", "data.csv"][..], 2),
            (&["csvm", "cols zz", "data.csv", "--header", "$H"], 4),
            (&["csvm", "cols zz", "data.csv", "-o", "$OUT"], 4),
        ] {
            let mut req = request(dir.path(), args);
            req.args[raw].raw = true;
            assert!(no_error(&reply_lines(&Session::default().answer(&req))));
        }
        // Parquet with `--header` is csvm's own error, not a column one.
        assert!(no_error(&ask_in(&[
            "csvm", "--header", "a", "--format", "parquet", "cols zz", "data.csv"
        ])));
        // A Parquet input csvm cannot read here: the file is not Parquet, or
        // this build has no Parquet support.
        assert!(no_error(&ask_in(&[
            "csvm", "--format", "parquet", "cols zz", "data.csv"
        ])));
    }

    #[test]
    fn a_relative_path_needs_the_shells_directory() {
        let dir = TempDir::new("cwd");
        let data = dir.write("data.csv", "a\n1\n");
        let right = dir.write("right.csv", "a,b\n1,2\n");
        let (data, right) = (data.to_str().unwrap(), right.to_str().unwrap());
        let ask_from = |cwd: &[u8], args: &[&str]| {
            let mut req = request("", args);
            req.cwd = cwd.to_vec();
            reply_lines(&Session::default().answer(&req))
        };
        // bash sends an empty `:cwd` when `PWD` is unset. A relative path
        // is then not looked up at all, and never from the helper's own
        // directory, where `cargo test` has a `Cargo.toml` whose first line
        // has no column `zz`.
        for cwd in [&b""[..], b"\xff", b"relative/dir"] {
            assert!(no_error(&ask_from(cwd, &["csvm", "cols zz", "Cargo.toml"])));
            let script = "join Cargo.toml on a | cols zz";
            assert!(no_error(&ask_from(cwd, &["csvm", script, data])));
            // An absolute path is still checked, the input's and a join's.
            assert_eq!(
                ask_from(cwd, &["csvm", "cols zz", data]).last().unwrap(),
                ":error 1 5 7 column not found: zz — have: a"
            );
            let script = format!("join {right} on a | cols zz");
            let at = script.len() - 2;
            assert_eq!(
                *ask_from(cwd, &["csvm", &script, data]).last().unwrap(),
                format!(":error 1 {at} {} column not found: zz — have: a, b", at + 2)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_never_opened() {
        let dir = TempDir::new("fifo");
        let fifo = std::ffi::CString::new(format!("{}/pipe.csv", dir.path())).unwrap();
        // SAFETY: `fifo` is a valid C string for the whole call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        // Opening the FIFO to read would wait for a writer forever, so each
        // answer runs on its own thread with a deadline.
        let answer = |args: &[&str]| {
            let req = request(dir.path(), args);
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(reply_lines(&Session::default().answer(&req)));
            });
            rx.recv_timeout(Duration::from_secs(5))
                .expect("the answer waited on the FIFO")
        };
        assert!(no_error(&answer(&["csvm", "cols zz", "pipe.csv"])));
        dir.write("left.csv", "k\n1\n");
        assert!(no_error(&answer(&[
            "csvm",
            "join pipe.csv on k | cols zz",
            "left.csv"
        ])));
    }

    #[test]
    fn a_header_is_read_again_only_when_its_file_changes() {
        let dir = TempDir::new("cache");
        let path = dir.write("data.csv", "aa,bb\n1,2\n");
        let mut headers = Headers::default();
        let names = |cols: &[&str]| Some(cols.iter().map(|c| c.to_string()).collect::<Vec<_>>());
        assert_eq!(
            headers.first_line(&path, InputFormat::Csv),
            names(&["aa", "bb"])
        );
        // Same size and time: the kept header, though the bytes differ.
        let time = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, "cc,dd\n1,2\n").unwrap();
        set_time(&path, time);
        assert_eq!(
            headers.first_line(&path, InputFormat::Csv),
            names(&["aa", "bb"])
        );
        // A new time: read again.
        set_time(&path, time + Duration::from_secs(1));
        assert_eq!(
            headers.first_line(&path, InputFormat::Csv),
            names(&["cc", "dd"])
        );
        // A new size, at the same time: read again.
        std::fs::write(&path, "eee,f\n").unwrap();
        set_time(&path, time + Duration::from_secs(1));
        assert_eq!(
            headers.first_line(&path, InputFormat::Csv),
            names(&["eee", "f"])
        );
        // A first line without a newline is still the header; one longer
        // than the limit is not read at all.
        let short = dir.write("short.csv", "x,y");
        assert_eq!(
            headers.first_line(&short, InputFormat::Csv),
            names(&["x", "y"])
        );
        let wide = dir.write("wide.csv", &"a".repeat(MAX_HEADER_BYTES as usize + 10));
        assert_eq!(headers.first_line(&wide, InputFormat::Csv), None);
        // That is kept too: while the file is unchanged it is not read
        // again, though its first line is now short.
        let time = std::fs::metadata(&wide).unwrap().modified().unwrap();
        let short_first = format!("x\n{}", "a".repeat(MAX_HEADER_BYTES as usize + 8));
        std::fs::write(&wide, short_first).unwrap();
        set_time(&wide, time);
        assert_eq!(headers.first_line(&wide, InputFormat::Csv), None);
        set_time(&wide, time + Duration::from_secs(1));
        assert_eq!(headers.first_line(&wide, InputFormat::Csv), names(&["x"]));
    }

    /// Set the modification time of the file at `path`.
    fn set_time(path: &Path, time: SystemTime) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn a_join_is_checked_against_its_right_file() {
        let dir = TempDir::new("join");
        dir.write("left.csv", "k,a\n1,2\n");
        dir.write("right.csv", "k,b\n1,3\n");
        let ask_in = |script: &str| {
            reply_lines(
                &Session::default().answer(&request(dir.path(), &["csvm", script, "left.csv"])),
            )
        };
        // A column the join brings in is there after it.
        assert!(no_error(&ask_in("join right.csv on k | cols b")));
        // One its sub-pipeline cannot find is the error, where it is named.
        assert_eq!(
            ask_in("join (cols zz) right.csv on k").last().unwrap(),
            ":error 1 11 13 column not found: zz — have: k, b"
        );
        // A join inside a join's sub-pipeline is resolved first.
        dir.write("inner.csv", "k,c\n1,4\n");
        assert!(no_error(&ask_in(
            "join (join inner.csv on k) right.csv on k | cols c"
        )));
        assert_eq!(
            ask_in("join (join inner.csv on k | cols zz) right.csv on k")
                .last()
                .unwrap(),
            ":error 1 33 35 column not found: zz — have: k, b, c"
        );
        // A right file that is not there is not a column error.
        assert!(no_error(&ask_in("join nope.csv on k | cols b")));
        // Nor is one whose first line is longer than the limit: it is not
        // read to its end.
        dir.write("wide.csv", &"k".repeat(MAX_HEADER_BYTES as usize + 10));
        assert!(no_error(&ask_in("join wide.csv on k | cols zz")));
    }

    #[test]
    fn a_join_file_is_read_again_only_when_it_changes() {
        let dir = TempDir::new("joincache");
        dir.write("left.csv", "k,a\n1,2\n");
        let right = dir.write("right.csv", "k,bb\n1,3\n");
        let mut session = Session::default();
        let mut ask_in = |script: &str| {
            reply_lines(&session.answer(&request(dir.path(), &["csvm", script, "left.csv"])))
        };
        let script = "join right.csv on k | cols bb";
        assert!(no_error(&ask_in(script)));
        // Same size and time: the kept header, though the bytes differ.
        let time = std::fs::metadata(&right).unwrap().modified().unwrap();
        std::fs::write(&right, "k,cc\n1,3\n").unwrap();
        set_time(&right, time);
        assert!(no_error(&ask_in(script)));
        // A new time: read again.
        set_time(&right, time + Duration::from_secs(1));
        assert_eq!(
            ask_in(script).last().unwrap(),
            ":error 1 27 29 column not found: bb — have: k, a, cc"
        );
    }

    #[test]
    fn a_join_file_that_cannot_be_read_is_not_read_again_while_unchanged() {
        let dir = TempDir::new("joinfail");
        dir.write("left.csv", "k,a\n1,2\n");
        let mut session = Session::default();
        let mut ask_in = |script: &str| {
            reply_lines(&session.answer(&request(dir.path(), &["csvm", script, "left.csv"])))
        };
        // A first line too long to read: no check, and while the file is
        // unchanged it is not read again, though its first line is now
        // short.
        let wide = dir.write("wide.csv", &"k".repeat(MAX_HEADER_BYTES as usize + 10));
        let script = "join wide.csv on k | cols zz";
        assert!(no_error(&ask_in(script)));
        let time = std::fs::metadata(&wide).unwrap().modified().unwrap();
        let short_first = format!("k\n{}", "k".repeat(MAX_HEADER_BYTES as usize + 8));
        std::fs::write(&wide, short_first).unwrap();
        set_time(&wide, time);
        assert!(no_error(&ask_in(script)));
        set_time(&wide, time + Duration::from_secs(1));
        assert_eq!(
            ask_in(script).last().unwrap(),
            ":error 1 26 28 column not found: zz — have: k, a"
        );
    }
}
