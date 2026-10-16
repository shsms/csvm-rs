//! Running a script from another program: [`run`] takes a script and an input
//! file, runs it in-process as the CLI would, and returns the output as typed
//! columns instead of CSV text.

use std::borrow::Cow;
use std::ops::Range;
use std::path::Path;

use crate::color::Style;
use crate::error::Error;
use crate::exec::{self, RunOpts};
use crate::mode_server::placed;
use crate::plan::{OutputFormat, Plan, Sources, Stage};
use crate::progress::Progress;
use crate::{cli, complete, csv, field, parse, pattern};
use inkline_mode_protocol::{Item, SpanKind};

/// What a script made of its input.
#[derive(Debug)]
pub struct Output {
    pub columns: Vec<Column>,
    /// The cells the script's `color` rules paint, row by row.
    pub paints: Vec<Paint>,
    /// One line per thing the run ignored (a `graph` sink).
    pub notices: Vec<String>,
}

/// One output column.
#[derive(Debug)]
pub struct Column {
    pub name: String,
    pub values: Values,
}

/// One painted cell. `row` counts the data rows from 0, the header not counted;
/// a rule that paints a whole row paints each of its cells.
#[derive(Debug)]
pub struct Paint {
    pub row: usize,
    pub col: usize,
    pub style: Style,
}

#[derive(Debug)]
pub enum Values {
    /// Every non-empty cell parsed as a number; empty cells are NaN.
    Num(Vec<f64>),
    Text(Vec<String>),
}

/// Runs `script` over `input`, a `.parquet` file or else a CSV file with a
/// header line. A blank script, or one of comments only, passes the input
/// through. A script error keeps its place in the script ([`Error::span`]).
pub fn run(script: &str, input: &Path) -> Result<Output, Error> {
    let join_opts = JoinOpts {
        dir: None,
        may_read: &|_| true,
    };
    run_with(script, input, &join_opts)
}

/// Where a script's `join` may read.
pub struct JoinOpts<'a> {
    /// The folder a relative join path starts from; `None` for the process's
    /// working directory.
    pub dir: Option<&'a Path>,
    /// Whether a joined file may be read, given its canonical path. Only a
    /// regular file with a UTF-8 path is read.
    pub may_read: &'a dyn Fn(&Path) -> bool,
}

/// [`run`], with `join_opts` setting where a relative join path starts and
/// which files the script may join.
pub fn run_with(script: &str, input: &Path, join_opts: &JoinOpts) -> Result<Output, Error> {
    // As the CLI does, an error says when a `|` may be missing.
    run_inner(script, input, join_opts).map_err(|e| parse::note_missing_pipe(script, e))
}

/// A script's coloured parts and the first thing wrong with it, for an editor.
#[derive(Debug)]
pub struct Marks {
    /// Byte ranges of the script, in order, none overlapping another.
    pub spans: Vec<(Range<usize>, SpanKind)>,
    pub error: Option<ScriptError>,
}

/// An error, with its bytes of the script when it has a place.
#[derive(Debug)]
pub struct ScriptError {
    pub at: Option<Range<usize>>,
    /// One line, with the missing-`|` note when it applies.
    pub message: String,
}

/// The colours of `script` and its first error, as [`run_with`] would find
/// it over an input with the columns `header`. With no header, unknown
/// columns are not found.
pub fn marks(script: &str, header: Option<&[String]>, join_opts: &JoinOpts) -> Marks {
    let mut recorder = parse::Recorder::default();
    let parsed = parse::parse_recorded(script, &mut recorder);
    let spans = recorder.into_spans();
    let error = if is_blank(script) {
        None
    } else {
        match parsed {
            Err(e) => Some(e),
            Ok(mut plan) => match header {
                Some(h) => resolve(&mut plan, h, join_opts).err(),
                None => prepare(&mut plan, join_opts).err(),
            },
        }
    };
    let error = error.map(|e| {
        let (at, message) = placed(script, e);
        ScriptError { at, message }
    });
    Marks { spans, error }
}

