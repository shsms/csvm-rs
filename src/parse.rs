//! Parsing the pipe command language into a [`Plan`].
//!
//! A script is a sequence of stages separated by `|`:
//!
//! ```text
//! cols a,b,c | select "amount > 1000 && flag == 't'" | sort amount=nr id
//! ```
//!
//! Commands take their arguments comma- or space-separated. `select` takes a
//! single infix expression (quote it so its operators and any `|` survive the
//! shell and the stage split). This module only parses; the compiled [`Plan`]
//! it produces is the same plain-Rust IR the executor runs — no interpreter in
//! the hot path.
//!
//! Comparison and sort modes are decided here from what an expression says on
//! its own (a number literal, arithmetic, a string literal); column types are
//! not known yet, so `Plan::resolve` decides again with the types it tracks
//! by position.

use crate::chart::MAX_CELLS;
use crate::color::{Ramp, parse_ramp, parse_style};
use crate::error::Error;
use crate::plan::{
    AddStmt, AffixKind, AggFunc, AggSpec, ArithOp, BoolExpr, Cmp, CmpMode, CmpOp, ColRef,
    ColorRule, ColorScope, Func, GraphKind, GraphOpts, GraphSpec, GroupStmt, JoinStmt, JoinType,
    OutputFormat, Plan, ProjectStmt, RenameStmt, SortKey, SortMode, SortStmt, Sources, Stage,
    StatsStmt, Stmt, TableOpts, UniqStmt, ValExpr, Written,
};
use std::ops::Range;

/// Compile a pipe script into an executable [`Plan`].
/// A compile error that can be placed in the script is an [`Error::At`], its
/// span a byte range of `script`: the token an expression stopped at, else
/// the stage it is in.
pub fn parse(script: &str) -> Result<Plan, Error> {
    let script = strip_comments(script);
    let (fns, rest) = parse_prologue(&script)?;
    parse_stages(rest, &fns, 0, &script, None)
}

/// [`parse`], noting in `rec` what each part of the script is, for
/// `csvm --highlight`. It builds the same plan as [`parse`], or fails the
/// same way. What was noted before an error stays in `rec`, and each stage
/// after the one that failed gets its command word noted. A `fn` definition
/// that fails is skipped, and the ones after it are still noted.
pub fn parse_recorded(script: &str, rec: &mut Recorder) -> Result<Plan, Error> {
    let script = strip_comments_noting(script, |at| rec.push(at, SpanKind::Comment));
    let (fns, rest) = parse_prologue_noting(&script, Some(&mut *rec))?;
    parse_stages(rest, &fns, 0, &script, Some(rec))
}

/// Parse stage text into a plan. Sub-pipelines and fragment bodies re-enter
/// here with the shared fn table and their expansion depth; `top` is the
/// whole script, which error spans are offsets into. `rec`, when given,
/// notes what each part of `top` is (see [`parse_recorded`]).
fn parse_stages(
    script: &str,
    fns: &FnTable,
    depth: usize,
    top: &str,
    rec: Option<&mut Recorder>,
) -> Result<Plan, Error> {
    let mut builder = Builder::new(fns, depth, top, rec);
    let stages = split_stages(script);
    builder.note_separators(script, &stages);
    for (i, stage) in stages.iter().enumerate() {
        let stage = stage.trim();
        // Skip blank stages: a blank or comment-only line in a multi-line `-f`
        // script, or a trailing `|`. A wholly empty script is caught below.
        if stage.is_empty() {
            continue;
        }
        if let Err(e) = builder.parse_stage(stage) {
            builder.note_commands(&stages[i + 1..]);
            return Err(place_on(top, stage, e));
        }
        builder.written_at(offset_in(top, stage).map(|at| at..at + stage.len()));
    }
    if builder.items.is_empty()
        && builder.output == OutputFormat::Csv
        && builder.colors.is_empty()
        && builder.graph.is_none()
    {
        return Err(err("empty script"));
    }
    Ok(builder.take_plan())
}

fn err(msg: impl Into<String>) -> Error {
    Error::Compile(msg.into())
}

/// Where `part` starts in `script`, when it is a slice of it (and not, say,
/// text a fragment call expanded to).
fn offset_in(script: &str, part: &str) -> Option<usize> {
    let (s, p) = (script.as_ptr() as usize, part.as_ptr() as usize);
    (p >= s && p + part.len() <= s + script.len()).then(|| p - s)
}

/// `e` placed on the whole of `part`, a slice of `script`, unless it has a
/// place already (or `part` is not in `script`).
fn place_on(script: &str, part: &str, e: Error) -> Error {
    match offset_in(script, part) {
        Some(at) => e.at(at..at + part.len()),
        None => e,
    }
}

/// What a part of the script is, for `csvm --highlight`: each kind is a
/// colour the editor paints that part with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanKind {
    Command,
    Keyword,
    Option,
    Operator,
    String,
    Number,
    Variable,
    Function,
    Comment,
}

impl SpanKind {
    /// The kind's name in the highlight protocol.
    pub fn name(self) -> &'static str {
        match self {
            SpanKind::Command => "command",
            SpanKind::Keyword => "keyword",
            SpanKind::Option => "option",
            SpanKind::Operator => "operator",
            SpanKind::String => "string",
            SpanKind::Number => "number",
            SpanKind::Variable => "variable",
            SpanKind::Function => "function",
            SpanKind::Comment => "comment",
        }
    }
}

/// The parts of a script [`parse_recorded`] noted: byte ranges of the
/// script, each with its kind.
#[derive(Debug, Default)]
pub struct Recorder {
    spans: Vec<(Range<usize>, SpanKind)>,
}

impl Recorder {
    /// Note the bytes `at` of the script as `kind`. An empty range is not
    /// noted.
    fn push(&mut self, at: Range<usize>, kind: SpanKind) {
        if at.start < at.end {
            self.spans.push((at, kind));
        }
    }

    /// Note `part` as `kind` when it is a slice of `script`. Text that is
    /// not (what a fragment call expanded to) is not noted.
    fn note(&mut self, script: &str, part: &str, kind: SpanKind) {
        if let Some(at) = offset_in(script, part) {
            self.push(at..at + part.len(), kind);
        }
    }

    /// The noted parts in script order, none overlapping another. Of two
    /// that overlap, the one that starts first is kept, and of two that
    /// start together, the shorter one.
    pub fn into_spans(mut self) -> Vec<(Range<usize>, SpanKind)> {
        self.spans.sort_by_key(|(at, _)| (at.start, at.end));
        let mut kept: Vec<(Range<usize>, SpanKind)> = Vec::with_capacity(self.spans.len());
        for (at, kind) in self.spans {
            if kept.last().is_none_or(|(last, _)| last.end <= at.start) {
                kept.push((at, kind));
            }
        }
        kept
    }
}

/// Known command names, for the "did you mean …?" hint on an unknown verb and
/// the help registry's drift check (see `crate::help`).
pub(crate) const COMMANDS: &[&str] = &[
    "cols", "select", "sort", "head", "tail", "stats", "uniq", "color", "rename", "fmt", "join",
    "add", "agg", "graph", "fn",
];

/// A command that was removed, with the advice that replaces a use of it.
/// The advice is built from the arguments the script gave, so the error
/// alone is enough to rewrite the script.
struct Removed {
    name: &'static str,
    advice: fn(&str) -> String,
}

const REMOVED: &[Removed] = &[
    Removed {
        name: "to-num",
        advice: num_cast_advice,
    },
    Removed {
        name: "to_num",
        advice: num_cast_advice,
    },
    Removed {
        name: "to-str",
        advice: str_cast_advice,
    },
    Removed {
        name: "to_str",
        advice: str_cast_advice,
    },
    Removed {
        name: "delta",
        advice: delta_advice,
    },
    Removed {
        name: "group",
        advice: group_advice,
    },
    Removed {
        name: "hdr",
        advice: hdr_advice,
    },
];

/// The error for a removed command, or `None` if `cmd` is not one.
pub(crate) fn removed(cmd: &str, args: &str) -> Option<Error> {
    let r = REMOVED.iter().find(|r| r.name == cmd)?;
    Some(err(format!("{cmd} was removed: {}", (r.advice)(args))))
}

/// Whether `name` is a command or an alias.
fn is_command(name: &str) -> bool {
    name == "colour" || COMMANDS.contains(&name)
}

/// Whether `name` is taken by a command, an alias, or a removed command
/// (kept reserved so its hint stays reachable), so a `fn` may not use it.
fn is_reserved(name: &str) -> bool {
    is_command(name) || REMOVED.iter().any(|r| r.name == name)
}

fn num_cast_advice(args: &str) -> String {
    cast_advice("num", args)
}

fn str_cast_advice(args: &str) -> String {
    cast_advice("str", args)
}

/// The `add` that does what a removed conversion command did, per column.
/// Inside the expression a name that is not a bare identifier (a position,
/// a name with spaces) is backtick-quoted; as the `add` target only a name
/// with spaces needs that.
fn cast_advice(cast: &str, args: &str) -> String {
    let cols = split_list(args);
    let stages: Vec<String> = if cols.is_empty() {
        vec![format!("add COL = {cast}(COL)")]
    } else {
        cols.iter()
            .map(|c| {
                let arg = if is_ident(c) {
                    c.clone()
                } else {
                    format!("`{c}`")
                };
                let target = if c.contains(char::is_whitespace) {
                    format!("`{c}`")
                } else {
                    c.clone()
                };
                format!("add {target} = {cast}({arg})")
            })
            .collect()
    };
    format!("convert with add, e.g. `{}`", stages.join(" | "))
}

/// `delta [-s SUF] COLS` was a stateful `add` per column; spell that out.
fn delta_advice(args: &str) -> String {
    let mut cols = Vec::new();
    let mut toks = split_list(args).into_iter();
    while let Some(t) = toks.next() {
        if t == "-s" {
            toks.next(); // the suffix
        } else if !t.starts_with("-s") {
            cols.push(t);
        }
    }
    let stages: Vec<String> = if cols.is_empty() {
        vec!["add COL_delta = COL - prev(COL)".to_string()]
    } else {
        cols.iter()
            .map(|c| format!("add {c}_delta = {c} - prev({c})"))
            .collect()
    };
    format!(
        "write the difference with add, e.g. `{}`",
        stages.join(" | ")
    )
}

fn group_advice(args: &str) -> String {
    let keys = args.trim();
    format!(
        "give the keys to agg, e.g. `agg count by {}`",
        if keys.is_empty() { "COLS" } else { keys }
    )
}

fn hdr_advice(_: &str) -> String {
    "name a headerless input's columns with `--header a,b,c` on the command line \
     (`--header -` auto-names them c1, c2, …)"
        .to_string()
}

/// How deep fragment expansion may nest before it is treated as runaway
/// recursion (a fragment that calls itself, directly or in a cycle).
const MAX_FN_DEPTH: usize = 64;

/// A user-defined pipeline fragment: parameter names plus the raw body text
/// between its braces (comment-stripped, substituted at each call).
#[derive(Debug)]
struct FnDef {
    params: Vec<String>,
    body: String,
}

type FnTable = std::collections::HashMap<String, FnDef>;

/// One parsed command: a statement, which joins the surrounding run into a
/// `Transform` stage, or a stage of its own.
enum Item {
    Stmt(Stmt),
    Stage(Stage),
}

struct Builder<'a> {
    /// Fragment definitions from the script prologue (shared, read-only).
    fns: &'a FnTable,
    /// Current fragment-expansion depth; `MAX_FN_DEPTH` stops recursion.
    depth: usize,
    /// The whole script, for placing an error in it.
    script: &'a str,
    items: Vec<Item>,
    output: OutputFormat,
    /// Colour rules from `color` commands (plan metadata, not stages).
    colors: Vec<ColorRule>,
    /// A `graph` sink (plan metadata; the last command, terminates the pipe).
    graph: Option<GraphSpec>,
    /// Where each item, colour rule and the graph were written in the script
    /// (see [`Builder::written_at`]).
    item_written: Vec<Written>,
    color_written: Vec<Written>,
    graph_written: Option<Written>,
    /// The columns read since the last [`Builder::written_at`], and where
    /// each is written (see [`Builder::read_column`]).
    columns: Vec<(String, Range<usize>)>,
    /// Where `--highlight` notes what each part of the script is; `None`
    /// for a plain parse.
    rec: Option<&'a mut Recorder>,
}

impl<'a> Builder<'a> {
    fn new(fns: &'a FnTable, depth: usize, script: &'a str, rec: Option<&'a mut Recorder>) -> Self {
        Builder {
            fns,
            depth,
            script,
            items: Vec::new(),
            output: OutputFormat::Csv,
            colors: Vec::new(),
            graph: None,
            item_written: Vec::new(),
            color_written: Vec::new(),
            graph_written: None,
            columns: Vec::new(),
            rec,
        }
    }

    /// Record `span` as where everything parsed since the last call was
    /// written: the stage just parsed, and whatever a fragment it called
    /// expanded to; with the columns read meanwhile.
    fn written_at(&mut self, span: Option<Range<usize>>) {
        let written = Written {
            span,
            columns: std::mem::take(&mut self.columns),
        };
        self.item_written
            .resize_with(self.items.len(), || written.clone());
        self.color_written
            .resize_with(self.colors.len(), || written.clone());
        if self.graph.is_some() && self.graph_written.is_none() {
            self.graph_written = Some(written);
        }
    }

    /// Note that the part being parsed reads column `name`, written at `at`
    /// of `src`. Dropped when `src` is not a slice of the script, as for a
    /// fragment's expansion.
    fn read_column(&mut self, name: &str, src: &str, at: Range<usize>) {
        if let Some(base) = offset_in(self.script, src) {
            let at = base + at.start..base + at.end;
            if let Some(rec) = self.rec.as_deref_mut() {
                rec.push(at.clone(), SpanKind::Variable);
            }
            self.columns.push((name.to_string(), at));
        }
    }

    /// [`Builder::read_column`] for `name`, read from the item at `item` of
    /// `src`: where it is written there, from byte `from` of the item's text
    /// in `src` on, else the whole item.
    fn read_column_in(&mut self, name: &str, src: &str, item: Range<usize>, from: usize) {
        let found = src
            .get(item.clone())
            .and_then(|text| text.get(from..))
            .and_then(|rest| rest.find(name));
        let at = match found {
            Some(i) => item.start + from + i..item.start + from + i + name.len(),
            None => item,
        };
        self.read_column(name, src, at);
    }

    /// The column list `s` (see [`split_list`]), each column noted as read.
    fn column_list(&mut self, s: &str) -> Vec<String> {
        let mut names = Vec::new();
        for (name, at) in split_items(s, false, false).0 {
            self.read_column(&name, s, at);
            names.push(name);
        }
        names
    }

    /// Whether this parse notes what the script's parts are (see
    /// [`parse_recorded`]).
    fn recording(&self) -> bool {
        self.rec.is_some()
    }

    /// Note `part`, a slice of the script, as `kind`. Does nothing without
    /// a recorder, or when `part` is not a slice of the script.
    fn note(&mut self, part: &str, kind: SpanKind) {
        if let Some(rec) = self.rec.as_deref_mut() {
            rec.note(self.script, part, kind);
        }
    }

    /// Note each `|` between `stages`, the stages [`split_stages`] cut
    /// `script` into. (A newline between stages is not noted.)
    fn note_separators(&mut self, script: &str, stages: &[&str]) {
        if !self.recording() {
            return;
        }
        for stage in &stages[..stages.len().saturating_sub(1)] {
            let Some(at) = offset_in(script, stage) else {
                continue;
            };
            let end = at + stage.len();
            if let Some(bar) = script.get(end..end + 1).filter(|c| *c == "|") {
                self.note(bar, SpanKind::Operator);
            }
        }
    }

    /// Note the command word of `stage`: a known command, or a call of a
    /// defined fragment. An unknown word is left for its error.
    fn note_command(&mut self, stage: &str) {
        if !self.recording() {
            return;
        }
        let word = match fragment_call(stage) {
            Some((name, _)) => self.fns.contains_key(name).then_some(name),
            None => {
                let (cmd, _) = split_first_word(stage);
                is_command(cmd).then_some(cmd)
            }
        };
        if let Some(word) = word {
            self.note(word, SpanKind::Command);
        }
    }

    /// Note the command word of each of `stages`: the stages after one that
    /// failed, which are not parsed.
    fn note_commands(&mut self, stages: &[&str]) {
        for stage in stages {
            self.note_command(stage.trim());
        }
    }

    /// Note each token of the expression `src`, as [`lex_expr`] split it,
    /// with each token's byte range in `src`.
    fn note_tokens(&mut self, src: &str, toks: &[ETok], spans: &[Range<usize>]) {
        for (i, (tok, at)) in toks.iter().zip(spans).enumerate() {
            let kind = match tok {
                ETok::Num(_) | ETok::Word(..) => SpanKind::Number,
                ETok::Str(_) => SpanKind::String,
                // A name right before `(` is a call, as `parse_atom` reads it.
                ETok::Ident(_) if toks.get(i + 1) == Some(&ETok::Sym("(")) => SpanKind::Function,
                ETok::Ident(_) => SpanKind::Variable,
                ETok::Sym("(" | ")" | ",") => continue,
                ETok::Sym(_) => SpanKind::Operator,
            };
            if let Some(part) = src.get(at.clone()) {
                self.note(part, kind);
            }
        }
    }

    /// Note a flag's name: `word` up to an `=` that gives its value.
    fn note_flag(&mut self, word: &str) {
        if !self.recording() {
            return;
        }
        self.note(
            &word[..word.find('=').unwrap_or(word.len())],
            SpanKind::Option,
        );
    }

    /// Note a `head`/`tail` count: the number, and the flag written before
    /// it (`-n`, `--lines`).
    fn note_count(&mut self, rest: &str) {
        if !self.recording() {
            return;
        }
        let rest = rest.trim();
        let number = head_count_text(rest);
        let flag = rest[..rest.len() - number.len()]
            .trim_end()
            .trim_end_matches('=');
        self.note(flag, SpanKind::Option);
        self.note(number, SpanKind::Number);
    }

    /// Note the parts of one `agg` item as written: the output name and its
    /// `=` when given, and the function's name.
    fn note_agg_item(&mut self, text: &str) {
        if !self.recording() {
            return;
        }
        let call = match split_name_eq(text) {
            Ok((_, after, Some(call))) => {
                self.note(text[..text.len() - after.len()].trim(), SpanKind::Variable);
                self.note(&after[..1], SpanKind::Operator);
                call.trim()
            }
            _ => text.trim(),
        };
        let func = call.find('(').map_or(call, |open| call[..open].trim_end());
        self.note(func, SpanKind::Function);
    }

    /// Note the `=` of an `old=new` pair written as `text`, and the name
    /// after it.
    fn note_pair_rhs(&mut self, text: &str) {
        if !self.recording() {
            return;
        }
        if let Some(eq) = text.find('=') {
            self.note(&text[eq..eq + 1], SpanKind::Operator);
            self.note(text[eq + 1..].trim_start(), SpanKind::Variable);
        }
    }

    /// Group the flat item list into stages: runs of statements become a
    /// `Transform`; every other item is already a stage of its own.
    fn take_plan(&mut self) -> Plan {
        let mut stages = Vec::new();
        // In step with `stages`: where each one's parts were written.
        let mut written: Vec<Vec<Written>> = Vec::new();
        let mut transform: Vec<Stmt> = Vec::new();
        let mut transform_written = Vec::new();
        let flush = |transform: &mut Vec<Stmt>,
                     transform_written: &mut Vec<Written>,
                     stages: &mut Vec<Stage>,
                     written: &mut Vec<Vec<Written>>| {
            if !transform.is_empty() {
                stages.push(Stage::Transform(std::mem::take(transform)));
                written.push(std::mem::take(transform_written));
            }
        };
        let item_written = std::mem::take(&mut self.item_written);
        debug_assert_eq!(self.items.len(), item_written.len());
        for (item, part) in self.items.drain(..).zip(item_written) {
            let stage = match item {
                Item::Stmt(s) => {
                    transform.push(s);
                    transform_written.push(part);
                    continue;
                }
                Item::Stage(stage) => stage,
            };
            flush(
                &mut transform,
                &mut transform_written,
                &mut stages,
                &mut written,
            );
            match stage {
                // A window folds into the one before it, and resolves no
                // columns, so it has no parts to place.
                Stage::Skip(n) => push_window(&mut stages, n, None),
                Stage::Head(n) => push_window(&mut stages, 0, Some(n)),
                stage => {
                    stages.push(stage);
                    written.push(vec![part]);
                }
            }
            written.resize_with(stages.len(), Vec::new);
        }
        flush(
            &mut transform,
            &mut transform_written,
            &mut stages,
            &mut written,
        );
        Plan {
            stages,
            output: self.output,
            colors: std::mem::take(&mut self.colors),
            graph: self.graph.take(),
            sources: Sources {
                stages: written,
                colors: std::mem::take(&mut self.color_written),
                graph: self.graph_written.take().unwrap_or_default(),
            },
        }
    }

    fn parse_stage(&mut self, stage: &str) -> Result<(), Error> {
        self.note_command(stage);
        // `graph` is a terminal sink: it emits a chart, not rows, so nothing may
        // follow it in the pipeline.
        if self.graph.is_some() {
            return Err(err("graph must be the last command in the pipeline"));
        }
        // A stage that is exactly `NAME(ARGS)` is a fragment call.
        if let Some((name, args)) = fragment_call(stage) {
            let fns = self.fns;
            return match fns.get(name) {
                Some(def) => self.expand_fragment(name, def, args),
                None => {
                    if let Some(e) = removed(name, args) {
                        return Err(e);
                    }
                    let mut cands: Vec<String> = fns.keys().cloned().collect();
                    cands.extend(COMMANDS.iter().map(|s| s.to_string()));
                    Err(err(match crate::error::did_you_mean(name, &cands) {
                        Some(s) => format!("unknown fragment: {name} (did you mean `{s}`?)"),
                        None => format!("unknown fragment: {name}"),
                    }))
                }
            };
        }
        let (cmd, rest) = split_first_word(stage);
        if let Some(e) = removed(cmd, rest) {
            return Err(e);
        }
        let result = match cmd {
            "cols" => self.parse_cols(rest),
            "select" => self.parse_select(rest),
            "sort" => self.parse_sort(rest),
            "head" => self.parse_head(rest),
            "tail" => self.parse_tail(rest),
            "stats" => self.parse_stats(rest),
            "agg" => self.parse_agg(rest),
            "graph" => self.parse_graph(rest),
            "uniq" => self.parse_uniq(rest),
            "join" => self.parse_join(rest),
            "color" | "colour" => self.parse_color(rest),
            "rename" => self.parse_rename(rest),
            "add" => self.parse_add(rest),
            "fmt" => self.parse_fmt(rest),
            "fn" => Err(err("fn definitions must come before the first stage")),
            other => Err(place_on(
                self.script,
                other,
                err(if self.fns.contains_key(other) {
                    format!(
                        "unknown command: {other} (`{other}` is a fragment — call it as `{other}(ARGS)`)"
                    )
                } else {
                    let mut cands: Vec<String> = COMMANDS.iter().map(|s| s.to_string()).collect();
                    cands.extend(self.fns.keys().cloned());
                    match crate::error::did_you_mean(other, &cands) {
                        Some(s) => format!("unknown command: {other} (did you mean `{s}`?)"),
                        None => format!("unknown command: {other}"),
                    }
                }),
            )),
        };
        result.map_err(|e| self.hint_fragment(e))
    }

    /// A fragment name used inside an expression fails as an unknown
    /// function; point at the whole-stage call form.
    fn hint_fragment(&self, e: Error) -> Error {
        let Error::Compile(msg) = e.unplaced() else {
            return e;
        };
        let Some(tail) = msg.strip_prefix("unknown function: ") else {
            return e;
        };
        let name = tail.split([' ', '(']).next().unwrap_or("");
        if self.fns.contains_key(name) {
            let hinted = err(format!(
                "unknown function: {name} (`{name}` is a fragment — fragments expand only as whole stages)"
            ));
            return match e.span() {
                Some(span) => hinted.at(span),
                None => hinted,
            };
        }
        e
    }

    /// Parse the expression `src`, a slice of the stage being parsed, with
    /// `parse`. An error is placed on the part the parser names, else on the
    /// token it stopped at, or on the token the lexer was reading.
    fn parse_expr<T>(
        &mut self,
        src: &str,
        parse: impl FnOnce(&mut ExprParser) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let (toks, spans) = match lex_expr(src) {
            Ok(lexed) => lexed,
            Err((e, at)) => {
                // The lexer reads left to right, so the text before the
                // character it stopped at lexes to the same tokens.
                if self.recording()
                    && let Ok((toks, spans)) = lex_expr(&src[..at.start])
                {
                    self.note_tokens(src, &toks, &spans);
                }
                return Err(self.place_in(src, at, e));
            }
        };
        if self.recording() {
            self.note_tokens(src, &toks, &spans);
        }
        let mut parser = ExprParser {
            toks,
            spans,
            end: src.len(),
            pos: 0,
            fail_span: None,
            columns: Vec::new(),
        };
        let parsed = parse(&mut parser).map_err(|e| self.place_in(src, parser.error_span(), e))?;
        for (name, at) in parser.columns {
            self.read_column(&name, src, at);
        }
        Ok(parsed)
    }

