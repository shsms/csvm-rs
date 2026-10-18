//! `csvm --inkline-mode`: the mode server inkline asks to colour, indent
//! and complete a csvm command line while it is typed.
//!
//! `inkline_mode_protocol` reads the requests and writes the replies;
//! this module decides what csvm answers. A request's first argument is the
//! command's name as typed, which csvm skips. A colour reply's spans are
//! the script's parts as the parser reads them (`parse::Recorder`), a `|`
//! between two stages being a `separator`, and its error is the first
//! thing csvm would reject on the line. csvm names `indent` and `complete`
//! on its first line: an `:indent` request is answered from
//! `parse::depths`, and a `:complete` one from `complete::find`.

use crate::cli::{self, InputFormat, Parsed};
use crate::complete::{self, Found, Group};
use crate::error::Error;
use crate::exec;
use crate::parse::{self, Recorder};
use crate::pattern;
use crate::plan::{Plan, Stage};
use inkline_mode_protocol::{
    Depths, Item, MAX_ITEMS, Mode, Reply, ReplyError, Request, Span, items_fit,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// What the mode server keeps from one request to the next.
#[derive(Default)]
pub struct Session {
    headers: Headers,
}

impl Mode for Session {
    const INDENT: bool = true;
    const COMPLETE: bool = true;

    /// Answer a colour request: the script's colours, and the first thing
    /// csvm would reject on this command line.
    fn colors(&mut self, request: &Request) -> Reply {
        let mut reply = Reply::default();
        let words = match request.words() {
            Ok(words) => words,
            Err(place) => {
                // csvm cannot start with such an argument at all.
                reply.error = Some(ReplyError::new(
                    Some(place),
                    "this argument is not valid UTF-8",
                ));
                return reply;
            }
        };
        let args = match cli::parse_at(words) {
            Ok(Parsed::Run(args)) => args,
            // Help, the version and the mode server itself take no script.
            Ok(Parsed::Help { .. } | Parsed::Version | Parsed::ModeServer) => return reply,
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
        reply.error = error.map(|e| script_error(arg, &args.script, e));
        reply
    }

    fn indent(&mut self, request: &Request, at: (usize, usize)) -> Option<Depths> {
        indent(request, at)
    }

    /// The completion items at `at`, an argument and a byte offset in it: what
    /// fits where the cursor is in the script (see [`complete::find`]), each
    /// replacing the word the cursor is in. Commands and the script's `fn`
    /// names at the start of a stage, a command's flags, the columns at the
    /// cursor's stage, the functions in an expression, and the file names where
    /// a `join` or `from` item takes its file. None when `at` is not in the
    /// script argument, or csvm finds no script on the line (an argument is not
    /// UTF-8, the line is a usage error, help, or has `-f`), or the script is
    /// `raw`. When the columns cannot be known (no input file, a file that
    /// cannot be read, a `raw` argument, a stage before that does not resolve),
    /// the other items are still given. When a reply cannot hold every item
    /// (see [`inkline_mode_protocol::write_items`]), the ones that start with
    /// the typed part of the word, from its start to the cursor, come first.
    fn complete(&mut self, request: &Request, at: (usize, usize)) -> Vec<Item> {
        let Some(args) = script_args(request, at) else {
            return Vec::new();
        };
        let script = &args.script;
        // The columns and the files both read through the session's caches.
        let session = std::cell::RefCell::new(self);
        let mut columns = |found: &Found| session.borrow_mut().columns_at(request, &args, found);
        let mut files = |word: &Range<usize>, typed: &str| {
            // A closing quote, after an opening one.
            let closed = typed.starts_with('"') && script.as_bytes().get(word.end) == Some(&b'"');
            session
                .borrow_mut()
                .file_items(request, word, typed, closed)
        };
        complete::items_at(script, at.1, &mut columns, &mut files)
    }
}

impl Session {
    /// The items for a file whose path starts `typed`, replacing the bytes
    /// `at`: the entries of the directory `typed` names up to its last `/`
    /// (from the shell's directory), that start with the rest, in natural
    /// order. Hidden entries only when the rest starts with `.`; nothing
    /// for an unquoted pattern, which names its own files. After an opening
    /// `"`, every item is quoted; a quoted item is left open when `closed`:
    /// a closing quote already follows the word.
    fn file_items(
        &mut self,
        request: &Request,
        at: &Range<usize>,
        typed: &str,
        closed: bool,
    ) -> Vec<Item> {
        // A quoted path is taken literally; an unquoted pattern names its
        // own files.
        let (quoted, typed) = match typed.strip_prefix('"') {
            Some(rest) => (true, rest),
            None if pattern::is_pattern(typed) => return Vec::new(),
            None => (false, typed),
        };
        let (dir, prefix) = match typed.rfind('/') {
            Some(i) => typed.split_at(i + 1),
            None => ("", typed),
        };
        let listed = if dir.is_empty() { "." } else { dir };
        let Some(path) = from_dir(request.shell_dir(), Path::new(listed)) else {
            return Vec::new();
        };
        let Some(mut names) = self.headers.list(&path) else {
            return Vec::new();
        };
        names.retain(|n| n.starts_with(prefix) && (!n.starts_with('.') || prefix.starts_with('.')));
        // At most what a reply holds, before a look at each entry's type.
        names
            .iter()
            .filter_map(|n| complete::file_item(at, dir, n, path.join(n).is_dir(), quoted, closed))
            .take(MAX_ITEMS)
            .collect()
    }

    /// The columns at the cursor's stage, as `found` places it in the
    /// script of `args`: in the pipeline itself, the input's header (see
    /// [`Session::input_header`]); in a `join ( … )` group, the header of
    /// the file after the group, from the shell's directory; either one
    /// carried through the stages before the cursor's in its group. `None`
    /// when that header cannot be known, a stage before does not parse or
    /// resolve, or the cursor is in a group with no file yet or in an `fn`
    /// body.
    fn columns_at(
        &mut self,
        request: &Request,
        args: &cli::Args,
        found: &Found,
    ) -> Option<Vec<String>> {
        let cwd = request.shell_dir();
        let mut plan = if found.stages_before.is_empty() {
            None
        } else {
            Some(parse::parse(&found.stages_before).ok()?)
        };
        let header = match &found.group {
            Group::Top => match plan
                .as_mut()
                .and_then(|p| self.named_input_header(p, request, args))
            {
                Some(header) => header?,
                None => self.input_header(request, args)?,
            },
            Group::Join { file: Some(file) } => join_header(&mut self.headers, cwd, file)?,
            Group::Join { file: None } | Group::FnBody => return None,
        };
        let Some(mut plan) = plan else {
            return Some(header);
        };
        resolve_joins(&mut plan, cwd, &mut self.headers).ok()?;
        plan.resolve(&header).ok()
    }

    /// The error csvm would find resolving `plan` against the input's
    /// header. `None` when there is none, or when the header cannot be
    /// known here (see [`Session::input_header`]), or the same holds for a
    /// join's file.
    fn check_columns(
        &mut self,
        request: &Request,
        args: &cli::Args,
        plan: &mut Plan,
    ) -> Option<Error> {
        let cwd = request.shell_dir();
        let header = match self.named_input_header(plan, request, args) {
            Some(header) => header?,
            None => self.input_header(request, args)?,
        };
        if let Err(e) = resolve_joins(plan, cwd, &mut self.headers) {
            return e;
        }
        plan.resolve(&header).err()
    }

    /// When `plan` begins with `from`, turn it into its stages as csvm
    /// does and give the header csvm reads for its first file, with the
    /// other arguments as given (`Some(None)`: it cannot be known here).
    /// `None` when the plan has no `from`.
    fn named_input_header(
        &mut self,
        plan: &mut Plan,
        request: &Request,
        args: &cli::Args,
    ) -> Option<Option<Vec<String>>> {
        plan.from.as_ref()?;
        let mut fs = SessionFs {
            headers: &mut self.headers,
        };
        let Ok(Some(from)) = exec::prepare_from(plan, request.shell_dir(), &mut fs) else {
            return Some(None);
        };
        // Arguments csvm refuses to run have no header to check against.
        let Ok(args) = args.clone().with_from_input(from.path, from.several) else {
            return Some(None);
        };
        Some(self.input_header(request, &args))
    }

    /// The input's header, as csvm would read it for `args`: the file's
    /// first line, or the names `--header` gives. `None` when it cannot be
    /// known here: an argument is `raw`, or the input is stdin, a relative
    /// path with no shell directory to take it from, not a regular file, or
    /// cannot be read, or `--header` is given for Parquet.
    fn input_header(&mut self, request: &Request, args: &cli::Args) -> Option<Vec<String>> {
        // A `raw` argument anywhere may become other words, or none, and so
        // change the input, its header, or what csvm makes of the line.
        if request.args[1..].iter().any(|a| a.raw) {
            return None;
        }
        let path = args.in_path()?;
        let path = from_dir(request.shell_dir(), Path::new(path))?;
        let format = args.input_format();
        match (&args.header, format) {
            // csvm rejects `--header` for Parquet, which names its own columns.
            (Some(_), InputFormat::Parquet) => None,
            (Some(cli::Header::Named(names)), _) => {
                std::fs::metadata(&path).ok().filter(|m| m.is_file())?;
                Some(names.clone())
            }
            (spec, _) => {
                let first = self.headers.first_line(&path, format)?;
                Some(cli::Header::resolve(spec.as_ref(), first, 0).0)
            }
        }
    }
}

/// The answer to an `:indent` request: the [`Depths`] where `at`, its
/// `:at`, splits the script. `None`, for a reply with no `:depth`, when
/// csvm finds no script on the line (an argument is not UTF-8, the line is
/// a usage error, help, or has `-f`), when `at` is in another argument, or
/// when the script is `raw`: bash will still change it, so its text is not
/// what csvm gets. Unlike a colour request, a script that does not parse
/// still gets depths.
pub fn indent(request: &Request, at: (usize, usize)) -> Option<Depths> {
    script_args(request, at).map(|args| parse::depths(&args.script, at.1))
}

/// What csvm makes of `request`'s command line, when `at`, its `:at`
/// place, is in the script argument. `None` when csvm finds no script on
/// the line (an argument is not UTF-8, the line is a usage error, help, or
/// has `-f`), when `at` is in another argument, or when the script is
/// `raw`.
fn script_args(request: &Request, at: (usize, usize)) -> Option<Box<cli::Args>> {
    let words = request.words().ok()?;
    let Ok(Parsed::Run(args)) = cli::parse_at(words) else {
        return None;
    };
    let arg = args.script_at? + 1;
    (at.0 == arg && !request.args[arg].raw).then_some(args)
}

/// `e`, found in the script, which is argument `arg`, as a reply's error:
/// on its place in the script when it has one, noting a `|` that may be
/// missing (`parse::note_missing_pipe`).
fn script_error(arg: usize, script: &str, e: Error) -> ReplyError {
    let (at, message) = placed(script, e);
    ReplyError {
        place: at.map(|at| (arg, at)),
        message,
    }
}

/// An error in `script` as an editor shows it: its bytes, kept inside the
/// script, and its message on one line, with the missing-`|` note.
pub(crate) fn placed(script: &str, e: Error) -> (Option<Range<usize>>, String) {
    let e = parse::note_missing_pipe(script, e);
    let at = e.span().map(|at| {
        let end = at.end.min(script.len());
        at.start.min(end)..end
    });
    // The protocol's error message rule makes the message one line.
    (at, ReplyError::new(None, &e.to_string()).message)
}

/// `path` as the shell sees it from its directory `cwd`. `None` for a
/// relative path when that directory is not known: the mode server's own
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
    exec::expand_joins(plan, cwd, &mut SessionFs { headers }).map_err(|_| None)?;
    for stage in &mut plan.stages {
        if let Stage::Join(j) = stage {
            resolve_joins(&mut j.right_plan, cwd, headers)?;
            // Expanded above: the file is the one named, never a pattern.
            let header = file_header(headers, cwd, &j.file).ok_or(None)?;
            j.right_header = j.right_plan.resolve(&header).map_err(Some)?;
        }
    }
    Ok(())
}

/// The header of a join's right file, `file` from `cwd`. `None` when it
/// cannot be known (see [`from_dir`] and [`Headers::first_line`]).
fn join_header(headers: &mut Headers, cwd: Option<&Path>, file: &str) -> Option<Vec<String>> {
    // A pattern's columns are taken from its first file.
    let files = pattern::expand(file, cwd, &mut SessionFs { headers }).ok()?;
    file_header(headers, cwd, &files.into_iter().next()?.path)
}

/// The header of the CSV file `file`, from `cwd`.
fn file_header(headers: &mut Headers, cwd: Option<&Path>, file: &str) -> Option<Vec<String>> {
    // A join's right file is always CSV.
    headers.first_line(&from_dir(cwd, Path::new(file))?, InputFormat::Csv)
}

/// How many files [`Headers`] keeps; past that it starts again from none.
const MAX_FILES: usize = 64;

/// The most bytes read looking for the end of a CSV file's first line.
pub(crate) const MAX_HEADER_BYTES: u64 = 1 << 20;

/// The first line of each file read so far, by path and format. An entry
/// is used again only while the file's size and modification time are
/// what they were when it was read, so an unchanged file costs one look at
/// its size and time per request. A read that failed is kept too, so it is
/// not tried again until the file changes.
#[derive(Default)]
pub(crate) struct Headers {
    files: HashMap<(PathBuf, InputFormat), Seen>,
    /// Each directory a pattern was matched in: its names, and its time
    /// when they were listed.
    dirs: HashMap<PathBuf, (Option<SystemTime>, Vec<String>)>,
}

/// The filesystem as the mode server sees it: listings cached in
/// [`Headers`], and no relative path (with no shell directory to take it
/// from, it cannot be known here).
struct SessionFs<'a> {
    headers: &'a mut Headers,
}