/// The completion items at byte `at` of `script`, as csvm's mode server
/// gives them: commands, flags, functions, and the columns at the cursor's
/// stage, `header` carried through the stages before it. No columns inside a
/// `join ( … )` group or an `fn` body, or with no header, and no file names.
pub fn suggest(
    script: &str,
    at: usize,
    header: Option<&[String]>,
    join_opts: &JoinOpts,
) -> Vec<Item> {
    if !script.is_char_boundary(at) {
        return Vec::new();
    }
    let mut columns = |found: &complete::Found| {
        let header = header?;
        if !matches!(found.group, complete::Group::Top) {
            return None;
        }
        if found.stages_before.is_empty() {
            return Some(header.to_vec());
        }
        let mut plan = parse::parse(&found.stages_before).ok()?;
        resolve(&mut plan, header, join_opts).ok()
    };
    complete::items_at(script, at, &mut columns, &mut |_, _| Vec::new())
}

/// Whether `script` is only blanks and comments, which pass the input through.
fn is_blank(script: &str) -> bool {
    parse::strip_comments(script).trim().is_empty()
}

/// The header `plan` turns `header` into, checked as a run checks it.
fn resolve(plan: &mut Plan, header: &[String], join_opts: &JoinOpts) -> Result<Vec<String>, Error> {
    prepare(plan, join_opts)?;
    plan.resolve(header)
}

/// What a run does to the plan before it reads the input: a `from` stage is
/// refused at its place, and the joins are limited by `join_opts` and their
/// headers read. The output is CSV, which a run reads back into columns: the
/// caller draws its own table and charts, so a `fmt` is dropped, and a `graph`
/// sink is dropped with the notice this gives.
fn prepare(plan: &mut Plan, join_opts: &JoinOpts) -> Result<Vec<String>, Error> {
    if plan.from.is_some() {
        let e = Error::Compile("from: the input is given by the caller".into());
        return Err(match plan.sources.from.span.clone() {
            Some(span) => e.at(span),
            None => e,
        });
    }
    exec::expand_joins(plan, join_opts.dir, &mut AllowedFs(join_opts.may_read))?;
    check_joins(plan, join_opts)?;
    exec::read_join_headers(plan)?;
    plan.output = OutputFormat::Csv;
    let mut notices = Vec::new();
    if plan.graph.take().is_some() {
        notices.push("graph: ignored here; the caller draws its own charts".to_owned());
    }
    Ok(notices)
}

fn run_inner(script: &str, input: &Path, join_opts: &JoinOpts) -> Result<Output, Error> {
    let mut plan = if is_blank(script) {
        Plan {
            stages: Vec::new(),
            output: OutputFormat::Csv,
            colors: Vec::new(),
            graph: None,
            sources: Sources::default(),
            from: None,
        }
    } else {
        parse::parse(script)?
    };
    let notices = prepare(&mut plan, join_opts)?;
    // A big `sort` spills rows to files: keep them from other users.
    let spill = SpillDir::new()?;
    let opts = RunOpts {
        chunk_size: 1 << 20,
        threads: 1,
        temp_dir: spill.path().to_owned(),
        sort_buffer: 256 << 20,
        shard_bytes: exec::DEFAULT_SHARD_BYTES,
    };
    let progress = Progress::default();
    let mut buf = Vec::new();
    if cli::is_parquet_path(input) {
        run_parquet(&mut plan, input, &opts, &progress, &mut buf)?;
    } else {
        let (header, data_start, len) = exec::read_header_from_path(input)?;
        let out_header = plan.resolve(&header)?;
        let file = exec::InputFile {
            path: input,
            data_start,
            len,
        };
        exec::run_file(&plan, &out_header, &opts, file, &progress, &mut buf)?;
    }
    let text = String::from_utf8(buf)
        .map_err(|e| Error::Other(format!("output is not valid UTF-8: {}", e.utf8_error())))?;
    let mut rows: Vec<Vec<Cow<str>>> = Vec::new();
    csv::parse_records(&text, |r| {
        rows.push(r.drain(..).map(|f| f.into_text()).collect())
    });
    let rows: Vec<&[Cow<str>]> = rows.iter().map(Vec::as_slice).collect();
    Ok(Output {
        columns: columns(&rows),
        paints: paints(&plan, &rows),
        notices,
    })
}