    /// `e`, from the expression `src`, placed on the byte range `at` of it.
    fn place_in(&self, src: &str, at: Range<usize>, e: Error) -> Error {
        match offset_in(self.script, src) {
            Some(base) => e.at(base + at.start..base + at.end),
            None => e,
        }
    }

    /// Instantiate fragment `name` and splice its stages in at this position:
    /// substitute the arguments into the body, then parse the result into
    /// this builder one level deeper (so runaway recursion errors out).
    fn expand_fragment(
        &mut self,
        name: &str,
        def: &'a FnDef,
        args_text: &str,
    ) -> Result<(), Error> {
        if self.depth >= MAX_FN_DEPTH {
            return Err(err(format!(
                "fn expansion too deep at `{name}` — recursive fragments?"
            )));
        }
        let args: Vec<String> = if args_text.trim().is_empty() {
            Vec::new()
        } else {
            split_top_commas(args_text)
                .iter()
                .map(|a| a.trim().to_string())
                .collect()
        };
        if args.len() != def.params.len() {
            return Err(err(format!(
                "`{name}` expects {} argument(s), got {}",
                def.params.len(),
                args.len()
            )));
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let body = subst_params(&def.body, &def.params, &arg_refs);
        self.depth += 1;
        let result = (|| {
            for stage in split_stages(&body) {
                let stage = stage.trim();
                if stage.is_empty() {
                    continue;
                }
                self.parse_stage(stage)
                    .map_err(|e| err(format!("in fn `{name}`: {e}")))?;
            }
            Ok(())
        })();
        self.depth -= 1;
        result
    }

    fn parse_head(&mut self, rest: &str) -> Result<(), Error> {
        // A positive count keeps the first N rows; a negative one (`head -n -N`)
        // keeps all but the last N (coreutils' behaviour).
        match parse_count(rest, "head")? {
            Count::Rows(n) if n >= 0 => self.items.push(Item::Stage(Stage::Head(n as usize))),
            Count::Rows(n) => self
                .items
                .push(Item::Stage(Stage::DropLast(n.unsigned_abs() as usize))),
            Count::From(_) => return Err(err("head doesn't support +N (that is tail's form)")),
        }
        self.note_count(rest);
        Ok(())
    }

    /// `tail [N]` keeps the last N rows reaching it (a blocking stage; default
    /// 10). Same count spellings as `head`, but no negative form; `tail +N`
    /// (`-n +N`) prints from row N on, a streaming skip of the first N-1.
    fn parse_tail(&mut self, rest: &str) -> Result<(), Error> {
        match parse_count(rest, "tail")? {
            Count::Rows(n) if n >= 0 => self.items.push(Item::Stage(Stage::Tail(n as usize))),
            Count::Rows(_) => return Err(err("tail doesn't support a negative count")),
            Count::From(n) => self
                .items
                .push(Item::Stage(Stage::Skip(n.saturating_sub(1)))),
        }
        self.note_count(rest);
        Ok(())
    }

    /// `uniq [cols]` drops duplicate rows, keeping the first —
    /// by the whole row, or by the named key columns. Global (not adjacent), so
    /// no pre-sort is required.
    fn parse_uniq(&mut self, rest: &str) -> Result<(), Error> {
        let cols = self.column_list(rest);
        self.items.push(Item::Stage(Stage::Uniq(UniqStmt {
            cols,
            positions: Vec::new(),
        })));
        Ok(())
    }

    /// `join [FLAGS] ITEM[, ITEM...]` where `ITEM := [(SUBPIPELINE)] FILE [on
    /// KEYS]` merges one or more right-side files into the stream by key
    /// (desugaring to one join stage per file, left to right). Flags pick the
    /// join type (`-l/--left`, `-r/--right`, `-F/--full`; inner by default) and
    /// apply to every item. The optional parenthesized sub-pipeline is a full
    /// csvm script run over that right file before joining. `KEYS` is a
    /// comma/space list of `name` or `lname=rname`; either every item has its
    /// own `on`, or a single trailing `on` is shared by all. A keyless,
    /// paren-less fragment after an `on` clause reads as more keys (it is
    /// lexically identical to a composite key list), so a file missing its
    /// `on` in the all-explicit form surfaces at resolve time, with a hint —
    /// not here (with a sub-pipeline it still errors at parse time). A file
    /// path containing a comma must be quoted.
    fn parse_join(&mut self, rest: &str) -> Result<(), Error> {
        let mut s = rest.trim_start();
        let mut join_type = JoinType::Inner;
        let mut lsuffix = None;
        let mut rsuffix = None;
        loop {
            let (word, after) = split_first_word(s);
            if word.starts_with('-') && word != "-" {
                self.note_flag(word);
            }
            // `-L S` / `-R S` (or `=S`) set per-side clash suffixes.
            if let Some(v) = flag_value(word, after, &["-L", "--lsuffix"]) {
                let (val, rest_after) = v?;
                lsuffix = Some(val);
                s = rest_after;
                continue;
            }
            if let Some(v) = flag_value(word, after, &["-R", "--rsuffix"]) {
                let (val, rest_after) = v?;
                rsuffix = Some(val);
                s = rest_after;
                continue;
            }
            match word {
                "-l" | "--left" => join_type = JoinType::Left,
                "-r" | "--right" => join_type = JoinType::Right,
                "-F" | "--full" => join_type = JoinType::Full,
                "--inner" => join_type = JoinType::Inner,
                w if w.starts_with('-') && w != "-" => {
                    return Err(err(format!("join: unknown flag '{w}'")));
                }
                _ => break,
            }
            s = after;
        }

        // Comma-separated items: `[(SUBPIPELINE)] FILE [on KEYS]`.
        if s.is_empty() {
            return Err(err("join expects a right-side file"));
        }
        let mut stmts: Vec<JoinStmt> = Vec::new();
        for frag in split_top_commas(s) {
            let frag = frag.trim();
            if frag.is_empty() {
                return Err(err("join: empty item (stray comma?)"));
            }
            // Composite keys are comma-separated too: a fragment with no `on`
            // and no sub-pipeline extends the previous item's key list.
            if !frag.starts_with('(')
                && !has_bare_word(frag, "on")
                && let Some(prev) = stmts.last_mut().filter(|j| !j.keys.is_empty())
            {
                self.parse_join_keys(frag, &mut prev.keys)?;
                continue;
            }

            // Optional `(SUBPIPELINE)` — a full csvm script over the right file.
            let mut f = frag;
            let right_plan = if f.starts_with('(') {
                let (inner, after) = take_paren_group(f)?;
                f = after.trim_start();
                if inner.trim().is_empty() {
                    Box::new(identity_plan())
                } else {
                    Box::new(parse_stages(
                        inner,
                        self.fns,
                        self.depth,
                        self.script,
                        self.rec.as_deref_mut(),
                    )?)
                }
            } else {
                Box::new(identity_plan())
            };

            // The right-side file path.
            let (file, after) = take_token(f);
            if file.is_empty() {
                return Err(err("join expects a right-side file"));
            }
            if file == "-" {
                return Err(err("join's right side must be a file, not stdin"));
            }
            // The path as written, quotes included.
            let typed = f.trim_start();
            self.note(
                typed[..typed.len() - after.len()].trim_end(),
                SpanKind::String,
            );
            f = after.trim_start();

            // Optional `on KEY[,KEY...]`.
            let mut keys = Vec::new();
            if !f.is_empty() {
                let (kw, key_str) = split_first_word(f);
                if kw != "on" {
                    return Err(err("join expects `on KEY[,KEY...]` after the file"));
                }
                self.note(kw, SpanKind::Keyword);
                self.parse_join_keys(key_str, &mut keys)?;
                if keys.is_empty() {
                    return Err(err("join `on` expects at least one key column"));
                }
            }

            stmts.push(JoinStmt {
                join_type,
                right_plan,
                file: file.to_string(),
                own_keys: keys.len(),
                keys,
                lsuffix: lsuffix.clone(),
                rsuffix: rsuffix.clone(),
                right_header: Vec::new(),
                left_key_pos: Vec::new(),
                right_key_pos: Vec::new(),
                right_emit_pos: Vec::new(),
                left_ncols: 0,
            });
        }

        // Key rule: every item carries its own `on`, or only the last does and
        // its keys are shared by all — mixing the two forms is an error.
        // (`stmts` can't be empty: the first fragment either errors or pushes.)
        let (last, init) = stmts.split_last_mut().expect("non-empty");
        if last.keys.is_empty() {
            return Err(err("join expects `on KEY[,KEY...]` after the file"));
        }
        let missing = init.iter().filter(|j| j.keys.is_empty()).count();
        if missing > 0 && missing != init.len() {
            return Err(err(
                "join: give every file its own `on`, or one trailing `on` shared by all",
            ));
        }
        if missing > 0 {
            let shared = last.keys.clone();
            for j in init.iter_mut().filter(|j| j.keys.is_empty()) {
                j.keys = shared.clone();
                j.own_keys = last.own_keys;
            }
        }

        self.items
            .extend(stmts.into_iter().map(|j| Item::Stage(Stage::Join(j))));
        Ok(())
    }

    /// Parse `KEY[,KEY...]` specs (`name` or `lname=rname`) onto `keys`.
    fn parse_join_keys(
        &mut self,
        spec_list: &str,
        keys: &mut Vec<(String, String)>,
    ) -> Result<(), Error> {
        for (spec, at) in split_items(spec_list, false, false).0 {
            let (l, r) = match spec.split_once('=') {
                None => (spec.as_str(), spec.as_str()),
                Some((l, r)) if !l.is_empty() && !r.is_empty() => (l, r),
                Some(_) => return Err(err(format!("join `on`: bad key '{spec}'"))),
            };
            let text = &spec_list[at.clone()];
            // Only the left key is looked up in the stream this part reads.
            self.read_column_in(l, spec_list, at, 0);
            self.note_pair_rhs(text);
            keys.push((l.to_string(), r.to_string()));
        }
        Ok(())
    }

    /// `stats [cols]` profiles the named columns (or all of them, if none are
    /// named): a blocking stage that reduces the input to one summary row per
    /// column.
    fn parse_stats(&mut self, rest: &str) -> Result<(), Error> {
        let cols = self.column_list(rest);
        self.items.push(Item::Stage(Stage::Stats(StatsStmt {
            cols,
            positions: Vec::new(),
        })));
        Ok(())
    }

    /// `agg [NAME=]FN(col),… [by COLS]` reduces to one row per key; `by COLS`
    /// gives the keys, and without it the whole input is one group.
    fn parse_agg(&mut self, rest: &str) -> Result<(), Error> {
        // One item list: the aggregates, then an unquoted `by` item, then
        // the keys (`by COLS`; without it, one global aggregate row).
        let items = split_specs(rest)?;
        let by_at = items.iter().position(|(t, _)| t == "by");
        let (specs, keys) = match by_at {
            Some(at) => (&items[..at], &items[at + 1..]),
            None => (&items[..], &[][..]),
        };
        let mut aggs = Vec::new();
        for (spec, at) in specs {
            let agg = parse_agg_spec(spec)?;
            self.note_agg_item(&rest[at.clone()]);
            if let Some(col) = &agg.col {
                // The column is inside the call's parentheses, after any
                // `NAME=` (which may hold a `(` too), as parse_agg_spec reads it.
                let from = rest.get(at.clone()).map_or(0, |t| {
                    let call = split_name_eq(t)
                        .ok()
                        .and_then(|(_, _, spec)| spec)
                        .unwrap_or(t);
                    t.len() - call.len() + call.find('(').unwrap_or(0)
                });
                self.read_column_in(col, rest, at.clone(), from);
            }
            aggs.push(agg);
        }
        if let Some(i) = by_at {
            self.note(&rest[items[i].1.clone()], SpanKind::Keyword);
        }
        if aggs.is_empty() {
            return Err(err(
                "agg expects at least one aggregate, e.g. agg sum(amount)",
            ));
        }
        if by_at.is_some() && keys.is_empty() {
            return Err(err("agg: `by` expects at least one key column"));
        }
        let keys: Vec<String> = keys
            .iter()
            .map(|(k, at)| {
                let key = unquote(k);
                self.read_column_in(key, rest, at.clone(), 0);
                key.to_string()
            })
            .collect();
        self.items.push(Item::Stage(Stage::Group(GroupStmt {
            keys,
            key_positions: Vec::new(),
            aggs,
        })));
        Ok(())
    }

    /// `graph KIND COLS [-b N] [-s F] [-t T] [-S] [-W N] [-H N]` — a
    /// terminal-chart sink. Draws from the columns reaching it instead of
    /// emitting CSV, so it must be the last command. `hist COL`, `spark COL`,
    /// `bar LABEL VALUE`, `scatter X Y`, `line X Y`, `heatmap X Y`.
    fn parse_graph(&mut self, rest: &str) -> Result<(), Error> {
        // Without a chart type the first word is a column, and the columns
        // choose the chart (see `default_graph_kind`).
        let (kind_word, after_kind) = split_first_word(rest.trim());
        let (named, rest) = match graph_kind(kind_word) {
            Some(kind) => {
                self.note(kind_word, SpanKind::Keyword);
                (Some(kind), after_kind)
            }
            None => (None, rest.trim()),
        };
        let mut opts = GraphOpts::default();
        let mut cols = Vec::new();
        let mut s = rest.trim();
        while !s.is_empty() {
            let (word, after) = split_first_word(s);
            if word.starts_with('-') && word != "-" {
                self.note_flag(word);
            }
            if let Some(v) = flag_value(word, after, &["-b", "--bins"]) {
                let (val, tail) = v?;
                opts.bins = Some(parse_positive(&val, "-b/--bins", MAX_CELLS)?);
                s = tail.trim_start();
            } else if let Some(v) = flag_value(word, after, &["-s", "--scale"]) {
                let (val, tail) = v?;
                opts.scale = parse_scale(&val)?;
                s = tail.trim_start();
            } else if let Some(v) = flag_value(word, after, &["-t", "--title"]) {
                let (val, tail) = v?;
                opts.title = Some(val);
                s = tail.trim_start();
            } else if let Some(v) = flag_value(word, after, &["--xlabel"]) {
                let (val, tail) = v?;
                opts.xlabel = Some(val);
                s = tail.trim_start();
            } else if let Some(v) = flag_value(word, after, &["--ylabel"]) {
                let (val, tail) = v?;
                opts.ylabel = Some(val);
                s = tail.trim_start();
            } else if word == "-S" || word == "--svg" {
                opts.svg = true;
                s = after;
            } else if word == "-A" || word == "--ascii" {
                opts.ascii = true;
                s = after;
            } else if word == "-D" || word == "--data" {
                opts.data = true;
                s = after;
            } else if word == "-l" || word == "--log" {
                opts.log = true;
                s = after;
            } else if let Some(v) = flag_value(word, after, &["-W", "--width"]) {
                let (val, tail) = v?;
                opts.width = Some(parse_positive(&val, "-W/--width", MAX_CELLS)?);
                s = tail.trim_start();
            } else if let Some(v) = flag_value(word, after, &["-H", "--height"]) {
                let (val, tail) = v?;
                opts.height = Some(parse_positive(&val, "-H/--height", MAX_CELLS)?);
                s = tail.trim_start();
            } else if let Some(v) = flag_value(word, after, &["-x", "--xrange"]) {
                let (val, tail) = v?;
                opts.xrange = Some(parse_range(&val, "-x/--xrange")?);
                s = tail.trim_start();
            } else if let Some(v) = flag_value(word, after, &["-r", "--ramp"]) {
                let (val, tail) = v?;
                opts.ramp = Some(
                    crate::color::parse_ramp(&val).map_err(|e| err(format!("-r/--ramp: {e}")))?,
                );
                s = tail.trim_start();
            } else if let Some(v) = flag_value(word, after, &["-c", "--color-by"]) {
                let (val, tail) = v?;
                // The value follows the flag's name, after a space or `=`.
                let from = word.find('=').map_or(word.len(), |eq| eq + 1);
                self.read_column_in(&val, s, 0..s.len() - tail.len(), from);
                opts.color_by = Some(ColRef::new(val));
                s = tail.trim_start();
            } else if let Some(v) = flag_value(word, after, &["-y", "--yrange"]) {
                let (val, tail) = v?;
                opts.yrange = Some(parse_range(&val, "-y/--yrange")?);
                s = tail.trim_start();
            } else if word.starts_with('-') && word != "-" {
                return Err(err(format!("graph: unknown flag `{word}`")));
            } else {
                cols.extend(self.column_list(word));
                s = after;
            }
        }
        // Both write to the normal output, so only one of them can have it.
        if opts.data && opts.svg {
            return Err(err("graph: -D/--data and -S/--svg are exclusive"));
        }
        let (kind, kind_word) = match named {
            Some(kind) => (kind, kind_word),
            None => {
                let kind = default_graph_kind(cols.len())?;
                (kind, kind.name())
            }
        };
        check_graph_arity(kind, kind_word, cols.len())?;
        check_graph_flags(kind, kind_word, &opts, cols.len())?;
        self.graph = Some(GraphSpec {
            kind,
            kind_named: named.is_some(),
            cols: cols.into_iter().map(ColRef::new).collect(),
            opts,
        });
        Ok(())
    }

    /// `color [-c COL] COLOUR EXPR` (predicate) or `color -g COL RAMP [LO HI]`
    /// (gradient). Colour rules are plan metadata, not stages — applied to the
    /// output rows at render time.
    fn parse_color(&mut self, rest: &str) -> Result<(), Error> {
        let (first, after) = split_first_word(rest.trim());
        if first == "-g" || first == "-c" {
            self.note(first, SpanKind::Option);
        }
        match first {
            "" => Err(err("color expects arguments")),
            "-g" => self.parse_color_gradient(after),
            "-c" => {
                let (col, tail) = split_first_word(after);
                if col.is_empty() {
                    return Err(err("color -c expects a column name"));
                }
                self.note(col, SpanKind::Variable);
                self.parse_color_predicate(ColorScope::Cell(ColRef::new(col.to_string())), tail)
            }
            // Colour-first: the rest (colour + expression) is the predicate form.
            _ => self.parse_color_predicate(ColorScope::Row, rest.trim()),
        }
    }

    fn parse_color_predicate(&mut self, scope: ColorScope, s: &str) -> Result<(), Error> {
        let (spec, expr_src) = split_first_word(s);
        if spec.is_empty() {
            return Err(err("color expects a colour"));
        }
        let style = parse_style(spec).map_err(err)?;
        self.note(spec, SpanKind::Keyword);
        let expr_src = expr_src.trim();
        if expr_src.is_empty() {
            return Err(err("color expects a condition expression"));
        }
        let expr = self.parse_expr(expr_src, ExprParser::parse)?;
        // Colour rules render from the buffered output rows, where there is no
        // previous-row/rownum context to read. (Checked here rather than at
        // resolve time, where an unresolvable rule is silently dropped.)
        if expr.is_stateful() {
            return Err(err("prev()/rownum() are not allowed in a color condition"));
        }
        self.colors
            .push(ColorRule::Predicate { scope, style, expr });
        Ok(())
    }

    fn parse_color_gradient(&mut self, s: &str) -> Result<(), Error> {
        let mut it = s.split_whitespace().peekable();
        // One or more leading column names, then the optional ramp and bounds.
        // A ramp token contains ':' and a bound parses as a number, so columns
        // are the leading tokens that are neither.
        let mut cols = Vec::new();
        while let Some(t) = it.peek() {
            if t.contains(':') || t.parse::<f64>().is_ok() {
                break;
            }
            let t = it.next().unwrap();
            self.note(t, SpanKind::Variable);
            cols.push(t.to_string());
        }
        if cols.is_empty() {
            return Err(err("color -g expects a column name"));
        }
        // The ramp is optional (defaults to green:red); when present it applies
        // to every listed column, as do the bounds.
        let ramp = match it.peek() {
            Some(t) if t.contains(':') => {
                let t = it.next().unwrap();
                self.note(t, SpanKind::Keyword);
                parse_ramp(t).map_err(err)?
            }
            _ => Ramp::default(),
        };
        let bounds = match (it.next(), it.next()) {
            (Some(lo), Some(hi)) => {
                let bounds = (
                    lo.parse::<f64>()
                        .map_err(|_| err(format!("color -g: bad lower bound '{lo}'")))?,
                    hi.parse::<f64>()
                        .map_err(|_| err(format!("color -g: bad upper bound '{hi}'")))?,
                );
                self.note(lo, SpanKind::Number);
                self.note(hi, SpanKind::Number);
                Some(bounds)
            }
            (None, None) => None,
            _ => return Err(err("color -g needs both LO and HI, or neither")),
        };
        if it.next().is_some() {
            return Err(err("color -g: too many arguments"));
        }
        for col in cols {
            self.colors.push(ColorRule::Gradient {
                col: ColRef::new(col),
                ramp,
                bounds,
            });
        }
        Ok(())
    }

    fn parse_rename(&mut self, rest: &str) -> Result<(), Error> {
        let mut pairs = Vec::new();
        for (spec, at) in split_items(rest, false, false).0 {
            match spec.split_once('=') {
                Some((from, to)) if !from.is_empty() && !to.is_empty() => {
                    let text = &rest[at.clone()];
                    self.read_column_in(from, rest, at, 0);
                    self.note_pair_rhs(text);
                    pairs.push((from.to_string(), to.to_string()));
                }
                _ => return Err(err(format!("rename expects old=new pairs, got '{spec}'"))),
            }
        }
        if pairs.is_empty() {
            return Err(err("rename expects at least one old=new pair"));
        }
        self.items
            .push(Item::Stmt(Stmt::Rename(RenameStmt { pairs })));
        Ok(())
    }

    /// `fmt [-s] [-f | -p N] [-h]`: align the output as a table.
    fn parse_fmt(&mut self, rest: &str) -> Result<(), Error> {
        let mut table = TableOpts::default();
        let (mut saw_full, mut saw_precision) = (false, false);
        let mut s = rest.trim();
        while !s.is_empty() {
            let (word, after) = split_first_word(s);
            self.note_flag(word);
            let flag = if let Some(v) = flag_value(word, after, &["-p", "--precision"]) {
                let (n, tail) = v?;
                table.decimals = Some(n.parse().map_err(|_| {
                    err(format!(
                        "fmt -p expects a number of decimals from 0 to 255, not {n:?}"
                    ))
                })?);
                s = tail.trim_start();
                &mut saw_precision
            } else {
                s = after;
                match word {
                    "-s" | "--stripes" => &mut table.stripes,
                    "-f" | "--full" => {
                        table.decimals = None;
                        &mut saw_full
                    }
                    "-h" | "--human" => &mut table.human,
                    other => {
                        return Err(err(format!(
                            "fmt takes -s (--stripes), -f (--full), -p N (--precision) and \
                             -h (--human), not {other:?}"
                        )));
                    }
                }
            };
            if std::mem::replace(flag, true) {
                let name = word.split_once('=').map_or(word, |(name, _)| name);
                return Err(err(format!("fmt takes {name} only once")));
            }
        }
        if saw_full && saw_precision {
            return Err(err(
                "fmt takes -f (every digit) or -p N (N decimals), not both",
            ));
        }
        self.output = OutputFormat::Aligned(table);
        Ok(())
    }

    fn parse_cols(&mut self, rest: &str) -> Result<(), Error> {
        let (exclude, list) = match rest.strip_prefix("-v") {
            Some(r) => {
                self.note(&rest[..2], SpanKind::Option);
                (true, r.trim_start())
            }
            None => (false, rest),
        };
        let names = self.column_list(list);
        if names.is_empty() {
            return Err(err("cols expects at least one column"));
        }
        self.items.push(Item::Stmt(Stmt::Cols(ProjectStmt {
            exclude,
            names,
            positions: Vec::new(),
        })));
        Ok(())
    }

    /// `add NAME = EXPR` — append (or replace, if `NAME` exists) a computed
    /// column. The expression is the value-expression grammar (arithmetic,
    /// `++` concat, functions, `?:`, `prev()`/`rownum()`). A quoted `NAME`
    /// (`'`, `"` or backticks) may contain spaces.
    fn parse_add(&mut self, rest: &str) -> Result<(), Error> {
        let (name, after, assigned) = split_name_eq(rest)?;
        if name.is_empty() {
            return Err(err(
                "add expects `add NAME = EXPR`, e.g. add total = amount * qty",
            ));
        }
        let expr_src = match assigned {
            Some(e) => {
                let written = rest.trim_start();
                self.note(
                    written[..written.len() - after.len()].trim_end(),
                    SpanKind::Variable,
                );
                self.note(&after[..1], SpanKind::Operator);
                e.trim()
            }
            None => {
                let old = after.trim();
                // The hints quote the name as the script must (a name with
                // spaces or an `=` needs backticks).
                let target = if name.contains(|c: char| c.is_whitespace() || c == '=') {
                    format!("`{name}`")
                } else {
                    name.clone()
                };
                return Err(err(if old.is_empty() {
                    "add expects `add NAME = EXPR`".to_string()
                } else if let Some(value) = old.strip_prefix("==") {
                    let value = value.trim();
                    format!(
                        "add: `==` is a comparison, not an assignment; write `add {target} = {value}` \
                         (or `add {target} = `{name}` == {value}` for a t/f column)"
                    )
                } else {
                    format!("add expects `add NAME = EXPR`, i.e. `add {target} = {old}`")
                }));
            }
        };
        if expr_src.is_empty() {
            return Err(err("add expects an expression after `=`"));
        }
        let expr = self.parse_expr(expr_src, ExprParser::parse_value_top)?;
        let stateful = expr.is_stateful();
        self.items.push(Item::Stmt(Stmt::Add(AddStmt {
            name,
            expr,
            pos: None,
            stateful,
        })));
        Ok(())
    }

    fn parse_sort(&mut self, rest: &str) -> Result<(), Error> {
        let mut keys = Vec::new();
        for (spec, at) in split_items(rest, false, false).0 {
            // `col=flags`.
            let (name, flags) = match spec.split_once('=') {
                Some((n, f)) => (n.to_string(), f),
                None => (spec.clone(), ""),
            };
            if name.is_empty() {
                return Err(err("sort spec is missing a column name"));
            }
            let text = &rest[at.clone()];
            self.read_column_in(&name, rest, at, 0);
            // The flags, from their `=` on.
            if self.recording()
                && !flags.is_empty()
                && let Some(eq) = text.rfind('=')
            {
                self.note(&text[eq..], SpanKind::Option);
            }
            let mut key = SortKey {
                mode: SortMode::Auto,
                name,
                pos: 0,
                descending: false,
            };
            for ch in flags.chars() {
                match ch {
                    'n' => key.mode = SortMode::Numeric,
                    's' => key.mode = SortMode::Lexical,
                    'r' => key.descending = true,
                    other => {
                        return Err(err(format!(
                            "unknown sort flag '{other}' (use n or s, and/or r)"
                        )));
                    }
                }
            }
            keys.push(key);
        }
        if keys.is_empty() {
            return Err(err("sort expects at least one column"));
        }
        self.items.push(Item::Stage(Stage::Sort(SortStmt { keys })));
        Ok(())
    }

    fn parse_select(&mut self, rest: &str) -> Result<(), Error> {
        // The expression is bare (not wrapped in quotes); string *literals*
        // inside still use quotes. `||` and `&&` are handled by the lexer, and
        // `||` survives the stage split (see `split_stages`). A leading `-v`
        // (like `cols -v`) negates the *whole* expression — `select -v EXPR`
        // drops the matching rows — which is `!(EXPR)`, sidestepping the De
        // Morgan trap of negating each operator.
        let rest = rest.trim();
        let (negate, expr_src) = match rest.strip_prefix("-v") {
            Some(r) if r.is_empty() || r.starts_with(char::is_whitespace) => {
                self.note(&rest[..2], SpanKind::Option);
                (true, r.trim())
            }
            _ => (false, rest),
        };
        if expr_src.is_empty() {
            return Err(err("select expects an expression"));
        }
        let expr = self.parse_expr(expr_src, ExprParser::parse)?;
        let expr = if negate {
            BoolExpr::Not(Box::new(expr))
        } else {
            expr
        };
        self.items.push(Item::Stmt(Stmt::Select(expr)));
        Ok(())
    }
}

/// Rows kept by `head`/`tail` when no count is given (bash defaults to 10).
const DEFAULT_ROWS: usize = 10;

/// A `head`/`tail` row count.
enum Count {
    /// `N` rows; negative is `head`'s "all but the last N".
    Rows(i64),
    /// `+N`: from row N on (coreutils' `tail -n +N`).
    From(usize),
}

/// Parse the row count shared by `head`/`tail`: no argument ⇒ 10; a bare
/// count (`head 20`), `-n`/`--lines` (`-n 20`, `-n20`, `--lines=20`), or the
/// obsolete `-N` (`head -20`, positive). A reduced text that itself starts
/// with `-` (i.e. `-n -N` / `--lines=-N`) is *negative*, and one that starts
/// with `+` is [`Count::From`]; each verb decides what it accepts. Byte mode
/// (`-c`) is not supported. `verb` names the command in errors.
fn parse_count(rest: &str, verb: &str) -> Result<Count, Error> {
    let rest = rest.trim();
    if rest.is_empty() {
        return Ok(Count::Rows(DEFAULT_ROWS as i64));
    }
    let bad = || err(format!("{verb} expects a row count, got '{rest}'"));
    let text = head_count_text(rest);
    match text.strip_prefix('+') {
        Some(from) if from.bytes().all(|b| b.is_ascii_digit()) => {
            from.parse().map(Count::From).map_err(|_| bad())
        }
        Some(_) => Err(bad()),
        None => text.parse().map(Count::Rows).map_err(|_| bad()),
    }
}

/// Reduce a `head` argument to its numeric text, accepting bash's spellings:
/// `-n N` / `-nN`, `--lines N` / `--lines=N`, and the obsolete `-N`. A bare
/// count is returned unchanged.
fn head_count_text(rest: &str) -> &str {
    if let Some(r) = rest.strip_prefix("--lines") {
        return r.strip_prefix('=').unwrap_or(r).trim_start();
    }
    if let Some(r) = rest.strip_prefix("-n") {
        return r.trim_start();
    }
    if let Some(r) = rest.strip_prefix('-') {
        return r.trim_start(); // obsolete `-N`
    }
    rest
}

// --- stage / word splitting -------------------------------------------------

/// Remove `#`-to-end-of-line comments, respecting string and backtick quoting (a
/// `#` inside `'…'`, `"…"`, or `` `…` `` is data, not a comment). Newlines are
/// kept so stage splitting and trimming are unchanged. Mainly for multi-line
/// scripts read via `-f`, but works inline too.
fn strip_comments(script: &str) -> String {
    strip_comments_noting(script, |_| {})
}

/// [`strip_comments`], calling `comment` with each comment's byte range:
/// from its `#` up to the end of its line, the newline not included.
fn strip_comments_noting(script: &str, mut comment: impl FnMut(Range<usize>)) -> String {
    let mut out = String::with_capacity(script.len());
    let mut quote: Option<char> = None;
    let mut chars = script.char_indices();
    while let Some((i, c)) = chars.next() {
        match quote {
            Some(q) => {
                out.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' | '`' => {
                    quote = Some(c);
                    out.push(c);
                }
                '#' => {
                    // Blank through end of line, keeping the newline itself,
                    // so everything after the comment stays at its offset in
                    // the script (where an error's span points).
                    out.push(' ');
                    let mut end = script.len();
                    for (j, d) in chars.by_ref() {
                        if d == '\n' {
                            out.push('\n');
                            end = j;
                            break;
                        }
                        out.extend(std::iter::repeat_n(' ', d.len_utf8()));
                    }
                    comment(i..end);
                }
                _ => out.push(c),
            },
        }
    }
    out
}

/// Split a script into stages on a lone, unquoted `|` **or a newline** — so a
/// multi-line `-f` script can write one stage per line without trailing `|`s. A
/// `||` (the *or* operator) and a `|`/newline inside a string literal or a
/// `join (…)` group are left intact, so `select` expressions need no quoting of
/// their own. Blank stages (blank or comment-only lines) are dropped by `parse`.
fn split_stages(script: &str) -> Vec<&str> {
    let mut stages = Vec::new();
    let bytes = script.as_bytes();
    let mut start = 0;
    let mut i = 0;
    let mut quote: Option<u8> = None;
    let mut depth = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
                i += 1;
            }
            None if c == b'"' || c == b'\'' => {
                quote = Some(c);
                i += 1;
            }
            // A `join (SUBPIPELINE)` group has its own `|`s; don't split inside it.
            None if c == b'(' => {
                depth += 1;
                i += 1;
            }
            None if c == b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            None if c == b'|' && depth > 0 => i += 1,
            None if c == b'|' => {
                if bytes.get(i + 1) == Some(&b'|') {
                    i += 2; // `||` is the or-operator, not a stage separator
                } else {
                    stages.push(&script[start..i]);
                    start = i + 1;
                    i += 1;
                }
            }
            // A newline separates stages too (for multi-line `-f` scripts), but
            // not inside a `join (…)` group, whose own stages split on their own.
            None if c == b'\n' && depth == 0 => {
                stages.push(&script[start..i]);
                start = i + 1;
                i += 1;
            }
            None => i += 1,
        }
    }
    stages.push(&script[start..]);
    stages
}

/// A `Plan` that passes its input through unchanged — the right side of a `join`
/// with no sub-pipeline (the file is loaded as-is).
fn identity_plan() -> Plan {
    Plan {
        stages: Vec::new(),
        output: OutputFormat::Csv,
        colors: Vec::new(),
        graph: None,
        sources: Sources::default(),
    }
}

/// Given `s` starting with `(`, return the contents of the balanced,
/// quote-aware parenthesized group and the remainder after the closing `)`.
fn take_paren_group(s: &str) -> Result<(&str, &str), Error> {
    let bytes = s.as_bytes();
    debug_assert_eq!(bytes.first(), Some(&b'('));
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == b'"' || c == b'\'' || c == b'`' => quote = Some(c),
            None if c == b'(' => depth += 1,
            None if c == b')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok((&s[1..i], &s[i + 1..]));
                }
            }
            None => {}
        }
        i += 1;
    }
    Err(err("join: unbalanced '(' in the right-side sub-pipeline"))
}

