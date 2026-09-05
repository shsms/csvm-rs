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

use crate::color::{Ramp, parse_ramp, parse_style};
use crate::error::Error;
use crate::plan::{
    AddStmt, AffixKind, AggFunc, AggSpec, ArithOp, BoolExpr, Cmp, CmpMode, CmpOp, ColRef,
    ColorRule, ColorScope, Func, GraphKind, GraphOpts, GraphSpec, GroupStmt, JoinStmt, JoinType,
    OutputFormat, Plan, ProjectStmt, RenameStmt, SortKey, SortMode, SortStmt, Stage, StatsStmt,
    Stmt, UniqStmt, ValExpr,
};

/// Compile a pipe script into an executable [`Plan`].
pub fn parse(script: &str) -> Result<Plan, Error> {
    let script = strip_comments(script);
    let (fns, rest) = parse_prologue(&script)?;
    parse_stages(rest, &fns, 0)
}

/// Parse stage text into a plan. Sub-pipelines and fragment bodies re-enter
/// here with the shared fn table and their expansion depth.
fn parse_stages(script: &str, fns: &FnTable, depth: usize) -> Result<Plan, Error> {
    let mut builder = Builder::new(fns, depth);
    for stage in split_stages(script) {
        let stage = stage.trim();
        // Skip blank stages: a blank or comment-only line in a multi-line `-f`
        // script, or a trailing `|`. A wholly empty script is caught below.
        if stage.is_empty() {
            continue;
        }
        builder.parse_stage(stage)?;
    }
    if builder.items.is_empty()
        && builder.output == OutputFormat::Csv
        && builder.header.is_none()
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

/// Known command names, for the "did you mean …?" hint on an unknown verb and
/// the help registry's drift check (see `crate::help`).
pub(crate) const COMMANDS: &[&str] = &[
    "cols", "cut", "select", "where", "filter", "sort", "head", "tail", "stats", "uniq", "color",
    "rename", "fmt", "hdr", "join", "add", "delta", "group", "agg", "graph", "fn",
];

/// Names a `fn` may not take: every command, every alias, `fn` itself, and
/// the removed conversion commands (kept reserved so their hint stays
/// reachable).
const RESERVED: &[&str] = &[
    "cols", "cut", "select", "where", "filter", "sort", "head", "tail", "stats", "uniq", "dedup",
    "color", "colour", "rename", "fmt", "hdr", "join", "add", "delta", "group", "agg", "graph",
    "plot", "fn", "to-num", "to_num", "to-str", "to_str",
];

/// The cast that replaced a removed conversion command, if `cmd` is one.
fn removed_cast(cmd: &str) -> Option<&'static str> {
    match cmd {
        "to-num" | "to_num" => Some("num"),
        "to-str" | "to_str" => Some("str"),
        _ => None,
    }
}

/// The error for a removed conversion command, spelling out the `add` that
/// does the same for each column it named. Inside the expression a name that
/// is not a bare identifier (a position, a name with spaces) is
/// backtick-quoted; as the `add` target only a name with spaces needs that.
fn removed_hint(cmd: &str, cast: &str, args: &str) -> Error {
    let cols = split_list(args);
    let stages: Vec<String> = if cols.is_empty() {
        vec![format!("add COL {cast}(COL)")]
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
                format!("add {target} {cast}({arg})")
            })
            .collect()
    };
    err(format!(
        "{cmd} was removed: convert with add, e.g. `{}`",
        stages.join(" | ")
    ))
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

enum Item {
    Stmt(Stmt),
    Sort(SortStmt),
    Head(usize),
    Tail(usize),
    DropLast(usize),
    Skip(usize),
    Stats(StatsStmt),
    Uniq(UniqStmt),
    Group(GroupStmt),
    Join(JoinStmt),
}

struct Builder<'a> {
    /// Fragment definitions from the script prologue (shared, read-only).
    fns: &'a FnTable,
    /// Current fragment-expansion depth; `MAX_FN_DEPTH` stops recursion.
    depth: usize,
    items: Vec<Item>,
    output: OutputFormat,
    /// Column names from a `hdr` command, for headerless input.
    header: Option<Vec<String>>,
    /// Colour rules from `color` commands (plan metadata, not stages).
    colors: Vec<ColorRule>,
    /// A `graph` sink (plan metadata; the last command, terminates the pipe).
    graph: Option<GraphSpec>,
}

impl<'a> Builder<'a> {
    fn new(fns: &'a FnTable, depth: usize) -> Self {
        Builder {
            fns,
            depth,
            items: Vec::new(),
            output: OutputFormat::Csv,
            header: None,
            colors: Vec::new(),
            graph: None,
        }
    }

    /// Group the flat item list into stages: runs of statements become a
    /// `Transform`; each `sort` and `head` is isolated into its own stage.
    fn take_plan(&mut self) -> Plan {
        let mut stages = Vec::new();
        let mut transform: Vec<Stmt> = Vec::new();
        let flush = |transform: &mut Vec<Stmt>, stages: &mut Vec<Stage>| {
            if !transform.is_empty() {
                stages.push(Stage::Transform(std::mem::take(transform)));
            }
        };
        for item in self.items.drain(..) {
            match item {
                Item::Stmt(s) => transform.push(s),
                Item::Sort(s) => {
                    flush(&mut transform, &mut stages);
                    stages.push(Stage::Sort(s));
                }
                Item::Head(n) => {
                    flush(&mut transform, &mut stages);
                    stages.push(Stage::Head(n));
                }
                Item::Tail(n) => {
                    flush(&mut transform, &mut stages);
                    stages.push(Stage::Tail(n));
                }
                Item::DropLast(n) => {
                    flush(&mut transform, &mut stages);
                    stages.push(Stage::DropLast(n));
                }
                Item::Skip(n) => {
                    flush(&mut transform, &mut stages);
                    stages.push(Stage::Skip(n));
                }
                Item::Stats(s) => {
                    flush(&mut transform, &mut stages);
                    stages.push(Stage::Stats(s));
                }
                Item::Uniq(u) => {
                    flush(&mut transform, &mut stages);
                    stages.push(Stage::Uniq(u));
                }
                Item::Group(g) => {
                    flush(&mut transform, &mut stages);
                    stages.push(Stage::Group(g));
                }
                Item::Join(j) => {
                    flush(&mut transform, &mut stages);
                    stages.push(Stage::Join(j));
                }
            }
        }
        flush(&mut transform, &mut stages);
        Plan {
            stages,
            output: self.output,
            input_header: self.header.take(),
            colors: std::mem::take(&mut self.colors),
            graph: self.graph.take(),
        }
    }