/// A new folder in the temp directory that only this user can read, removed
/// when dropped.
struct SpillDir(std::path::PathBuf);

impl SpillDir {
    fn new() -> Result<Self, Error> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        // A name someone else took is skipped: the clock makes the next one
        // hard to guess.
        let mut tries = 0;
        loop {
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            let name = format!("csvm-embed-{}-{n}-{nanos:x}", std::process::id());
            let path = std::env::temp_dir().join(name);
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 100 => {
                    tries += 1;
                }
                Err(e) => {
                    let text = format!(
                        "cannot make a folder for sorting in {}: {e}",
                        path.display()
                    );
                    return Err(Error::Io(std::io::Error::new(e.kind(), text)));
                }
            }
        }
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for SpillDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Refuses a joined file that `join_opts` does not allow, or that is not a
/// regular file with a UTF-8 path, in `plan` and the plans it joins. The plan
/// then reads each allowed file by its checked, canonical path. A refusal says
/// the same whether the file exists or not.
fn check_joins(plan: &mut Plan, join_opts: &JoinOpts) -> Result<(), Error> {
    for (i, stage) in plan.stages.iter_mut().enumerate() {
        if let Stage::Join(j) = stage {
            check_joins(&mut j.right_plan, join_opts)?;
            let refused = || Error::Other(format!("{}: may not be read here", j.file));
            let path = allowed(&pattern::full(join_opts.dir, &j.file), join_opts.may_read)
                .ok_or_else(|| {
                    let written = plan.sources.stages.get(i).and_then(|w| w.first());
                    written.map_or_else(refused, |w| w.place_whole(refused()))
                })?;
            // `allowed` takes only paths that are valid UTF-8.
            j.file = path.to_string_lossy().into_owned();
        }
    }
    Ok(())
}

/// `path`'s canonical form, when it is valid UTF-8 and a regular file
/// `may_read` allows.
fn allowed(path: &Path, may_read: &dyn Fn(&Path) -> bool) -> Option<std::path::PathBuf> {
    let path = path.canonicalize().ok()?;
    (path.to_str().is_some() && path.is_file() && may_read(&path)).then_some(path)
}

/// The filesystem as a join pattern sees it: a wildcard matches only the files
/// the caller may read, so it names nothing else. A name spelled out in braces
/// is left to [`check_joins`], which refuses it as it refuses a plain path.
struct AllowedFs<'a>(&'a dyn Fn(&Path) -> bool);

impl pattern::Fs for AllowedFs<'_> {
    fn list(&mut self, dir: &Path) -> Option<Vec<String>> {
        let names = pattern::RealFs.list(dir)?;
        Some(
            names
                .into_iter()
                .filter(|name| allowed(&dir.join(name), self.0).is_some())
                .collect(),
        )
    }

    fn is_file(&mut self, _: &Path) -> bool {
        true
    }
}

#[cfg(feature = "parquet")]
fn run_parquet(
    plan: &mut Plan,
    input: &Path,
    opts: &RunOpts,
    progress: &Progress,
    buf: &mut Vec<u8>,
) -> Result<(), Error> {
    let header = crate::parquet::read_header(input)?;
    let out_header = plan.resolve(&header)?;
    exec::run_parquet(plan, &out_header, opts, input, progress, buf)
}

#[cfg(not(feature = "parquet"))]
fn run_parquet(
    _: &mut Plan,
    _: &Path,
    _: &RunOpts,
    _: &Progress,
    _: &mut Vec<u8>,
) -> Result<(), Error> {
    Err(Error::Other(cli::NO_PARQUET.into()))
}