/// Given `s` starting with `{`, return the contents of the balanced,
/// quote-aware brace group and the remainder after the closing `}`.
fn take_brace_group(s: &str) -> Result<(&str, &str), Error> {
    let bytes = s.as_bytes();
    debug_assert_eq!(bytes.first(), Some(&b'{'));
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == b'"' || c == b'\'' || c == b'`' => quote = Some(c),
            None if c == b'{' => depth += 1,
            None if c == b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok((&s[1..i], &s[i + 1..]));
                }
            }
            None => {}
        }
        i += 1;
    }
    Err(err("unterminated `{` group"))
}

// --- nesting, for indenting a line ------------------------------------------

/// How deep two lines of a script sit, counted in the `fn NAME(…) { … }`
/// bodies and `join (…)` groups open around them, for an editor that
/// indents a line it is splitting (`csvm --highlight`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Depths {
    /// The line that starts where the split is.
    pub new: usize,
    /// The line the split is on.
    pub current: usize,
}

/// The [`Depths`] for splitting `script` at byte `at` (at most its length). The
/// script is read with the parser's own rules: comments as `strip_comments`
/// finds them, `'…'`, `"…"` and `` `…` `` quotes as the group takers
/// (`take_paren_group`, `take_brace_group`) skip them, and stages split on a
/// lone `|` or a newline as in `split_stages`, except in an `fn` header before
/// its `{`, which `parse_fn_def` reads across newlines. A `{` opens an `fn`
/// body when its stage's first word is `fn`, and a `(` opens a `join` group
/// when its stage's first word is `join`; any other bracket only has to be
/// closed. Text inside a string or a comment is at the depth where the string
/// or comment started. A line that starts, blanks skipped, with a `)` or `}`
/// that closes a group gets the depth just after that bracket. When the
/// brackets nest well, that is one step out. The script need not parse: an
/// unclosed group runs to the end, and a closing bracket with no group open is
/// passed over, so no depth is ever below 0.
pub fn depths(script: &str, at: usize) -> Depths {
    let text = strip_comments(script);
    let at = at.min(text.len());
    let line_start = text.as_bytes()[..at]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |nl| nl + 1);
    let mut nesting = Nesting::new(&text);
    nesting.scan(0..line_start);
    let current = nesting.line_depth(line_start);
    nesting.scan(line_start..at);
    let new = nesting.line_depth(at);
    Depths { new, current }
}

/// What a stage's first word makes of the brackets in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandWord {
    /// `join`: a `(` opens a sub-pipeline.
    Join,
    /// `fn`: a `{` opens a fragment body.
    Fn,
    /// Anything else: a bracket is only a bracket.
    Other,
}

/// One bracket left open, or the whole script (the first entry).
#[derive(Debug)]
struct Bracket {
    /// A `join` group or an `fn` body: one step deeper.
    counts: bool,
    /// Where its current stage starts, when it holds stages (the whole
    /// script, an `fn` body, a `join` group); `None` inside an expression.
    stage: Option<usize>,
    /// What the current stage's first word is, once a bracket, a `|` or a
    /// newline asked.
    command_word: Option<CommandWord>,
}

/// A left-to-right scan of comment-free script text that keeps the open
/// brackets and whether it is inside a quote.
struct Nesting<'s> {
    text: &'s str,
    /// Never empty: the first entry is the whole script and is never closed.
    brackets: Vec<Bracket>,
    /// The quote mark of the string the scan is inside.
    quote: Option<u8>,
    /// Indices into `brackets` of the open brackets closed by `)`, oldest
    /// first.
    parens: Vec<usize>,
    /// The same, for brackets closed by `}`.
    braces: Vec<usize>,
}

impl<'s> Nesting<'s> {
    fn new(text: &'s str) -> Nesting<'s> {
        Nesting {
            text,
            brackets: vec![Bracket {
                counts: false,
                stage: Some(0),
                command_word: None,
            }],
            quote: None,
            parens: Vec::new(),
            braces: Vec::new(),
        }
    }

    /// Read the bytes of `range`, which starts where the last scan ended.
    fn scan(&mut self, range: Range<usize>) {
        let bytes = self.text.as_bytes();
        let mut i = range.start;
        while i < range.end {
            let c = bytes[i];
            match self.quote {
                Some(q) => {
                    if c == q {
                        self.quote = None;
                    }
                }
                None => match c {
                    b'\'' | b'"' | b'`' => self.quote = Some(c),
                    b'(' | b'{' => self.open(c, i),
                    b')' | b'}' => self.close(c, i),
                    // `||` is the or-operator, not a stage separator.
                    b'|' if bytes.get(i + 1) == Some(&b'|') => i += 1,
                    b'|' | b'\n' if !self.fn_header_pending(i) => self.new_stage(i + 1),
                    _ => {}
                },
            }
            i += 1;
        }
    }

    /// True while the innermost bracket's current stage, read so far, starts
    /// `fn` and has not opened its body yet: `parse_fn_def` reads an `fn`'s
    /// parameter list and the blank before its `{` across newlines, so a
    /// `|` or a newline there does not start a new stage (the `}` that
    /// closes the body still does, from [`close`](Self::close)). `at` is
    /// the `|` or newline. A newline counts as the blank after `fn`, as it
    /// does for the parser. The first word is kept once found, so a long
    /// header is read once.
    fn fn_header_pending(&mut self, at: usize) -> bool {
        let end = if self.text.as_bytes()[at] == b'\n' {
            at + 1
        } else {
            at
        };
        // A first word that is not `fn` is dropped with the stage, which
        // starts again after this `|` or newline.
        self.stage_command_word(end) == CommandWord::Fn
    }

    /// What the innermost bracket's current stage, read up to `end`, makes
    /// of a bracket; `Other` inside an expression. The first word is kept
    /// once found.
    fn stage_command_word(&mut self, end: usize) -> CommandWord {
        let text = self.text;
        let top = self.brackets.last_mut().expect("the whole script stays");
        match top.stage {
            Some(start) => *top
                .command_word
                .get_or_insert_with(|| command_word(&text[start..end])),
            None => CommandWord::Other,
        }
    }

    /// A stage starts at `at` in the innermost bracket, when that bracket holds
    /// stages.
    fn new_stage(&mut self, at: usize) {
        let top = self.brackets.last_mut().expect("the whole script stays");
        if top.stage.is_some() {
            top.stage = Some(at);
            top.command_word = None;
        }
    }

    /// The bracket `c` at byte `i` opens a new level.
    fn open(&mut self, c: u8, i: usize) {
        let word = self.stage_command_word(i);
        let counts = matches!(
            (c, word),
            (b'(', CommandWord::Join) | (b'{', CommandWord::Fn)
        );
        self.brackets.push(Bracket {
            counts,
            stage: counts.then_some(i + 1),
            command_word: None,
        });
        let idx = self.brackets.len() - 1;
        if c == b'(' {
            self.parens.push(idx);
        } else {
            self.braces.push(idx);
        }
    }

    /// The bracket `c` at byte `i` closes the innermost bracket it can close,
    /// and any left open inside it. After an `fn` body a new stage starts,
    /// as the prologue reads on after the body's `}`.
    fn close(&mut self, c: u8, i: usize) {
        let Some(at) = self.closed_by(c) else {
            return;
        };
        let fn_body = c == b'}' && self.brackets[at].counts;
        self.brackets.truncate(at);
        // The stacks keep only the open brackets.
        while self.parens.last().is_some_and(|&p| p >= at) {
            self.parens.pop();
        }
        while self.braces.last().is_some_and(|&b| b >= at) {
            self.braces.pop();
        }
        if fn_body {
            self.new_stage(i + 1);
        }
    }

    /// The index of the bracket `c` would close; `None` when none is open.
    fn closed_by(&self, c: u8) -> Option<usize> {
        let stack = if c == b')' {
            &self.parens
        } else {
            &self.braces
        };
        stack.last().copied()
    }

    /// The depth of the brackets up to (not including) `end`.
    fn depth_below(&self, end: usize) -> usize {
        self.brackets[..end].iter().filter(|f| f.counts).count()
    }

    /// The depth of a line whose text starts at byte `at`, where the scan
    /// has stopped: the depth there, or, when the text starts (blanks
    /// skipped) with a bracket that closes an open one, the depth after it.
    fn line_depth(&self, at: usize) -> usize {
        let open = self.brackets.len();
        if self.quote.is_some() {
            return self.depth_below(open);
        }
        let first = self.text.as_bytes()[at..]
            .iter()
            .find(|&&b| b != b' ' && b != b'\t');
        match first {
            Some(&c @ (b')' | b'}')) => self.depth_below(self.closed_by(c).unwrap_or(open)),
            _ => self.depth_below(open),
        }
    }
}

/// What the stage text before a bracket, `head`, makes of it: the stage's
/// first word, when a blank ends it before the bracket. A bracket inside
/// the first word (`prep(`, `join(`) is not after a command.
fn command_word(head: &str) -> CommandWord {
    let head = head.trim_start();
    let (word, _) = split_first_word(head);
    if word.len() == head.len() {
        return CommandWord::Other;
    }
    match word {
        "join" => CommandWord::Join,
        "fn" => CommandWord::Fn,
        _ => CommandWord::Other,
    }
}

/// A bare identifier: ASCII letter or `_` first, then letters/digits/`_`.
fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Peel leading `fn NAME(PARAM, ...) { BODY }` definitions off the
/// (comment-stripped) script. Returns the table and the remaining script.
fn parse_prologue(script: &str) -> Result<(FnTable, &str), Error> {
    parse_prologue_noting(script, None)
}

/// [`parse_prologue`], noting each definition's parts in `rec` when there
/// is one: `fn`, the name, the parameters, and each body's stages.
///
/// With a recorder, a definition that fails does not stop the reading: it
/// is skipped (up to the `}` that closes its body, when there is one), the
/// definitions after it are read and noted, and each stage after them gets
/// its command word noted, as after a stage that fails. The first error is
/// still what comes back.
fn parse_prologue_noting<'s>(
    script: &'s str,
    mut rec: Option<&mut Recorder>,
) -> Result<(FnTable, &'s str), Error> {
    use std::collections::hash_map::Entry;
    let mut fns = FnTable::new();
    // The bodies as written, noted once every fragment is defined.
    let mut bodies: Vec<&'s str> = Vec::new();
    // The first error, kept while a recorder reads on past it.
    let mut first_error: Option<Error> = None;
    let mut rest = script.trim_start();
    loop {
        let (word, after) = split_first_word(rest);
        if word != "fn" {
            break;
        }
        if let Some(rec) = rec.as_deref_mut() {
            rec.note(script, word, SpanKind::Keyword);
        }
        let error = match parse_fn_def(script, after, rec.as_deref_mut()) {
            Ok((name, def, body, after_body)) => {
                rest = after_body.trim_start();
                bodies.push(body);
                match fns.entry(name) {
                    Entry::Vacant(slot) => {
                        slot.insert(def);
                        continue;
                    }
                    Entry::Occupied(slot) => err(format!("fn `{}` is defined twice", slot.key())),
                }
            }
            Err(e) => {
                // Read on after the body's closing `}`; without one, the
                // rest of the script is the body.
                rest = after
                    .find('{')
                    .and_then(|brace| take_brace_group(&after[brace..]).ok())
                    .map_or("", |(_, after_body)| after_body.trim_start());
                e
            }
        };
        if rec.is_none() {
            return Err(error);
        }
        first_error.get_or_insert(error);
    }
    if let Some(rec) = rec {
        for body in &bodies {
            // A body's parameters stand for text each call gives, so it may
            // not parse on its own: what parses is noted, and its errors
            // are dropped. At the depth limit a fragment call in it is
            // noted but not expanded.
            let _ = parse_stages(body, &fns, MAX_FN_DEPTH, script, Some(&mut *rec));
        }
        if first_error.is_some() {
            let stages = split_stages(rest);
            let mut builder = Builder::new(&fns, 0, script, Some(rec));
            builder.note_separators(rest, &stages);
            builder.note_commands(&stages);
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok((fns, rest)),
    }
}

/// Parse one definition, `after` being the text after its `fn`: its name,
/// the fragment, its body as written, and the text after the body. The
/// name and parameters are noted in `rec` when there is one.
fn parse_fn_def<'s>(
    script: &str,
    after: &'s str,
    rec: Option<&mut Recorder>,
) -> Result<(String, FnDef, &'s str, &'s str), Error> {
    let brace = after
        .find('{')
        .ok_or_else(|| err("fn: malformed definition — expected `fn NAME(PARAMS) { BODY }`"))?;
    let header = after[..brace].trim();
    let (name, params_text) = header
        .split_once('(')
        .ok_or_else(|| err("fn: malformed definition — expected `fn NAME(PARAMS) { BODY }`"))?;
    let name = name.trim();
    if !is_ident(name) {
        return Err(err(format!(
            "fn: `{name}` is not a valid name (bare identifier)"
        )));
    }
    if is_reserved(name) {
        return Err(err(format!("fn `{name}` collides with a built-in command")));
    }
    let params_text = params_text
        .trim()
        .strip_suffix(')')
        .ok_or_else(|| err(format!("fn `{name}`: malformed parameter list")))?;
    if let Some(rec) = rec {
        rec.note(script, name, SpanKind::Function);
        for (_, at) in split_items(params_text, false, false).0 {
            if let Some(param) = params_text.get(at) {
                rec.note(script, param, SpanKind::Variable);
            }
        }
    }
    let mut params = Vec::new();
    for p in split_list(params_text) {
        if !is_ident(&p) {
            return Err(err(format!(
                "fn `{name}`: `{p}` is not a valid parameter name"
            )));
        }
        if params.contains(&p) {
            return Err(err(format!("fn `{name}`: duplicate parameter `{p}`")));
        }
        params.push(p);
    }
    let (body, after_body) = take_brace_group(&after[brace..])
        .map_err(|_| err(format!("fn `{name}`: unterminated body — missing `}}`")))?;
    let def = FnDef {
        params,
        body: body.trim().to_string(),
    };
    Ok((name.to_string(), def, body, after_body))
}

/// Split off the first token: a whitespace-delimited word, or a quoted run
/// (`'…'`/`"…"`/`` `…` ``, surrounding quotes stripped) for paths with spaces.
fn take_token(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    let bytes = s.as_bytes();
    match bytes.first() {
        Some(&q @ (b'"' | b'\'' | b'`')) => match s[1..].find(q as char) {
            Some(end) => (&s[1..1 + end], &s[2 + end..]),
            None => (&s[1..], ""), // unterminated: take the rest
        },
        _ => split_first_word(s),
    }
}

/// Match a `-x VALUE` / `-x=VALUE` flag under any of its spellings (`names`
/// lists the short and long forms). `None` if `word` isn't this flag;
/// otherwise the value and the input remaining after it.
fn flag_value<'a>(
    word: &str,
    after: &'a str,
    names: &[&str],
) -> Option<Result<(String, &'a str), Error>> {
    for name in names {
        let Some(tail) = word.strip_prefix(name) else {
            continue;
        };
        if tail.is_empty() {
            let (val, rest) = take_token(after);
            if val.is_empty() {
                return Some(Err(err(format!("{name} expects a value"))));
            }
            return Some(Ok((val.to_string(), rest)));
        }
        if let Some(val) = tail.strip_prefix('=') {
            return Some(Ok((val.to_string(), after)));
        }
    }
    None
}

