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
//! Column *types* (`Str`/`Num`) are tracked left-to-right so comparisons go
//! numeric implicitly: against a number literal, or against a column an earlier
//! `to-num` marked numeric.

use std::collections::HashMap;

use crate::color::{parse_ramp, parse_style};
use crate::error::Error;
use crate::plan::{
    BoolExpr, Cmp, CmpOp, ColRef, ColorRule, ColorScope, ConvStmt, Operand, OutputFormat, Plan,
    ProjectStmt, RenameStmt, SortKey, SortStmt, Stage, StatsStmt, Stmt,
};

/// Compile a pipe script into an executable [`Plan`].
pub fn parse(script: &str) -> Result<Plan, Error> {
    let mut builder = Builder::default();
    for stage in split_stages(script) {
        let stage = stage.trim();
        if stage.is_empty() {
            return Err(err("empty pipeline stage (stray '|'?)"));
        }
        builder.parse_stage(stage)?;
    }
    if builder.items.is_empty()
        && builder.output == OutputFormat::Csv
        && builder.header.is_none()
        && builder.colors.is_empty()
    {
        return Err(err("empty script"));
    }
    Ok(builder.take_plan())
}

fn err(msg: impl Into<String>) -> Error {
    Error::Compile(msg.into())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ColType {
    Str,
    Num,
}

enum Item {
    Stmt(Stmt),
    Sort(SortStmt),
    Head(usize),
    Stats(StatsStmt),
}

#[derive(Default)]
struct Builder {
    items: Vec<Item>,
    col_types: HashMap<String, ColType>,
    output: OutputFormat,
    /// Column names from a `hdr` command, for headerless input.
    header: Option<Vec<String>>,
    /// Colour rules from `color` commands (plan metadata, not stages).
    colors: Vec<ColorRule>,
}

impl Builder {
    fn is_num(&self, name: &str) -> bool {
        self.col_types.get(name) == Some(&ColType::Num)
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
                Item::Stats(s) => {
                    flush(&mut transform, &mut stages);
                    stages.push(Stage::Stats(s));
                }
            }
        }
        flush(&mut transform, &mut stages);
        Plan {
            stages,
            output: self.output,
            input_header: self.header.take(),
            colors: std::mem::take(&mut self.colors),
        }
    }

    fn parse_stage(&mut self, stage: &str) -> Result<(), Error> {
        let (cmd, rest) = split_first_word(stage);
        match cmd {
            "cols" => self.parse_cols(rest),
            "select" => self.parse_select(rest),
            "sort" => self.parse_sort(rest),
            "to-num" | "to_num" => self.parse_conv(rest, true),
            "to-str" | "to_str" => self.parse_conv(rest, false),
            "head" => self.parse_head(rest),
            "stats" => self.parse_stats(rest),
            "color" | "colour" => self.parse_color(rest),
            "rename" => self.parse_rename(rest),
            "fmt" => self.parse_fmt(rest),
            "hdr" => self.parse_hdr(rest),
            other => Err(err(format!("unknown command: {other}"))),
        }
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
        // Bash-like: no argument means 10 rows; the count may be bare (`head 20`),
        // via `-n`/`--lines` (`-n 20`, `-n20`, `--lines=20`), or the obsolete
        // `-N` form (`head -20`). Byte mode (`-c`) and negative counts (all but
        // last N) are not supported.
        let rest = rest.trim();
        if rest.is_empty() {
            self.items.push(Item::Head(DEFAULT_HEAD_ROWS));
            return Ok(());
        }
        let count = head_count_text(rest);
        if let Some(digits) = count.strip_prefix('-')
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(err(
                "head doesn't support a negative count (all-but-last-N)",
            ));
        }
        let n: usize = count
            .parse()
            .map_err(|_| err(format!("head expects a row count, got '{rest}'")))?;
        self.items.push(Item::Head(n));
        Ok(())
    }

    /// `stats [cols]` profiles the named columns (or all of them, if none are
    /// named): a blocking stage that reduces the input to one summary row per
    /// column.
    fn parse_stats(&mut self, rest: &str) -> Result<(), Error> {
        self.items.push(Item::Stats(StatsStmt {
            cols: split_list(rest),
            positions: Vec::new(),
            names: Vec::new(),
        }));
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
        let mut parser = ExprParser {
            toks,
            pos: 0,
            types: &self.col_types,
        };
        let expr = parser.parse()?;
        self.colors
            .push(ColorRule::Predicate { scope, style, expr });
        Ok(())
    }

    fn parse_color_gradient(&mut self, s: &str) -> Result<(), Error> {
        let mut it = s.split_whitespace();
        let col = it
            .next()
            .ok_or_else(|| err("color -g expects a column name"))?;
        let ramp = parse_ramp(
            it.next()
                .ok_or_else(|| err("color -g expects a ramp like green:red"))?,
        )
        .map_err(err)?;
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
        self.colors.push(ColorRule::Gradient {
            col: ColRef::new(col.to_string()),
            ramp,
            bounds,
        });
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

    fn parse_conv(&mut self, rest: &str, to_num: bool) -> Result<(), Error> {
        let names = split_list(rest);
        if names.is_empty() {
            return Err(err("to-num/to-str expects at least one column"));
        }
        for n in &names {
            self.col_types
                .insert(n.clone(), if to_num { ColType::Num } else { ColType::Str });
        }
        let conv = ConvStmt {
            names,
            positions: Vec::new(),
        };
        self.items.push(Item::Stmt(if to_num {
            Stmt::ToNum(conv)
        } else {
            Stmt::ToStr(conv)
        }));
        Ok(())
    }

    fn parse_sort(&mut self, rest: &str) -> Result<(), Error> {
        let mut keys = Vec::new();
        for spec in split_list(rest) {
            let (name, flags) = match spec.split_once('=') {
                Some((n, f)) => (n.to_string(), f),
                None => (spec, ""),
            };
            if name.is_empty() {
                return Err(err("sort spec is missing a column name"));
            }
            let mut key = SortKey {
                numeric: self.is_num(&name),
                name,
                pos: 0,
                descending: false,
            };
            for ch in flags.chars() {
                match ch {
                    'n' => key.numeric = true,
                    'r' => key.descending = true,
                    other => {
                        return Err(err(format!("unknown sort flag '{other}' (use n and/or r)")));
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
        // `||` survives the stage split (see `split_stages`).
        let expr_src = rest.trim();
        if expr_src.is_empty() {
            return Err(err("select expects an expression"));
        }
        let toks = lex_expr(expr_src)?;
        let mut parser = ExprParser {
            toks,
            pos: 0,
            types: &self.col_types,
        };
        let expr = parser.parse()?;
        self.items.push(Item::Stmt(Stmt::Select(expr)));
        Ok(())
    }
}

/// Rows kept by `head` when no count is given (bash `head` defaults to 10).
const DEFAULT_HEAD_ROWS: usize = 10;

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

/// Split a script into stages on a lone, unquoted `|`. A `||` (the *or*
/// operator) and a `|` inside a string literal are left intact, so `select`
/// expressions need no quoting of their own.
fn split_stages(script: &str) -> Vec<&str> {
    let mut stages = Vec::new();
    let bytes = script.as_bytes();
    let mut start = 0;
    let mut i = 0;
    let mut quote: Option<u8> = None;
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
            None if c == b'|' => {
                if bytes.get(i + 1) == Some(&b'|') {
                    i += 2; // `||` is the or-operator, not a stage separator
                } else {
                    stages.push(&script[start..i]);
                    start = i + 1;
                    i += 1;
                }
            }
            None => i += 1,
        }
    }
    stages.push(&script[start..]);
    stages
}

/// Split off the first whitespace-delimited word (the command) from the rest.
fn split_first_word(stage: &str) -> (&str, &str) {
    match stage.find(char::is_whitespace) {
        Some(p) => (&stage[..p], stage[p..].trim_start()),
        None => (stage, ""),
    }
}

/// Split an argument string into items on commas and whitespace, respecting
/// quotes; surrounding quotes are stripped from each item.
fn split_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut in_item = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
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
                    return Err(err("unterminated string in select expression"));
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
            '-' | '+' if cs.get(i + 1).is_some_and(|d| d.is_ascii_digit()) => {
                lex_number(&cs, &mut i, &mut toks)?;
            }
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

struct ExprParser<'a> {
    toks: Vec<ETok>,
    pos: usize,
    types: &'a HashMap<String, ColType>,
}

impl ExprParser<'_> {
    fn parse(&mut self) -> Result<BoolExpr, Error> {
        let expr = self.parse_or()?;
        if self.pos != self.toks.len() {
            return Err(err("trailing tokens in select expression"));
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

    fn parse_or(&mut self) -> Result<BoolExpr, Error> {
        let mut parts = vec![self.parse_and()?];
        while self.eat("||") {
            parts.push(self.parse_and()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            BoolExpr::Or(parts)
        })
    }

    fn parse_and(&mut self) -> Result<BoolExpr, Error> {
        let mut parts = vec![self.parse_not()?];
        while self.eat("&&") {
            parts.push(self.parse_not()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            BoolExpr::And(parts)
        })
    }

    fn parse_not(&mut self) -> Result<BoolExpr, Error> {
        if self.eat("!") {
            Ok(BoolExpr::Not(Box::new(self.parse_not()?)))
        } else {
            self.parse_primary()
        }
    }

    fn parse_primary(&mut self) -> Result<BoolExpr, Error> {
        if self.eat("(") {
            let expr = self.parse_or()?;
            if !self.eat(")") {
                return Err(err("expected ')' in select expression"));
            }
            Ok(expr)
        } else {
            self.parse_comparison()
        }
    }

    fn parse_comparison(&mut self) -> Result<BoolExpr, Error> {
        let lhs = self.parse_operand()?;
        let op = match self.toks.get(self.pos) {
            Some(ETok::Sym(s)) => *s,
            _ => return Err(err("expected a comparison operator in select expression")),
        };
        self.pos += 1;
        if op == "=~" || op == "!~" {
            let Operand::Col(col) = lhs else {
                return Err(err("left side of =~ must be a column"));
            };
            let pattern = match self.parse_operand()? {
                Operand::Str(s) => s,
                _ => return Err(err("=~ pattern must be a string")),
            };
            let regex = regex::Regex::new(&pattern)
                .map_err(|e| err(format!("invalid regex '{pattern}': {e}")))?;
            return Ok(BoolExpr::Match {
                col,
                regex,
                negate: op == "!~",
            });
        }
        let cmp_op = match op {
            "==" => CmpOp::Eq,
            "!=" => CmpOp::Ne,
            "<" => CmpOp::Lt,
            ">" => CmpOp::Gt,
            "<=" => CmpOp::Le,
            ">=" => CmpOp::Ge,
            other => return Err(err(format!("'{other}' is not a comparison operator"))),
        };
        let mut lhs = lhs;
        let mut rhs = self.parse_operand()?;
        let numeric = self.is_numeric(&lhs) || self.is_numeric(&rhs);
        if numeric {
            lhs = numericize(lhs)?;
            rhs = numericize(rhs)?;
        }
        Ok(BoolExpr::Cmp(Cmp {
            op: cmp_op,
            lhs,
            rhs,
            numeric,
        }))
    }

    fn parse_operand(&mut self) -> Result<Operand, Error> {
        match self.toks.get(self.pos).cloned() {
            Some(ETok::Ident(name)) => {
                self.pos += 1;
                Ok(Operand::Col(ColRef::new(name)))
            }
            Some(ETok::Num(n)) => {
                self.pos += 1;
                Ok(Operand::Num(n))
            }
            Some(ETok::Str(s)) => {
                self.pos += 1;
                Ok(Operand::Str(s))
            }
            _ => Err(err(
                "expected a column, number, or string in select expression",
            )),
        }
    }

    fn is_numeric(&self, op: &Operand) -> bool {
        match op {
            Operand::Num(_) => true,
            Operand::Col(c) => self.types.get(&c.name) == Some(&ColType::Num),
            Operand::Str(_) => false,
        }
    }
}

/// In a numeric comparison, a string literal is parsed at parse time.
fn numericize(op: Operand) -> Result<Operand, Error> {
    match op {
        Operand::Str(s) => match s.trim().parse::<f64>() {
            Ok(n) => Ok(Operand::Num(n)),
            Err(_) => Err(err(format!(
                "non-numeric literal '{s}' in numeric comparison"
            ))),
        },
        other => Ok(other),
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
        assert!(!eq.numeric);
        let BoolExpr::Cmp(gt) = &parts[1] else {
            panic!()
        };
        assert!(gt.numeric);
    }

    #[test]
    fn to_num_makes_compare_numeric() {
        let plan = parse("to-num c | select c == '5'").unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Select(BoolExpr::Cmp(c)) = &stmts[1] else {
            panic!()
        };
        assert!(c.numeric);
        assert!(matches!(c.rhs, Operand::Num(n) if n == 5.0));
    }

    #[test]
    fn sort_flags_and_stage_split() {
        let plan = parse("to-num c | select c > 0 | sort c=r a id=nr").unwrap();
        assert_eq!(plan.stages.len(), 2);
        let Stage::Sort(s) = &plan.stages[1] else {
            panic!()
        };
        assert!(s.keys[0].descending && s.keys[0].numeric); // c: =r, and numeric from to-num
        assert!(!s.keys[1].descending && !s.keys[1].numeric); // a: default
        assert!(s.keys[2].descending && s.keys[2].numeric); // id: =nr
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
    fn backtick_quoted_column_name() {
        // A hyphenated name isn't a bare identifier; backticks make it a column
        // ref (an Ident), not a string literal.
        let plan = parse(r#"select `frequenz-app-edge` != """#).unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Select(BoolExpr::Cmp(c)) = &stmts[0] else {
            panic!()
        };
        let Operand::Col(col) = &c.lhs else { panic!() };
        assert_eq!(col.name, "frequenz-app-edge");
        assert!(!c.numeric);
        assert!(matches!(&c.rhs, Operand::Str(s) if s.is_empty()));
    }

    #[test]
    fn backtick_errors() {
        assert!(parse("select `unterminated == 'x'").is_err());
        assert!(parse("select `` == 'x'").is_err()); // empty name
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
        // negative (all-but-last) is out of scope; a non-integer still errors.
        assert!(parse("head -n -3").is_err());
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
    }

    #[test]
    fn color_errors() {
        assert!(parse("color").is_err()); // no args
        assert!(parse("color red").is_err()); // colour but no expression
        assert!(parse("color notacolour x > 0").is_err()); // unknown colour
        assert!(parse("color -g amount").is_err()); // gradient needs a ramp
        assert!(parse("color -g amount green:red 0").is_err()); // only one bound
        assert!(parse("color -g amount notaramp").is_err()); // bad ramp
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
    }
}