    fn parse_stage(&mut self, stage: &str) -> Result<(), Error> {
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
                    if let Some(cast) = removed_cast(name) {
                        return Err(removed_hint(name, cast, args));
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
        if let Some(cast) = removed_cast(cmd) {
            return Err(removed_hint(cmd, cast, rest));
        }
        let result = match cmd {
            "cols" | "cut" => self.parse_cols(rest),
            "select" | "where" | "filter" => self.parse_select(rest),
            "sort" => self.parse_sort(rest),
            "head" => self.parse_head(rest),
            "tail" => self.parse_tail(rest),
            "stats" => self.parse_stats(rest),
            "group" => self.parse_group(rest),
            "agg" => self.parse_agg(rest),
            "graph" | "plot" => self.parse_graph(rest),
            "uniq" | "dedup" => self.parse_uniq(rest),
            "join" => self.parse_join(rest),
            "color" | "colour" => self.parse_color(rest),
            "rename" => self.parse_rename(rest),
            "add" => self.parse_add(rest),
            "delta" => self.parse_delta(rest),
            "fmt" => self.parse_fmt(rest),
            "hdr" => self.parse_hdr(rest),
            "fn" => Err(err("fn definitions must come before the first stage")),
            other => Err(err(if self.fns.contains_key(other) {
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
            })),
        };
        result.map_err(|e| self.hint_fragment(e))
    }

    /// A fragment name used inside an expression fails as an unknown
    /// function; point at the whole-stage call form.
    fn hint_fragment(&self, e: Error) -> Error {
        let Error::Compile(msg) = &e else { return e };
        let Some(tail) = msg.strip_prefix("unknown function: ") else {
            return e;
        };
        let name = tail.split([' ', '(']).next().unwrap_or("");
        if self.fns.contains_key(name) {
            return err(format!(
                "unknown function: {name} (`{name}` is a fragment — fragments expand only as whole stages)"
            ));
        }
        e
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

    /// `hdr a,b,c` supplies column names for headerless input: the whole input
    /// becomes data and these names are prepended as the header on output. It is
    /// plan-level metadata, so it must come first and may appear only once.
    fn parse_hdr(&mut self, rest: &str) -> Result<(), Error> {
        if self.header.is_some() {
            return Err(err("hdr may be given only once"));
        }
        if !self.items.is_empty() || self.output != OutputFormat::Csv {
            return Err(err("hdr must be the first command in the pipeline"));
        }
        let names = split_list(rest);
        if names.is_empty() {
            return Err(err("hdr expects at least one column name"));
        }
        self.header = Some(names);
        Ok(())
    }

    fn parse_head(&mut self, rest: &str) -> Result<(), Error> {
        // A positive count keeps the first N rows; a negative one (`head -n -N`)
        // keeps all but the last N (coreutils' behaviour).
        match parse_count(rest, "head")? {
            Count::Rows(n) if n >= 0 => self.items.push(Item::Head(n as usize)),
            Count::Rows(n) => self.items.push(Item::DropLast(n.unsigned_abs() as usize)),
            Count::From(_) => return Err(err("head doesn't support +N (that is tail's form)")),
        }
        Ok(())
    }

    /// `tail [N]` keeps the last N rows reaching it (a blocking stage; default
    /// 10). Same count spellings as `head`, but no negative form; `tail +N`
    /// (`-n +N`) prints from row N on, a streaming skip of the first N-1.
    fn parse_tail(&mut self, rest: &str) -> Result<(), Error> {
        match parse_count(rest, "tail")? {
            Count::Rows(n) if n >= 0 => self.items.push(Item::Tail(n as usize)),
            Count::Rows(_) => return Err(err("tail doesn't support a negative count")),
            Count::From(n) => self.items.push(Item::Skip(n.saturating_sub(1))),
        }
        Ok(())
    }

    /// `uniq [cols]` (alias `dedup`) drops duplicate rows, keeping the first —
    /// by the whole row, or by the named key columns. Global (not adjacent), so
    /// no pre-sort is required.
    fn parse_uniq(&mut self, rest: &str) -> Result<(), Error> {
        self.items.push(Item::Uniq(UniqStmt {
            cols: split_list(rest),
            positions: Vec::new(),
        }));
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
            // `--lsuffix S` / `--rsuffix S` (or `=S`) set per-side clash suffixes.
            if let Some(v) = flag_value(word, after, "--lsuffix") {
                let (val, rest_after) = v?;
                lsuffix = Some(val);
                s = rest_after;
                continue;
            }
            if let Some(v) = flag_value(word, after, "--rsuffix") {
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
                parse_join_keys(frag, &mut prev.keys)?;
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
                    Box::new(parse_stages(inner, self.fns, self.depth)?)
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
            f = after.trim_start();

            // Optional `on KEY[,KEY...]`.
            let mut keys = Vec::new();
            if !f.is_empty() {
                let (kw, key_str) = split_first_word(f);
                if kw != "on" {
                    return Err(err("join expects `on KEY[,KEY...]` after the file"));
                }
                parse_join_keys(key_str, &mut keys)?;
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

        self.items.extend(stmts.into_iter().map(Item::Join));
        Ok(())
    }

    /// `stats [cols]` profiles the named columns (or all of them, if none are
    /// named): a blocking stage that reduces the input to one summary row per
    /// column.
    fn parse_stats(&mut self, rest: &str) -> Result<(), Error> {
        self.items.push(Item::Stats(StatsStmt {
            cols: split_list(rest),
            positions: Vec::new(),
        }));
        Ok(())
    }

    /// `group COLS` sets the group keys for a following `agg`. On its own it
    /// reduces to one row per key with a `count` of the rows in each group.
    fn parse_group(&mut self, rest: &str) -> Result<(), Error> {
        let keys = split_list(rest);
        if keys.is_empty() {
            return Err(err(
                "group expects at least one column (use `agg …` alone for a global aggregate)",
            ));
        }
        self.items.push(Item::Group(GroupStmt {
            keys,
            key_positions: Vec::new(),
            aggs: vec![count_rows_agg()],
        }));
        Ok(())
    }

    /// `agg FN(col),… [by COLS]` reduces to one row per key. Keys come from a
    /// trailing `by`, or an immediately preceding `group` (fused, replacing its
    /// default count); with neither it's a single-row global aggregate.
    fn parse_agg(&mut self, rest: &str) -> Result<(), Error> {
        let (specs, by) = split_off_by(rest);
        if specs.is_empty() {
            return Err(err(
                "agg expects at least one aggregate, e.g. agg sum(amount)",
            ));
        }
        let aggs = split_specs(specs)
            .iter()
            .map(|t| parse_agg_spec(t))
            .collect::<Result<Vec<_>, _>>()?;
        let preceding_group = self.items.iter().any(|it| matches!(it, Item::Group(_)));
        let keys = match by {
            Some(k) => {
                if matches!(self.items.last(), Some(Item::Group(_))) {
                    return Err(err(
                        "`agg … by COLS` can't follow a `group` — the group already sets the keys",
                    ));
                }
                split_list(k)
            }
            // Fuse with a directly preceding `group`: adopt its keys and drop its
            // placeholder count.
            None if matches!(self.items.last(), Some(Item::Group(_))) => match self.items.pop() {
                Some(Item::Group(g)) => g.keys,
                _ => unreachable!(),
            },
            // A `group` is present but a stage sits between it and this `agg`, so
            // they can't fuse (the group already reduced the rows). Flag it
            // rather than silently making a global aggregate / a cryptic error.
            None if preceding_group => {
                return Err(err(
                    "`agg` must directly follow its `group` (no stage in between), or use `agg … by COLS`",
                ));
            }
            // No group anywhere ⇒ a single global aggregate row.
            None => Vec::new(),
        };
        self.items.push(Item::Group(GroupStmt {
            keys,
            key_positions: Vec::new(),
            aggs,
        }));
        Ok(())
    }

    /// `graph KIND COLS [--bins N] [--scale F] [--title T] [--svg]` — a
    /// terminal-chart sink. Draws from the columns reaching it instead of
    /// emitting CSV, so it must be the last command. `hist COL`, `spark COL`,
    /// `bar LABEL VALUE`, `scatter X Y`, `line X Y`.
    fn parse_graph(&mut self, rest: &str) -> Result<(), Error> {
        let (kind_word, rest) = split_first_word(rest.trim());
        let kind = match kind_word {
            "hist" | "histogram" => GraphKind::Hist,
            "bar" => GraphKind::Bar,
            "spark" | "sparkline" => GraphKind::Spark,
            "scatter" => GraphKind::Scatter,
            "line" => GraphKind::Line,
            "" => return Err(err("graph expects a chart type, e.g. graph hist amount")),
            other => {
                return Err(err(format!(
                    "graph: unknown chart type `{other}` (try: hist, bar, spark, scatter, line)"
                )));
            }
        };
        let mut opts = GraphOpts::default();
        let mut positional = String::new();
        let mut s = rest.trim();
        while !s.is_empty() {
            let (word, after) = split_first_word(s);
            if let Some(v) = flag_value(word, after, "--bins") {
                let (val, tail) = v?;
                opts.bins = Some(parse_positive(&val, "--bins")?);
                s = tail.trim_start();
            } else if let Some(v) = flag_value(word, after, "--scale") {
                let (val, tail) = v?;
                opts.scale = parse_scale(&val)?;
                s = tail.trim_start();
            } else if let Some(v) = flag_value(word, after, "--title") {
                let (val, tail) = v?;
                opts.title = Some(val);
                s = tail.trim_start();
            } else if word == "--svg" {
                opts.svg = true;
                s = after;
            } else if word.starts_with('-') && word != "-" {
                return Err(err(format!("graph: unknown flag `{word}`")));
            } else {
                if !positional.is_empty() {
                    positional.push(' ');
                }
                positional.push_str(word);
                s = after;
            }
        }
        let cols = split_list(&positional);
        check_graph_arity(kind, kind_word, cols.len())?;
        self.graph = Some(GraphSpec {
            kind,
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
        match first {
            "" => Err(err("color expects arguments")),
            "-g" => self.parse_color_gradient(after),
            "-c" => {
                let (col, tail) = split_first_word(after);
                if col.is_empty() {
                    return Err(err("color -c expects a column name"));
                }
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
        let expr_src = expr_src.trim();
        if expr_src.is_empty() {
            return Err(err("color expects a condition expression"));
        }
        let toks = lex_expr(expr_src)?;
        let mut parser = ExprParser { toks, pos: 0 };
        let expr = parser.parse()?;
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
            cols.push(it.next().unwrap().to_string());
        }
        if cols.is_empty() {
            return Err(err("color -g expects a column name"));
        }
        // The ramp is optional (defaults to green:red); when present it applies
        // to every listed column, as do the bounds.
        let ramp = match it.peek() {
            Some(t) if t.contains(':') => parse_ramp(it.next().unwrap()).map_err(err)?,
            _ => Ramp::default(),
        };
        let bounds = match (it.next(), it.next()) {
            (Some(lo), Some(hi)) => Some((
                lo.parse::<f64>()
                    .map_err(|_| err(format!("color -g: bad lower bound '{lo}'")))?,
                hi.parse::<f64>()
                    .map_err(|_| err(format!("color -g: bad upper bound '{hi}'")))?,
            )),
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
        for spec in split_list(rest) {
            match spec.split_once('=') {
                Some((from, to)) if !from.is_empty() && !to.is_empty() => {
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

    fn parse_fmt(&mut self, rest: &str) -> Result<(), Error> {
        if !rest.trim().is_empty() {
            return Err(err("fmt takes no arguments"));
        }
        self.output = OutputFormat::Aligned;
        Ok(())
    }

    fn parse_cols(&mut self, rest: &str) -> Result<(), Error> {
        let (exclude, list) = match rest.strip_prefix("-v") {
            Some(r) => (true, r.trim_start()),
            None => (false, rest),
        };
        let names = split_list(list);
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

    /// `add NAME EXPR` — append (or replace, if `NAME` exists) a computed
    /// column. The expression is the value-expression grammar (arithmetic,
    /// `++` concat, functions, `?:`, `prev()`/`rownum()`). A backtick-quoted
    /// `NAME` may contain spaces.
    fn parse_add(&mut self, rest: &str) -> Result<(), Error> {
        let (name, expr_src) = split_add_name(rest)?;
        if name.is_empty() {
            return Err(err(
                "add expects a column name, e.g. add total amount * qty",
            ));
        }
        if expr_src.trim().is_empty() {
            return Err(err("add expects an expression after the column name"));
        }
        let toks = lex_expr(expr_src.trim())?;
        let mut parser = ExprParser { toks, pos: 0 };
        let expr = parser.parse_value_top()?;
        let stateful = expr.is_stateful();
        self.items.push(Item::Stmt(Stmt::Add(AddStmt {
            name,
            expr,
            pos: None,
            stateful,
        })));
        Ok(())
    }

    /// `delta [-s SUFFIX] COLS` — shorthand for the per-column step difference.
    /// For each column `C` it appends `C<SUFFIX> = C - prev(C)` (suffix defaults
    /// to `_delta`), i.e. the same stateful `add` written once per column.
    fn parse_delta(&mut self, rest: &str) -> Result<(), Error> {
        let rest = rest.trim();
        let (suffix, list_src) = match rest.strip_prefix("-s") {
            Some(r) if r.is_empty() || r.starts_with('=') || r.starts_with(char::is_whitespace) => {
                let r = r.strip_prefix('=').unwrap_or(r).trim_start();
                let (suf, rest2) = split_first_word(r);
                if suf.is_empty() {
                    return Err(err("delta -s expects a suffix, e.g. delta -s _change a"));
                }
                (suf.to_string(), rest2)
            }
            _ => ("_delta".to_string(), rest),
        };
        let cols = split_list(list_src);
        if cols.is_empty() {
            return Err(err("delta expects at least one column"));
        }
        for c in cols {
            let name = format!("{c}{suffix}");
            let expr = ValExpr::Arith {
                op: ArithOp::Sub,
                lhs: Box::new(ValExpr::Col(ColRef::new(c.clone()))),
                rhs: Box::new(ValExpr::Prev(ColRef::new(c))),
            };
            self.items.push(Item::Stmt(Stmt::Add(AddStmt {
                name,
                expr,
                pos: None,
                stateful: true,
            })));
        }
        Ok(())
    }

    fn parse_sort(&mut self, rest: &str) -> Result<(), Error> {
        let mut keys = Vec::new();
        for spec in split_list(rest) {
            // `col=flags` or `col:flags` — `:` is accepted alongside `=` (it
            // reads better and matches the original csvm's `b:r`).
            let (name, flags) = match spec.split_once(['=', ':']) {
                Some((n, f)) => (n.to_string(), f),
                None => (spec, ""),
            };
            if name.is_empty() {
                return Err(err("sort spec is missing a column name"));
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
        self.items.push(Item::Sort(SortStmt { keys }));
        Ok(())
    }

    fn parse_select(&mut self, rest: &str) -> Result<(), Error> {
        // The expression is bare (not wrapped in quotes); string *literals*
        // inside still use quotes. `||` and `&&` are handled by the lexer, and
        // `||` survives the stage split (see `split_stages`). A leading `-v`
        // (like `cols -v`) negates the *whole* expression — `select -v EXPR`
        // drops the matching rows — which is `!(EXPR)`, sidestepping the De
        // Morgan trap of negating each operator.
        let (negate, expr_src) = match rest.trim().strip_prefix("-v") {
            Some(r) if r.is_empty() || r.starts_with(char::is_whitespace) => (true, r.trim()),
            _ => (false, rest.trim()),
        };
        if expr_src.is_empty() {
            return Err(err("select expects an expression"));
        }
        let toks = lex_expr(expr_src)?;
        let mut parser = ExprParser { toks, pos: 0 };
        let expr = parser.parse()?;
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
    let mut out = String::with_capacity(script.len());
    let mut quote: Option<char> = None;
    let mut chars = script.chars();
    while let Some(c) = chars.next() {
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
                    // Drop through end of line, keeping the newline itself.
                    for d in chars.by_ref() {
                        if d == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
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
        input_header: None,
        colors: Vec::new(),
        graph: None,
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

/// A bare identifier: ASCII letter or `_` first, then letters/digits/`_`.
fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Peel leading `fn NAME(PARAM, ...) { BODY }` definitions off the
/// (comment-stripped) script. Returns the table and the remaining script.
fn parse_prologue(script: &str) -> Result<(FnTable, &str), Error> {
    let mut fns = FnTable::new();
    let mut rest = script.trim_start();
    loop {
        let (word, after) = split_first_word(rest);
        if word != "fn" {
            return Ok((fns, rest));
        }
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
        if RESERVED.contains(&name) {
            return Err(err(format!("fn `{name}` collides with a built-in command")));
        }
        let params_text = params_text
            .trim()
            .strip_suffix(')')
            .ok_or_else(|| err(format!("fn `{name}`: malformed parameter list")))?;
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
        if fns
            .insert(
                name.to_string(),
                FnDef {
                    params,
                    body: body.trim().to_string(),
                },
            )
            .is_some()
        {
            return Err(err(format!("fn `{name}` is defined twice")));
        }
        rest = after_body.trim_start();
    }
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

/// Match a `--name VALUE` or `--name=VALUE` flag. `None` if `word` isn't this
/// flag; otherwise the value and the input remaining after it.
fn flag_value<'a>(
    word: &str,
    after: &'a str,
    name: &str,
) -> Option<Result<(String, &'a str), Error>> {
    if word == name {
        let (val, rest) = take_token(after);
        if val.is_empty() {
            return Some(Err(err(format!("{name} expects a value"))));
        }
        return Some(Ok((val.to_string(), rest)));
    }
    word.strip_prefix(name)
        .and_then(|r| r.strip_prefix('='))
        .map(|val| Ok((val.to_string(), after)))
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

/// Split `add`'s target column name from the rest (its expression). A
/// backtick-quoted name may contain spaces; otherwise it's the first word.
fn split_add_name(rest: &str) -> Result<(String, &str), Error> {
    let rest = rest.trim_start();
    if let Some(after_tick) = rest.strip_prefix('`') {
        match after_tick.find('`') {
            Some(end) => Ok((
                after_tick[..end].to_string(),
                after_tick[end + 1..].trim_start(),
            )),
            None => Err(err("unterminated backtick column name in add")),
        }
    } else {
        let (name, expr) = split_first_word(rest);
        Ok((name.to_string(), expr))
    }
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

/// Parse `KEY[,KEY...]` specs (`name` or `lname=rname`) onto `keys`.
fn parse_join_keys(spec_list: &str, keys: &mut Vec<(String, String)>) -> Result<(), Error> {
    for spec in split_list(spec_list) {
        match spec.split_once('=') {
            None => keys.push((spec.clone(), spec)),
            Some((l, r)) if !l.is_empty() && !r.is_empty() => {
                keys.push((l.to_string(), r.to_string()));
            }
            Some(_) => return Err(err(format!("join `on`: bad key '{spec}'"))),
        }
    }
    Ok(())
}

/// Split an argument string into items on commas and whitespace, respecting
/// quotes; surrounding quotes are stripped from each item. Single quotes, double
/// quotes, and backticks all quote, so a column name with a comma/space (or just
/// for consistency with `select`'s backticks) can be written `` `odd, name` ``.
fn split_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut in_item = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' || c == '`' => {
                quote = Some(c);
                in_item = true;
            }
            None if c == ',' || c.is_whitespace() => {
                if in_item {
                    out.push(std::mem::take(&mut cur));
                    in_item = false;
                }
            }
            None => {
                cur.push(c);
                in_item = true;
            }
        }
    }
    if in_item {
        out.push(cur);
    }
    out
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

/// Validate the column count for a chart type: hist/spark take one, bar takes
/// two, scatter/line take an x plus one or more y-series.
fn check_graph_arity(kind: GraphKind, word: &str, n: usize) -> Result<(), Error> {
    let ok = match kind {
        GraphKind::Hist | GraphKind::Spark => n == 1,
        GraphKind::Bar => n == 2,
        GraphKind::Scatter | GraphKind::Line => n >= 2,
    };
    if ok {
        return Ok(());
    }
    let want = match kind {
        GraphKind::Hist | GraphKind::Spark => "one column",
        GraphKind::Bar => "two columns (label value)",
        GraphKind::Scatter | GraphKind::Line => "an x column and one or more y columns",
    };
    Err(err(format!("graph {word} expects {want}, got {n}")))
}

/// Parse a positive-integer flag value (`--bins`).
fn parse_positive(s: &str, name: &str) -> Result<usize, Error> {
    s.parse::<usize>()
        .ok()
        .filter(|&n| n >= 1)
        .ok_or_else(|| err(format!("{name} expects a positive integer, got `{s}`")))
}

/// Parse the `--scale` factor: a positive, finite number.
fn parse_scale(s: &str) -> Result<f64, Error> {
    s.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or_else(|| err(format!("--scale expects a positive number, got `{s}`")))
}

/// The default `count` aggregate (counts rows) `group` uses when no `agg` follows.
fn count_rows_agg() -> AggSpec {
    AggSpec {
        func: AggFunc::Count,
        col: None,
        pos: None,
        name: "count".to_string(),
    }
}

/// Split an `agg` argument at a top-level (paren-depth-0) `by` keyword into the
/// aggregate specs and the optional grouping-key list. `func(col)` parens are
/// skipped so a column couldn't accidentally swallow the keyword.
fn split_off_by(s: &str) -> (&str, Option<&str>) {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'b' if depth == 0
                && (i == 0 || b[i - 1].is_ascii_whitespace())
                && s[i..].starts_with("by")
                && b.get(i + 2).is_none_or(u8::is_ascii_whitespace) =>
            {
                return (s[..i].trim(), Some(s[i + 2..].trim()));
            }
            _ => {}
        }
        i += 1;
    }
    (s.trim(), None)
}

/// Split an `agg` spec list on top-level commas/whitespace, keeping `func(col)`
/// groups intact.
fn split_specs(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                cur.push(c);
            }
            c if depth == 0 && (c == ',' || c.is_whitespace()) => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Parse one aggregate spec: `func` (only `count` may omit a column) or
/// `func(col)`. The output column is named `col_func` (`amount_sum`), or `count`.
fn parse_agg_spec(tok: &str) -> Result<AggSpec, Error> {
    let (func_name, col) = match tok.find('(') {
        Some(open) => {
            let inner = tok[open + 1..]
                .strip_suffix(')')
                .ok_or_else(|| err(format!("agg: malformed aggregate `{tok}`")))?;
            let col = inner.trim();
            if col.is_empty() {
                return Err(err(format!("agg: `{}` needs a column name", &tok[..open])));
            }
            (tok[..open].trim(), Some(col.to_string()))
        }
        None => (tok.trim(), None),
    };
    let func = match func_name {
        "count" => AggFunc::Count,
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
    let name = match &col {
        Some(c) => format!("{c}_{}", func.name()),
        None => "count".to_string(),
    };
    Ok(AggSpec {
        func,
        col,
        pos: None,
        name,
    })
}

// --- expression lexer -------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum ETok {
    Ident(String),
    Num(f64),
    Str(String),
    Sym(&'static str),
}

fn lex_expr(s: &str) -> Result<Vec<ETok>, Error> {
    let cs: Vec<char> = s.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '(' => push_sym(&mut toks, "(", &mut i),
            ')' => push_sym(&mut toks, ")", &mut i),
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
                Some('=') => push2(&mut toks, "==", &mut i),
                Some('~') => push2(&mut toks, "=~", &mut i),
                _ => push_sym(&mut toks, "==", &mut i), // a lone `=` means equals
            },
            '!' => match cs.get(i + 1) {
                Some('=') => push2(&mut toks, "!=", &mut i),
                Some('~') => push2(&mut toks, "!~", &mut i),
                _ => push_sym(&mut toks, "!", &mut i),
            },
            '<' => match cs.get(i + 1) {
                Some('=') => push2(&mut toks, "<=", &mut i),
                _ => push_sym(&mut toks, "<", &mut i),
            },
            '>' => match cs.get(i + 1) {
                Some('=') => push2(&mut toks, ">=", &mut i),
                _ => push_sym(&mut toks, ">", &mut i),
            },
            '&' if cs.get(i + 1) == Some(&'&') => push2(&mut toks, "&&", &mut i),
            '|' if cs.get(i + 1) == Some(&'|') => push2(&mut toks, "||", &mut i),
            // Affix operators: begins-with / contains / ends-with. A lone
            // `^`/`$` is reserved (no exponent operator), so it errors.
            '^' if cs.get(i + 1) == Some(&'=') => push2(&mut toks, "^=", &mut i),
            '$' if cs.get(i + 1) == Some(&'=') => push2(&mut toks, "$=", &mut i),
            '*' if cs.get(i + 1) == Some(&'=') => push2(&mut toks, "*=", &mut i),
            // `++` is string concat (for `add`); kept distinct from `+`.
            '+' if cs.get(i + 1) == Some(&'+') => push2(&mut toks, "++", &mut i),
            // A leading `+`/`-` is part of a numeric literal only in *unary*
            // position (expression start, or right after an operator/`(`). After
            // a value it is the binary add/subtract operator — so `amount - 5`
            // subtracts, while `a > -5` compares against negative five.
            '-' | '+'
                if !ends_value(&toks) && cs.get(i + 1).is_some_and(|d| d.is_ascii_digit()) =>
            {
                lex_number(&cs, &mut i, &mut toks)?;
            }
            // Arithmetic / value-expression operators (used by `add`).
            '+' => push_sym(&mut toks, "+", &mut i),
            '-' => push_sym(&mut toks, "-", &mut i),
            '*' => push_sym(&mut toks, "*", &mut i),
            '/' => push_sym(&mut toks, "/", &mut i),
            '%' => push_sym(&mut toks, "%", &mut i),
            '?' => push_sym(&mut toks, "?", &mut i),
            ':' => push_sym(&mut toks, ":", &mut i),
            ',' => push_sym(&mut toks, ",", &mut i),
            c if c.is_ascii_digit() => lex_number(&cs, &mut i, &mut toks)?,
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '_' || cs[i] == '.') {
                    i += 1;
                }
                toks.push(ETok::Ident(cs[start..i].iter().collect()));
            }
            other => return Err(err(format!("unexpected character '{other}' in expression"))),
        }
    }
    Ok(toks)
}

/// Whether the last lexed token completes a value (a literal, a column, or a
/// closing paren). A following `+`/`-` is then a binary operator, not a sign.
fn ends_value(toks: &[ETok]) -> bool {
    matches!(
        toks.last(),
        Some(ETok::Num(_) | ETok::Ident(_) | ETok::Str(_) | ETok::Sym(")"))
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

fn lex_number(cs: &[char], i: &mut usize, toks: &mut Vec<ETok>) -> Result<(), Error> {
    let start = *i;
    if cs[*i] == '-' || cs[*i] == '+' {
        *i += 1;
    }
    while *i < cs.len() && (cs[*i].is_ascii_digit() || cs[*i] == '.') {
        *i += 1;
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
    pos: usize,
}

/// A parsed subexpression that is either a boolean or a value. The unified
/// grammar parses each position once and classifies afterward; a
/// `V(ValExpr::Bool(_))` is a parenthesized boolean usable as either.
enum BV {
    B(BoolExpr),
    V(ValExpr),
}

impl ExprParser {
    /// The token at the cursor, quoted, for an error message — or "end of
    /// expression" when the cursor is past the last token.
    fn here(&self) -> String {
        match self.toks.get(self.pos) {
            Some(ETok::Ident(s)) | Some(ETok::Str(s)) => format!("'{s}'"),
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
        let lhs = self.parse_concat()?;
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
        if op == "=~" || op == "!~" {
            let ValExpr::Col(col) = lhs else {
                return Err(err("left side of =~ must be a column"));
            };
            let pattern = match self.parse_concat()? {
                ValExpr::Str(s) => s,
                _ => return Err(err("=~ pattern must be a string")),
            };
            let regex = regex::Regex::new(&pattern)
                .map_err(|e| err(format!("invalid regex '{pattern}': {e}")))?;
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
                return Err(err(format!("left side of {op} must be a column")));
            };
            let needle = match self.parse_concat()? {
                ValExpr::Str(s) => s,
                _ => return Err(err(format!("{op} needs a string literal on the right"))),
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
    /// atoms. A boolean subexpression used as a value (`add ok amount > 0`,
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
        if name == "rownum" {
            if !self.eat(")") {
                return Err(err("rownum() takes no arguments"));
            }
            return Ok(ValExpr::Rownum);
        }
        let args = self.parse_args()?;
        if name == "prev" {
            let [ValExpr::Col(c)] = &args[..] else {
                return Err(err("prev() takes a single column, e.g. prev(amount)"));
            };
            return Ok(ValExpr::Prev(c.clone()));
        }
        let func = Func::from_name(name).ok_or_else(|| {
            err(match crate::error::did_you_mean(name, Func::NAMES) {
                Some(s) => format!("unknown function: {name} (did you mean `{s}`?)"),
                None => format!("unknown function: {name}"),
            })
        })?;
        check_arity(func, args.len())?;
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
        let plan = parse("add qty str(qty) | select qty > stock").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Select(BoolExpr::Cmp(c)) = &stmts[1] else {
            panic!()
        };
        assert_eq!(c.mode, CmpMode::Auto);
        let plan = parse("add c num(c) | select c == '5'").unwrap();
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
                err.contains(&format!("add a {cast}(a) | add b {cast}(b)")),
                "{err}"
            );
        }
        let err = parse("to-str 2").unwrap_err().to_string();
        assert!(err.contains("add 2 str(`2`)"), "{err}");
        let err = parse("to-num 'my col'").unwrap_err().to_string();
        assert!(err.contains("add `my col` num(`my col`)"), "{err}");
        let err = parse("to_num(qty)").unwrap_err().to_string();
        assert!(err.contains("add qty num(qty)"), "{err}");
        let err = parse("to-num").unwrap_err().to_string();
        assert!(err.contains("add COL num(COL)"), "{err}");
    }

    #[test]
    fn sort_flags_and_stage_split() {
        let plan = parse("add c num(c) | select c > 0 | sort c=r a id=nr").unwrap();
        assert_eq!(plan.stages.len(), 2);
        let Stage::Sort(s) = &plan.stages[1] else {
            panic!()
        };
        assert!(s.keys[0].descending && s.keys[0].mode == SortMode::Auto); // c: =r (typed at resolve)
        assert!(!s.keys[1].descending && s.keys[1].mode == SortMode::Auto); // a: default
        assert!(s.keys[2].descending && s.keys[2].mode == SortMode::Numeric); // id: =nr

        // `:` is an accepted alternative to `=` for the flags.
        let plan = parse("sort a:nr b").unwrap();
        let Stage::Sort(s) = &plan.stages[0] else {
            panic!()
        };
        assert!(s.keys[0].descending && s.keys[0].mode == SortMode::Numeric); // a: :nr
        assert!(!s.keys[1].descending && s.keys[1].mode == SortMode::Auto); // b: default
    }

    #[test]
    fn sort_mode_flags() {
        // `=s` pins lexical and `=n` numeric; a bare key is auto until
        // `Plan::resolve` sees the column's type.
        let plan = parse("add z str(z) | sort a=s b z z=n").unwrap();
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
    fn group_and_agg_parse_into_one_group_stage() {
        // `group` + a following `agg` fuse into a single Group stage; the agg's
        // functions replace group's placeholder count.
        let plan = parse("group region | agg sum(amount),mean(amount)").unwrap();
        assert_eq!(plan.stages.len(), 1);
        let Stage::Group(g) = &plan.stages[0] else {
            panic!("expected a group stage");
        };
        assert_eq!(g.keys, ["region"]);
        let names: Vec<&str> = g.aggs.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["amount_sum", "amount_mean"]);
        // `by` supplies keys directly; a bare `group` keeps its default count.
        assert!(matches!(
            parse("agg count by a,b").unwrap().stages[0],
            Stage::Group(_)
        ));
        let solo = parse("group region").unwrap();
        let Stage::Group(g) = &solo.stages[0] else {
            panic!()
        };
        assert_eq!(g.aggs.len(), 1);
        assert_eq!(g.aggs[0].func, AggFunc::Count);
    }

    #[test]
    fn agg_rejects_bad_specs() {
        assert!(parse("agg frobnicate(x)").is_err()); // unknown function
        assert!(parse("agg sum").is_err()); // sum needs a column
        assert!(parse("agg sum()").is_err()); // empty column
        assert!(parse("agg").is_err()); // no aggregates
        assert!(parse("group").is_err()); // no keys
    }

    #[test]
    fn agg_must_directly_follow_group() {
        // Adjacent fuses; a global agg with no group is fine.
        assert!(parse("group r | agg sum(a)").is_ok());
        assert!(parse("agg sum(a)").is_ok());
        assert!(parse("agg sum(a) by r").is_ok());
        // A stage between group and agg can't fuse — clear error, not a silent
        // global aggregate or a cryptic column error.
        assert!(parse("group r | head 5 | agg count").is_err());
        assert!(parse("group r | select a > 0 | agg sum(a)").is_err());
        // `by` contradicts a preceding group's keys.
        assert!(parse("group r | agg sum(a) by b").is_err());
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
        assert!(parse("plot hist x").unwrap().graph.is_some()); // `plot` alias
    }

    #[test]
    fn graph_must_be_last_and_well_formed() {
        assert!(parse("graph hist x | sort x").is_err()); // nothing may follow a sink
        assert!(parse("graph").is_err()); // missing chart type
        assert!(parse("graph pie x").is_err()); // unknown chart type
        assert!(parse("graph hist a b").is_err()); // hist takes exactly one column
        assert!(parse("graph hist x --bins 0").is_err()); // bins must be positive
        assert!(parse("graph hist x --frob 1").is_err()); // unknown flag
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
    fn uniq_parses_whole_row_and_keys() {
        let plan = parse("uniq").unwrap();
        let Stage::Uniq(u) = &plan.stages[0] else {
            panic!()
        };
        assert!(u.cols.is_empty()); // whole-row

        let plan = parse("dedup a,b").unwrap(); // alias
        let Stage::Uniq(u) = &plan.stages[0] else {
            panic!()
        };
        assert_eq!(u.cols, ["a", "b"]);
    }

    #[test]
    fn command_aliases() {
        // where/filter alias select; cut aliases cols.
        for s in ["select a > 0", "where a > 0", "filter a > 0"] {
            let plan = parse(s).unwrap();
            let Stage::Transform(stmts) = &plan.stages[0] else {
                panic!()
            };
            assert!(matches!(stmts[0], Stmt::Select(_)), "{s}");
        }
        let plan = parse("cut a,b").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Cols(p) = &stmts[0] else { panic!() };
        assert_eq!(p.names, ["a", "b"]);
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
        assert_eq!(plan.output, OutputFormat::Aligned);
        // fmt is not a stage.
        assert!(plan.stages.iter().all(|s| !matches!(s, Stage::Head(_))));

        // fmt alone (no transforms) is valid — align the input.
        assert_eq!(parse("fmt").unwrap().output, OutputFormat::Aligned);
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
        assert!(parse("fmt x").is_err()); // fmt takes no args
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
        assert!(msg("add x a + b c").contains("unexpected 'c'"));
        // A missing operator lists the valid ones and says what it found.
        let m = msg("select a");
        assert!(
            m.contains("comparison operator") && m.contains("end of expression"),
            "{m}"
        );
        // Missing operand / close paren report what was found.
        assert!(msg("select a >").contains("found end of expression"));
        assert!(msg("select (a > 0").contains("expected ')'"));
        assert!(msg("add x )").contains("found ')'"));
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
        } = add_expr("add v a + b * c")
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
            add_expr("add d amount - prev(amount)"),
            ValExpr::Arith {
                op: ArithOp::Sub,
                ..
            }
        ));
        // But in unary position a signed number is still a literal.
        assert!(matches!(add_expr("add d -5"), ValExpr::Num(n) if n == -5.0));
    }

    #[test]
    fn add_prev_and_rownum_are_stateful() {
        for script in ["add d a - prev(a)", "add n rownum()"] {
            let plan = parse(script).unwrap();
            let Stage::Transform(stmts) = &plan.stages[0] else {
                panic!();
            };
            assert!(stmts[0].is_stateful(), "{script} should be stateful");
        }
        // Pure arithmetic is not stateful (it can shard).
        let plan = parse("add t a * 2").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!();
        };
        assert!(!stmts[0].is_stateful());
    }

    #[test]
    fn add_ternary_and_concat() {
        assert!(matches!(
            add_expr("add tier a > 1 ? 'big' : 'small'"),
            ValExpr::Cond { .. }
        ));
        assert!(matches!(
            add_expr("add full a ++ ' ' ++ b"),
            ValExpr::Concat(parts) if parts.len() == 3
        ));
    }

    #[test]
    fn add_rejects_bad_input() {
        assert!(parse("add").is_err()); // no name
        assert!(parse("add x").is_err()); // no expression
        assert!(parse("add x a +").is_err()); // dangling operator
        assert!(parse("add x bogus(a)").is_err()); // unknown function
        assert!(parse("add x prev(a + 1)").is_err()); // prev needs a bare column
        assert!(parse("add x round(a, b)").is_err()); // wrong arity
    }

    #[test]
    fn delta_expands_to_one_stateful_add_per_column() {
        let plan = parse("delta a b").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!();
        };
        assert_eq!(stmts.len(), 2);
        let names: Vec<&str> = stmts
            .iter()
            .map(|s| match s {
                Stmt::Add(a) => a.name.as_str(),
                _ => panic!("expected add"),
            })
            .collect();
        assert_eq!(names, ["a_delta", "b_delta"]);
        // C_delta = C - prev(C), and stateful.
        let Stmt::Add(a) = &stmts[0] else { panic!() };
        assert!(a.stateful);
        assert!(matches!(
            &a.expr,
            ValExpr::Arith {
                op: ArithOp::Sub,
                ..
            }
        ));
    }

    #[test]
    fn delta_custom_suffix_and_errors() {
        let plan = parse("delta -s _change a").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!();
        };
        let Stmt::Add(a) = &stmts[0] else { panic!() };
        assert_eq!(a.name, "a_change");
        assert!(parse("delta").is_err()); // no columns
        assert!(parse("delta -s").is_err()); // suffix flag, no suffix
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
        let script = "# header comment\nselect a > 0\n\nadd b a * 2   # trailing comment\nfmt";
        let plan = parse(script).unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!();
        };
        assert!(matches!(stmts[0], Stmt::Select(_)));
        assert!(matches!(&stmts[1], Stmt::Add(a) if a.name == "b"));
        assert_eq!(plan.output, OutputFormat::Aligned);

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
        assert_eq!(subst_params("add x 9a", &params, &args), "add x 9a");
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
    fn reserved_covers_every_command() {
        // A new command added to COMMANDS must also be fn-reserved, or a
        // user fragment could shadow it.
        for c in COMMANDS {
            assert!(RESERVED.contains(c), "`{c}` missing from RESERVED");
        }
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
        let e = parse("fn pf(a) { head }\nadd x pf(a)")
            .unwrap_err()
            .to_string();
        assert!(e.contains("whole stages"), "{e}");
        // No fragment of that name: the plain unknown-function error stands.
        let e = parse("add x bogus(a)").unwrap_err().to_string();
        assert!(!e.contains("whole stages"), "{e}");
    }
}