/// `NAME(ARGS)` filling the whole stage — the fragment-call shape. Returns
/// the name and the raw argument text when the stage is exactly one
/// identifier followed by one balanced paren group.
fn fragment_call(stage: &str) -> Option<(&str, &str)> {
    let open = stage.find('(')?;
    let name = &stage[..open];
    if !is_ident(name) {
        return None;
    }
    let (inner, after) = take_paren_group(&stage[open..]).ok()?;
    after.trim().is_empty().then_some((name, inner))
}

/// Split off the first whitespace-delimited word (the command) from the rest.
fn split_first_word(stage: &str) -> (&str, &str) {
    match stage.find(char::is_whitespace) {
        Some(p) => (&stage[..p], stage[p..].trim_start()),
        None => (stage, ""),
    }
}

/// Split a `NAME = …` assignment: the name, everything after it, and what
/// follows a single `=` (`None` when there is no `=`, or a `==`, which is a
/// comparison). A quoted name (`'`, `"` or backticks) may contain spaces;
/// otherwise the name ends at whitespace or at the `=`.
fn split_name_eq(rest: &str) -> Result<(String, &str, Option<&str>), Error> {
    let rest = rest.trim_start();
    let (name, after) = if let Some(q) = rest.chars().next().filter(|c| "'\"`".contains(*c)) {
        let body = &rest[1..];
        match body.find(q) {
            Some(end) => (body[..end].to_string(), body[end + 1..].trim_start()),
            None => return Err(err("unterminated quoted column name")),
        }
    } else {
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or(rest.len());
        (rest[..end].to_string(), rest[end..].trim_start())
    };
    let assigned = match after.strip_prefix('=') {
        Some(e) if !e.starts_with('=') => Some(e),
        _ => None,
    };
    Ok((name, after, assigned))
}

/// Split `s` on commas outside quotes and parens — `join`'s item separator.
/// Only quoted and parenthesized (sub-pipeline) commas are protected; a
/// composite key list's commas split here too, and the key-continuation step
/// in `parse_join` stitches those fragments back onto their `on` clause.
fn split_top_commas(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut start = 0;
    for (i, &c) in bytes.iter().enumerate() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == b'"' || c == b'\'' || c == b'`' => quote = Some(c),
            None if c == b'(' => depth += 1,
            None if c == b')' => depth = depth.saturating_sub(1),
            None if c == b',' && depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            None => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// True when `word` appears as a bare (unquoted) whitespace-delimited token.
fn has_bare_word(s: &str, word: &str) -> bool {
    let mut rest = s.trim_start();
    while !rest.is_empty() {
        let quoted = matches!(rest.as_bytes()[0], b'"' | b'\'' | b'`');
        let (tok, after) = take_token(rest);
        if !quoted && tok == word {
            return true;
        }
        // Always progresses: a quoted token consumes its quotes even when
        // empty, and an unquoted token is non-empty (`rest` is trimmed).
        rest = after.trim_start();
    }
    false
}

/// True when the separator `c` sits next to an unquoted `=` and so does not
/// end an item (`a = b` is the one item `a=b`): `after_eq` says the item so
/// far ends in one, `rest` is the text from `c` on.
fn joins_at_eq(c: char, after_eq: bool, rest: &str) -> bool {
    c.is_whitespace() && (after_eq || rest.trim_start().starts_with('='))
}

/// Split an argument string into items on commas and whitespace outside
/// quotes (`'`, `"` and backticks all quote, so a column name with a
/// comma/space can be written `` `odd, name` ``). With `keep_quotes` the
/// quote characters stay in the item, else they are stripped; with
/// `nest_parens` a `func(a, b)` group is one item. An unquoted `=` binds
/// tighter than whitespace: `a = b` is one item. Each item comes with the
/// byte range of `s` it is written at, quotes included. The second value is
/// a quote left open at the end (its text is in the last item).
fn split_items(
    s: &str,
    keep_quotes: bool,
    nest_parens: bool,
) -> (Vec<(String, Range<usize>)>, Option<char>) {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0i32;
    let mut in_item = false;
    // Where the current item is written.
    let mut at = 0..0;
    // The last character pushed was an unquoted `=`.
    let mut after_eq = false;
    for (i, c) in s.char_indices() {
        let next = i + c.len_utf8();
        match quote {
            Some(q) if c == q => {
                quote = None;
                if keep_quotes {
                    cur.push(c);
                }
                at.end = next;
            }
            Some(_) => {
                cur.push(c);
                at.end = next;
            }
            None if c == '"' || c == '\'' || c == '`' => {
                quote = Some(c);
                if !in_item {
                    at.start = i;
                }
                in_item = true;
                after_eq = false;
                if keep_quotes {
                    cur.push(c);
                }
                at.end = next;
            }
            None if depth == 0 && (c == ',' || c.is_whitespace()) => {
                if in_item && !joins_at_eq(c, after_eq, &s[i..]) {
                    out.push((std::mem::take(&mut cur), at.clone()));
                    in_item = false;
                }
            }
            None => {
                if nest_parens && c == '(' {
                    depth += 1;
                } else if nest_parens && c == ')' {
                    depth -= 1;
                }
                cur.push(c);
                if !in_item {
                    at.start = i;
                }
                in_item = true;
                after_eq = c == '=';
                at.end = next;
            }
        }
    }
    if in_item {
        out.push((cur, at));
    }
    (out, quote)
}

/// A column/argument list: [`split_items`] with the quotes stripped (an
/// open quote runs to the end).
fn split_list(s: &str) -> Vec<String> {
    split_items(s, false, false)
        .0
        .into_iter()
        .map(|(item, _)| item)
        .collect()
}

/// An `agg` argument: [`split_items`] keeping `func(col)` groups and quoted
/// names intact, quotes included, so `by` can be told from `'by'` and a
/// trailing `=` is known to be unquoted. An open quote is an error.
fn split_specs(s: &str) -> Result<Vec<(String, Range<usize>)>, Error> {
    match split_items(s, true, true) {
        (items, None) => Ok(items),
        (_, Some('`')) => Err(err("agg: unterminated backtick")),
        (_, Some(q)) => Err(err(format!(
            "agg: unterminated {q} quote; backtick a column name containing a quote"
        ))),
    }
}

/// Replace each parameter, wherever it appears in `body` as a whole
/// identifier outside quoted literals, with its argument's verbatim text.
/// Textual on purpose: this is what lets one mechanism parameterize file
/// operands, column names, rename halves, and expression operands alike.
fn subst_params(body: &str, params: &[String], args: &[&str]) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    let mut quote: Option<char> = None;
    while let Some(c) = rest.chars().next() {
        match quote {
            Some(q) => {
                out.push(c);
                if c == q {
                    quote = None;
                }
                rest = &rest[c.len_utf8()..];
            }
            None if matches!(c, '"' | '\'' | '`') => {
                quote = Some(c);
                out.push(c);
                rest = &rest[1..];
            }
            None if c.is_ascii_alphanumeric() || c == '_' => {
                // Consume the whole identifier-ish run; only a run that
                // starts like an identifier can be a parameter (`9a` can't).
                let end = rest
                    .find(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                    .unwrap_or(rest.len());
                let word = &rest[..end];
                match params.iter().position(|p| p == word) {
                    Some(k) if is_ident(word) => out.push_str(args[k]),
                    _ => out.push_str(word),
                }
                rest = &rest[end..];
            }
            None => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out
}

/// The chart type `word` names, if any.
fn graph_kind(word: &str) -> Option<GraphKind> {
    match word {
        "hist" | "histogram" => Some(GraphKind::Hist),
        "bar" => Some(GraphKind::Bar),
        "spark" | "sparkline" => Some(GraphKind::Spark),
        "scatter" => Some(GraphKind::Scatter),
        "line" => Some(GraphKind::Line),
        "heatmap" | "heat" => Some(GraphKind::Heatmap),
        _ => None,
    }
}

/// The chart for `ncols` columns given without a type: one column's
/// distribution, else a line of the others against the first.
fn default_graph_kind(ncols: usize) -> Result<GraphKind, Error> {
    match ncols {
        0 => Err(err(format!(
            "graph expects columns, e.g. graph date price, or a chart type first: {}",
            GraphKind::name_list()
        ))),
        1 => Ok(GraphKind::Hist),
        2.. => Ok(GraphKind::Line),
    }
}

/// Validate the column count for a chart type: hist/spark take one, bar takes
/// two, scatter/line take an x plus one or more y-series, and a heatmap takes
/// exactly an x and a y (its second dimension is binned, not a series).
fn check_graph_arity(kind: GraphKind, word: &str, n: usize) -> Result<(), Error> {
    let ok = match kind {
        GraphKind::Hist | GraphKind::Spark => n == 1,
        GraphKind::Bar => n >= 2,
        GraphKind::Scatter | GraphKind::Line => n >= 2,
        GraphKind::Heatmap => n == 2,
    };
    if ok {
        return Ok(());
    }
    let want = match kind {
        GraphKind::Hist | GraphKind::Spark => "one column",
        GraphKind::Bar => "a label column and one or more value columns",
        GraphKind::Scatter | GraphKind::Line => "an x column and one or more y columns",
        GraphKind::Heatmap => "an x column and a y column",
    };
    Err(err(format!("graph {word} expects {want}, got {n}")))
}

/// Reject a flag the chart kind has no use for (a `hist` bins along x only,
/// `bar`/`spark` have no x axis to range, and only a scatter/line has points to
/// colour), or a range a log axis cannot span. `ncols` is the column count, so
/// the one-y-series flags can be checked here too.
fn check_graph_flags(
    kind: GraphKind,
    word: &str,
    opts: &GraphOpts,
    ncols: usize,
) -> Result<(), Error> {
    let no = |flag: &str| Err(err(format!("graph {word} does not take {flag}")));
    let xy = matches!(kind, GraphKind::Scatter | GraphKind::Line);
    if opts.color_by.is_some() && !xy {
        return no("-c/--color-by");
    }
    // Both colourings paint one value per point, and a cell shared by several
    // series has no single value to paint, so they need one y series.
    if xy && ncols > 2 {
        let one = |flag: &str| Err(err(format!("graph {word} takes {flag} with one y series")));
        if opts.color_by.is_some() {
            return one("-c/--color-by");
        }
        if opts.ramp.is_some() {
            return one("-r/--ramp");
        }
    }
    // Same rule on a grouped bar: with several value columns the series
    // palette says which column a bar belongs to, so a ramp by value has
    // nowhere to paint.
    if kind == GraphKind::Bar && opts.ramp.is_some() && ncols > 2 {
        return Err(err(format!(
            "graph {word} takes -r/--ramp with one value column"
        )));
    }
    if opts.xrange.is_some()
        && !matches!(
            kind,
            GraphKind::Hist | GraphKind::Scatter | GraphKind::Line | GraphKind::Heatmap
        )
    {
        return no("-x/--xrange");
    }
    if opts.yrange.is_some() && matches!(kind, GraphKind::Hist) {
        return no("-y/--yrange");
    }
    // A log axis has no room for a bound at or below zero: the range is the
    // axis, and log10 of a non-positive number does not exist. A heatmap is the
    // exception — both of its own axes are binned, so its `-l` is the *count*
    // axis and leaves the y range alone.
    if opts.log && kind != GraphKind::Heatmap && opts.yrange.is_some_and(|(lo, _)| lo <= 0.0) {
        return Err(err(format!(
            "graph {word}: -l/--log needs a positive -y range"
        )));
    }
    Ok(())
}

/// Parse `lo:hi` for an axis range flag; `lo` must be below `hi`.
fn parse_range(s: &str, flag: &str) -> Result<(f64, f64), Error> {
    let bad = || err(format!("{flag} expects lo:hi with lo < hi, got `{s}`"));
    let (lo, hi) = s.split_once(':').ok_or_else(bad)?;
    let (lo, hi): (f64, f64) = (
        lo.parse().map_err(|_| bad())?,
        hi.parse().map_err(|_| bad())?,
    );
    if !(lo.is_finite() && hi.is_finite() && lo < hi) {
        return Err(bad());
    }
    Ok((lo, hi))
}

/// Parse a positive-integer flag value (`--bins`, `--width`, `--height`), up to
/// `max`.
fn parse_positive(s: &str, name: &str, max: usize) -> Result<usize, Error> {
    s.parse::<usize>()
        .ok()
        .filter(|&n| (1..=max).contains(&n))
        .ok_or_else(|| {
            err(format!(
                "{name} expects a positive integer up to {max}, got `{s}`"
            ))
        })
}

/// Parse the `--scale` factor: a positive, finite number.
fn parse_scale(s: &str) -> Result<f64, Error> {
    s.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or_else(|| err(format!("-s/--scale expects a positive number, got `{s}`")))
}

/// Push a window (`tail +N` is `skip` rows, `head N` a `limit`), folding it
/// into the window the stages end with: a run of `head` / `tail +N` stages
/// is one window, kept as `[Skip(a)?, Head(l)?]`. `skip a | head l | skip b`
/// is `skip a+b | head l-b`, and a second `head` keeps the smaller limit. A
/// `Skip(0)` is dropped once there is a limit (a bare one stays, as `tail +1`
/// on its own).
fn push_window(stages: &mut Vec<Stage>, skip: usize, limit: Option<usize>) {
    let (prev_skip, prev_limit, start) = match stages.as_slice() {
        [.., Stage::Skip(a), Stage::Head(l)] => (*a, Some(*l), stages.len() - 2),
        [.., Stage::Skip(a)] => (*a, None, stages.len() - 1),
        [.., Stage::Head(l)] => (0, Some(*l), stages.len() - 1),
        _ => (0, None, stages.len()),
    };
    let limit = match (prev_limit.map(|l| l.saturating_sub(skip)), limit) {
        (Some(l), Some(m)) => Some(l.min(m)),
        (l, m) => l.or(m),
    };
    let skip = prev_skip.saturating_add(skip);
    stages.truncate(start);
    if skip > 0 || limit.is_none() {
        stages.push(Stage::Skip(skip));
    }
    if let Some(l) = limit {
        stages.push(Stage::Head(l));
    }
}

/// `s` without one pair of surrounding quotes (`'`, `"` or backticks), if it
/// has one; the items of [`split_specs`] keep theirs. Stricter than
/// [`take_token`]: text after the closing quote, or an unterminated quote,
/// is left as it is.
fn unquote(s: &str) -> &str {
    match s.as_bytes() {
        [q @ (b'\'' | b'"' | b'`'), .., last] if last == q => &s[1..s.len() - 1],
        _ => s,
    }
}

/// Parse one aggregate spec: `func` (only `count` may omit a column),
/// `func(col)`, or `NAME=func(col)`. A given `NAME` is the output column;
/// otherwise the name is left unset for resolve to default to `col_func`
/// (`amount_sum`), or `count`.
fn parse_agg_spec(tok: &str) -> Result<AggSpec, Error> {
    // `NAME=FN(col)` names the output column.
    let (given, tok) = match split_name_eq(tok)? {
        (name, _, Some(spec)) => {
            if name.is_empty() {
                return Err(err(format!("agg: `{tok}` has an empty output name")));
            }
            (Some(name), spec.trim())
        }
        (_, _, None) => (None, tok.trim()),
    };
    if tok.is_empty() {
        return Err(err(
            "agg: `NAME=` needs an aggregate, e.g. total=sum(amount)",
        ));
    }
    let (func_name, col) = match tok.find('(') {
        Some(open) => {
            let inner = tok[open + 1..]
                .strip_suffix(')')
                .ok_or_else(|| err(format!("agg: malformed aggregate `{tok}`")))?;
            let col = unquote(inner.trim());
            if col.is_empty() {
                return Err(err(format!("agg: `{}` needs a column name", &tok[..open])));
            }
            (tok[..open].trim(), Some(col.to_string()))
        }
        None => (tok.trim(), None),
    };
    let func = match func_name {
        "count" => AggFunc::Count,
        "count_distinct" => AggFunc::CountDistinct,
        "sum" => AggFunc::Sum,
        "min" => AggFunc::Min,
        "max" => AggFunc::Max,
        "mean" | "avg" => AggFunc::Mean,
        "stddev" | "std" => AggFunc::Stddev,
        other => return Err(err(format!("agg: unknown function `{other}`"))),
    };
    // Only `count` aggregates rows; every other function needs a column.
    if col.is_none() && func != AggFunc::Count {
        return Err(err(format!(
            "agg: {func_name} needs a column, e.g. {func_name}(col)"
        )));
    }
    Ok(AggSpec {
        func,
        col,
        pos: None,
        name: given,
    })
}

// --- expression lexer -------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum ETok {
    Ident(String),
    Num(f64),
    /// A bare word that is a number (`inf`, `nan`), kept with its text.
    Word(String, f64),
    Str(String),
    Sym(&'static str),
}

/// The tokens of the expression `s`, each with its byte range in `s`. An
/// error comes with the range of the first character of the token the lexer
/// was reading.
#[allow(
    clippy::type_complexity,
    reason = "the two halves of a lex, as the one caller wants them"
)]
fn lex_expr(s: &str) -> Result<(Vec<ETok>, Vec<Range<usize>>), (Error, Range<usize>)> {
    let cs: Vec<char> = s.chars().collect();
    // The byte offset of each char, and of the end.
    let bytes: Vec<usize> = s.char_indices().map(|(b, _)| b).chain([s.len()]).collect();
    let (mut toks, mut spans, mut at) = (Vec::new(), Vec::new(), 0);
    match lex_tokens(&cs, &mut toks, &mut spans, &mut at) {
        Ok(()) => Ok((
            toks,
            spans
                .into_iter()
                .map(|r| bytes[r.start]..bytes[r.end])
                .collect(),
        )),
        Err(e) => Err((e, bytes[at]..bytes[(at + 1).min(cs.len())])),
    }
}

/// Lex `cs` into `toks`, with each token's char range in `spans`; `at` is the
/// char the token being lexed starts at, which an error is placed on.
fn lex_tokens(
    cs: &[char],
    toks: &mut Vec<ETok>,
    spans: &mut Vec<Range<usize>>,
    at: &mut usize,
) -> Result<(), Error> {
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        *at = i;
        match c {
            '(' => push_sym(toks, "(", &mut i),
            ')' => push_sym(toks, ")", &mut i),
            '\'' | '"' => {
                i += 1;
                let mut lit = String::new();
                while i < cs.len() && cs[i] != c {
                    lit.push(cs[i]);
                    i += 1;
                }
                if i >= cs.len() {
                    return Err(err("unterminated string literal in expression"));
                }
                i += 1; // closing quote
                toks.push(ETok::Str(lit));
            }
            '`' => {
                // Backtick quotes a column name that isn't a bare identifier
                // (e.g. it contains '-'); emit an Ident (column ref), not a Str.
                i += 1;
                let start = i;
                while i < cs.len() && cs[i] != '`' {
                    i += 1;
                }
                if i >= cs.len() {
                    return Err(err("unterminated backtick column name in expression"));
                }
                if i == start {
                    return Err(err("empty backtick column name in expression"));
                }
                let name: String = cs[start..i].iter().collect();
                i += 1; // closing backtick
                toks.push(ETok::Ident(name));
            }
            '=' => match cs.get(i + 1) {
                Some('=') => push2(toks, "==", &mut i),
                Some('~') => push2(toks, "=~", &mut i),
                _ => push_sym(toks, "==", &mut i), // a lone `=` means equals
            },
            '!' => match cs.get(i + 1) {
                Some('=') => push2(toks, "!=", &mut i),
                Some('~') => push2(toks, "!~", &mut i),
                _ => push_sym(toks, "!", &mut i),
            },
            '<' => match cs.get(i + 1) {
                Some('=') => push2(toks, "<=", &mut i),
                _ => push_sym(toks, "<", &mut i),
            },
            '>' => match cs.get(i + 1) {
                Some('=') => push2(toks, ">=", &mut i),
                _ => push_sym(toks, ">", &mut i),
            },
            '&' if cs.get(i + 1) == Some(&'&') => push2(toks, "&&", &mut i),
            '|' if cs.get(i + 1) == Some(&'|') => push2(toks, "||", &mut i),
            // Affix operators: begins-with / contains / ends-with. A lone
            // `^`/`$` is reserved (no exponent operator), so it errors.
            '^' if cs.get(i + 1) == Some(&'=') => push2(toks, "^=", &mut i),
            '$' if cs.get(i + 1) == Some(&'=') => push2(toks, "$=", &mut i),
            '*' if cs.get(i + 1) == Some(&'=') => push2(toks, "*=", &mut i),
            // `++` is string concat (for `add`); kept distinct from `+`.
            '+' if cs.get(i + 1) == Some(&'+') => push2(toks, "++", &mut i),
            // A leading `+`/`-` is part of a numeric literal only in *unary*
            // position (expression start, or right after an operator/`(`). After
            // a value it is the binary add/subtract operator — so `amount - 5`
            // subtracts, while `a > -5` compares against negative five.
            '-' | '+' if !ends_value(toks) && starts_number(&cs[i + 1..]) => {
                lex_number(cs, &mut i, toks)?;
            }
            // Arithmetic / value-expression operators (used by `add`).
            '+' => push_sym(toks, "+", &mut i),
            '-' => push_sym(toks, "-", &mut i),
            '*' => push_sym(toks, "*", &mut i),
            '/' => push_sym(toks, "/", &mut i),
            '%' => push_sym(toks, "%", &mut i),
            '?' => push_sym(toks, "?", &mut i),
            ':' => push_sym(toks, ":", &mut i),
            ',' => push_sym(toks, ",", &mut i),
            _ if starts_number(&cs[i..]) => lex_number(cs, &mut i, toks)?,
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '_' || cs[i] == '.') {
                    i += 1;
                }
                let word: String = cs[start..i].iter().collect();
                // `inf`, `infinity` and `nan` (any case) are numbers, as in a
                // cell; a column with such a name needs backticks.
                match word.parse::<f64>() {
                    Ok(n) => toks.push(ETok::Word(word, n)),
                    Err(_) => toks.push(ETok::Ident(word)),
                }
            }
            other => return Err(err(format!("unexpected character '{other}' in expression"))),
        }
        spans.push(*at..i);
    }
    Ok(())
}

/// Whether the last lexed token completes a value (a literal, a column, or a
/// closing paren). A following `+`/`-` is then a binary operator, not a sign.
fn ends_value(toks: &[ETok]) -> bool {
    matches!(
        toks.last(),
        Some(ETok::Num(_) | ETok::Word(..) | ETok::Ident(_) | ETok::Str(_) | ETok::Sym(")"))
    )
}

fn push_sym(toks: &mut Vec<ETok>, s: &'static str, i: &mut usize) {
    toks.push(ETok::Sym(s));
    *i += 1;
}
fn push2(toks: &mut Vec<ETok>, s: &'static str, i: &mut usize) {
    toks.push(ETok::Sym(s));
    *i += 2;
}

/// Whether a number literal starts here: a digit, or `.` then a digit.
fn starts_number(cs: &[char]) -> bool {
    match cs {
        [d, ..] if d.is_ascii_digit() => true,
        ['.', d, ..] => d.is_ascii_digit(),
        _ => false,
    }
}

fn lex_number(cs: &[char], i: &mut usize, toks: &mut Vec<ETok>) -> Result<(), Error> {
    let start = *i;
    if cs[*i] == '-' || cs[*i] == '+' {
        *i += 1;
    }
    while *i < cs.len() && (cs[*i].is_ascii_digit() || cs[*i] == '.') {
        *i += 1;
    }
    // An exponent: `e`/`E`, an optional sign, digits (`1e3`, `2.5E-3`). The
    // float parse below rejects a malformed one such as `1e`.
    if matches!(cs.get(*i), Some('e' | 'E')) {
        *i += 1;
        if matches!(cs.get(*i), Some('+' | '-')) {
            *i += 1;
        }
        while *i < cs.len() && cs[*i].is_ascii_digit() {
            *i += 1;
        }
    }
    let text: String = cs[start..*i].iter().collect();
    let n = text
        .parse::<f64>()
        .map_err(|_| err(format!("invalid number '{text}' in expression")))?;
    toks.push(ETok::Num(n));
    Ok(())
}

// --- expression parser (recursive descent) ----------------------------------

struct ExprParser {
    toks: Vec<ETok>,
    /// Each token's byte range in the expression.
    spans: Vec<Range<usize>>,
    /// The expression's length, where "end of expression" is.
    end: usize,
    pos: usize,
    /// Where the error being returned is, when that is not the token at the
    /// cursor: a part already read past, such as a call to its `)`.
    fail_span: Option<Range<usize>>,
    /// Each column the expression reads, with the byte range it is written
    /// at.
    columns: Vec<(String, Range<usize>)>,
}