/// Row `row`'s cell `i`, empty past the row's end.
fn cell<'r>(row: &'r [Cow<str>], i: usize) -> &'r str {
    row.get(i).map_or("", |c| c.as_ref())
}

/// The cells `plan`'s colour rules paint in `rows` (header first).
fn paints(plan: &Plan, rows: &[&[Cow<str>]]) -> Vec<Paint> {
    if plan.colors.is_empty() {
        return Vec::new();
    }
    let grid = exec::compute_styles(&plan.colors, rows);
    // A ragged row's extra cells are not columns.
    let width = rows.first().map_or(0, |header| header.len());
    let mut paints = Vec::new();
    for (ri, row) in grid.into_iter().enumerate().skip(1) {
        for (col, style) in row.into_iter().enumerate().take(width) {
            if !style.is_empty() {
                paints.push(Paint {
                    row: ri - 1,
                    col,
                    style,
                });
            }
        }
    }
    paints
}

/// Whether `cell` reads as an id, not a number: digits only, after a leading
/// `+`, or with a leading zero before another digit.
fn id_like(cell: &str) -> bool {
    let (plus, digits) = match cell.strip_prefix('+') {
        Some(rest) => (true, rest),
        None => (false, cell.strip_prefix('-').unwrap_or(cell)),
    };
    let whole = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    whole && (plus || (digits.len() > 1 && digits.starts_with('0')))
}