impl pattern::Fs for SessionFs<'_> {
    fn list(&mut self, dir: &Path) -> Option<Vec<String>> {
        self.headers.list(dir)
    }

    fn is_file(&mut self, path: &Path) -> bool {
        path.is_absolute() && std::fs::metadata(path).is_ok_and(|m| m.is_file())
    }
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
    pub(crate) fn first_line(&mut self, path: &Path, format: InputFormat) -> Option<Vec<String>> {
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

    /// The names in directory `dir`, in natural order, listed again only
    /// when its time changes. `None` when it is relative or cannot be
    /// listed.
    fn list(&mut self, dir: &Path) -> Option<Vec<String>> {
        if dir.is_relative() {
            return None;
        }
        let modified = std::fs::metadata(dir).ok()?.modified().ok();
        if let Some((seen, names)) = self.dirs.get(dir)
            && *seen == modified
        {
            return Some(names.clone());
        }
        let mut names = pattern::Fs::list(&mut pattern::RealFs, dir)?;
        names.sort_by(|a, b| pattern::natural_cmp(a, b));
        if self.dirs.len() >= MAX_FILES {
            self.dirs.clear();
        }
        self.dirs
            .insert(dir.to_path_buf(), (modified, names.clone()));
        Some(names)
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

/// `items`, with the ones whose text starts with `typed` first when a reply
/// cannot hold them all, so that the ones cut are the others. A backtick or
/// double quote at the start of either is not compared. The order is kept
/// otherwise.
pub(crate) fn typed_first(items: Vec<Item>, typed: &str) -> Vec<Item> {
    if items_fit(&items) {
        return items;
    }
    fn unquoted(text: &str) -> &str {
        text.strip_prefix(['`', '"']).unwrap_or(text)
    }
    let typed = unquoted(typed);
    let (mut first, rest): (Vec<Item>, Vec<Item>) = items
        .into_iter()
        .partition(|item| unquoted(&item.text).starts_with(typed));
    first.extend(rest);
    first
}

/// Be csvm's mode server: [`inkline_mode_protocol::serve`] with a new
/// [`Session`], until `input` ends.
pub fn serve(input: &mut impl BufRead, output: &mut impl Write) -> io::Result<()> {
    inkline_mode_protocol::serve(input, output, &mut Session::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use inkline_mode_protocol::{Arg, Ask, MAX_ITEM_BYTES, write_items, write_reply};

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
            ask: Ask::Colors,
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
        reply_lines(&Session::default().colors(&request("/", args)))
    }

    /// [`ask`], with the arguments at `raw` sent as `raw`.
    fn ask_raw(args: &[&str], raw: &[usize]) -> Vec<String> {
        let mut req = request("/", args);
        for &i in raw {
            req.args[i].raw = true;
        }
        reply_lines(&Session::default().colors(&req))
    }

    #[test]
    fn the_script_is_coloured_where_it_is_on_the_line() {
        assert_eq!(
            ask(&["csvm", "-n", "2", "cols a | head 3"]),
            [
                ":span 3 0 4 command",
                ":span 3 5 6 variable",
                ":span 3 7 8 separator",
                ":span 3 9 13 command",
                ":span 3 14 15 number",
            ]
        );
    }

    #[test]
    fn a_multi_line_script_is_coloured_on_every_line() {
        assert_eq!(
            ask(&["csvm", "cols a\n| select a > 1"]),
            [
                ":span 1 0 4 command",
                ":span 1 5 6 variable",
                ":span 1 7 8 separator",
                ":span 1 9 15 command",
                ":span 1 16 17 variable",
                ":span 1 18 19 operator",
                ":span 1 20 21 number",
            ]
        );
    }

    #[test]
    fn a_missing_pipe_is_noted_in_the_error() {
        assert_eq!(
            ask(&["csvm", "select a > 1\nhead"]).last().unwrap(),
            ":error 1 13 17 missing `|` before `head`? \
             unexpected 'head' after the expression"
        );
        // A row count quoted over two lines still sends the whole note.
        assert_eq!(
            ask(&["csvm", "head 5\nfmt"]).last().unwrap(),
            ":error 1 0 10 missing `|` before `fmt`? \
             head expects a row count, got '5 fmt'"
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
                ":span 3 7 8 separator",
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
    fn an_inkline_mode_command_line_is_not_a_script() {
        // `csvm --inkline-mode` alone starts the mode server: there is no
        // script.
        assert_eq!(ask(&["csvm", "--inkline-mode"]), Vec::<String>::new());
        // With anything else it is a usage error, on `--inkline-mode`.
        assert_eq!(
            ask(&["csvm", "--inkline-mode", "cols a"]),
            [":error 1 0 14 --inkline-mode takes no other arguments"]
        );
        assert_eq!(
            ask(&["csvm", "cols a", "data.csv", "--inkline-mode"]),
            [":error 3 0 14 --inkline-mode takes no other arguments"]
        );
        // A raw argument may expand to nothing and leave `--inkline-mode`
        // alone, wherever it is on the line.
        assert_eq!(
            ask_raw(&["csvm", "--inkline-mode", "$EMPTY"], &[2]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_command_name_is_not_used() {
        // inkline sends the command's name as typed: an alias, or a path to
        // csvm. Only the arguments after it are csvm's.
        let want = ask(&["csvm", "select a >> 1"]);
        assert!(!want.is_empty());
        for name in [
            "c",
            "./target/debug/csvm",
            "/usr/local/bin/csvm",
            "--inkline-mode",
        ] {
            assert_eq!(ask(&[name, "select a >> 1"]), want, "{name}");
        }
        let mut req = request("/", &["csvm", "select a >> 1"]);
        req.args[0].bytes = b"c\xff".to_vec();
        assert_eq!(reply_lines(&Session::default().colors(&req)), want);
    }

    #[test]
    fn an_argument_that_is_not_utf8_is_an_error_at_its_first_bad_byte() {
        let mut req = request("/", &["csvm", "cols a"]);
        req.args[1].bytes = b"cols \xff a".to_vec();
        assert_eq!(
            reply_lines(&Session::default().colors(&req)),
            [":error 1 5 6 this argument is not valid UTF-8"]
        );
        // Cut off inside a character: up to the end.
        req.args[1].bytes = b"cols \xc3".to_vec();
        assert_eq!(
            reply_lines(&Session::default().colors(&req)),
            [":error 1 5 6 this argument is not valid UTF-8"]
        );
    }

    #[test]
    fn an_empty_cwd_is_answered_like_any_other() {
        assert_eq!(
            reply_lines(&Session::default().colors(&request("", &["csvm", "cols a"]))),
            [":span 1 0 4 command", ":span 1 5 6 variable"]
        );
    }

    #[test]
    fn a_reply_to_a_very_long_script_stays_under_inklines_limit() {
        // Over 3 MiB of spans in all, and an error at the very end.
        let script = format!("cols {}| selct", "a ".repeat(100_000));
        let reply = Session::default().colors(&request("/", &["csvm", &script]));
        // `cols`, each `a` and the `|`.
        assert_eq!(reply.spans.len(), 100_002);
        let mut out = Vec::new();
        write_reply(&mut out, 1, &reply).unwrap();
        assert!(out.len() < 1 << 20, "{} bytes", out.len());
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
            let dir = std::env::temp_dir()
                .join(format!("csvm_mode_server_{}_{name}", std::process::id()));
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
        let mut ask_in = |args: &[&str]| reply_lines(&session.colors(&request(dir.path(), args)));
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
            |args: &[&str]| reply_lines(&Session::default().colors(&request(dir.path(), args)));
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
        assert!(no_error(&reply_lines(&Session::default().colors(&req))));
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
            assert!(no_error(&reply_lines(&Session::default().colors(&req))));
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
            reply_lines(&Session::default().colors(&req))
        };
        // bash sends an empty `:cwd` when `PWD` is unset. A relative path
        // is then not looked up at all, and never from the mode server's own
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
                let _ = tx.send(reply_lines(&Session::default().colors(&req)));
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
                &Session::default().colors(&request(dir.path(), &["csvm", script, "left.csv"])),
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
    fn a_from_script_is_checked_against_its_files() {
        let dir = TempDir::new("from");
        dir.write("pv.1.csv", "ts,v\n1,2\n");
        dir.write("pv.2.csv", "ts,v\n1,3\n");
        let ask_in = |script: &str| {
            reply_lines(&Session::default().colors(&request(dir.path(), &["csvm", script])))
        };
        assert!(no_error(&ask_in("from pv.{1,2}.csv on ts | cols v_1, v_2")));
        let last = ask_in("from pv.{1,2}.csv on ts | cols zz")
            .last()
            .unwrap()
            .clone();
        assert!(
            last.contains("column not found: zz — have: ts, v_1, v_2"),
            "{last}"
        );
        // A pattern that matches nothing is not a column error here.
        assert!(no_error(&ask_in("from q.*.csv on ts | cols zz")));
        // `--header` names the first file's columns, as for an input file.
        let named = reply_lines(&Session::default().colors(&request(
            dir.path(),
            &["csvm", "--header", "x,y", "from pv.1.csv | cols x"],
        )));
        assert!(no_error(&named), "{named:?}");
        // csvm refuses `--header` when from joins several files, so there is
        // no header to check against.
        let refused = reply_lines(&Session::default().colors(&request(
            dir.path(),
            &[
                "csvm",
                "--header",
                "x,y",
                "from pv.{1,2}.csv on x | cols zz",
            ],
        )));
        assert!(no_error(&refused), "{refused:?}");
    }

    #[test]
    fn a_join_pattern_is_checked_against_its_files() {
        let dir = TempDir::new("joinpat");
        dir.write("left.csv", "ts,a\n1,2\n");
        dir.write("r.x.csv", "ts,b\n1,3\n");
        dir.write("r.y.csv", "ts,b\n1,4\n");
        let ask_in = |script: &str| {
            reply_lines(
                &Session::default().colors(&request(dir.path(), &["csvm", script, "left.csv"])),
            )
        };
        assert!(no_error(&ask_in("join r.*.csv on ts | cols b_x, b_y")));
        // A quoted path is that one file, `?` and all.
        dir.write("r1.csv", "ts,b\n1,5\n");
        dir.write("r?.csv", "ts,q\n1,6\n");
        assert!(no_error(&ask_in("join \"r?.csv\" on ts | cols q")));
        let last = ask_in("join r.*.csv on ts | cols zz")
            .last()
            .unwrap()
            .clone();
        assert!(
            last.contains("column not found: zz — have: ts, a, b_x, b_y"),
            "{last}"
        );
    }

    #[test]
    fn a_join_file_is_read_again_only_when_it_changes() {
        let dir = TempDir::new("joincache");
        dir.write("left.csv", "k,a\n1,2\n");
        let right = dir.write("right.csv", "k,bb\n1,3\n");
        let mut session = Session::default();
        let mut ask_in = |script: &str| {
            reply_lines(&session.colors(&request(dir.path(), &["csvm", script, "left.csv"])))
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
            reply_lines(&session.colors(&request(dir.path(), &["csvm", script, "left.csv"])))
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

    #[test]
    fn serve_greets_then_answers_each_request_in_order() {
        let input = b":request 1\n:cwd 1\n/\n:arg final 4\ncsvm\n:arg final 4\nfmt \n:done\n\
                      :request 2\n:cwd 1\n/\n:arg final 4\ncsvm\n:arg final 6\n--colr\n:done\n";
        let mut out = Vec::new();
        serve(&mut &input[..], &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "inkline-mode 1 indent complete\n\
             :span 1 0 3 command\n:end 1\n\
             :error 1 0 6 unknown option: --colr\n:end 2\n"
        );
    }

    #[test]
    fn the_script_is_split_where_it_is_on_the_line() {
        let script = "head\n| join (\n  cols a";
        assert_eq!(
            indent(
                &request("/", &["csvm", "-n", "2", script, "x.csv"]),
                (3, script.len())
            ),
            Some(Depths {
                new: 1,
                current: None
            })
        );
        let script = "head\n| join (";
        assert_eq!(
            indent(&request("/", &["csvm", script]), (1, script.len())),
            Some(Depths {
                new: 1,
                current: None
            })
        );
        // Offsets count bytes, as for colours.
        let script = "select a == 'é' | join (\n  cols b";
        assert_eq!(
            indent(&request("/", &["csvm", script]), (1, script.len())),
            Some(Depths {
                new: 1,
                current: None
            })
        );
        // A script that does not parse still gets depths from its brackets.
        let script = "selct a\nfn f() {";
        assert_eq!(
            indent(&request("/", &["csvm", script]), (1, script.len())),
            Some(Depths {
                new: 1,
                current: None
            })
        );
    }

    #[test]
    fn no_depths_without_a_script_to_split() {
        // The split is in another argument than the script.
        assert_eq!(
            indent(&request("/", &["csvm", "join (", "x.csv"]), (2, 1)),
            None
        );
        assert_eq!(indent(&request("/", &["csvm", "join ("]), (0, 2)), None);
        // No script on the line: `-f`, help, a usage error, nothing yet.
        assert_eq!(
            indent(&request("/", &["csvm", "-f", "p.csvm", "x"]), (3, 1)),
            None
        );
        assert_eq!(
            indent(&request("/", &["csvm", "help", "join"]), (2, 1)),
            None
        );
        assert_eq!(
            indent(&request("/", &["csvm", "--colr", "join ("]), (2, 6)),
            None
        );
        assert_eq!(indent(&request("/", &["csvm"]), (0, 4)), None);
        // An argument that is not UTF-8.
        let mut req = request("/", &["csvm", "join ("]);
        req.args[1].bytes = b"join (\xff".to_vec();
        assert_eq!(indent(&req, (1, 6)), None);
        // A raw script is not the text csvm gets.
        let mut req = request("/", &["csvm", "\"join (\n\""]);
        req.args[1].raw = true;
        assert_eq!(indent(&req, (1, 8)), None);
        // A raw argument elsewhere does not stop it, as for colours.
        let mut req = request("/", &["csvm", "join (", "$f"]);
        req.args[2].raw = true;
        assert_eq!(
            indent(&req, (1, 6)),
            Some(Depths {
                new: 1,
                current: None
            })
        );
    }

    #[test]
    fn serve_answers_colour_and_indent_requests_in_order() {
        let input = b":request 1\n:cwd 1\n/\n:arg final 4\ncsvm\n:arg final 4\nfmt \n:done\n\
                      :indent 2\n:cwd 1\n/\n:arg final 4\ncsvm\n:arg final 6\njoin (\n:at 1 6\n:done\n\
                      :indent 3\n:cwd 1\n/\n:arg final 4\ncsvm\n:arg final 6\njoin (\n:at 0 4\n:done\n";
        let mut out = Vec::new();
        serve(&mut &input[..], &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "inkline-mode 1 indent complete\n\
             :span 1 0 3 command\n:end 1\n\
             :depth 1 -\n:end 2\n\
             :end 3\n"
        );
    }

    /// The `:item` texts of the reply to a `:complete` request for the
    /// script `script` with the cursor at its end, on `data.csv` in `dir`,
    /// and the reply's length in bytes.
    fn reply_items(dir: &TempDir, script: &str) -> (Vec<String>, usize) {
        let mut session = Session::default();
        let req = request(dir.path(), &["csvm", script, "data.csv"]);
        let mut out = Vec::new();
        write_items(&mut out, 1, &session.complete(&req, (1, script.len()))).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.ends_with(":end 1\n"));
        let texts = out
            .lines()
            .filter_map(|l| l.strip_prefix(":item "))
            .map(|l| l.splitn(3, ' ').nth(2).unwrap().to_string())
            .collect();
        (texts, out.len())
    }

    #[test]
    fn a_very_wide_header_keeps_the_reply_small_and_the_typed_columns() {
        let dir = TempDir::new("complete-wide");
        // 40,000 columns, the ones that start with `zz` last.
        let mut names: Vec<String> = (0..39_990).map(|i| format!("column_{i:05}")).collect();
        names.extend((0..10).map(|i| format!("zz {i}")));
        dir.write("data.csv", &format!("{}\n", names.join(",")));
        let (texts, len) = reply_items(&dir, "sort zz");
        assert!(len < 1 << 20, "{len}");
        assert_eq!(texts.len(), MAX_ITEMS);
        let zz: Vec<String> = (0..10).map(|i| format!("`zz {i}`")).collect();
        assert_eq!(texts[..10], zz);
        assert_eq!(texts[10], "column_00000");
        // Typed with its backtick, too.
        let (texts, _) = reply_items(&dir, "sort `zz");
        assert_eq!(texts[..10], zz);
        // Nothing typed: the first columns, in order.
        let (texts, _) = reply_items(&dir, "sort ");
        assert_eq!(texts.len(), MAX_ITEMS);
        assert_eq!(texts[..2], ["column_00000", "column_00001"]);
    }

    #[test]
    fn more_columns_than_a_reply_holds_keep_the_typed_ones() {
        let dir = TempDir::new("complete-many");
        // 1,100 short columns, the ones that start with `zz` last: more
        // items than a reply holds, but far fewer bytes.
        let mut names: Vec<String> = (0..1_090).map(|i| format!("c{i:04}")).collect();
        names.extend((0..10).map(|i| format!("zz{i}")));
        dir.write("data.csv", &format!("{}\n", names.join(",")));
        let (texts, len) = reply_items(&dir, "sort zz");
        assert!(len < MAX_ITEM_BYTES / 10, "{len}");
        assert_eq!(texts.len(), MAX_ITEMS);
        assert_eq!(texts[..10], names[1_090..]);
        assert_eq!(texts[10], "c0000");
    }

    #[test]
    fn long_column_names_keep_the_reply_small_and_the_typed_columns() {
        let dir = TempDir::new("complete-long");
        // 250 names of 4000 bytes: more item bytes than a reply holds.
        let mut names: Vec<String> = (0..248)
            .map(|i| format!("c{i:03}{}", "x".repeat(3996)))
            .collect();
        names.extend((0..2).map(|i| format!("zz{i}{}", "x".repeat(3997))));
        dir.write("data.csv", &format!("{}\n", names.join(",")));
        let (texts, len) = reply_items(&dir, "sort zz");
        assert!(len <= MAX_ITEM_BYTES + 100, "{len}");
        assert!(texts.len() < 250);
        assert_eq!(texts[..2], names[248..]);
        assert_eq!(texts[2], names[0]);
    }

    #[test]
    fn the_greeting_names_complete() {
        let mut out = Vec::new();
        serve(&mut "".as_bytes(), &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "inkline-mode 1 indent complete\n"
        );
    }

    #[test]
    fn a_complete_request_is_answered_in_turn() {
        let input = ":complete 1\n:cwd 1\n/\n:arg final 4\ncsvm\n:arg final 1\nx\n:at 1 1\n:done\n\
                     :request 2\n:cwd 1\n/\n:arg final 4\ncsvm\n:done\n";
        let mut out = Vec::new();
        serve(&mut input.as_bytes(), &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        let ends: Vec<&str> = out.lines().filter(|l| l.starts_with(":end")).collect();
        assert_eq!(ends, [":end 1", ":end 2"]);
    }

    /// The items for the command line `args` (after `csvm`), run from
    /// `dir`, with the argument `SCRIPT` replaced by `marked` less its `@`,
    /// which marks the cursor, as (text, note) pairs.
    fn complete_in(dir: &TempDir, args: &[&str], marked: &str) -> Vec<(String, Option<String>)> {
        let at = marked.find('@').unwrap();
        let script = marked.replacen('@', "", 1);
        let pos = args.iter().position(|a| *a == "SCRIPT").unwrap();
        let mut all = vec!["csvm"];
        all.extend(
            args.iter()
                .map(|a| if *a == "SCRIPT" { script.as_str() } else { a }),
        );
        let mut session = Session::default();
        let req = request(dir.path(), &all);
        session
            .complete(&req, (pos + 1, at))
            .into_iter()
            .map(|i| (i.text, i.note))
            .collect()
    }

    fn texts(items: &[(String, Option<String>)]) -> Vec<&str> {
        items.iter().map(|(t, _)| t.as_str()).collect()
    }

    #[test]
    fn a_stage_start_offers_commands_and_fns() {
        let dir = TempDir::new("complete-commands");
        let items = complete_in(&dir, &["SCRIPT"], "fn prep(n) { cols n }\nsort a | @");
        assert!(items.contains(&("sort".into(), Some("stable multi-key sort".into()))));
        assert!(items.contains(&("prep".into(), Some("fn".into()))));
    }

    #[test]
    fn a_flag_place_offers_that_commands_flags() {
        let dir = TempDir::new("complete-flags");
        let items = complete_in(&dir, &["SCRIPT"], "fmt -@");
        assert!(texts(&items).contains(&"-s"), "{items:?}");
        assert!(items.iter().all(|(t, n)| t.starts_with('-') && n.is_some()));
        // `colour` takes `color`'s flags.
        let items = complete_in(&dir, &["SCRIPT"], "colour -@");
        assert!(texts(&items).contains(&"-c"), "{items:?}");
    }

    #[test]
    fn columns_follow_the_stages_before() {
        let dir = TempDir::new("complete-columns");
        dir.write("data.csv", "id,amount,first name\n1,2,x\n");
        let items = complete_in(
            &dir,
            &["SCRIPT", "data.csv"],
            "rename id=key | add t = 1 | sort @",
        );
        assert_eq!(texts(&items), ["key", "amount", "`first name`", "t"]);
        assert!(items.iter().all(|(_, n)| n.as_deref() == Some("column")));
    }

    #[test]
    fn columns_follow_a_fragment_call_and_a_join_before() {
        let dir = TempDir::new("complete-fragment");
        dir.write("data.csv", "id,a,b\n");
        dir.write("right.csv", "id,c\n");
        let items = complete_in(
            &dir,
            &["SCRIPT", "data.csv"],
            "fn keep(n) { cols id, n }\nkeep(b) | sort @",
        );
        assert_eq!(texts(&items), ["id", "b"]);
        let items = complete_in(
            &dir,
            &["SCRIPT", "data.csv"],
            "join right.csv on id | sort @",
        );
        assert_eq!(texts(&items), ["id", "a", "b", "c"]);
    }

    #[test]
    fn an_expression_offers_columns_then_functions() {
        let dir = TempDir::new("complete-expr");
        dir.write("data.csv", "id,amount\n");
        let items = complete_in(&dir, &["SCRIPT", "data.csv"], "select am@");
        let t = texts(&items);
        assert_eq!(&t[..2], ["id", "amount"]);
        assert!(t.contains(&"len") && t.contains(&"rownum"), "{t:?}");
        assert!(t.contains(&"prev"), "{t:?}");
        assert!(
            items[2..]
                .iter()
                .all(|(_, n)| n.as_deref() == Some("function"))
        );
    }

    #[test]
    fn a_join_group_takes_its_own_files_columns() {
        let dir = TempDir::new("complete-join");
        dir.write("left.csv", "id,a\n");
        dir.write("right.csv", "id,b,c\n");
        let items = complete_in(
            &dir,
            &["SCRIPT", "left.csv"],
            "join (cols -v c | sort @) right.csv on id",
        );
        assert_eq!(texts(&items), ["id", "b"]);
        // No file after the group yet: no columns.
        let items = complete_in(&dir, &["SCRIPT", "left.csv"], "join (sort @");
        assert!(items.is_empty(), "{items:?}");
    }

    #[test]
    fn a_from_script_completes_its_files_columns() {
        let dir = TempDir::new("complete-from");
        dir.write("pv.1.csv", "ts,v\n");
        dir.write("pv.2.csv", "ts,v\n");
        let items = complete_in(&dir, &["SCRIPT"], "from pv.{1,2}.csv on ts | sort @");
        assert_eq!(texts(&items), ["ts", "v_1", "v_2"]);
        // In a group whose file is a pattern, its first file's columns.
        let items = complete_in(&dir, &["SCRIPT"], "from (sort @) pv.*.csv");
        assert_eq!(texts(&items), ["ts", "v"]);
    }

    #[test]
    fn a_file_slot_completes_file_names() {
        let dir = TempDir::new("complete-files");
        for name in [
            "pv.1.csv",
            "pv.10.csv",
            "pv.9.csv",
            ".hidden.csv",
            "my file.csv",
        ] {
            dir.write(name, "");
        }
        std::fs::create_dir(Path::new(dir.path()).join("sub")).unwrap();
        dir.write("sub/a.csv", "");
        let items = complete_in(&dir, &["SCRIPT"], "join pv.@");
        assert_eq!(texts(&items), ["pv.1.csv", "pv.9.csv", "pv.10.csv"]);
        assert_eq!(items[0].1.as_deref(), Some("file"));
        // Quoted when it needs to be; a directory ends in `/`; no hidden.
        let items = complete_in(&dir, &["SCRIPT"], "from @");
        assert_eq!(
            texts(&items),
            [
                "\"my file.csv\"",
                "pv.1.csv",
                "pv.9.csv",
                "pv.10.csv",
                "sub/"
            ]
        );
        assert_eq!(items[4].1.as_deref(), Some("directory"));
        assert_eq!(
            texts(&complete_in(&dir, &["SCRIPT"], "join sub/@")),
            ["sub/a.csv"]
        );
        assert_eq!(
            texts(&complete_in(&dir, &["SCRIPT"], "join .h@")),
            [".hidden.csv"]
        );
        // A pattern is left as it is.
        dir.write("pv.{1}.csv", "");
        assert!(complete_in(&dir, &["SCRIPT"], "join pv.{@").is_empty());
        // Inside a quoted path, and on into a directory that needs quotes.
        std::fs::create_dir(Path::new(dir.path()).join("my dir")).unwrap();
        dir.write("my dir/a.csv", "");
        assert_eq!(
            texts(&complete_in(&dir, &["SCRIPT"], "join \"my f@")),
            ["\"my file.csv\""]
        );
        assert_eq!(
            texts(&complete_in(&dir, &["SCRIPT"], "join \"my dir/@")),
            ["\"my dir/a.csv\""]
        );
        // A plain name is quoted too after an opening quote, and a closing
        // quote already there is not written again.
        assert_eq!(
            texts(&complete_in(&dir, &["SCRIPT"], "join \"pv.1@")),
            ["\"pv.1.csv\"", "\"pv.10.csv\""]
        );
        assert_eq!(
            texts(&complete_in(&dir, &["SCRIPT"], "join \"my d@\" on k")),
            ["\"my dir/"]
        );
        assert_eq!(
            texts(&complete_in(&dir, &["SCRIPT"], "join \"my dir/@\" on k")),
            ["\"my dir/a.csv"]
        );
        // A quote glued after an unquoted word does not close it.
        assert_eq!(
            texts(&complete_in(&dir, &["SCRIPT"], "join my@\"x\" on k")),
            ["\"my dir/", "\"my file.csv\""]
        );
    }

    #[test]
    fn the_input_header_follows_the_header_flag() {
        let dir = TempDir::new("complete-header");
        dir.write("data.csv", "1,2\n");
        let items = complete_in(&dir, &["--header", "x,y", "SCRIPT", "data.csv"], "sort @");
        assert_eq!(texts(&items), ["x", "y"]);
        let items = complete_in(&dir, &["--header=", "SCRIPT", "data.csv"], "sort @");
        assert_eq!(texts(&items), ["c1", "c2"]);
    }

    #[test]
    fn unknown_columns_leave_the_other_items() {
        let dir = TempDir::new("complete-unknown");
        // No file on the line: no columns, but functions still.
        let items = complete_in(&dir, &["SCRIPT"], "select am@");
        assert!(texts(&items).contains(&"len"));
        assert!(!items.iter().any(|(_, n)| n.as_deref() == Some("column")));
        // A file that is not there.
        let items = complete_in(&dir, &["SCRIPT", "none.csv"], "sort @");
        assert!(items.is_empty());
        // A stage before that does not resolve, or does not parse.
        dir.write("data.csv", "id\n");
        let items = complete_in(&dir, &["SCRIPT", "data.csv"], "cols nope | sort @");
        assert!(items.is_empty());
        let items = complete_in(&dir, &["SCRIPT", "data.csv"], "head -n x | sort @");
        assert!(items.is_empty());
    }

    #[test]
    fn a_raw_argument_hides_the_input_columns() {
        let dir = TempDir::new("complete-raw");
        dir.write("data.csv", "id\n");
        let mut session = Session::default();
        let mut req = request(dir.path(), &["csvm", "sort ", "data.csv"]);
        assert_eq!(session.complete(&req, (1, 5)).len(), 1);
        req.args[2].raw = true;
        assert!(session.complete(&req, (1, 5)).is_empty());
    }

    #[test]
    fn nothing_for_other_arguments_or_a_raw_script() {
        let dir = TempDir::new("complete-other");
        let mut session = Session::default();
        let req = request(dir.path(), &["csvm", "sort a", "data.csv"]);
        assert!(
            session.complete(&req, (2, 4)).is_empty(),
            "the file argument"
        );
        let mut req = request(dir.path(), &["csvm", "sort $x", "data.csv"]);
        req.args[1].raw = true;
        assert!(session.complete(&req, (1, 7)).is_empty(), "a raw script");
        let req = request(dir.path(), &["csvm", "-f", "s.csvm", "data.csv"]);
        assert!(session.complete(&req, (2, 0)).is_empty(), "a script file");
        let req = request(dir.path(), &["csvm", "--help", "sort"]);
        assert!(session.complete(&req, (2, 0)).is_empty(), "help");
        let req = request(dir.path(), &["csvm", "--nope", "sort"]);
        assert!(session.complete(&req, (2, 0)).is_empty(), "a usage error");
        let mut req = request(dir.path(), &["csvm", "so"]);
        req.args[1].bytes.push(0xff);
        assert!(session.complete(&req, (1, 2)).is_empty(), "not UTF-8");
    }

    #[test]
    fn nothing_is_glued_onto_a_closed_name_string_or_call() {
        let dir = TempDir::new("complete-glued");
        dir.write("data.csv", "id,amount,first name\n");
        for marked in [
            "cols id,`first name`@",
            "select len(amount)@",
            "select id == \"x\"@",
        ] {
            let items = complete_in(&dir, &["SCRIPT", "data.csv"], marked);
            assert!(items.is_empty(), "{marked}: {items:?}");
        }
    }

    #[test]
    fn items_replace_the_word_at_the_cursor() {
        let dir = TempDir::new("complete-word");
        let mut session = Session::default();
        let req = request(dir.path(), &["csvm", "sort id | he"]);
        let items = session.complete(&req, (1, 12));
        assert!(!items.is_empty());
        assert!(items.iter().all(|i| i.at == (10..12)));
    }
}