/// A parsed subexpression that is either a boolean or a value. The unified
/// grammar parses each position once and classifies afterward; a
/// `V(ValExpr::Bool(_))` is a parenthesized boolean usable as either.
enum BV {
    B(BoolExpr),
    V(ValExpr),
}

impl ExprParser {
    /// The byte range a parse error is placed on: the part it names when one
    /// was given ([`ExprParser::fail_on`]), else the token at the cursor, or
    /// the end of the expression past the last one.
    fn error_span(&self) -> Range<usize> {
        if let Some(span) = &self.fail_span {
            return span.clone();
        }
        self.spans
            .get(self.pos)
            .cloned()
            .unwrap_or(self.end..self.end)
    }

    /// `e`, about the part of the expression made of the tokens at `tokens`
    /// (indices, the end one past the last), placed on that part.
    fn fail_on(&mut self, tokens: Range<usize>, e: Error) -> Error {
        let last = tokens.end.saturating_sub(1).max(tokens.start);
        self.fail_span = Some(self.spans[tokens.start].start..self.spans[last].end);
        e
    }

    /// The token at the cursor, quoted, for an error message — or "end of
    /// expression" when the cursor is past the last token.
    fn here(&self) -> String {
        match self.toks.get(self.pos) {
            Some(ETok::Ident(s)) | Some(ETok::Str(s)) | Some(ETok::Word(s, _)) => format!("'{s}'"),
            Some(ETok::Num(n)) => format!("'{}'", crate::field::format_num(*n)),
            Some(ETok::Sym(s)) => format!("'{s}'"),
            None => "end of expression".to_string(),
        }
    }

    fn parse(&mut self) -> Result<BoolExpr, Error> {
        let expr = self.parse_bv()?;
        let expr = self.need_bool(expr)?;
        if self.pos != self.toks.len() {
            return Err(err(format!(
                "unexpected {} after the expression",
                self.here()
            )));
        }
        Ok(expr)
    }