/// The output `rows` (header first) as columns.
fn columns(rows: &[&[Cow<str>]]) -> Vec<Column> {
    // An output with no columns writes empty lines: no columns, not one
    // named "".
    if rows
        .iter()
        .all(|row| matches!(row, [only] if only.is_empty()))
    {
        return Vec::new();
    }
    let numeric = exec::numeric_columns(rows);
    let Some((header, data)) = rows.split_first() else {
        return Vec::new();
    };
    header
        .iter()
        .enumerate()
        .map(|(i, name)| {
            // A column whose cells are all empty is a series with no data:
            // numbers. A header-only output has no cells to tell, so text.
            let blank = !data.is_empty()
                && data
                    .iter()
                    .all(|row| field::trim_cell(cell(row, i)).is_empty());
            // `02134` and `+5` read as numbers, but they are ids: text.
            let number = numeric.get(i) == Some(&true)
                && !data
                    .iter()
                    .any(|row| id_like(field::trim_cell(cell(row, i))));
            let values = if number || blank {
                Values::Num(
                    data.iter()
                        .map(|row| field::trim_cell(cell(row, i)).parse().unwrap_or(f64::NAN))
                        .collect(),
                )
            } else {
                Values::Text(data.iter().map(|row| cell(row, i).to_owned()).collect())
            };
            Column {
                name: name.to_string(),
                values,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{Base, Color};

    fn allow(_: &Path) -> bool {
        true
    }

    /// Joins from the working directory, any file allowed.
    fn any() -> JoinOpts<'static> {
        JoinOpts {
            dir: None,
            may_read: &allow,
        }
    }

    fn cols(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn marks_place_a_missing_equals() {
        let m = marks("add new one + two", Some(&cols(&["one", "two"])), &any());
        assert!(m.error.expect("an error").at.is_some());
        assert!(!m.spans.is_empty());
    }

    #[test]
    fn marks_name_an_unknown_column_only_with_a_header() {
        let script = "select nope > 1";
        assert!(marks(script, Some(&cols(&["a"])), &any()).error.is_some());
        assert!(marks(script, None, &any()).error.is_none());
    }

    #[test]
    fn marks_ignore_a_graph_as_a_run_does() {
        let m = marks("graph line a nope", Some(&cols(&["a"])), &any());
        assert!(m.error.is_none(), "{:?}", m.error);
    }

    #[test]
    fn marks_without_a_header_still_refuse_from_and_joins() {
        assert!(
            marks("from x.csv | select a > 1", None, &any())
                .error
                .is_some()
        );
        let there = csv("refuse-no-header", "k\n1\n");
        let deny = limited(&|_| false);
        let script = format!("join '{}' on k", there.display());
        let error = marks(&script, None, &deny).error.expect("an error");
        assert!(
            error.message.contains("may not be read here"),
            "{}",
            error.message
        );
    }

    #[test]
    fn marks_of_a_blank_script_find_nothing() {
        for script in ["", "   ", "# c\n"] {
            let m = marks(script, Some(&cols(&["a"])), &any());
            assert!(m.error.is_none(), "{script:?}: {:?}", m.error);
        }
    }

    #[test]
    fn suggest_offers_no_columns_inside_a_join_group() {
        let s = "join ( add y = a";
        let items = suggest(s, s.len(), Some(&cols(&["amp"])), &any());
        assert!(!items.iter().any(|i| i.text == "amp"), "{items:?}");
    }

    #[test]
    fn suggest_carries_a_new_column_into_later_stages() {
        let s = "add new = a + 1 | select n";
        let items = suggest(s, s.len(), Some(&cols(&["a"])), &any());
        assert!(items.iter().any(|i| i.text == "new"), "{items:?}");
    }

    #[test]
    fn suggest_after_a_space_at_a_stage_start_offers_commands() {
        let s = "add x = 1 | ";
        assert!(
            suggest(s, s.len(), None, &any())
                .iter()
                .any(|i| i.text == "select")
        );
    }

    #[test]
    fn a_refused_join_reads_the_same_whether_the_file_exists() {
        let deny = limited(&|_| false);
        let header = cols(&["k"]);
        let error = |s: &str| {
            marks(s, Some(&header), &deny)
                .error
                .expect("an error")
                .message
        };
        let there = csv("refuse-there", "k\n1\n");
        let absent = there.with_file_name("absent.csv");
        let a = error(&format!("join '{}' on k", there.display()));
        let b = error(&format!("join '{}' on k", absent.display()));
        let (there, absent) = (there.display().to_string(), absent.display().to_string());
        assert_eq!(a.replace(&there, "X"), b.replace(&absent, "X"));
    }

    /// Joins from the working directory, as `may_read` allows.
    fn limited<'a>(may_read: &'a dyn Fn(&Path) -> bool) -> JoinOpts<'a> {
        JoinOpts {
            dir: None,
            may_read,
        }
    }

    /// Writes `text` to a file named after `tag` in the temp dir.
    fn csv(tag: &str, text: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("csvm-embed-{tag}-{}.csv", std::process::id()));
        std::fs::write(&path, text).unwrap();
        path
    }

    fn nums(out: &Output, name: &str) -> Vec<f64> {
        let col = out.columns.iter().find(|c| c.name == name).expect(name);
        match &col.values {
            Values::Num(v) => v.clone(),
            Values::Text(v) => panic!("{name} is text: {v:?}"),
        }
    }

    fn same(a: &[f64], b: &[f64]) -> bool {
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .all(|(x, y)| x == y || (x.is_nan() && y.is_nan()))
    }

    #[test]
    fn blank_script_is_identity() {
        let out = run("  \n", &csv("blank", "timestamp,x\nt1,1\nt2,\n")).unwrap();
        let names: Vec<_> = out.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["timestamp", "x"]);
        match &out.columns[0].values {
            Values::Text(v) => assert_eq!(v, &["t1", "t2"]),
            Values::Num(v) => panic!("timestamp is numeric: {v:?}"),
        }
        assert!(same(&nums(&out, "x"), &[1.0, f64::NAN]));
    }

    #[test]
    fn add_and_select_run() {
        let out = run("add y = x * 2 | select y > 1", &csv("add", "x\n0\n1\n")).unwrap();
        assert!(same(&nums(&out, "y"), &[2.0]));
    }

    #[test]
    fn header_only_output_keeps_columns() {
        let out = run("select x > 100", &csv("none", "t,x\na,1\n")).unwrap();
        assert_eq!(out.columns.len(), 2);
        // No cell tells the type: text.
        assert!(out.columns.iter().all(|c| match &c.values {
            Values::Text(v) => v.is_empty(),
            Values::Num(v) => panic!("{} is numeric: {v:?}", c.name),
        }));
    }

    #[test]
    fn numeric_column_with_gaps_is_num() {
        let out = run("select x != 'no'", &csv("gaps", "x\n1\n\n3\n")).unwrap();
        assert!(same(&nums(&out, "x"), &[1.0, f64::NAN, 3.0]));
    }

    #[test]
    fn all_empty_column_is_num() {
        // A series with no data: numbers, not text.
        let out = run("cols x", &csv("allempty", "t,x\na,\nb,\n")).unwrap();
        assert!(same(&nums(&out, "x"), &[f64::NAN, f64::NAN]));
    }

    #[test]
    fn a_newline_in_a_cell_keeps_the_rows() {
        let out = run("add z = 'p\nq'", &csv("newline", "x\n1\n2\n")).unwrap();
        assert!(same(&nums(&out, "x"), &[1.0, 2.0]));
        match &out.columns[1].values {
            Values::Text(v) => assert_eq!(v, &["p\nq", "p\nq"]),
            Values::Num(v) => panic!("numeric: {v:?}"),
        }
    }

    #[test]
    fn paints_stay_inside_the_columns() {
        let out = run("color red x > 0", &csv("ragged", "x\n1\n2,3\n")).unwrap();
        assert!(!out.paints.is_empty());
        assert!(out.paints.iter().all(|p| p.col < out.columns.len()));
    }

    #[test]
    fn a_script_of_comments_is_blank() {
        let out = run("# nothing yet\n", &csv("comment", "x\n1\n")).unwrap();
        assert!(same(&nums(&out, "x"), &[1.0]));
    }

    #[test]
    fn a_missing_pipe_is_named() {
        let e = run("add y = x\nselect y > 1", &csv("pipe", "x\n1\n")).unwrap_err();
        assert!(e.to_string().contains("missing `|`"), "{e}");
    }

    #[test]
    fn id_like_columns_stay_text() {
        let out = run("", &csv("ids", "id,plus,n\n02134,+5,0\n00001,+6,-0.5\n")).unwrap();
        for name in ["id", "plus"] {
            let col = out.columns.iter().find(|c| c.name == name).unwrap();
            assert!(matches!(col.values, Values::Text(_)), "{name}");
        }
        assert!(same(&nums(&out, "n"), &[0.0, -0.5]));
    }

    #[test]
    fn joined_files_are_checked() {
        let dir = std::env::temp_dir();
        let right = dir.join(format!("csvm-embed-right-{}.csv", std::process::id()));
        std::fs::write(&right, "k,v\n1,a\n").unwrap();
        let script = format!("join '{}' on k", right.display());
        let input = csv("joinpolicy", "k\n1\n");
        let e = run_with(&script, &input, &limited(&|_| false)).unwrap_err();
        assert!(e.to_string().contains("may not be read"), "{e}");
        let only = |p: &Path| p == right.canonicalize().unwrap();
        let out = run_with(&script, &input, &limited(&only)).unwrap();
        assert_eq!(out.columns.len(), 2);
    }

    #[test]
    fn nested_joins_and_folders_are_checked() {
        let dir = std::env::temp_dir().join(format!("csvm-embed-nest-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let right = dir.join("r.csv");
        std::fs::write(&right, "k,v\n1,a\n").unwrap();
        let input = csv("nest", "k\n1\n");
        let only = |p: &Path| p == right.canonicalize().unwrap();
        // The outer file is allowed, the inner one is not.
        let inner = dir.join("sub").join("s.csv");
        std::fs::write(&inner, "k,w\n1,x\n").unwrap();
        let script = format!(
            "join (join '{}' on k) '{}' on k",
            inner.display(),
            right.display()
        );
        assert!(run_with(&script, &input, &limited(&only)).is_err());
        // A folder is not a file.
        let script = format!("join '{}' on k", dir.join("sub").display());
        assert!(run_with(&script, &input, &limited(&|_| true)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let dir = std::env::temp_dir().join(format!("csvm-embed-utf8-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let odd = dir.join(std::ffi::OsStr::from_bytes(b"r\xff.csv"));
        std::fs::write(&odd, "k,v\n1,a\n").unwrap();
        let link = dir.join("link.csv");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&odd, &link).unwrap();
        let script = format!("join '{}' on k", link.display());
        let input = csv("utf8", "k\n1\n");
        let e = run_with(&script, &input, &limited(&|_| true)).unwrap_err();
        assert!(e.to_string().contains("may not be read"), "{e}");
    }

    #[test]
    fn a_relative_join_starts_in_the_given_folder() {
        let dir = std::env::temp_dir().join(format!("csvm-embed-rel-{}", std::process::id()));
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("r.csv"), "k,v\n1,a\n").unwrap();
        std::fs::write(dir.join("out.csv"), "k,v\n1,a\n").unwrap();
        let input = csv("rel", "k\n1\n");
        let inside = data.canonicalize().unwrap();
        let may_read = |p: &Path| p.starts_with(&inside);
        let join_opts = JoinOpts {
            dir: Some(&data),
            may_read: &may_read,
        };
        for script in ["join r.csv on k", "join r*.csv on k"] {
            let out = run_with(script, &input, &join_opts).unwrap();
            assert_eq!(out.columns.len(), 2, "{script}");
        }
        let e = run_with("join '../out.csv' on k", &input, &join_opts).unwrap_err();
        assert!(e.to_string().contains("may not be read"), "{e}");
    }

    #[test]
    fn a_refused_path_says_nothing_about_the_disk() {
        let dir = std::env::temp_dir().join(format!("csvm-embed-hide-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("secret.csv"), "k,v\n1,a\n").unwrap();
        let input = csv("hide", "k\n1\n");
        let deny = limited(&|_: &Path| false);
        let error = |script: String| run_with(&script, &input, &deny).unwrap_err().to_string();
        // A refused file reads like a missing one, named or in braces.
        for name in ["'secret.csv'", "'nothing.csv'", "{secret,nothing}.csv"] {
            let e = error(format!("join {}/{name} on k", dir.display()));
            assert!(e.contains("may not be read"), "{e}");
        }
        // A wildcard matches no file it was not given.
        let e = error(format!("join {}/*.csv on k", dir.display()));
        assert!(e.contains("no file matches"), "{e}");
        assert!(!e.contains("secret"), "{e}");
    }

    #[test]
    fn signed_decimals_are_numbers() {
        let out = run("", &csv("signed", "d\n+0.5\n-0.3\n")).unwrap();
        assert!(same(&nums(&out, "d"), &[0.5, -0.3]));
    }

    #[test]
    fn no_columns_is_no_columns() {
        let out = run("cols -v a,b", &csv("nocols", "a,b\n1,2\n")).unwrap();
        assert!(out.columns.is_empty(), "{:?}", out.columns);
    }

    #[cfg(unix)]
    #[test]
    fn spills_go_to_a_private_folder() {
        use std::os::unix::fs::PermissionsExt;
        let spill = SpillDir::new().unwrap();
        let path = spill.path().to_owned();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        drop(spill);
        assert!(!path.exists());
    }

    #[test]
    fn text_column_stays_text() {
        let out = run("cols side", &csv("text", "side\nlag\nlead\n")).unwrap();
        match &out.columns[0].values {
            Values::Text(v) => assert_eq!(v, &["lag", "lead"]),
            Values::Num(v) => panic!("numeric: {v:?}"),
        }
    }

    #[test]
    fn fmt_dropped_graph_noted() {
        let input = csv("sinks", "t,x\n1,2\n2,3\n");
        let out = run("fmt -s", &input).unwrap();
        assert!(same(&nums(&out, "x"), &[2.0, 3.0]));
        assert!(out.notices.is_empty(), "{:?}", out.notices);
        let out = run("graph line t x", &input).unwrap();
        assert!(same(&nums(&out, "x"), &[2.0, 3.0]));
        assert_eq!(
            out.notices,
            ["graph: ignored here; the caller draws its own charts"]
        );
    }

    #[test]
    fn unknown_column_error_has_span() {
        let e = run("add y = nope * 2", &csv("unknown", "x\n1\n")).unwrap_err();
        assert_eq!(e.span(), Some(8..12), "{e}");
    }

    #[test]
    fn parse_error_has_span() {
        let e = run("add y = (", &csv("parse", "x\n1\n")).unwrap_err();
        assert!(e.span().is_some(), "{e}");
    }

    #[test]
    fn from_is_refused_at_its_place() {
        let e = run("from other.csv | cols x", &csv("from", "x\n1\n")).unwrap_err();
        assert_eq!(e.span().map(|s| s.start), Some(0), "{e}");
    }

    #[test]
    fn gradient_paints_numeric_cells() {
        let out = run("color -g x", &csv("grad", "x\n0\n5\n10\n")).unwrap();
        let fg: Vec<_> = out
            .paints
            .iter()
            .map(|p| (p.row, p.col, p.style.fg))
            .collect();
        assert_eq!(fg.len(), 3, "{fg:?}");
        assert!(
            fg.iter()
                .all(|&(_, col, c)| col == 0 && matches!(c, Some(Color::Rgb(_))))
        );
        assert_eq!(fg.iter().map(|p| p.0).collect::<Vec<_>>(), [0, 1, 2]);
        assert_ne!(fg[0].2, fg[2].2);
    }

    #[test]
    fn gradient_skips_empty_cells() {
        let out = run("color -g x", &csv("gradgap", "x\n0\n\n10\n")).unwrap();
        let rows: Vec<_> = out.paints.iter().map(|p| p.row).collect();
        assert_eq!(rows, [0, 2]);
    }

    #[test]
    fn predicate_with_prev_paints_whole_rows() {
        // `b` falls by more than 100 into the second row and the fourth.
        let input = csv("prev", "a,b\n1,500\n2,300\n3,290\n4,100\n5,90\n");
        let out = run("color red b - prev(b) < -100", &input).unwrap();
        let cells: Vec<_> = out.paints.iter().map(|p| (p.row, p.col)).collect();
        assert_eq!(cells, [(1, 0), (1, 1), (3, 0), (3, 1)]);
        assert!(
            out.paints
                .iter()
                .all(|p| p.style.fg == Some(Color::Base(Base::Red)))
        );
    }

    #[test]
    fn no_color_rules_no_paints() {
        let out = run("add y = x", &csv("plain", "x\n1\n")).unwrap();
        assert!(out.paints.is_empty());
    }

    #[cfg(feature = "parquet")]
    #[test]
    fn parquet_input_runs() {
        use arrow::array::{Float64Array, RecordBatch, StringArray};
        use arrow::datatypes::{DataType, Field, Schema};
        use parquet::arrow::ArrowWriter;
        use std::sync::Arc;

        let schema = Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::Utf8, false),
            Field::new("x", DataType::Float64, true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(vec!["t1", "t2"])),
                Arc::new(Float64Array::from(vec![Some(1.5), None])),
            ],
        )
        .unwrap();
        let path =
            std::env::temp_dir().join(format!("csvm-embed-pq-{}.parquet", std::process::id()));
        let mut writer =
            ArrowWriter::try_new(std::fs::File::create(&path).unwrap(), schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        let out = run("add y = x + 1", &path).unwrap();
        assert!(same(&nums(&out, "y"), &[2.5, 1.0]));
        assert!(same(&nums(&out, "x"), &[1.5, f64::NAN]));
    }
}