    fn eat(&mut self, sym: &str) -> bool {
        if matches!(self.toks.get(self.pos), Some(ETok::Sym(s)) if *s == sym) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Whether the cursor is at the symbol `sym` (without consuming it).
    fn at(&self, sym: &str) -> bool {
        matches!(self.toks.get(self.pos), Some(ETok::Sym(s)) if *s == sym)
    }

    /// Require a boolean where the grammar demands one, unwrapping a
    /// parenthesized boolean that came back as a value (`(a > 0) && …`).
    fn need_bool(&self, e: BV) -> Result<BoolExpr, Error> {
        match e {
            BV::B(b) => Ok(b),
            BV::V(ValExpr::Bool(b)) => Ok(*b),
            BV::V(_) => Err(err(format!(
                "expected a comparison operator (==, !=, <, >, <=, >=, =~, ^=, *=, $=), found {}",
                self.here()
            ))),
        }
    }

    // The boolean/value grammar is single-pass: every token position parses
    // once, and whether a subexpression is a boolean or a value is settled by
    // lookahead (a following comparison operator, `&&`, `?`, …), never by
    // re-parsing. `parse_bv_cmp` parses a value and upgrades it to a
    // comparison if an operator follows; the connective levels demand booleans
    // via `need_bool`.

    /// `||` level.
    fn parse_bv(&mut self) -> Result<BV, Error> {
        let first = self.parse_bv_and()?;
        if !self.at("||") {
            return Ok(first);
        }
        let mut parts = vec![self.need_bool(first)?];
        while self.eat("||") {
            let next = self.parse_bv_and()?;
            parts.push(self.need_bool(next)?);
        }
        Ok(BV::B(BoolExpr::Or(parts)))
    }

    /// `&&` level.
    fn parse_bv_and(&mut self) -> Result<BV, Error> {
        let first = self.parse_bv_not()?;
        if !self.at("&&") {
            return Ok(first);
        }
        let mut parts = vec![self.need_bool(first)?];
        while self.eat("&&") {
            let next = self.parse_bv_not()?;
            parts.push(self.need_bool(next)?);
        }
        Ok(BV::B(BoolExpr::And(parts)))
    }

    /// `!` level.
    fn parse_bv_not(&mut self) -> Result<BV, Error> {
        if self.eat("!") {
            let operand = self.parse_bv_not()?;
            Ok(BV::B(BoolExpr::Not(Box::new(self.need_bool(operand)?))))
        } else {
            self.parse_bv_cmp()
        }
    }

    /// Comparison level: a value, optionally completed into a comparison by a
    /// following operator. Without one it stays a value — the enclosing level
    /// decides whether that is acceptable.
    fn parse_bv_cmp(&mut self) -> Result<BV, Error> {
        let lhs_at = self.pos;
        let lhs = self.parse_concat()?;
        let lhs_tokens = lhs_at..self.pos;
        let op = match self.toks.get(self.pos) {
            Some(ETok::Sym(s))
                if matches!(
                    *s,
                    "==" | "!=" | "<" | ">" | "<=" | ">=" | "=~" | "!~" | "^=" | "*=" | "$="
                ) =>
            {
                *s
            }
            _ => return Ok(BV::V(lhs)),
        };
        self.pos += 1;
        let rhs_at = self.pos;
        if op == "=~" || op == "!~" {
            let ValExpr::Col(col) = lhs else {
                return Err(self.fail_on(lhs_tokens, err("left side of =~ must be a column")));
            };
            let pattern = match self.parse_concat()? {
                ValExpr::Str(s) => s,
                _ => {
                    let e = err("=~ pattern must be a string");
                    return Err(self.fail_on(rhs_at..self.pos, e));
                }
            };
            let regex = regex::Regex::new(&pattern).map_err(|e| {
                let e = err(format!("invalid regex '{pattern}': {e}"));
                self.fail_on(rhs_at..self.pos, e)
            })?;
            return Ok(BV::B(BoolExpr::Match {
                col,
                regex,
                negate: op == "!~",
            }));
        }
        let affix = match op {
            "^=" => Some(AffixKind::StartsWith),
            "*=" => Some(AffixKind::Contains),
            "$=" => Some(AffixKind::EndsWith),
            _ => None,
        };
        if let Some(kind) = affix {
            let ValExpr::Col(col) = lhs else {
                let e = err(format!("left side of {op} must be a column"));
                return Err(self.fail_on(lhs_tokens, e));
            };
            let needle = match self.parse_concat()? {
                ValExpr::Str(s) => s,
                _ => {
                    let e = err(format!("{op} needs a string literal on the right"));
                    return Err(self.fail_on(rhs_at..self.pos, e));
                }
            };
            return Ok(BV::B(BoolExpr::Affix { col, needle, kind }));
        }
        let cmp_op = match op {
            "==" => CmpOp::Eq,
            "!=" => CmpOp::Ne,
            "<" => CmpOp::Lt,
            ">" => CmpOp::Gt,
            "<=" => CmpOp::Le,
            ">=" => CmpOp::Ge,
            _ => unreachable!("filtered above"),
        };
        let rhs = self.parse_concat()?;
        // The mode is decided in `Plan::resolve`, where column types are known.
        Ok(BV::B(BoolExpr::Cmp(Cmp {
            op: cmp_op,
            lhs,
            rhs,
            mode: CmpMode::Auto,
        })))
    }

    // --- value expressions (for `add`) --------------------------------------

    /// Parse a complete value expression, erroring on trailing tokens.
    fn parse_value_top(&mut self) -> Result<ValExpr, Error> {
        let expr = self.parse_value()?;
        if self.pos != self.toks.len() {
            return Err(err(format!(
                "unexpected {} after the expression",
                self.here()
            )));
        }
        Ok(expr)
    }

    /// A value expression. Precedence (loosest first): `?:` ternary, the
    /// boolean connectives, `++` concat, `+ -`, `* / %`, unary `-`, then
    /// atoms. A boolean subexpression used as a value (`add ok = amount > 0`,
    /// `(a > 0) ++ '!'`) renders csvm-style `t`/`f`.
    fn parse_value(&mut self) -> Result<ValExpr, Error> {
        let e = self.parse_bv()?;
        if self.at("?") {
            let test = self.need_bool(e)?;
            self.pos += 1;
            let then_ = self.parse_value()?;
            if !self.eat(":") {
                return Err(err("expected ':' in ?: expression"));
            }
            let else_ = self.parse_value()?;
            return Ok(ValExpr::Cond {
                test: Box::new(test),
                then_: Box::new(then_),
                else_: Box::new(else_),
            });
        }
        Ok(match e {
            BV::B(b) => ValExpr::Bool(Box::new(b)),
            BV::V(v) => v,
        })
    }

    fn parse_concat(&mut self) -> Result<ValExpr, Error> {
        let mut parts = vec![self.parse_additive()?];
        while self.eat("++") {
            parts.push(self.parse_additive()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            ValExpr::Concat(parts)
        })
    }

    fn parse_additive(&mut self) -> Result<ValExpr, Error> {
        let mut e = self.parse_mul()?;
        loop {
            let op = if self.eat("+") {
                ArithOp::Add
            } else if self.eat("-") {
                ArithOp::Sub
            } else {
                break;
            };
            let rhs = self.parse_mul()?;
            e = ValExpr::Arith {
                op,
                lhs: Box::new(e),
                rhs: Box::new(rhs),
            };
        }
        Ok(e)
    }

    fn parse_mul(&mut self) -> Result<ValExpr, Error> {
        let mut e = self.parse_unary()?;
        loop {
            let op = if self.eat("*") {
                ArithOp::Mul
            } else if self.eat("/") {
                ArithOp::Div
            } else if self.eat("%") {
                ArithOp::Mod
            } else {
                break;
            };
            let rhs = self.parse_unary()?;
            e = ValExpr::Arith {
                op,
                lhs: Box::new(e),
                rhs: Box::new(rhs),
            };
        }
        Ok(e)
    }

    fn parse_unary(&mut self) -> Result<ValExpr, Error> {
        if self.eat("-") {
            Ok(ValExpr::Neg(Box::new(self.parse_unary()?)))
        } else if self.eat("+") {
            self.parse_unary()
        } else {
            self.parse_atom()
        }
    }

    fn parse_atom(&mut self) -> Result<ValExpr, Error> {
        if self.eat("(") {
            let e = self.parse_value()?;
            if !self.eat(")") {
                return Err(err(format!("expected ')', found {}", self.here())));
            }
            return Ok(e);
        }
        match self.toks.get(self.pos).cloned() {
            Some(ETok::Num(n)) => {
                self.pos += 1;
                Ok(ValExpr::Num(n))
            }
            Some(ETok::Word(w, n)) => {
                self.pos += 1;
                Ok(ValExpr::Word(w, n))
            }
            Some(ETok::Str(s)) => {
                self.pos += 1;
                Ok(ValExpr::Str(s))
            }
            Some(ETok::Ident(name)) => {
                self.pos += 1;
                // A name directly followed by `(` is a function/`prev`/`rownum`
                // call; otherwise it is a column reference.
                if self.eat("(") {
                    self.parse_call(&name)
                } else {
                    self.columns
                        .push((name.clone(), self.spans[self.pos - 1].clone()));
                    Ok(ValExpr::Col(ColRef::new(name)))
                }
            }
            _ => Err(err(format!(
                "expected a column, number, string, or function, found {}",
                self.here()
            ))),
        }
    }

    /// Parse a call `name(...)` — the opening `(` already consumed.
    fn parse_call(&mut self, name: &str) -> Result<ValExpr, Error> {
        let name_at = self.pos - 2;
        if name == "rownum" {
            if !self.eat(")") {
                return Err(err("rownum() takes no arguments"));
            }
            return Ok(ValExpr::Rownum);
        }
        let args = self.parse_args()?;
        if name == "prev" {
            let [ValExpr::Col(c)] = &args[..] else {
                let e = err("prev() takes a single column, e.g. prev(amount)");
                return Err(self.fail_on(name_at..self.pos, e));
            };
            return Ok(ValExpr::Prev(c.clone()));
        }
        let Some(func) = Func::from_name(name) else {
            let e = err(match crate::error::did_you_mean(name, Func::NAMES) {
                Some(s) => format!("unknown function: {name} (did you mean `{s}`?)"),
                None => format!("unknown function: {name}"),
            });
            return Err(self.fail_on(name_at..self.pos, e));
        };
        check_arity(func, args.len()).map_err(|e| self.fail_on(name_at..self.pos, e))?;
        Ok(ValExpr::Func(func, args))
    }

    /// Parse a comma-separated argument list up to and including the closing `)`.
    fn parse_args(&mut self) -> Result<Vec<ValExpr>, Error> {
        let mut args = Vec::new();
        if self.eat(")") {
            return Ok(args);
        }
        loop {
            args.push(self.parse_value()?);
            if self.eat(")") {
                break;
            }
            if !self.eat(",") {
                return Err(err("expected ',' or ')' in function arguments"));
            }
        }
        Ok(args)
    }
}

/// Reject a function call with the wrong number of arguments.
fn check_arity(func: Func, n: usize) -> Result<(), Error> {
    let ok = match func {
        Func::Min | Func::Max | Func::Coalesce => n >= 1,
        Func::Pow => n == 2,
        // The rest are unary.
        _ => n == 1,
    };
    if ok {
        Ok(())
    } else {
        Err(err(format!("{}() got {n} argument(s)", func.name())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_placed_in_the_script() {
        let span = |script: &str| parse(script).unwrap_err().span();
        // The token an expression stopped at, or the character it could not
        // lex.
        assert_eq!(span("select a >> 1"), Some(10..11));
        assert_eq!(span("select a @ 1"), Some(9..10));
        assert_eq!(span("add b = a +"), Some(11..11)); // the end
        // A comment keeps the offsets after it the script's own.
        assert_eq!(span("cols a # note\nselect a >> 1"), Some(24..25));
        // An unknown command's word; a call, from its name to its `)`.
        assert_eq!(span("cols a | selct a"), Some(9..14));
        assert_eq!(span("add c = pow(a)"), Some(8..14));
        // The operand an operator's error is about, read past by then.
        assert_eq!(span("select 1 =~ 'x'"), Some(7..8));
        assert_eq!(span("select a =~ b && b > 1"), Some(12..13));
        assert_eq!(span("select a =~ '(' && b > 1"), Some(12..15));
        assert_eq!(span("select 1 ^= 'x' && b > 1"), Some(7..8));
        assert_eq!(span("select a $= b"), Some(12..13));
        // Inside a join's sub-pipeline, still the token.
        assert_eq!(span("join (select x >> 1) r.csv on k"), Some(16..17));
        // Anything else: the stage it is in.
        assert_eq!(span("cols a | agg bogus(a)"), Some(9..21));
        // A fragment's body is not where it is called: the call stage.
        assert_eq!(span("fn f(x) { select x >> 1 }\nf(a)"), Some(26..30));
        // The message itself is unchanged by its place.
        assert_eq!(
            parse("select a >> 1").unwrap_err().to_string(),
            "expected a column, number, string, or function, found '>'"
        );
    }

    #[test]
    fn resolve_errors_are_placed_on_their_stage() {
        let header = ["a".to_string()];
        let span = |script: &str| {
            let mut plan = parse(script).unwrap();
            plan.resolve(&header).unwrap_err().span()
        };
        // An error that is not about a column is placed on its part.
        assert_eq!(span("cols a | select num(a) > 'x'"), Some(9..28));
        assert_eq!(span("color red num(a) > 'x'"), Some(0..22));
        // A column error, in each kind of stage, on where it is named.
        assert_eq!(span("cols a | select b > 1"), Some(16..17));
        assert_eq!(span("cols a | sort 5=n"), Some(14..15));
        assert_eq!(span("cols a | uniq zz"), Some(14..16));
        assert_eq!(span("cols a | stats zz"), Some(15..17));
        assert_eq!(span("cols a | agg sum(zz)"), Some(17..19));
        assert_eq!(span("graph hist zz"), Some(11..13));
        // A fragment's statements were written at its call.
        assert_eq!(span("fn f(x) { select x > 1 }\nf(zz)"), Some(25..30));
    }

    #[test]
    fn a_column_error_is_placed_where_the_script_names_it() {
        let place = |header: &[&str], script: &str| {
            let header: Vec<String> = header.iter().map(|h| h.to_string()).collect();
            let mut plan = parse(script).unwrap();
            plan.resolve(&header).unwrap_err().span().unwrap()
        };
        let mark = |header: &[&str], script: &str| script[place(header, script)].to_string();
        let a = ["a"];
        assert_eq!(mark(&a, "cols a | select b > 1"), "b");
        // The first time the part names it.
        assert_eq!(place(&a, "cols a | select zz > 1 && zz < 5"), 16..18);
        // A column, not a string literal or a number that reads the same.
        assert_eq!(mark(&a, "cols a | select a == 'b' || b > 1"), "b");
        assert_eq!(mark(&a, "cols a | select 3 < `3`"), "`3`");
        // Not the name of the column an `add` or `agg` makes.
        assert_eq!(place(&a, "cols a | add zz = zz + 1"), 18..20);
        assert_eq!(place(&a, "cols a | agg zz=sum(zz)"), 20..22);
        assert_eq!(place(&a, "cols a | agg t = sum(u)"), 21..22);
        assert_eq!(place(&a, "cols a | agg `s(`=sum(s)"), 22..23);
        assert_eq!(place(&a, "cols a | agg sum(`a(b`)"), 18..21);
        // A whole item of a list, not part of a longer name.
        assert_eq!(mark(&["a", "x-zz"], "cols x-zz, zz"), "zz");
        assert_eq!(mark(&a, "cols a | sort 5=n"), "5");
        assert_eq!(mark(&a, "cols a | agg count by zz"), "zz");
        assert_eq!(mark(&a, "rename zz=b"), "zz");
        assert_eq!(mark(&a, "graph line a zz"), "zz");
        assert_eq!(mark(&a, "graph scatter a a -c zz"), "zz");
        assert_eq!(mark(&a, "graph scatter a a --color-by=c"), "c");
        // A join's left key, not the letters of its file's path; a right key
        // is in the right file, so it marks the whole join.
        assert_eq!(mark(&a, "join zz.csv on zz"), "zz");
        assert_eq!(place(&a, "join zz.csv on zz"), 15..17);
        assert_eq!(mark(&a, "join f.csv on a=zz"), "join f.csv on a=zz");
        // A fragment's expansion is not in the script: its call is the place.
        assert_eq!(mark(&a, "fn f(x) { select x > 1 }\nf(zz)"), "f(zz)");
    }

    #[test]
    fn cols_keep_and_exclude() {
        let plan = parse("cols id,fieldA countZ").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Cols(p) = &stmts[0] else { panic!() };
        assert!(!p.exclude);
        assert_eq!(p.names, ["id", "fieldA", "countZ"]);

        let plan = parse("cols -v x,y").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Cols(p) = &stmts[0] else { panic!() };
        assert!(p.exclude);
        assert_eq!(p.names, ["x", "y"]);
    }

    #[test]
    fn select_numeric_vs_string() {
        // Bare expression — string literals quoted, whole expression is not.
        let plan = parse("select a == 't' && b > 0").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Select(BoolExpr::And(parts)) = &stmts[0] else {
            panic!()
        };
        let BoolExpr::Cmp(eq) = &parts[0] else {
            panic!()
        };
        // Modes are decided at resolve time (see `modes_are_pinned_from_column_types_at_resolve`).
        assert_eq!(eq.mode, CmpMode::Auto);
        let BoolExpr::Cmp(gt) = &parts[1] else {
            panic!()
        };
        assert_eq!(gt.mode, CmpMode::Auto);
    }

    #[test]
    fn select_untyped_ordering_is_auto_equality_is_string() {
        // Two bare columns: an ordering auto-detects per row; `==`/`!=` stay
        // lexical (numeric equality on floats is fragile). Decided at resolve.
        let mut plan = parse("select qty > stock && a == b && a != b").unwrap();
        plan.resolve(&["qty".into(), "stock".into(), "a".into(), "b".into()])
            .unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Select(BoolExpr::And(parts)) = &stmts[0] else {
            panic!()
        };
        let modes: Vec<CmpMode> = parts
            .iter()
            .map(|p| {
                let BoolExpr::Cmp(c) = p else { panic!() };
                c.mode
            })
            .collect();
        assert_eq!(modes, [CmpMode::Auto, CmpMode::String, CmpMode::String]);
    }

    #[test]
    fn column_types_are_left_to_resolve() {
        // The parser decides no modes: every compare is `Auto` until
        // `Plan::resolve` decides from the operands and the column types it
        // tracks by position; a string literal stays a literal until then.
        let plan = parse("add qty = str(qty) | select qty > stock").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Select(BoolExpr::Cmp(c)) = &stmts[1] else {
            panic!()
        };
        assert_eq!(c.mode, CmpMode::Auto);
        let plan = parse("add c = num(c) | select c == '5'").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Select(BoolExpr::Cmp(c)) = &stmts[1] else {
            panic!()
        };
        assert_eq!(c.mode, CmpMode::Auto);
        assert!(matches!(&c.rhs, ValExpr::Str(s) if s == "5"));
    }

    #[test]
    fn removed_conversion_commands_point_at_the_casts() {
        // Every column named gets its own add; a position or an awkward name
        // is backtick-quoted where the expression needs it; the call spelling
        // of the old command gets the same hint.
        for (cmd, cast) in [
            ("to-num", "num"),
            ("to_num", "num"),
            ("to-str", "str"),
            ("to_str", "str"),
        ] {
            let err = parse(&format!("{cmd} a,b")).unwrap_err().to_string();
            assert!(err.contains("removed"), "{err}");
            assert!(
                err.contains(&format!("add a = {cast}(a) | add b = {cast}(b)")),
                "{err}"
            );
        }
        let err = parse("to-str 2").unwrap_err().to_string();
        assert!(err.contains("add 2 = str(`2`)"), "{err}");
        let err = parse("to-num 'my col'").unwrap_err().to_string();
        assert!(err.contains("add `my col` = num(`my col`)"), "{err}");
        let err = parse("to_num(qty)").unwrap_err().to_string();
        assert!(err.contains("add qty = num(qty)"), "{err}");
        let err = parse("to-num").unwrap_err().to_string();
        assert!(err.contains("add COL = num(COL)"), "{err}");
    }

    #[test]
    fn sort_flags_and_stage_split() {
        let plan = parse("add c = num(c) | select c > 0 | sort c=r a id=nr").unwrap();
        assert_eq!(plan.stages.len(), 2);
        let Stage::Sort(s) = &plan.stages[1] else {
            panic!()
        };
        assert!(s.keys[0].descending && s.keys[0].mode == SortMode::Auto); // c: =r (typed at resolve)
        assert!(!s.keys[1].descending && s.keys[1].mode == SortMode::Auto); // a: default
        assert!(s.keys[2].descending && s.keys[2].mode == SortMode::Numeric); // id: =nr

        // `=` is the only flag separator: `a:nr` is a column name.
        let plan = parse("sort a:nr b").unwrap();
        let Stage::Sort(s) = &plan.stages[0] else {
            panic!()
        };
        assert_eq!(s.keys[0].name, "a:nr");
        assert!(!s.keys[0].descending && s.keys[0].mode == SortMode::Auto);
    }

    #[test]
    fn sort_mode_flags() {
        // `=s` pins lexical and `=n` numeric; a bare key is auto until
        // `Plan::resolve` sees the column's type.
        let plan = parse("add z = str(z) | sort a=s b z z=n").unwrap();
        let Stage::Sort(s) = &plan.stages[1] else {
            panic!()
        };
        assert_eq!(s.keys[0].mode, SortMode::Lexical); // a=s
        assert_eq!(s.keys[1].mode, SortMode::Auto); // b
        assert_eq!(s.keys[2].mode, SortMode::Auto); // z: typed at resolve
        assert_eq!(s.keys[3].mode, SortMode::Numeric); // z=n
        assert!(s.keys.iter().all(|k| !k.descending));
        // `=sr` combines like `=nr`.
        let plan = parse("sort a=sr").unwrap();
        let Stage::Sort(s) = &plan.stages[0] else {
            panic!()
        };
        assert_eq!(s.keys[0].mode, SortMode::Lexical);
        assert!(s.keys[0].descending);
        // Unknown flags still error, naming the accepted set.
        let err = parse("sort a=x").unwrap_err().to_string();
        assert!(err.contains("unknown sort flag 'x'"), "{err}");
    }

    #[test]
    fn nested_and_or_with_parens() {
        let plan = parse("select a == 't' && (b > 0 || c > 0)").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        assert!(matches!(&stmts[0], Stmt::Select(BoolExpr::And(_))));
    }

    #[test]
    fn regex_and_negation() {
        let plan = parse("select name =~ '^a.*z$'").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        assert!(matches!(
            &stmts[0],
            Stmt::Select(BoolExpr::Match { negate: false, .. })
        ));
    }

    #[test]
    fn comments_stripped_quote_aware() {
        // A full-line and a trailing comment are removed; a multi-line script
        // (as from -f) still parses to the same stages.
        let plan = parse("select a > 0   # positives\n| cols a  # project\n").unwrap();
        assert_eq!(plan.stages.len(), 1); // select + cols => one transform
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        assert!(matches!(stmts[0], Stmt::Select(_)) && matches!(stmts[1], Stmt::Cols(_)));
        // A `#` inside a string literal is data, not a comment.
        let plan = parse("select tag == '#urgent'").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Select(BoolExpr::Cmp(c)) = &stmts[0] else {
            panic!()
        };
        assert!(matches!(&c.rhs, ValExpr::Str(s) if s == "#urgent"));
    }

    #[test]
    fn unknown_command_suggests_closest() {
        let e = parse("selct a > 0").unwrap_err().to_string();
        assert!(e.contains("did you mean `select`"), "{e}");
        // No suggestion when nothing is close.
        let e = parse("frobnicate a").unwrap_err().to_string();
        assert!(
            e.contains("unknown command") && !e.contains("did you mean"),
            "{e}"
        );
    }

    #[test]
    fn select_v_negates_whole_expression() {
        // `select -v EXPR` == `select !(EXPR)` — drop the matching rows.
        let plan = parse("select -v a > 0 || b > 0").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        assert!(matches!(stmts[0], Stmt::Select(BoolExpr::Not(_))));
        // Without -v, the same expression is kept as-is (an Or here).
        let plan = parse("select a > 0 || b > 0").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        assert!(matches!(stmts[0], Stmt::Select(BoolExpr::Or(_))));
    }

    #[test]
    fn head_negative_is_drop_last() {
        // `head -n -N` keeps all but the last N -> a DropLast stage.
        let plan = parse("head -n -3").unwrap();
        assert!(matches!(plan.stages[0], Stage::DropLast(3)));
        // Positive forms still mean "first N", including the obsolete `-N`.
        assert!(matches!(parse("head 3").unwrap().stages[0], Stage::Head(3)));
        assert!(matches!(
            parse("head -3").unwrap().stages[0],
            Stage::Head(3)
        ));
        // tail has no negative form.
        assert!(parse("tail -n -3").is_err());
    }

    #[test]
    fn tail_plus_n_is_from_row_n() {
        // coreutils' `tail -n +N` prints from row N on: skip the first N-1.
        assert!(matches!(
            parse("tail +3").unwrap().stages[0],
            Stage::Skip(2)
        ));
        assert!(matches!(
            parse("tail -n +3").unwrap().stages[0],
            Stage::Skip(2)
        ));
        assert!(matches!(
            parse("tail --lines=+1").unwrap().stages[0],
            Stage::Skip(0)
        ));
        // `+0` is the whole input, as in coreutils.
        assert!(matches!(
            parse("tail +0").unwrap().stages[0],
            Stage::Skip(0)
        ));
        // head has no `+N` form.
        let err = parse("head +3").unwrap_err().to_string();
        assert!(err.contains("head"), "{err}");
        assert!(parse("tail +x").is_err());
        assert!(parse("tail ++3").is_err());
        assert!(parse("tail +").is_err());
    }

    #[test]
    fn agg_by_keyword_splits_on_any_blank() {
        // The `by` keyword is found by any Unicode blank around it, like the
        // items themselves (a no-break space is two bytes).
        let plan = parse("agg count(a)\u{a0}by\u{a0}g,\u{2003}h").unwrap();
        let Stage::Group(g) = &plan.stages[0] else {
            panic!()
        };
        assert_eq!(g.keys, ["g", "h"]);
        // `by` inside a name or a call is not the keyword.
        let plan = parse("agg count(by) by baby").unwrap();
        let Stage::Group(g) = &plan.stages[0] else {
            panic!()
        };
        assert_eq!(g.keys, ["baby"]);
        assert_eq!(g.aggs[0].col.as_deref(), Some("by"));
        // A `b` at the end, or `by` glued to a multi-byte char, is not the
        // keyword; a multi-byte key after it is fine.
        assert!(parse("agg count(x) b").is_err());
        assert!(parse("agg count(x) byé").is_err());
        let plan = parse("agg count(x) by bé").unwrap();
        let Stage::Group(g) = &plan.stages[0] else {
            panic!()
        };
        assert_eq!(g.keys, ["bé"]);
        // A comma is a separator too, as it is between items.
        let plan = parse("agg count(x),by,g").unwrap();
        let Stage::Group(g) = &plan.stages[0] else {
            panic!()
        };
        assert_eq!(g.keys, ["g"]);
        // Nor is a `by` (or a paren) inside a backticked name.
        let plan = parse("agg `a (b` = sum(x), c = max(x) by g").unwrap();
        let Stage::Group(g) = &plan.stages[0] else {
            panic!()
        };
        assert_eq!(g.keys, ["g"]);
        let names: Vec<_> = g.aggs.iter().map(|a| a.name.as_deref()).collect();
        assert_eq!(names, [Some("a (b"), Some("c")]);
        let plan = parse("agg `sales by region` = sum(x) by g").unwrap();
        let Stage::Group(g) = &plan.stages[0] else {
            panic!()
        };
        assert_eq!(g.keys, ["g"]);
        assert_eq!(g.aggs[0].name.as_deref(), Some("sales by region"));
    }

    #[test]
    fn exponent_literals_lex_as_numbers() {
        assert!(parse("add v = 1e3 + 2.5E-3 - 1e+2").is_ok());
        assert!(parse("select price > 1e3").is_ok());
        // A malformed exponent is an invalid number, not a silent `1`.
        for bad in ["add v = 1e", "add v = 1e+", "add v = 1e-x"] {
            let err = parse(bad).unwrap_err().to_string();
            assert!(err.contains("invalid number"), "{bad}: {err}");
        }
    }

    #[test]
    fn agg_unquotes_columns_and_keys_alike() {
        // One tokenizer for the whole argument: a quoted column inside a
        // call, a quoted key, and a key literally named `by`.
        let plan = parse("agg sum(`a b`), n = count('c d') by `by`, \"e f\"").unwrap();
        let Stage::Group(g) = &plan.stages[0] else {
            panic!()
        };
        assert_eq!(g.keys, ["by", "e f"]);
        assert_eq!(g.aggs[0].col.as_deref(), Some("a b"));
        assert_eq!(g.aggs[1].col.as_deref(), Some("c d"));
        assert_eq!(g.aggs[1].name.as_deref(), Some("n"));
        // An apostrophe opens a quote here, as in any list; unclosed, it
        // is an error that names the quote, not a swallowed `by`.
        let err = parse("agg sum(driver's_id) by g").unwrap_err().to_string();
        assert!(err.contains("unterminated ' quote"), "{err}");
        let err = parse("agg sum(`a)").unwrap_err().to_string();
        assert!(err.contains("unterminated backtick"), "{err}");
    }

    #[test]
    fn assigned_names_take_any_quote() {
        // The NAME half of `NAME = …` takes `'`, `"` or backticks, in `add`
        // and `agg` alike; the quotes come off.
        let plan = parse("agg 'my n' = sum(x), \"m\"=count").unwrap();
        let Stage::Group(g) = &plan.stages[0] else {
            panic!()
        };
        let names: Vec<_> = g.aggs.iter().map(|a| a.name.as_deref()).collect();
        assert_eq!(names, [Some("my n"), Some("m")]);
        let plan = parse("add \"n n\" = x").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Add(a) = &stmts[0] else { panic!() };
        assert_eq!(a.name, "n n");
        let err = parse("add 'x = 1").unwrap_err().to_string();
        assert!(err.contains("unterminated quoted column name"), "{err}");
    }

    #[test]
    fn number_literals_match_the_cell_grammar() {
        // `.5`, `inf` and `NaN` are numbers in a cell, so they are literals
        // here too (any case, with a sign); a column with such a name needs
        // backticks.
        assert!(parse("add v = .5 + -.25 * 2.").is_ok());
        for lit in ["inf", "Infinity", "NaN", "nan"] {
            let mut plan = parse(&format!("add v = {lit}")).unwrap();
            plan.resolve(&["x".to_string()]).unwrap();
            let Stage::Transform(stmts) = &plan.stages[0] else {
                panic!()
            };
            let Stmt::Add(a) = &stmts[0] else { panic!() };
            assert!(matches!(a.expr, ValExpr::Num(_)), "{lit}: {:?}", a.expr);
        }
        assert!(parse("select x > inf").is_ok());
        assert!(parse("add v = infx").is_ok()); // a column, not a literal
        // A column of that name is an error, not a silent constant; in
        // backticks it is the column.
        let header = ["inf".to_string(), "x".to_string()];
        let err = parse("select inf > 1")
            .unwrap()
            .resolve(&header)
            .unwrap_err();
        assert!(
            err.to_string().contains("`inf` is the number here"),
            "{err}"
        );
        assert!(parse("select `inf` > 1").unwrap().resolve(&header).is_ok());
        assert!(parse("cols inf").unwrap().resolve(&header).is_ok());
        // Checked where the word is used, against the live header: a
        // column made earlier counts, one dropped before does not, an
        // expression that does not use the word is not affected, and colour
        // rules count too.
        assert!(
            parse("rename x = nan | select nan > 1")
                .unwrap()
                .resolve(&header)
                .is_err()
        );
        assert!(
            parse("cols x | select x > inf")
                .unwrap()
                .resolve(&header)
                .is_ok()
        );
        assert!(
            parse("color red inf > 1")
                .unwrap()
                .resolve(&header)
                .is_err()
        );
        assert!(
            parse("add v = inf | rename x = inf | select v > 1")
                .unwrap()
                .resolve(&header[1..])
                .is_ok()
        );
    }

    #[test]
    fn adjacent_windows_fold_into_one() {
        // `skip a | head l | skip b` is `skip a+b | head l-b`; `head` after
        // `head` keeps the smaller; a transform in between stops the fold.
        let stages = |s: &str| parse(s).unwrap().stages;
        assert!(matches!(
            stages("head 5 | tail +2")[..],
            [Stage::Skip(1), Stage::Head(4)]
        ));
        assert!(matches!(stages("tail +3 | tail +2")[..], [Stage::Skip(3)]));
        assert!(matches!(stages("head 5 | head 3")[..], [Stage::Head(3)]));
        assert!(matches!(
            stages("tail +2 | head 3 | tail +2 | head 5")[..],
            [Stage::Skip(2), Stage::Head(2)]
        ));
        assert!(matches!(
            stages("head 2 | tail +5")[..],
            [Stage::Skip(4), Stage::Head(0)]
        ));
        // Skips add without overflowing.
        let huge = format!("tail +{0} | tail +{0}", usize::MAX);
        assert!(matches!(stages(&huge)[..], [Stage::Skip(usize::MAX)]));
        assert!(matches!(
            stages("head 5 | cols a | tail +2")[..],
            [Stage::Head(5), Stage::Transform(_), Stage::Skip(1)]
        ));
        // A window from a fragment folds too.
        assert!(matches!(
            stages("fn t2() { tail +2 }\nhead 5 | t2()")[..],
            [Stage::Skip(1), Stage::Head(4)]
        ));
    }

    #[test]
    fn agg_by_parses_into_a_group_stage() {
        let plan = parse("agg sum(amount),mean(amount) by region").unwrap();
        assert_eq!(plan.stages.len(), 1);
        let Stage::Group(g) = &plan.stages[0] else {
            panic!("expected a group stage");
        };
        assert_eq!(g.keys, ["region"]);
        assert!(g.aggs.iter().all(|a| a.name.is_none())); // default names, at resolve
        let mut plan = plan;
        let out = plan.resolve(&["amount".into(), "region".into()]).unwrap();
        assert_eq!(out, ["region", "amount_sum", "amount_mean"]);
        // A bare count per key.
        let plan = parse("agg count by a,b").unwrap();
        let Stage::Group(g) = &plan.stages[0] else {
            panic!()
        };
        assert_eq!(g.keys, ["a", "b"]);
        assert_eq!(g.aggs[0].func, AggFunc::Count);
        // `group` is gone; the hint carries the keys over.
        let err = parse("group region").unwrap_err().to_string();
        assert!(err.contains("agg count by region"), "{err}");
        assert!(parse("group r | agg sum(a)").is_err());
    }

    #[test]
    fn agg_rejects_bad_specs() {
        assert!(parse("agg frobnicate(x)").is_err()); // unknown function
        assert!(parse("agg sum").is_err()); // sum needs a column
        assert!(parse("agg count_distinct").is_err()); // so does count_distinct
        assert!(parse("agg count_distinct(x) by g").is_ok());
        assert!(parse("agg sum()").is_err()); // empty column
        assert!(parse("agg").is_err()); // no aggregates
        assert!(parse("agg ,").is_err()); // still none
        assert!(parse("agg , , by g").is_err());
        assert!(parse("agg count by").is_err()); // no keys
        assert!(parse("agg =sum(x)").is_err()); // empty name
        assert!(parse("agg total=").is_err()); // name without an aggregate
        assert!(parse("agg `odd name`=sum(x)").is_ok());
    }

    #[test]
    fn spaces_around_equals_do_not_split_an_argument() {
        // `=` binds tighter than the argument separator in every list, so the
        // three assignment sites agree with `add NAME = EXPR`.
        let plan = parse("agg total = sum(x), n=count by g").unwrap();
        let Stage::Group(g) = &plan.stages[0] else {
            panic!()
        };
        assert_eq!(g.aggs[0].name.as_deref(), Some("total"));
        assert_eq!(g.aggs[1].name.as_deref(), Some("n"));
        let plan = parse("rename a = b, `odd name` =c, d= e").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Rename(r) = &stmts[0] else { panic!() };
        let pairs: Vec<(&str, &str)> = r
            .pairs
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        assert_eq!(pairs, [("a", "b"), ("odd name", "c"), ("d", "e")]);
        let plan = parse("sort a = nr b").unwrap();
        let Stage::Sort(s) = &plan.stages[0] else {
            panic!()
        };
        assert_eq!(s.keys.len(), 2);
        assert!(s.keys[0].descending && s.keys[0].mode == SortMode::Numeric);
        assert_eq!(s.keys[1].name, "b");
        // A `join` key pair too.
        assert!(parse("join r.csv on id = rid").is_ok());
        // Any Unicode blank between items separates or joins the same way
        // (a no-break space is two bytes; the `by` keyword itself still needs
        // ASCII blanks around it).
        let plan = parse("cols a\u{a0}b | rename a\u{a0}=\u{a0}c").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Cols(p) = &stmts[0] else { panic!() };
        assert_eq!(p.names, ["a", "b"]);
        assert!(parse("agg count(a)\u{a0}sum(b) by g").is_ok());
        // A dangling `=` is still an error, not a silent key.
        assert!(parse("rename a =").is_err());
        assert!(parse("agg sum(x) =").is_err());
    }

    #[test]
    fn graph_parses_kind_column_and_flags() {
        let plan = parse("graph hist amount --bins 12 --title Spread").unwrap();
        let g = plan.graph.expect("graph metadata");
        assert_eq!(g.kind, GraphKind::Hist);
        assert_eq!(g.cols.len(), 1);
        assert_eq!(g.cols[0].name, "amount");
        assert_eq!(g.opts.bins, Some(12));
        assert_eq!(g.opts.title.as_deref(), Some("Spread"));
        // Every flag has a short spelling too.
        let plan = parse("graph hist amount -b 12 -s 1.5 -t Spread -S").unwrap();
        let g = plan.graph.expect("graph metadata");
        assert_eq!(g.opts.bins, Some(12));
        assert_eq!(g.opts.title.as_deref(), Some("Spread"));
        assert!(g.opts.svg);
        assert!(parse("graph hist amount -b=3").is_ok());
    }

    #[test]
    fn graph_must_be_last_and_well_formed() {
        assert!(parse("graph hist x | sort x").is_err()); // nothing may follow a sink
        assert!(parse("graph").is_err()); // no columns
        assert!(parse("graph hist a b").is_err()); // hist takes exactly one column
        assert!(parse("graph hist x --bins 0").is_err()); // bins must be positive
        assert!(parse("graph hist x --frob 1").is_err()); // unknown flag
    }

    #[test]
    fn graph_without_a_type_is_chosen_by_its_columns() {
        let kind = |script: &str| {
            let g = parse(script).unwrap().graph.unwrap();
            (g.kind, g.kind_named, g.cols.len())
        };
        assert_eq!(kind("graph date price"), (GraphKind::Line, false, 2));
        assert_eq!(kind("graph date a,b -t T"), (GraphKind::Line, false, 3));
        assert_eq!(kind("graph price"), (GraphKind::Hist, false, 1));
        assert_eq!(kind("graph line date price"), (GraphKind::Line, true, 2));
        // A column named like a chart type is backticked.
        assert_eq!(kind("graph `hist` b"), (GraphKind::Line, false, 2));
        // The type's own flags still apply: a histogram takes no -y.
        assert!(parse("graph price -y 0:1").is_err());
        // A mistyped type reads as a column, which resolve points out.
        let header = ["x".to_string(), "price".to_string()];
        let script = "graph scater x price";
        let e = parse(script).unwrap().resolve(&header).unwrap_err();
        assert!(e.to_string().contains("`graph scatter …`"), "{e}");
        assert_eq!(e.span().map(|s| &script[s]), Some("scater"));
        // Not close to one: the kinds are listed.
        let e = parse("graph chart x")
            .unwrap()
            .resolve(&header)
            .unwrap_err();
        assert!(
            e.to_string()
                .ends_with("hist, bar, spark, scatter, line, heatmap)"),
            "{e}"
        );
        // No columns: the kinds are listed.
        let e = parse("graph").unwrap_err().to_string();
        assert!(e.contains("or a chart type first: hist, bar"), "{e}");
    }

    #[test]
    fn graph_bar_and_spark_arities() {
        let bar = parse("graph bar region total").unwrap().graph.unwrap();
        assert_eq!(bar.kind, GraphKind::Bar);
        assert_eq!(bar.cols.len(), 2);
        let spark = parse("graph spark value").unwrap().graph.unwrap();
        assert_eq!(spark.kind, GraphKind::Spark);
        assert!(parse("graph bar region").is_err()); // bar needs label + value
        assert!(parse("graph spark a b").is_err()); // spark takes one column
    }

    #[test]
    fn graph_scatter_and_line_accept_multiple_y_columns() {
        let g = parse("graph line t a,b,c --scale 2")
            .unwrap()
            .graph
            .unwrap();
        assert_eq!(g.kind, GraphKind::Line);
        assert_eq!(g.cols.len(), 4); // x + 3 series
        assert_eq!(g.opts.scale, 2.0);
        assert_eq!(
            parse("graph scatter x y").unwrap().graph.unwrap().kind,
            GraphKind::Scatter
        );
        assert!(parse("graph scatter x").is_err()); // needs x + at least one y
    }

    #[test]
    fn graph_svg_flag_sets_the_option() {
        assert!(parse("graph hist x --svg").unwrap().graph.unwrap().opts.svg);
        assert!(!parse("graph hist x").unwrap().graph.unwrap().opts.svg);
    }

    #[test]
    fn graph_scale_parses_and_validates() {
        assert_eq!(
            parse("graph hist x").unwrap().graph.unwrap().opts.scale,
            1.0
        ); // default
        assert_eq!(
            parse("graph hist x --scale 1.5")
                .unwrap()
                .graph
                .unwrap()
                .opts
                .scale,
            1.5
        );
        assert!(parse("graph hist x --scale 0").is_err()); // must be positive
        assert!(parse("graph hist x --scale -1").is_err());
        assert!(parse("graph hist x --scale big").is_err());
    }

    #[test]
    fn graph_ranges_parse_and_are_checked_per_kind() {
        let g = parse("graph scatter x y -x 0:10 --yrange -1:1")
            .unwrap()
            .graph
            .unwrap();
        assert_eq!(g.opts.xrange, Some((0.0, 10.0)));
        assert_eq!(g.opts.yrange, Some((-1.0, 1.0)));
        for bad in [
            "graph hist a -x 5:5",
            "graph hist a -x a:b",
            "graph hist a -x 5",
        ] {
            let err = parse(bad).unwrap_err().to_string();
            assert!(err.contains("-x/--xrange"), "{bad}: {err}");
        }
        // A range a kind has no axis for.
        let err = parse("graph hist a -y 0:1").unwrap_err().to_string();
        assert!(
            err.contains("graph hist") && err.contains("-y/--yrange"),
            "{err}"
        );
        assert!(parse("graph bar a b -x 0:1").is_err());
        assert!(parse("graph spark a -x 0:1").is_err());
        assert!(parse("graph bar a b -y 0:1").is_ok());
        assert!(parse("graph spark a -y 0:1").is_ok());
    }

    #[test]
    fn graph_log_parses_and_needs_a_positive_y_range() {
        assert!(parse("graph hist a -l").unwrap().graph.unwrap().opts.log);
        assert!(
            parse("graph spark a --log")
                .unwrap()
                .graph
                .unwrap()
                .opts
                .log
        );
        assert!(!parse("graph spark a").unwrap().graph.unwrap().opts.log);
        // A log axis cannot span a non-positive value.
        let err = parse("graph spark a -l -y 0:9").unwrap_err().to_string();
        assert!(
            err.contains("graph spark") && err.contains("-l/--log"),
            "{err}"
        );
        assert!(parse("graph spark a -l -y 1:9").is_ok());
        // A heatmap's `-l` is its *count* axis — both of its own axes are
        // binned — so its y range is free to run below zero.
        assert!(parse("graph heatmap a b -l -y -3:3").is_ok());
    }

    #[test]
    fn graph_color_by_needs_one_xy_series() {
        let g = parse("graph scatter x y -c z -r blue:red")
            .unwrap()
            .graph
            .unwrap();
        assert_eq!(g.opts.color_by.as_ref().map(|c| c.name.as_str()), Some("z"));
        assert!(parse("graph scatter x y1,y2 -c z").is_err());
        assert!(parse("graph hist x -c z").is_err());
        assert!(parse("graph line x y1,y2 -r blue:red").is_err());
    }

    #[test]
    fn graph_sizes_and_bins_are_capped() {
        // A chart cell is a byte of memory (or four), so an unbounded -b/-W/-H
        // asks the allocator for a chart no terminal could show.
        assert!(parse("graph heatmap a b -b 4096").is_ok());
        let e = parse("graph heatmap a b -b 4097").unwrap_err().to_string();
        assert!(
            e.contains("-b/--bins expects a positive integer up to 4096, got `4097`"),
            "{e}"
        );
        assert!(parse("graph hist a -W 4097").is_err());
        assert!(parse("graph hist a -H 4097").is_err());
        assert!(parse("graph hist a -W 4096 -H 4096").is_ok());
        // Same cap, a value far past it.
        assert!(parse("graph heatmap a b -b 4294967296").is_err());
    }

    #[test]
    fn graph_ramp_needs_one_bar_value_column() {
        // One value column ramps by value; a group takes the series palette,
        // so a ramp there would have no meaning and is rejected.
        assert!(parse("graph bar k v -r blue:red").is_ok());
        let e = parse("graph bar k v1,v2 -r blue:red")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("graph bar takes -r/--ramp with one value column"),
            "{e}"
        );
        assert!(parse("graph bar k v1,v2").is_ok());
    }

    #[test]
    fn uniq_parses_whole_row_and_keys() {
        let plan = parse("uniq").unwrap();
        let Stage::Uniq(u) = &plan.stages[0] else {
            panic!()
        };
        assert!(u.cols.is_empty()); // whole-row
    }

    #[test]
    fn command_aliases() {
        // Only `colour` remains; the old aliases are plain unknown commands.
        assert!(parse("colour red a > 0").is_ok());
        for s in [
            "where a > 0",
            "filter a > 0",
            "cut a,b",
            "dedup a",
            "plot hist a",
        ] {
            let err = parse(s).unwrap_err().to_string();
            assert!(err.contains("unknown command"), "{s}: {err}");
        }
    }

    #[test]
    fn affix_operators() {
        for (src, want) in [
            ("select name ^= 'foo'", AffixKind::StartsWith),
            ("select name *= 'oo'", AffixKind::Contains),
            ("select path $= '.csv'", AffixKind::EndsWith),
        ] {
            let plan = parse(src).unwrap();
            let Stage::Transform(stmts) = &plan.stages[0] else {
                panic!()
            };
            let Stmt::Select(BoolExpr::Affix { kind, needle, .. }) = &stmts[0] else {
                panic!("{src}")
            };
            assert_eq!(*kind, want);
            assert!(!needle.is_empty());
        }
        // Negation composes through `!`.
        assert!(matches!(
            parse("select !(name ^= 'foo')").unwrap().stages[0],
            Stage::Transform(ref s) if matches!(s[0], Stmt::Select(BoolExpr::Not(_)))
        ));
        // RHS must be a string literal; a lone `^`/`*`/`$` is reserved.
        assert!(parse("select name ^= other").is_err());
        assert!(parse("select name ^ 'x'").is_err());
        assert!(parse("select 5 *= 'x'").is_err()); // LHS must be a column
    }

    #[test]
    fn backtick_quoted_column_name() {
        // A hyphenated name isn't a bare identifier; backticks make it a column
        // ref (an Ident), not a string literal.
        let mut plan = parse(r#"select `frequenz-app-edge` != """#).unwrap();
        plan.resolve(&["frequenz-app-edge".into()]).unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Select(BoolExpr::Cmp(c)) = &stmts[0] else {
            panic!()
        };
        let ValExpr::Col(col) = &c.lhs else { panic!() };
        assert_eq!(col.name, "frequenz-app-edge");
        assert_eq!(c.mode, CmpMode::String); // RHS is a string literal (decided at resolve)
        assert!(matches!(&c.rhs, ValExpr::Str(s) if s.is_empty()));
    }

    #[test]
    fn backtick_errors() {
        assert!(parse("select `unterminated == 'x'").is_err());
        assert!(parse("select `` == 'x'").is_err()); // empty name
    }

    #[test]
    fn backtick_quoting_in_arg_lists() {
        // A comma inside a backtick-quoted name keeps it one column.
        let plan = parse("cols `first,last`,age").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Cols(p) = &stmts[0] else { panic!() };
        assert_eq!(p.names, ["first,last", "age"]);
        // rename can quote a hyphenated source name (backtick stripped).
        let plan = parse("rename `a-b`=clean").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Rename(r) = &stmts[0] else { panic!() };
        assert_eq!(r.pairs, [("a-b".to_string(), "clean".to_string())]);
    }

    #[test]
    fn or_operator_is_not_a_stage_split() {
        // `||` in a bare expression must not split the stage; a lone `|` does.
        let plan = parse("select a > 0 || b > 0 | cols a").unwrap();
        assert_eq!(plan.stages.len(), 1); // select + cols merge into one transform stage
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        assert!(matches!(&stmts[0], Stmt::Select(BoolExpr::Or(_))));
        assert!(matches!(&stmts[1], Stmt::Cols(_)));
    }

    #[test]
    fn pipe_inside_a_string_literal_is_not_a_split() {
        let plan = parse("select x == 'a|b'").unwrap();
        assert_eq!(plan.stages.len(), 1);
    }

    #[test]
    fn head_rename_fmt() {
        let plan = parse("select a > 0 | head 5 | cols a").unwrap();
        // [Transform(select), Head(5), Transform(cols)]
        assert!(matches!(plan.stages[1], Stage::Head(5)));

        let plan = parse("rename old=new, qty=quantity").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Rename(r) = &stmts[0] else { panic!() };
        assert_eq!(
            r.pairs,
            [
                ("old".into(), "new".into()),
                ("qty".into(), "quantity".into())
            ]
        );

        let plan = parse("sort a | fmt").unwrap();
        assert_eq!(plan.output, OutputFormat::Aligned(TableOpts::default()));
        // fmt is not a stage.
        assert!(plan.stages.iter().all(|s| !matches!(s, Stage::Head(_))));

        // fmt alone (no transforms) is valid — align the input.
        assert_eq!(
            parse("fmt").unwrap().output,
            OutputFormat::Aligned(TableOpts::default())
        );
    }

    #[test]
    fn fmt_flags() {
        let table = |src: &str| match parse(src).unwrap().output {
            OutputFormat::Aligned(t) => t,
            OutputFormat::Csv => panic!("{src}: no table"),
        };
        let six = TableOpts::default();
        assert_eq!(six.decimals, Some(6));
        for striped in ["fmt -s", "fmt --stripes"] {
            assert_eq!(
                table(striped),
                TableOpts {
                    stripes: true,
                    ..six
                }
            );
        }
        for full in ["fmt -f", "fmt --full"] {
            assert_eq!(
                table(full),
                TableOpts {
                    decimals: None,
                    ..six
                }
            );
        }
        for two in [
            "fmt -p 2",
            "fmt --precision 2",
            "fmt -p=2",
            "fmt --precision=2",
        ] {
            assert_eq!(
                table(two),
                TableOpts {
                    decimals: Some(2),
                    ..six
                }
            );
        }
        for human in ["fmt -h", "fmt --human"] {
            assert_eq!(table(human), TableOpts { human: true, ..six });
        }
        assert_eq!(
            table("fmt -h -s -p 0"),
            TableOpts {
                stripes: true,
                decimals: Some(0),
                human: true
            }
        );
        assert_eq!(
            table("fmt -f -h"),
            TableOpts {
                decimals: None,
                human: true,
                ..six
            }
        );

        let msg = |src: &str| parse(src).unwrap_err().to_string();
        assert!(
            msg("fmt -f -p 2").contains("not both"),
            "{}",
            msg("fmt -f -p 2")
        );
        assert!(msg("fmt -p").contains("-p expects a value"));
        assert!(msg("fmt -p=2 -p 3").contains("fmt takes -p only once"));
        assert!(msg("fmt -p 2 -p=3").contains("fmt takes -p only once"));
        assert!(msg("fmt -p x").contains("\"x\""));
        assert!(msg("fmt -p -1").contains("\"-1\""));
        assert!(msg("fmt -x").contains("-h (--human), not \"-x\""));
    }

    #[test]
    fn head_count_forms() {
        // bash-like spellings all yield the same count; bare `head` defaults to 10.
        for (script, want) in [
            ("head", 10),
            ("head 7", 7),
            ("head -n 7", 7),
            ("head -n7", 7),
            ("head --lines 7", 7),
            ("head --lines=7", 7),
            ("head -7", 7),
        ] {
            let plan = parse(script).unwrap();
            let got = plan.stages.iter().find_map(|s| match s {
                Stage::Head(n) => Some(*n),
                _ => None,
            });
            assert_eq!(got, Some(want), "script: {script}");
        }
        // A non-integer count still errors (the negative form is tested in
        // head_negative_is_drop_last).
        assert!(parse("head -3.5").is_err());
    }

    #[test]
    fn stats_all_and_named() {
        // Bare `stats` profiles every column (empty list).
        let plan = parse("stats").unwrap();
        let Stage::Stats(s) = &plan.stages[0] else {
            panic!()
        };
        assert!(s.cols.is_empty());

        // `stats a,b` after a filter is its own stage with the named columns.
        let plan = parse("select a > 0 | stats a,b").unwrap();
        let Stage::Stats(s) = &plan.stages[1] else {
            panic!()
        };
        assert_eq!(s.cols, ["a", "b"]);
    }

    #[test]
    fn color_predicate_and_gradient() {
        // whole-row predicate
        let plan = parse("color red amount < 0 | fmt").unwrap();
        assert_eq!(plan.colors.len(), 1);
        let ColorRule::Predicate { scope, expr, .. } = &plan.colors[0] else {
            panic!()
        };
        assert!(matches!(scope, ColorScope::Row));
        assert!(matches!(expr, BoolExpr::Cmp(_)));

        // cell-scoped predicate
        let plan = parse("color -c amount yellow amount > 1000").unwrap();
        assert!(matches!(
            &plan.colors[0],
            ColorRule::Predicate {
                scope: ColorScope::Cell(_),
                ..
            }
        ));

        // gradient: explicit bounds, then default bounds
        let plan = parse("color -g amount green:red 0 5000 | fmt").unwrap();
        let ColorRule::Gradient { bounds, .. } = &plan.colors[0] else {
            panic!()
        };
        assert_eq!(*bounds, Some((0.0, 5000.0)));
        let plan = parse("color -g price green:red").unwrap();
        let ColorRule::Gradient { bounds, .. } = &plan.colors[0] else {
            panic!()
        };
        assert_eq!(*bounds, None);

        // ramp omitted ⇒ default green:red; bounds may still follow.
        let plan = parse("color -g amount | fmt").unwrap();
        let ColorRule::Gradient { ramp, bounds, .. } = &plan.colors[0] else {
            panic!()
        };
        assert_eq!(*ramp, Ramp::default());
        assert_eq!(*bounds, None);
        let plan = parse("color -g amount 0 5000").unwrap();
        let ColorRule::Gradient { ramp, bounds, .. } = &plan.colors[0] else {
            panic!()
        };
        assert_eq!(*ramp, Ramp::default());
        assert_eq!(*bounds, Some((0.0, 5000.0)));
    }

    #[test]
    fn color_errors() {
        assert!(parse("color").is_err()); // no args
        assert!(parse("color red").is_err()); // colour but no expression
        assert!(parse("color notacolour x > 0").is_err()); // unknown colour
        assert!(parse("color -g amount green:red 0").is_err()); // only one bound
        assert!(parse("color -g amount green:notacolour").is_err()); // bad ramp colour
    }

    #[test]
    fn errors() {
        assert!(parse("").is_err());
        assert!(parse("frobnicate a").is_err());
        assert!(parse("cols").is_err());
        assert!(parse("select a >< b").is_err());
        assert!(parse("sort a=z").is_err());
        // A whole-expression quote is rejected (string literals only).
        assert!(parse(r#"select "a > 0""#).is_err());
        assert!(parse("head abc").is_err()); // head needs a number
        assert!(parse("rename old").is_err()); // rename needs old=new
        assert!(parse("fmt x").is_err()); // not a flag
        assert!(parse("fmt -s -s").is_err());
        assert!(parse("fmt -p 2 --precision 3").is_err());
        assert!(parse("join r.csv").is_err()); // missing `on KEYS`
        assert!(parse("join on sku").is_err()); // missing file
        assert!(parse("join r.csv on").is_err()); // empty key list
        assert!(parse("join --bogus r.csv on sku").is_err()); // unknown flag
        assert!(parse("join r.csv on a=").is_err()); // malformed key pair
    }

    #[test]
    fn expression_errors_name_the_offending_token() {
        let msg = |s: &str| parse(s).unwrap_err().to_string();
        // Trailing junk after a complete expression names the stray token.
        assert!(
            msg("select a > 0 b").contains("unexpected 'b'"),
            "{}",
            msg("select a > 0 b")
        );
        assert!(msg("add x = a + b c").contains("unexpected 'c'"));
        // A missing operator lists the valid ones and says what it found.
        let m = msg("select a");
        assert!(
            m.contains("comparison operator") && m.contains("end of expression"),
            "{m}"
        );
        // Missing operand / close paren report what was found.
        assert!(msg("select a >").contains("found end of expression"));
        assert!(msg("select (a > 0").contains("expected ')'"));
        assert!(msg("add x = )").contains("found ')'"));
        // The shared lexer message no longer hardcodes "select".
        let u = msg("color red a == 'x");
        assert!(
            u.contains("unterminated string literal") && !u.contains("select"),
            "{u}"
        );
    }

    #[test]
    fn join_parses_flags_keys_and_subpipeline() {
        // Type flag, aliased + composite keys, and a sub-pipeline whose inner
        // `|` must not split the outer pipeline.
        let plan =
            parse("join -l (cols sku,price | select price > 0) r.csv on sku=item,qty").unwrap();
        let [Stage::Join(j)] = plan.stages.as_slice() else {
            panic!("expected a single join stage, got {:?}", plan.stages);
        };
        assert_eq!(j.join_type, JoinType::Left);
        assert_eq!(j.file, "r.csv");
        assert_eq!(
            j.keys,
            vec![
                ("sku".to_string(), "item".to_string()),
                ("qty".to_string(), "qty".to_string()),
            ]
        );
        assert_eq!(j.right_plan.stages.len(), 1); // the sub-pipeline's transform
    }

    #[test]
    fn join_suffix_flags() {
        let plan = parse("join --lsuffix _l --rsuffix=_r r.csv on k").unwrap();
        let [Stage::Join(j)] = plan.stages.as_slice() else {
            panic!("expected a join stage");
        };
        assert_eq!(j.lsuffix.as_deref(), Some("_l"));
        assert_eq!(j.rsuffix.as_deref(), Some("_r"));
        // Short spellings: `-L S` / `-R S`.
        let plan = parse("join -L _l -R=_r r.csv on k").unwrap();
        let Stage::Join(j) = &plan.stages[0] else {
            panic!()
        };
        assert_eq!(j.lsuffix.as_deref(), Some("_l"));
        assert_eq!(j.rsuffix.as_deref(), Some("_r"));
    }

    #[test]
    fn join_multiple_files_shared_trailing_keys() {
        // Comma-separated files with one trailing `on` shared by all; the
        // composite key list's own commas must not split items.
        let plan = parse("join a.csv, b.csv on ts,serial").unwrap();
        let [Stage::Join(a), Stage::Join(b)] = plan.stages.as_slice() else {
            panic!("expected two join stages, got {:?}", plan.stages);
        };
        assert_eq!(a.file, "a.csv");
        assert_eq!(b.file, "b.csv");
        let keys = vec![
            ("ts".to_string(), "ts".to_string()),
            ("serial".to_string(), "serial".to_string()),
        ];
        assert_eq!(a.keys, keys);
        assert_eq!(b.keys, keys);
    }

    #[test]
    fn join_multiple_files_per_item_keys_and_subpipelines() {
        // Every item carries its own `on`; sub-pipelines and aliased keys are
        // per-item. A keyless fragment after an `on` extends that key list.
        let plan =
            parse("join (cols -v metric) a.csv on ts, sn, (rename v=w) b.csv on ts=stamp").unwrap();
        let [Stage::Join(a), Stage::Join(b)] = plan.stages.as_slice() else {
            panic!("expected two join stages, got {:?}", plan.stages);
        };
        assert_eq!(a.file, "a.csv");
        assert_eq!(
            a.keys,
            vec![
                ("ts".to_string(), "ts".to_string()),
                ("sn".to_string(), "sn".to_string()),
            ]
        );
        assert_eq!(a.right_plan.stages.len(), 1);
        assert_eq!(b.file, "b.csv");
        assert_eq!(b.keys, vec![("ts".to_string(), "stamp".to_string())]);
        assert_eq!(b.right_plan.stages.len(), 1);
    }

    #[test]
    fn join_shared_flags_apply_to_every_item() {
        let plan = parse("join -l --rsuffix _x a.csv, b.csv on k").unwrap();
        let [Stage::Join(a), Stage::Join(b)] = plan.stages.as_slice() else {
            panic!("expected two join stages, got {:?}", plan.stages);
        };
        for j in [a, b] {
            assert_eq!(j.join_type, JoinType::Left);
            assert_eq!(j.rsuffix.as_deref(), Some("_x"));
        }
    }

    #[test]
    fn join_rejects_mixed_key_forms() {
        // A keyless first item followed by keyed items is neither the
        // all-explicit nor the single-trailing-`on` form.
        let e = parse("join a.csv, b.csv on x, c.csv on y").unwrap_err();
        assert!(e.to_string().contains("every file"), "{e}");
        // A stray comma is a dedicated error.
        let e = parse("join a.csv,, b.csv on k").unwrap_err();
        assert!(e.to_string().contains("stray comma"), "{e}");
        // A lone file still requires `on`.
        let e = parse("join a.csv").unwrap_err();
        assert!(e.to_string().contains("expects `on"), "{e}");
        // `on` with no keys.
        let e = parse("join a.csv on").unwrap_err();
        assert!(e.to_string().contains("at least one key"), "{e}");
        // No file at all: the pre-branch message, not the stray-comma one.
        let e = parse("join").unwrap_err();
        assert!(e.to_string().contains("right-side file"), "{e}");
        let e = parse("join -l").unwrap_err();
        assert!(e.to_string().contains("right-side file"), "{e}");
        // An empty quoted token is not a silent key-extension.
        let e = parse("join a.csv on x, '' b.csv on y").unwrap_err();
        assert!(e.to_string().contains("right-side file"), "{e}");
    }

    #[test]
    fn join_quoted_path_keeps_its_comma() {
        // A file path containing a comma must be quoted; the quoted comma is
        // not an item separator.
        let plan = parse("join 'a,b.csv' on k").unwrap();
        let [Stage::Join(j)] = plan.stages.as_slice() else {
            panic!("expected one join stage, got {:?}", plan.stages);
        };
        assert_eq!(j.file, "a,b.csv");
    }

    /// The single `add`'s expression, for assertions.
    fn add_expr(script: &str) -> ValExpr {
        let plan = parse(script).unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!("expected a transform stage");
        };
        let Stmt::Add(a) = &stmts[0] else {
            panic!("expected an add statement, got {:?}", stmts[0]);
        };
        a.expr.clone()
    }

    #[test]
    fn add_arithmetic_precedence() {
        // a + b * c parses as a + (b * c).
        let ValExpr::Arith {
            op: ArithOp::Add,
            rhs,
            ..
        } = add_expr("add v = a + b * c")
        else {
            panic!("expected a top-level +");
        };
        assert!(matches!(
            *rhs,
            ValExpr::Arith {
                op: ArithOp::Mul,
                ..
            }
        ));
    }

    #[test]
    fn add_binary_minus_is_not_a_negative_literal() {
        // `amount - prev(amount)` must lex `-` as subtraction, not a sign on a
        // number — the case that makes a step delta expressible.
        assert!(matches!(
            add_expr("add d = amount - prev(amount)"),
            ValExpr::Arith {
                op: ArithOp::Sub,
                ..
            }
        ));
        // But in unary position a signed number is still a literal.
        assert!(matches!(add_expr("add d = -5"), ValExpr::Num(n) if n == -5.0));
    }

    #[test]
    fn add_prev_and_rownum_are_stateful() {
        for script in ["add d = a - prev(a)", "add n = rownum()"] {
            let plan = parse(script).unwrap();
            let Stage::Transform(stmts) = &plan.stages[0] else {
                panic!();
            };
            assert!(stmts[0].is_stateful(), "{script} should be stateful");
        }
        // Pure arithmetic is not stateful (it can shard).
        let plan = parse("add t = a * 2").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!();
        };
        assert!(!stmts[0].is_stateful());
    }

    #[test]
    fn add_ternary_and_concat() {
        assert!(matches!(
            add_expr("add tier = a > 1 ? 'big' : 'small'"),
            ValExpr::Cond { .. }
        ));
        assert!(matches!(
            add_expr("add full = a ++ ' ' ++ b"),
            ValExpr::Concat(parts) if parts.len() == 3
        ));
    }

    #[test]
    fn add_rejects_bad_input() {
        assert!(parse("add").is_err()); // no name
        assert!(parse("add x").is_err()); // no expression
        assert!(parse("add x = a +").is_err()); // dangling operator
        assert!(parse("add x = bogus(a)").is_err()); // unknown function
        assert!(parse("add x = prev(a + 1)").is_err()); // prev needs a bare column
        assert!(parse("add x = round(a, b)").is_err()); // wrong arity
    }

    #[test]
    fn add_requires_an_equals_sign() {
        let plan = parse("add total = amount * qty").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Add(a) = &stmts[0] else { panic!() };
        assert_eq!(a.name, "total");
        // Spaces around `=` are optional; a backticked name works too.
        assert!(parse("add total=amount * qty").is_ok());
        assert!(parse("add `my col` = 1").is_ok());
        // The old spacing form is an error that shows the `=` spelling.
        let err = parse("add total amount * qty").unwrap_err().to_string();
        assert!(err.contains("add total = amount * qty"), "{err}");
        let err = parse("add total").unwrap_err().to_string();
        assert!(err.contains("add NAME = EXPR"), "{err}");
        // `==` is not an assignment, and the hint does not paste it back in.
        let err = parse("add total == 1").unwrap_err().to_string();
        assert!(err.contains("`add total = 1`"), "{err}");
        assert!(!err.contains("= =="), "{err}");
        // The hints see past a backticked name (which the rest is sliced after).
        let err = parse("add `x y` foo").unwrap_err().to_string();
        assert!(err.contains("add `x y` = foo"), "{err}");
        let err = parse("add `é` == 1").unwrap_err().to_string();
        assert!(err.contains("`add é = 1`"), "{err}");
    }

    #[test]
    fn delta_points_at_the_add_form() {
        let err = parse("delta a b").unwrap_err().to_string();
        assert!(err.contains("removed"), "{err}");
        assert!(
            err.contains("add a_delta = a - prev(a) | add b_delta = b - prev(b)"),
            "{err}"
        );
        // `-s SUF` (and `-sSUF`) is a flag, not a column.
        let err = parse("delta -s _change a").unwrap_err().to_string();
        assert!(err.contains("`add a_delta = a - prev(a)`"), "{err}");
        let err = parse("delta -s_change a").unwrap_err().to_string();
        assert!(err.contains("`add a_delta = a - prev(a)`"), "{err}");
        assert!(
            parse("delta")
                .unwrap_err()
                .to_string()
                .contains("add COL_delta")
        );
    }

    #[test]
    fn color_gradient_multiple_columns() {
        // One gradient rule per column, sharing the ramp/bounds.
        let plan = parse("color -g a b c 0 10").unwrap();
        assert_eq!(plan.colors.len(), 3);
        for (rule, name) in plan.colors.iter().zip(["a", "b", "c"]) {
            let ColorRule::Gradient { col, bounds, .. } = rule else {
                panic!("expected a gradient");
            };
            assert_eq!(col.name, name);
            assert_eq!(*bounds, Some((0.0, 10.0)));
        }
    }

    #[test]
    fn newlines_separate_stages_and_blank_lines_are_skipped() {
        // A multi-line `-f`-style script: newlines split stages, and blank or
        // comment-only lines are dropped.
        let script = "# header comment\nselect a > 0\n\nadd b = a * 2   # trailing comment\nfmt";
        let plan = parse(script).unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!();
        };
        assert!(matches!(stmts[0], Stmt::Select(_)));
        assert!(matches!(&stmts[1], Stmt::Add(a) if a.name == "b"));
        assert_eq!(plan.output, OutputFormat::Aligned(TableOpts::default()));

        // A newline inside a `join (…)` group doesn't split the outer pipeline.
        let plan = parse("rename value=a\njoin (\n rename value=b\n) r.csv on key\nfmt").unwrap();
        assert!(plan.stages.iter().any(|s| matches!(s, Stage::Join(_))));
    }

    #[test]
    fn prologue_extracts_fn_definitions() {
        let (fns, rest) =
            parse_prologue("fn prep(n) { rename value=n | cols -v m }\nfn t() { head }\nprep(x)")
                .unwrap();
        assert_eq!(fns.len(), 2);
        let prep = &fns["prep"];
        assert_eq!(prep.params, vec!["n".to_string()]);
        assert_eq!(prep.body, "rename value=n | cols -v m");
        assert!(fns["t"].params.is_empty());
        assert_eq!(rest, "prep(x)");
        // No prologue: everything is the remainder.
        let (fns, rest) = parse_prologue("head 3").unwrap();
        assert!(fns.is_empty());
        assert_eq!(rest, "head 3");
    }

    #[test]
    fn prologue_definition_errors() {
        let m = |s: &str| parse_prologue(s).unwrap_err().to_string();
        assert!(
            m("fn cols(a) { head }").contains("collides"),
            "{}",
            m("fn cols(a) { head }")
        );
        assert!(m("fn f(a) { head }\nfn f(b) { tail }").contains("defined twice"));
        assert!(m("fn f(a, a) { head }").contains("duplicate parameter"));
        assert!(m("fn f(a) { head").contains("missing `}`"));
        assert!(m("fn f a { head }").contains("malformed"));
        assert!(m("fn 9x(a) { head }").contains("not a valid name"));
        assert!(m("fn f(a-b) { head }").contains("not a valid parameter"));
    }

    #[test]
    fn subst_params_is_identifier_bounded_and_quote_aware() {
        let params = vec!["a".to_string(), "value_f".to_string()];
        let args = ["pv_active", "grid_q"];
        // Whole identifiers substitute; substrings (`abs`, `a1`) and quoted
        // literals do not; params inside `old=new` tokens do.
        assert_eq!(
            subst_params(
                "abs(a) + a*a1 ++ 'a' | rename value=value_f",
                &params,
                &args
            ),
            "abs(pv_active) + pv_active*a1 ++ 'a' | rename value=grid_q"
        );
        // A digit-led run is one token: `9a` does not substitute its tail.
        assert_eq!(subst_params("add x = 9a", &params, &args), "add x = 9a");
    }

    #[test]
    fn fn_fragment_expands_as_a_stage() {
        let plan = parse("fn prep(n) { rename value=n | cols -v metric }\nprep(pv)").unwrap();
        let [Stage::Transform(stmts)] = plan.stages.as_slice() else {
            panic!("expected one transform stage, got {:?}", plan.stages);
        };
        assert_eq!(stmts.len(), 2); // rename + cols, spliced in place
        // Zero-parameter fragments call as `name()`.
        let plan = parse("fn t() { head 3 }\nt()").unwrap();
        assert!(matches!(plan.stages.as_slice(), [Stage::Head(3)]));
    }

    #[test]
    fn fn_fragment_call_inside_join_subpipeline() {
        let plan = parse("fn prep(n) { rename value=n }\njoin (prep(x)) r.csv on k").unwrap();
        let [Stage::Join(j)] = plan.stages.as_slice() else {
            panic!("expected a join stage, got {:?}", plan.stages);
        };
        assert_eq!(j.right_plan.stages.len(), 1);
    }

    #[test]
    fn fn_fragment_calls_fragment() {
        let plan = parse("fn a(x) { rename v=x }\nfn b(y) { a(y) | cols -v m }\nb(q)").unwrap();
        let [Stage::Transform(stmts)] = plan.stages.as_slice() else {
            panic!("expected one transform stage, got {:?}", plan.stages);
        };
        assert_eq!(stmts.len(), 2);
    }

    #[test]
    fn fn_call_errors() {
        let m = |s: &str| parse(s).unwrap_err().to_string();
        assert!(m("fn f(a) { head }\nf(x, y)").contains("expects 1 argument"));
        assert!(m("prep(x)").contains("unknown fragment"));
        let e = m("fn prep(n) { head }\nprepp(x)");
        assert!(e.contains("did you mean `prep`"), "{e}");
        assert!(m("fn f(a) { f(a) }\nf(x)").contains("too deep"));
        let e = m("fn f(a) { bogus x }\nf(y)");
        assert!(
            e.contains("in fn `f`") && e.contains("unknown command"),
            "{e}"
        );
        assert!(m("head\nfn f(a) { tail }").contains("before the first stage"));
        // A defined fragment called without parens points at the call form.
        let e = m("fn prep(n) { head }\nprep pv");
        assert!(e.contains("call it as `prep(ARGS)`"), "{e}");
        // ...and a near-miss bare word suggests fragment names too.
        let e = m("fn prep(n) { head }\nprepp pv");
        assert!(e.contains("did you mean `prep`"), "{e}");
    }

    #[test]
    fn fn_may_not_take_a_command_or_removed_name() {
        // A user fragment may shadow neither a command nor a removed one
        // (whose hint must stay reachable).
        for name in ["cols", "colour", "delta", "group", "hdr", "to_num"] {
            let err = parse(&format!("fn {name}(a) {{ head }}\n{name}(a)"))
                .unwrap_err()
                .to_string();
            assert!(err.contains("collides"), "{name}: {err}");
        }
        // A removed command called as a fragment gets the same hint.
        let err = parse("delta(a)").unwrap_err().to_string();
        assert!(err.contains("add a_delta = a - prev(a)"), "{err}");
    }

    #[test]
    fn fn_recursion_through_join_subpipeline_hits_depth_cap() {
        // Depth threads through the join sub-parse; without it this would
        // recurse unboundedly instead of erroring.
        let e = parse("fn f(a) { join (f(a)) r.csv on k }\nf(x)")
            .unwrap_err()
            .to_string();
        assert!(e.contains("too deep"), "{e}");
    }

    #[test]
    fn fn_used_in_expression_gets_a_hint() {
        let e = parse("fn pf(a) { head }\nadd x = pf(a)")
            .unwrap_err()
            .to_string();
        assert!(e.contains("whole stages"), "{e}");
        // No fragment of that name: the plain unknown-function error stands.
        let e = parse("add x = bogus(a)").unwrap_err().to_string();
        assert!(!e.contains("whole stages"), "{e}");
    }

    /// Each part `parse_recorded` notes in `script`: its text and kind.
    fn noted(script: &str) -> Vec<(&str, &'static str)> {
        let mut rec = Recorder::default();
        let _ = parse_recorded(script, &mut rec);
        rec.into_spans()
            .into_iter()
            .map(|(at, kind)| (&script[at], kind.name()))
            .collect()
    }

    #[test]
    fn every_kind_has_its_protocol_name() {
        let kinds = [
            SpanKind::Command,
            SpanKind::Keyword,
            SpanKind::Option,
            SpanKind::Operator,
            SpanKind::String,
            SpanKind::Number,
            SpanKind::Variable,
            SpanKind::Function,
            SpanKind::Comment,
        ];
        assert_eq!(
            kinds.map(SpanKind::name),
            [
                "command", "keyword", "option", "operator", "string", "number", "variable",
                "function", "comment"
            ]
        );
    }

    #[test]
    fn recording_notes_commands_expressions_and_columns() {
        assert_eq!(
            noted("cols a | select b > 1.5 && name == 'x' || !(c =~ 'y')"),
            [
                ("cols", "command"),
                ("a", "variable"),
                ("|", "operator"),
                ("select", "command"),
                ("b", "variable"),
                (">", "operator"),
                ("1.5", "number"),
                ("&&", "operator"),
                ("name", "variable"),
                ("==", "operator"),
                ("'x'", "string"),
                ("||", "operator"),
                ("!", "operator"),
                ("c", "variable"),
                ("=~", "operator"),
                ("'y'", "string"),
            ]
        );
        // A name before `(` is a function; `inf` is a number.
        assert_eq!(
            noted("select abs(c) - inf < 2"),
            [
                ("select", "command"),
                ("abs", "function"),
                ("c", "variable"),
                ("-", "operator"),
                ("inf", "number"),
                ("<", "operator"),
                ("2", "number"),
            ]
        );
    }

    #[test]
    fn recording_after_an_error_keeps_what_came_before() {
        // An unknown command: the stages after it get their command word only.
        assert_eq!(
            noted("cols a | selct b > 1 | sort c"),
            [
                ("cols", "command"),
                ("a", "variable"),
                ("|", "operator"),
                ("|", "operator"),
                ("sort", "command"),
            ]
        );
        // An expression that does not parse: every token of it is noted.
        assert_eq!(
            noted("select a >> 1 | cols b"),
            [
                ("select", "command"),
                ("a", "variable"),
                (">", "operator"),
                (">", "operator"),
                ("1", "number"),
                ("|", "operator"),
                ("cols", "command"),
            ]
        );
        // A token the lexer cannot finish: the tokens before it are noted.
        assert_eq!(
            noted("select a == 'ab | sort c"),
            [("select", "command"), ("a", "variable"), ("==", "operator")]
        );
    }

    #[test]
    fn recording_notes_comments_where_they_are() {
        assert_eq!(
            noted("cols a # keep a\nselect a > 1 # and 'this'"),
            [
                ("cols", "command"),
                ("a", "variable"),
                ("# keep a", "comment"),
                ("select", "command"),
                ("a", "variable"),
                (">", "operator"),
                ("1", "number"),
                ("# and 'this'", "comment"),
            ]
        );
        // A `#` in a string is not a comment.
        assert_eq!(
            noted("select a == '#x'"),
            [
                ("select", "command"),
                ("a", "variable"),
                ("==", "operator"),
                ("'#x'", "string"),
            ]
        );
    }

    #[test]
    fn recording_counts_bytes_not_characters() {
        let mut rec = Recorder::default();
        parse_recorded("cols café | select né > 1", &mut rec).unwrap();
        assert_eq!(
            rec.into_spans(),
            [
                (0..4, SpanKind::Command),
                (5..10, SpanKind::Variable),
                (11..12, SpanKind::Operator),
                (13..19, SpanKind::Command),
                (20..23, SpanKind::Variable),
                (24..25, SpanKind::Operator),
                (26..27, SpanKind::Number),
            ]
        );
    }

    #[test]
    fn recorded_spans_are_in_order_and_never_overlap() {
        let mut rec = Recorder::default();
        rec.push(5..6, SpanKind::Number);
        rec.push(0..4, SpanKind::Variable);
        rec.push(0..4, SpanKind::Variable);
        rec.push(2..3, SpanKind::Operator);
        rec.push(0..2, SpanKind::Keyword);
        rec.push(7..7, SpanKind::String);
        assert_eq!(
            rec.into_spans(),
            [
                (0..2, SpanKind::Keyword),
                (2..3, SpanKind::Operator),
                (5..6, SpanKind::Number),
            ]
        );
    }

    #[test]
    fn recording_builds_the_same_plan() {
        for script in [
            "cols a,b | select a > 1 && b == 'x' | sort a=nr | head 5",
            "fn f(x) { select x > 1 }\nf(a) | agg n=sum(a) by b",
            "join -l (cols k | uniq k) r.csv on k=id | color red v > 1 | fmt -s",
            "add c = a ++ '!' # note\ngraph line a c -t T",
            "cols a | selct b",
            "select a >> 1",
            "join (selct a) r.csv on k | cols b",
            "join -l (cols a | select a >> 1) r.csv on k, (uniq k) s.csv on k",
            "cols -v a | rename a = b | add t = abs(c) | select -v t > 1 | tail --lines=+3",
            "color -g v w green:red 0 10 | graph hist v -b 5",
            "fn f(x, y) { select x > 1 | g(y) }\nfn g(z) { uniq z }\nf(a, b) | cols c",
            "fn t(n) { head n }\nt(3)",
            "fn f(x) { head }\nfn f(y) { tail }\nf(a)",
            "fn g(y { uniq y }\nfn h() { cols a }\nh() | sort b",
            "fn g(y) { uniq y\ncols a",
            "fn g(y { uniq y }\nfn f(x) { head }\nfn f(y) { tail }\nf(a)",
        ] {
            let plain = format!("{:?}", parse(script));
            let recorded = format!("{:?}", parse_recorded(script, &mut Recorder::default()));
            assert_eq!(plain, recorded, "{script}");
        }
    }

    #[test]
    fn recording_notes_flags_names_and_keywords() {
        for (script, want) in [
            (
                "cols -v a",
                vec![("cols", "command"), ("-v", "option"), ("a", "variable")],
            ),
            (
                "select -v a > 1",
                vec![
                    ("select", "command"),
                    ("-v", "option"),
                    ("a", "variable"),
                    (">", "operator"),
                    ("1", "number"),
                ],
            ),
            (
                "sort d=nr e",
                vec![
                    ("sort", "command"),
                    ("d", "variable"),
                    ("=nr", "option"),
                    ("e", "variable"),
                ],
            ),
            (
                "head -n 5",
                vec![("head", "command"), ("-n", "option"), ("5", "number")],
            ),
            (
                "tail --lines=+3",
                vec![("tail", "command"), ("--lines", "option"), ("+3", "number")],
            ),
            (
                "add t = abs(c)",
                vec![
                    ("add", "command"),
                    ("t", "variable"),
                    ("=", "operator"),
                    ("abs", "function"),
                    ("c", "variable"),
                ],
            ),
            (
                "rename a = b",
                vec![
                    ("rename", "command"),
                    ("a", "variable"),
                    ("=", "operator"),
                    ("b", "variable"),
                ],
            ),
            (
                "agg n=sum(e), count by f",
                vec![
                    ("agg", "command"),
                    ("n", "variable"),
                    ("=", "operator"),
                    ("sum", "function"),
                    ("e", "variable"),
                    ("count", "function"),
                    ("by", "keyword"),
                    ("f", "variable"),
                ],
            ),
            (
                "join -l --lsuffix=_x (cols k) 'r s.csv' on k=id",
                vec![
                    ("join", "command"),
                    ("-l", "option"),
                    ("--lsuffix", "option"),
                    ("cols", "command"),
                    ("k", "variable"),
                    ("'r s.csv'", "string"),
                    ("on", "keyword"),
                    ("k", "variable"),
                    ("=", "operator"),
                    ("id", "variable"),
                ],
            ),
            (
                "color -c v red v > 1",
                vec![
                    ("color", "command"),
                    ("-c", "option"),
                    ("v", "variable"),
                    ("red", "keyword"),
                    ("v", "variable"),
                    (">", "operator"),
                    ("1", "number"),
                ],
            ),
            (
                "color -g v w green:red 0 10",
                vec![
                    ("color", "command"),
                    ("-g", "option"),
                    ("v", "variable"),
                    ("w", "variable"),
                    ("green:red", "keyword"),
                    ("0", "number"),
                    ("10", "number"),
                ],
            ),
            (
                "fmt -s -p 2",
                vec![("fmt", "command"), ("-s", "option"), ("-p", "option")],
            ),
            (
                "graph line x y -t T --color-by=z",
                vec![
                    ("graph", "command"),
                    ("line", "keyword"),
                    ("x", "variable"),
                    ("y", "variable"),
                    ("-t", "option"),
                    ("--color-by", "option"),
                    ("z", "variable"),
                ],
            ),
        ] {
            assert_eq!(noted(script), want, "{script}");
        }
    }

    #[test]
    fn recording_notes_fn_definitions_but_not_expansions() {
        assert_eq!(
            noted("fn f(x, y) { select x > 1 | g(y) }\nfn g(z) { uniq z }\nf(a, b) | cols c"),
            [
                ("fn", "keyword"),
                ("f", "function"),
                ("x", "variable"),
                ("y", "variable"),
                ("select", "command"),
                ("x", "variable"),
                (">", "operator"),
                ("1", "number"),
                ("|", "operator"),
                ("g", "command"),
                ("fn", "keyword"),
                ("g", "function"),
                ("z", "variable"),
                ("uniq", "command"),
                ("z", "variable"),
                ("f", "command"),
                ("|", "operator"),
                ("cols", "command"),
                ("c", "variable"),
            ]
        );
        // A body that does not parse on its own is noted as far as it goes,
        // and the script still parses.
        let mut rec = Recorder::default();
        assert!(parse_recorded("fn t(n) { head n }\nt(3)", &mut rec).is_ok());
        let script = "fn t(n) { head n }\nt(3)";
        let kinds: Vec<(&str, &str)> = rec
            .into_spans()
            .into_iter()
            .map(|(at, kind)| (&script[at], kind.name()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("fn", "keyword"),
                ("t", "function"),
                ("n", "variable"),
                ("head", "command"),
                ("t", "command"),
            ]
        );
    }

    #[test]
    fn recording_after_a_fn_error_keeps_the_rest() {
        // A definition that fails: the ones before and after it are noted,
        // and each stage gets its command word, as after a stage error.
        assert_eq!(
            noted(
                "fn f(x) { select x > 1 }\nfn g(y { uniq y }\nfn h() { cols a }\nf(b) | h() | sort c"
            ),
            [
                ("fn", "keyword"),
                ("f", "function"),
                ("x", "variable"),
                ("select", "command"),
                ("x", "variable"),
                (">", "operator"),
                ("1", "number"),
                ("fn", "keyword"),
                ("fn", "keyword"),
                ("h", "function"),
                ("cols", "command"),
                ("a", "variable"),
                ("f", "command"),
                ("|", "operator"),
                ("h", "command"),
                ("|", "operator"),
                ("sort", "command"),
            ]
        );
        // A name defined twice: both bodies are noted.
        assert_eq!(
            noted("fn f(x) { head }\nfn f(y) { tail }\nf(a)"),
            [
                ("fn", "keyword"),
                ("f", "function"),
                ("x", "variable"),
                ("head", "command"),
                ("fn", "keyword"),
                ("f", "function"),
                ("y", "variable"),
                ("tail", "command"),
                ("f", "command"),
            ]
        );
        // A body left open runs to the end, so no stage follows it.
        assert_eq!(
            noted("fn f(x) { head }\nfn g(y) { uniq y\ncols a"),
            [
                ("fn", "keyword"),
                ("f", "function"),
                ("x", "variable"),
                ("head", "command"),
                ("fn", "keyword"),
                ("g", "function"),
                ("y", "variable"),
            ]
        );
    }

    /// [`depths`] for `marked`, a script with one `@` where it is split,
    /// as `(new, current)`.
    fn split_at(marked: &str) -> (usize, usize) {
        let at = marked.find('@').expect("a split mark");
        let d = depths(&marked.replacen('@', "", 1), at);
        (d.new, d.current)
    }

    #[test]
    fn top_level_lines_are_at_depth_0() {
        assert_eq!(split_at("@"), (0, 0));
        assert_eq!(split_at("head\n| sort x@"), (0, 0));
        assert_eq!(split_at("head |@"), (0, 0));
        assert_eq!(split_at("head@ | sort x"), (0, 0));
    }

    #[test]
    fn an_fn_body_is_one_step_in() {
        assert_eq!(split_at("fn prep(n) {@"), (1, 0));
        assert_eq!(split_at("fn prep(n) {\n  rename value=n@"), (1, 1));
        assert_eq!(
            split_at("fn prep(n) {\n  rename value=n\n  | cols -v metric\n}@"),
            (0, 0)
        );
        assert_eq!(split_at("fn prep(n) {\n  head\n}\nprep(pv)@"), (0, 0));
        // The parameter list is not a group.
        assert_eq!(split_at("fn prep(n@"), (0, 0));
        // After a body's `}` a new stage starts, as the prologue reads on.
        assert_eq!(split_at("fn f(x) { head } join (@"), (1, 0));
    }

    #[test]
    fn an_fn_headers_brace_may_be_on_its_own_line() {
        // `parse_fn_def` finds the body's `{` across newlines; a newline in
        // the header must not read as a new, non-`fn` stage.
        assert_eq!(split_at("fn f(x)\n{@"), (1, 0));
        assert_eq!(split_at("fn f(x)\n{\n  head@"), (1, 1));
        // The parameter list itself may start on its own line too.
        assert_eq!(split_at("fn f\n(x) {@"), (1, 0));
        assert_eq!(split_at("fn f\n(x) {\n  head@"), (1, 1));
        // And so may the name: the parser reads a newline after `fn` as a
        // blank.
        assert_eq!(split_at("fn\nf(x) {@"), (1, 0));
        assert_eq!(split_at("fn\nf(x) {\n  head@"), (1, 1));
        // The `}` that closes the body still starts a new stage.
        assert_eq!(split_at("fn f(x)\n{\n  head\n}@"), (0, 0));
    }

    #[test]
    fn a_join_group_is_one_step_in() {
        assert_eq!(split_at("head\n| join (@"), (1, 0));
        assert_eq!(split_at("head\n| join (\n  cols a,b@"), (1, 1));
        assert_eq!(
            split_at("head\n| join (\n  cols a,b\n) other.csv on a@"),
            (0, 0)
        );
        // After flags, and in a second item.
        assert_eq!(split_at("join -l --lsuffix _x (@"), (1, 0));
        assert_eq!(split_at("join (cols a) a.csv on k, (@"), (1, 0));
        // A lone `|` starts a stage; `||` does not.
        assert_eq!(split_at("select a | join (@"), (1, 0));
        assert_eq!(split_at("select a || join (@"), (0, 0));
    }

    #[test]
    fn other_brackets_are_not_groups() {
        assert_eq!(split_at("select (a > 1 ||@"), (0, 0));
        assert_eq!(split_at("select (\n  a > 1@"), (0, 0));
        assert_eq!(split_at("add b = abs(a@)"), (0, 0));
        // A fragment call, and `join(` with no blank, which is not `join`.
        assert_eq!(split_at("prep(@"), (0, 0));
        assert_eq!(split_at("join(@"), (0, 0));
        // A `)` that closes an expression's bracket is not a step out.
        assert_eq!(split_at("join (select (a > 1@)"), (1, 0));
    }

    #[test]
    fn groups_nest() {
        assert_eq!(split_at("fn f(x) {\n  join (\n    join (@"), (3, 2));
        assert_eq!(
            split_at("fn f(x) {\n  join (\n    join (inner.csv) b.csv on k@"),
            (2, 2)
        );
    }

    #[test]
    fn a_closing_bracket_after_the_split_moves_the_new_line_out() {
        assert_eq!(split_at("fn f(x) {\n  head@}"), (0, 1));
        assert_eq!(split_at("fn f(x) {\n  head@   }"), (0, 1));
        assert_eq!(split_at("join (\n  cols a@ ) b.csv on k"), (0, 1));
        // The line split starts with one: that line is one step out too.
        assert_eq!(split_at("join (\n  cols a\n  ) b.csv on k@"), (0, 0));
        assert_eq!(split_at("fn f(x) {\n  head\n  }@"), (0, 0));
        // A closer that closes no group is passed over.
        assert_eq!(split_at("join (\n  cols a\n  }@"), (1, 1));
    }

    #[test]
    fn a_bracket_in_a_quote_or_a_comment_is_text() {
        assert_eq!(split_at("select a == '{(' @"), (0, 0));
        assert_eq!(split_at("select a == \"join (\" | head@"), (0, 0));
        assert_eq!(split_at("join (`a)b` @"), (1, 0));
        assert_eq!(split_at("fn f(x) { select a == '}' @"), (1, 0));
        assert_eq!(split_at("head # join (@"), (0, 0));
        assert_eq!(split_at("join ( # )\n  cols a@"), (1, 1));
        // A `#` inside a quote is not a comment.
        assert_eq!(split_at("select a == '#' | join (@"), (1, 0));
        // Split inside a comment: the depth where the comment started.
        assert_eq!(split_at("join ( # a@ b"), (1, 0));
    }

    #[test]
    fn text_in_an_unclosed_string_is_at_the_depth_where_it_started() {
        assert_eq!(split_at("join (\n  select a == 'x@"), (1, 1));
        // A `)` inside the string does not close the group.
        assert_eq!(split_at("join (\n  select a == 'x\n) y@"), (1, 1));
        assert_eq!(split_at("join (\n  select a == 'x@\n) y"), (1, 1));
    }

    #[test]
    fn depths_never_go_below_0() {
        assert_eq!(split_at("head\n)@"), (0, 0));
        assert_eq!(split_at("}})@)"), (0, 0));
        assert_eq!(split_at("join (a.csv on k))\n)@"), (0, 0));
    }

    #[test]
    fn a_split_past_the_end_or_inside_a_character_is_safe() {
        assert_eq!(depths("join (", 99), Depths { new: 1, current: 0 });
        assert_eq!(depths("é(", 1), Depths { new: 0, current: 0 });
        // Byte 10 is inside the `é` of a comment.
        assert_eq!(depths("join ( # é\n", 10), Depths { new: 1, current: 0 });
    }

    #[test]
    fn tabs_and_crlf_lines_are_read_like_spaces_and_newlines() {
        assert_eq!(split_at("fn f(x) {\n\thead@\t}"), (0, 1));
        assert_eq!(split_at("join (\r\n  cols a@\r\n) b.csv on k"), (1, 1));
        assert_eq!(split_at("join (\r\n  cols a\r\n\t) b.csv on k@"), (0, 0));
    }

    #[test]
    fn a_split_at_the_start_of_a_line_reads_that_line() {
        assert_eq!(split_at("join (\n@  cols a"), (1, 1));
        assert_eq!(split_at("join (\n  cols a\n@) b.csv on k"), (0, 0));
    }

    #[test]
    fn a_script_nested_very_deep_is_scanned_without_recursion() {
        let script = format!("select {}", "(".repeat(100_000));
        assert_eq!(depths(&script, script.len()), Depths { new: 0, current: 0 });
        let script = "join (".repeat(10_000);
        assert_eq!(
            depths(&script, script.len()),
            Depths {
                new: 10_000,
                current: 0
            }
        );
    }

    #[test]
    fn a_closer_finds_the_innermost_open_group_of_its_kind() {
        // A `}` closes the `(` left open inside its `{` too; the `)` after
        // it then closes the `join` group.
        assert_eq!(split_at("join ( x { ( } ) b.csv\nhead@"), (0, 0));
        // A `(` closed that way is gone: a later `)` closes nothing.
        assert_eq!(
            split_at("fn g(y) {\n  { ( }\n  { { ) }\n  }\n  head@"),
            (1, 1)
        );
        // The same the other way: a `)` closes a `{` left open inside it.
        assert_eq!(split_at("fn f(x) {\n  join ( { )\n  head\n}@"), (0, 0));
    }

    #[test]
    fn a_run_of_the_other_kind_of_stray_closer_does_not_go_quadratic() {
        // 100,000 unmatched `(` (none of them a `join` or `fn` group), then
        // 100,000 `}`: a `}` finds that no bracket it can close is open
        // without walking every open bracket. A generous bound: this is a
        // debug build and the machine may be loaded.
        let script = format!("select {}{}", "(".repeat(100_000), "}".repeat(100_000));
        let start = std::time::Instant::now();
        assert_eq!(depths(&script, script.len()), Depths { new: 0, current: 0 });
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn a_long_fn_header_of_blank_lines_does_not_go_quadratic() {
        // `fn`, then 50,000 blank lines and no `(` yet: the stage's first
        // word is read once, not again at each newline over all the blanks
        // after `fn`. A generous bound: this is a debug build and the
        // machine may be loaded.
        let script = format!("fn {}", " \n".repeat(50_000));
        let start = std::time::Instant::now();
        assert_eq!(depths(&script, script.len()), Depths { new: 0, current: 0 });
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn closed_groups_before_the_split_are_gone() {
        assert_eq!(split_at("fn f(x) { head }\nfn g(y) {@"), (1, 0));
        assert_eq!(
            split_at("fn f(x) {\n  join (cols a) b.csv on k\n}@"),
            (0, 0)
        );
        assert_eq!(split_at("fn f(x) {\n  join (cols a) b.csv on k@"), (1, 1));
    }
}
