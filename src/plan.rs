//! The compiled execution plan — the hot path.
//!
//! A [`Plan`] is what the `tulisp` script compiles down to (see
//! [`crate::parse`]). Evaluating it touches no interpreter: column references
//! are plain indices, comparisons are monomorphic, and `and`/`or` short-circuit.
//!
//! Column references start life carrying only a name; [`Plan::resolve`] turns
//! names into indices against the header, threading the header through the
//! pipeline so a `cols` that reshapes the row is visible to later statements —
//! exactly as csvm's `set_header` does.

use crate::color::{Ramp, Style};
use crate::error::Error;
use crate::field::{Field, format_num};
use crate::stats::STATS_SCHEMA;
use regex::Regex;
use std::cmp::Ordering;

/// A reference to a column: a name, resolved to a position.
#[derive(Clone, Debug)]
pub struct ColRef {
    pub name: String,
    pub pos: usize,
}

impl ColRef {
    pub fn new(name: String) -> Self {
        ColRef { name, pos: 0 }
    }
    fn resolve(&mut self, header: &[String]) -> Result<(), Error> {
        self.pos = resolve_col(&self.name, header)?;
        Ok(())
    }
}

/// An operand of a comparison. String/number literals are normalized at compile
/// time, so a numeric comparison never sees a `Str`.
#[derive(Clone, Debug)]
pub enum Operand {
    Col(ColRef),
    Str(String),
    Num(f64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

/// A single comparison. `numeric` is decided at compile time from the operand
/// types and the column's tracked type.
#[derive(Clone, Debug)]
pub struct Cmp {
    pub op: CmpOp,
    pub lhs: Operand,
    pub rhs: Operand,
    pub numeric: bool,
}

/// Which end of the cell a substring test anchors to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AffixKind {
    /// `^=` — the cell begins with the needle.
    StartsWith,
    /// `*=` — the cell contains the needle.
    Contains,
    /// `$=` — the cell ends with the needle.
    EndsWith,
}

impl AffixKind {
    /// The operator spelling, for `--print-engine`.
    pub fn symbol(self) -> &'static str {
        match self {
            AffixKind::StartsWith => "^=",
            AffixKind::Contains => "*=",
            AffixKind::EndsWith => "$=",
        }
    }
}

/// A boolean expression tree for `select`. `And`/`Or` are n-ary and
/// short-circuit.
#[derive(Clone, Debug)]
pub enum BoolExpr {
    And(Vec<BoolExpr>),
    Or(Vec<BoolExpr>),
    Not(Box<BoolExpr>),
    Cmp(Cmp),
    Match {
        col: ColRef,
        regex: Regex,
        negate: bool,
    },
    /// A literal substring test (`^=` / `*=` / `$=`): faster and escape-free
    /// versus a regex. Negate with `!` around it.
    Affix {
        col: ColRef,
        needle: String,
        kind: AffixKind,
    },
}

/// A binary arithmetic operator for a value expression (`add`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

impl ArithOp {
    /// The operator spelling, for `--print-engine`.
    pub fn symbol(self) -> &'static str {
        match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
            ArithOp::Mod => "%",
        }
    }
}

/// A built-in function callable from a value expression. The starter set; string
/// extraction (`split`/regex) and date helpers are deferred (see `todo.org`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Func {
    Round,
    Floor,
    Ceil,
    Abs,
    Int,
    Min,
    Max,
    Len,
    Upper,
    Lower,
    Trim,
    Coalesce,
}

impl Func {
    /// The function name as written in a script.
    pub fn name(self) -> &'static str {
        match self {
            Func::Round => "round",
            Func::Floor => "floor",
            Func::Ceil => "ceil",
            Func::Abs => "abs",
            Func::Int => "int",
            Func::Min => "min",
            Func::Max => "max",
            Func::Len => "len",
            Func::Upper => "upper",
            Func::Lower => "lower",
            Func::Trim => "trim",
            Func::Coalesce => "coalesce",
        }
    }

    /// Resolve a name to a function, for the parser.
    pub fn from_name(name: &str) -> Option<Func> {
        Some(match name {
            "round" => Func::Round,
            "floor" => Func::Floor,
            "ceil" => Func::Ceil,
            "abs" => Func::Abs,
            "int" => Func::Int,
            "min" => Func::Min,
            "max" => Func::Max,
            "len" => Func::Len,
            "upper" => Func::Upper,
            "lower" => Func::Lower,
            "trim" => Func::Trim,
            "coalesce" => Func::Coalesce,
            _ => return None,
        })
    }

    /// Every function name, for the "did you mean …?" hint on a typo.
    pub const NAMES: &'static [&'static str] = &[
        "round", "floor", "ceil", "abs", "int", "min", "max", "len", "upper", "lower", "trim",
        "coalesce",
    ];
}

/// A value-producing expression for `add` (yields a number or a string). Like
/// [`BoolExpr`], it is parsed once and evaluated per row with no interpreter —
/// column refs are indices, operators monomorphic. Arithmetic coerces operands
/// to numbers (csvm's implicit `to-num`); `++` and the string functions coerce
/// to text.
#[derive(Clone, Debug)]
pub enum ValExpr {
    /// A column's cell value.
    Col(ColRef),
    Num(f64),
    Str(String),
    /// Unary minus.
    Neg(Box<ValExpr>),
    /// Binary arithmetic (numeric; div/mod by zero aborts the run).
    Arith {
        op: ArithOp,
        lhs: Box<ValExpr>,
        rhs: Box<ValExpr>,
    },
    /// `a ++ b ++ …` — string concatenation (deliberately not `+`, so `+` is
    /// always numeric and unambiguous).
    Concat(Vec<ValExpr>),
    /// A built-in function call.
    Func(Func, Vec<ValExpr>),
    /// A boolean expression used as a value (`add ok amount > 0`); renders
    /// csvm-style `t`/`f`.
    Bool(Box<BoolExpr>),
    /// `test ? then : else` — reuses the `select` boolean expression for `test`.
    Cond {
        test: Box<BoolExpr>,
        then_: Box<ValExpr>,
        else_: Box<ValExpr>,
    },
    /// `prev(col)` — the cell of `col` in the *previous* row (the current row's
    /// own cell on the first row, so a delta is 0 there). Stateful: forces the
    /// in-memory ordered execution path.
    Prev(ColRef),
    /// `rownum()` — the 1-based index of the current row. Stateful (as above).
    Rownum,
}

/// `cols(...)` (`keep`) or `drop-cols(...)` (`exclude`): both project the row to
/// a resolved list of positions; they differ only in how the positions are
/// computed from the header.
#[derive(Clone, Debug)]
pub struct ProjectStmt {
    pub exclude: bool,
    pub names: Vec<String>,
    pub positions: Vec<usize>,
}

/// `to-num(...)` or `to-str(...)`: convert the listed columns in place.
#[derive(Clone, Debug)]
pub struct ConvStmt {
    pub names: Vec<String>,
    pub positions: Vec<usize>,
}

/// One key of a `sort`.
#[derive(Clone, Debug)]
pub struct SortKey {
    pub name: String,
    pub pos: usize,
    pub descending: bool,
    pub numeric: bool,
}

#[derive(Clone, Debug)]
pub struct SortStmt {
    pub keys: Vec<SortKey>,
}

/// `stats [cols]`: reduce the input to one summary row per profiled column
/// (empty `cols` profiles every column). After resolution `names` holds the
/// resolved column names (the `field` cell of each output row) and `positions`
/// their indices; the output header becomes [`STATS_SCHEMA`].
#[derive(Clone, Debug)]
pub struct StatsStmt {
    pub cols: Vec<String>,
    pub positions: Vec<usize>,
    pub names: Vec<String>,
}

/// An aggregate function applied per group. Each maps onto a [`ColStats`] field
/// (`crate::stats`), so the group accumulator reuses the `stats` profiler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFunc {
    /// `count` (no column) counts rows; `count(col)` counts non-empty cells.
    Count,
    Sum,
    Min,
    Max,
    Mean,
    Stddev,
}

impl AggFunc {
    /// The verb as written, for the default output-column name and `--print-engine`.
    pub fn name(self) -> &'static str {
        match self {
            AggFunc::Count => "count",
            AggFunc::Sum => "sum",
            AggFunc::Min => "min",
            AggFunc::Max => "max",
            AggFunc::Mean => "mean",
            AggFunc::Stddev => "stddev",
        }
    }
}

/// One `func(col)` aggregate in an `agg` list. `col`/`pos` are `None` only for a
/// bare `count` (which counts rows, not a column's non-empty cells).
#[derive(Clone, Debug)]
pub struct AggSpec {
    pub func: AggFunc,
    pub col: Option<String>,
    pub pos: Option<usize>,
    /// Output column name (`amount_sum`, or `count` for a bare count).
    pub name: String,
}

/// `group COLS` + `agg FNS`: reduce the input to one row per distinct key,
/// emitting the key columns followed by one column per aggregate. The per-key
/// sibling of `stats` (which reduces globally); a blocking, reducing stage that
/// holds O(groups × aggregated-cols) accumulators, not O(rows). With no keys it
/// reduces to a single row (a chosen-function global aggregate).
#[derive(Clone, Debug)]
pub struct GroupStmt {
    pub keys: Vec<String>,
    pub key_positions: Vec<usize>,
    pub aggs: Vec<AggSpec>,
}

impl GroupStmt {
    /// Resolve key and aggregated columns, then reshape the header to
    /// `keys ++ agg names` so downstream `sort`/`cols`/`fmt` compose.
    fn resolve(&mut self, header: &mut Vec<String>) -> Result<(), Error> {
        self.key_positions = self
            .keys
            .iter()
            .map(|n| resolve_col(n, header))
            .collect::<Result<_, _>>()?;
        for a in &mut self.aggs {
            a.pos = match &a.col {
                Some(c) => Some(resolve_col(c, header)?),
                None => None,
            };
        }
        let mut out = self.keys.clone();
        out.extend(self.aggs.iter().map(|a| a.name.clone()));
        *header = out;
        Ok(())
    }
}

/// Rename one or more columns (a header-only change; row data is untouched).
#[derive(Clone, Debug)]
pub struct RenameStmt {
    pub pairs: Vec<(String, String)>,
}

/// `uniq [cols]`: drop duplicate rows, keeping the first occurrence, comparing
/// either the whole row (empty `cols`) or just the named key columns. Global
/// (not adjacent-only like Unix `uniq`), so the input needn't be pre-sorted.
#[derive(Clone, Debug)]
pub struct UniqStmt {
    pub cols: Vec<String>,
    pub positions: Vec<usize>,
}

impl UniqStmt {
    fn resolve(&mut self, header: &[String]) -> Result<(), Error> {
        // Empty `cols` ⇒ empty positions ⇒ dedup on the whole row.
        self.positions = self
            .cols
            .iter()
            .map(|n| resolve_col(n, header))
            .collect::<Result<_, _>>()?;
        Ok(())
    }
}

/// `add NAME EXPR`: append (or, if `NAME` already exists, replace in place) a
/// column computed per row from `expr`. After resolution `pos` is `Some(i)` to
/// replace column `i`, or `None` to append; `stateful` flags an `expr` that
/// reads `prev()`/`rownum()` (which routes the plan to the in-memory ordered
/// path so row order is well-defined).
#[derive(Clone, Debug)]
pub struct AddStmt {
    pub name: String,
    pub expr: ValExpr,
    pub pos: Option<usize>,
    pub stateful: bool,
}

impl AddStmt {
    fn resolve(&mut self, header: &mut Vec<String>) -> Result<(), Error> {
        // Resolve the expression against the *current* header first, so a
        // replacing `add price price * 1.1` sees the old `price`, and an
        // appending `add total amount * qty` can't reference itself.
        self.expr.resolve(header)?;
        match header.iter().position(|h| h == &self.name) {
            Some(i) => self.pos = Some(i),
            None => {
                self.pos = None;
                header.push(self.name.clone());
            }
        }
        Ok(())
    }
}

/// Which rows a `join` keeps when a side has no match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum JoinType {
    /// Only rows that match on both sides.
    #[default]
    Inner,
    /// Every left row; right columns empty when unmatched.
    Left,
    /// Every right row; left columns empty when unmatched.
    Right,
    /// Union of left and right (matched + both sides' unmatched rows).
    Full,
}

impl JoinType {
    /// Keep left rows that found no right match.
    pub fn keeps_left_unmatched(self) -> bool {
        matches!(self, JoinType::Left | JoinType::Full)
    }
    /// Keep right rows that found no left match.
    pub fn keeps_right_unmatched(self) -> bool {
        matches!(self, JoinType::Right | JoinType::Full)
    }
    /// The flag spelling, for `--print-engine`.
    pub fn label(self) -> &'static str {
        match self {
            JoinType::Inner => "inner",
            JoinType::Left => "left",
            JoinType::Right => "right",
            JoinType::Full => "full",
        }
    }
}

/// `join [(SUBPIPELINE)] FILE on KEYS`: merge a second (right) source into the
/// stream by matching key columns. The right side is a full sub-[`Plan`] run over
/// `file` and fully materialized (the build side); the left side (the main input)
/// probes it. Output is the left columns plus the right's non-key columns, with
/// clashing right names auto-suffixed `_r`.
#[derive(Clone, Debug)]
pub struct JoinStmt {
    pub join_type: JoinType,
    /// The right side's own pipeline (identity/empty if none was given).
    pub right_plan: Box<Plan>,
    /// Path to the right CSV file (never stdin).
    pub file: String,
    /// Key column name pairs `(left_name, right_name)`; equal names when the
    /// `on` entry was a bare column.
    pub keys: Vec<(String, String)>,
    /// Suffixes applied to *clashing* column names from each side (`--lsuffix` /
    /// `--rsuffix`). The left side is unsuffixed by default; the right side
    /// defaults to `_r`. Only columns that would otherwise collide are suffixed.
    pub lsuffix: Option<String>,
    pub rsuffix: Option<String>,
    /// The right sub-plan's *output* header, filled in before resolution by
    /// `exec::prepare_joins` (it requires reading the right file).
    pub right_header: Vec<String>,
    // --- resolved (set by `resolve`) ---
    /// Left key columns' positions in the left (incoming) header.
    pub left_key_pos: Vec<usize>,
    /// Right key columns' positions in `right_header`.
    pub right_key_pos: Vec<usize>,
    /// Right columns appended to the output (all of `right_header` bar the keys).
    pub right_emit_pos: Vec<usize>,
    /// Number of left columns at the join (the incoming header's length).
    pub left_ncols: usize,
}

impl JoinStmt {
    /// Resolve key columns against the left header (threaded in) and the
    /// (already-set) right header, then reshape `header` to the joined schema.
    ///
    /// Columns whose names appear on *both* sides (the left header and the right's
    /// emitted columns) are disambiguated: the left clashing column gets
    /// `lsuffix` (none by default), the right one `rsuffix` (`_r` by default).
    /// Non-clashing names are left untouched.
    fn resolve(&mut self, header: &mut Vec<String>) -> Result<(), Error> {
        self.left_ncols = header.len();
        self.left_key_pos = self
            .keys
            .iter()
            .map(|(l, _)| resolve_col(l, header))
            .collect::<Result<_, _>>()?;
        self.right_key_pos = self
            .keys
            .iter()
            .map(|(_, r)| resolve_col(r, &self.right_header))
            .collect::<Result<_, _>>()?;
        // Emit every right column except the (redundant) key columns, in order.
        self.right_emit_pos = (0..self.right_header.len())
            .filter(|i| !self.right_key_pos.contains(i))
            .collect();

        let lsuf = self.lsuffix.as_deref().unwrap_or("");
        let rsuf = self.rsuffix.as_deref().unwrap_or("_r");
        let left_orig = header.clone();
        let right_names: Vec<&String> = self
            .right_emit_pos
            .iter()
            .map(|&p| &self.right_header[p])
            .collect();

        // Suffix clashing left columns (only when a left suffix is configured).
        if !lsuf.is_empty() {
            for h in header.iter_mut() {
                if right_names.iter().any(|r| *r == h) {
                    h.push_str(lsuf);
                }
            }
        }
        // Append right columns: clashing names get `rsuffix`; `disambiguate`
        // resolves any residual collision so the header is never ambiguous.
        for base in right_names {
            let candidate = if left_orig.iter().any(|l| l == base) {
                format!("{base}{rsuf}")
            } else {
                base.clone()
            };
            let name = disambiguate(&candidate, header);
            header.push(name);
        }
        Ok(())
    }
}

/// Return `name`, or `name` with a `_r` (then `_r2`, `_r3`, …) suffix if it
/// already appears in `taken`, so appended right columns never collide.
fn disambiguate(name: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == name) {
        return name.to_owned();
    }
    let base = format!("{name}_r");
    if !taken.iter().any(|t| t == &base) {
        return base;
    }
    (2..)
        .map(|n| format!("{name}_r{n}"))
        .find(|c| !taken.iter().any(|t| t == c))
        .unwrap()
}

/// A row-by-row statement.
#[derive(Clone, Debug)]
pub enum Stmt {
    Cols(ProjectStmt),
    Select(BoolExpr),
    ToNum(ConvStmt),
    ToStr(ConvStmt),
    Rename(RenameStmt),
    /// Append or replace a computed column (`add NAME EXPR`).
    Add(AddStmt),
}

/// A pipeline stage. Transforms run streaming; a sort blocks on all its rows;
/// head keeps the first `n` rows reaching it; tail keeps the last `n` and
/// drop-last keeps all but the last `n` (both block); stats reduces to a
/// profile.
#[derive(Clone, Debug)]
pub enum Stage {
    Transform(Vec<Stmt>),
    Sort(SortStmt),
    Head(usize),
    Tail(usize),
    /// Keep all but the last `n` rows (`head -n -N`).
    DropLast(usize),
    Stats(StatsStmt),
    Uniq(UniqStmt),
    /// Reduce to one row per distinct key (`group … | agg …`). Blocking.
    Group(GroupStmt),
    /// Merge a second (right) source in by key. Blocking: the right side is
    /// materialized into a hash table that the left rows probe.
    Join(JoinStmt),
}

/// How the final output is rendered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Csv,
    /// Whitespace-aligned columns, like `column -t` (set by the `fmt` command).
    Aligned,
}

/// What a predicate colour rule paints.
#[derive(Clone, Debug)]
pub enum ColorScope {
    /// The whole row.
    Row,
    /// Only the named column's cell.
    Cell(ColRef),
}

/// One `color` rule, applied to the output rows at render time (so its column
/// references resolve against the *output* header).
#[derive(Clone, Debug)]
pub enum ColorRule {
    /// Paint `scope` with `style` on rows where `expr` is true.
    Predicate {
        scope: ColorScope,
        style: Style,
        expr: BoolExpr,
    },
    /// Colour `col`'s cells by where each value falls in `bounds` (default: the
    /// column's min/max), using `ramp`.
    Gradient {
        col: ColRef,
        ramp: Ramp,
        bounds: Option<(f64, f64)>,
    },
}

impl ColorRule {
    fn resolve(&mut self, header: &[String]) -> Result<(), Error> {
        match self {
            ColorRule::Predicate { scope, expr, .. } => {
                expr.resolve(header)?;
                if let ColorScope::Cell(c) = scope {
                    c.resolve(header)?;
                }
            }
            ColorRule::Gradient { col, .. } => col.resolve(header)?,
        }
        Ok(())
    }
}

/// What kind of chart `graph` draws. Only `Hist` so far; bar/scatter/line/spark
/// are the planned follow-ups (see `todo.org`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphKind {
    /// `graph hist COL`: distribution of one numeric column as binned bars.
    Hist,
}

/// Presentation options shared across chart kinds; an absent field falls back to
/// a sensible default (bin count from the data, width from the terminal).
#[derive(Clone, Debug, Default)]
pub struct GraphOpts {
    pub bins: Option<usize>,
    pub width: Option<usize>,
    pub title: Option<String>,
}

/// A `graph` sink: draw a terminal chart from the columns reaching it instead of
/// emitting CSV. Plan metadata (like `colors`) and the *last* command — it
/// terminates the pipe, so it renders from the buffered output (see
/// `exec::render`), reusing the whole executor upstream.
#[derive(Clone, Debug)]
pub struct GraphSpec {
    pub kind: GraphKind,
    pub cols: Vec<ColRef>,
    pub opts: GraphOpts,
}

/// A fully compiled pipeline.
#[derive(Clone, Debug)]
pub struct Plan {
    pub stages: Vec<Stage>,
    pub output: OutputFormat,
    /// Column names supplied by a `hdr` command, for input that has no header
    /// line. When set, the whole input is data and this is the header.
    pub input_header: Option<Vec<String>>,
    /// Colour rules, applied to the output rows at render time.
    pub colors: Vec<ColorRule>,
    /// A terminal chart to draw instead of emitting CSV (the `graph` sink).
    pub graph: Option<GraphSpec>,
}

fn resolve_col(name: &str, header: &[String]) -> Result<usize, Error> {
    header
        .iter()
        .position(|h| h == name)
        .ok_or_else(|| Error::Column {
            name: name.to_owned(),
            available: header.to_vec(),
        })
}

/// View a row cell as text, treating an out-of-range index as empty (rows can
/// be ragged; csvm would index out of bounds here).
#[inline]
fn cell_str<'r>(row: &'r [Field], pos: usize) -> std::borrow::Cow<'r, str> {
    match row.get(pos) {
        Some(f) => f.as_str(),
        None => std::borrow::Cow::Borrowed(""),
    }
}

/// Coerce a row cell to a number, treating an out-of-range index as `0.0`.
#[inline]
fn cell_num(row: &[Field], pos: usize) -> Result<f64, Error> {
    match row.get(pos) {
        Some(f) => Ok(f.coerce_num()?),
        None => Ok(0.0),
    }
}

impl Operand {
    #[inline]
    fn as_num(&self, row: &[Field]) -> Result<f64, Error> {
        match self {
            Operand::Col(c) => cell_num(row, c.pos),
            Operand::Num(n) => Ok(*n),
            // A numeric comparison never carries a `Str` operand (compile-time
            // normalization parses string literals), but coerce defensively.
            Operand::Str(s) => Field::Str(s).coerce_num().map_err(Error::Num),
        }
    }

    #[inline]
    fn as_str<'r>(&'r self, row: &'r [Field]) -> std::borrow::Cow<'r, str> {
        match self {
            Operand::Col(c) => cell_str(row, c.pos),
            Operand::Str(s) => std::borrow::Cow::Borrowed(s.as_str()),
            Operand::Num(n) => std::borrow::Cow::Owned(format_num(*n)),
        }
    }

    fn resolve(&mut self, header: &[String]) -> Result<(), Error> {
        if let Operand::Col(c) = self {
            c.resolve(header)?;
        }
        Ok(())
    }
}

impl Cmp {
    #[inline]
    fn eval(&self, row: &[Field]) -> Result<bool, Error> {
        let ord = if self.numeric {
            let l = self.lhs.as_num(row)?;
            let r = self.rhs.as_num(row)?;
            l.partial_cmp(&r)
        } else {
            Some(self.lhs.as_str(row).cmp(&self.rhs.as_str(row)))
        };
        // For numeric NaN, `partial_cmp` is None; treat as "unordered": equal is
        // false, not-equal is true, and orderings are false.
        Ok(match (self.op, ord) {
            (CmpOp::Eq, Some(Ordering::Equal)) => true,
            (CmpOp::Eq, _) => false,
            (CmpOp::Ne, Some(Ordering::Equal)) => false,
            (CmpOp::Ne, _) => true,
            (CmpOp::Lt, Some(Ordering::Less)) => true,
            (CmpOp::Gt, Some(Ordering::Greater)) => true,
            (CmpOp::Le, Some(Ordering::Less | Ordering::Equal)) => true,
            (CmpOp::Ge, Some(Ordering::Greater | Ordering::Equal)) => true,
            _ => false,
        })
    }
}

impl BoolExpr {
    #[inline]
    pub fn eval(&self, row: &[Field]) -> Result<bool, Error> {
        match self {
            BoolExpr::And(es) => {
                for e in es {
                    if !e.eval(row)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            BoolExpr::Or(es) => {
                for e in es {
                    if e.eval(row)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            BoolExpr::Not(e) => Ok(!e.eval(row)?),
            BoolExpr::Cmp(c) => c.eval(row),
            BoolExpr::Match { col, regex, negate } => {
                Ok(regex.is_match(&cell_str(row, col.pos)) ^ negate)
            }
            BoolExpr::Affix { col, needle, kind } => {
                let cell = cell_str(row, col.pos);
                Ok(match kind {
                    AffixKind::StartsWith => cell.starts_with(needle.as_str()),
                    AffixKind::Contains => cell.contains(needle.as_str()),
                    AffixKind::EndsWith => cell.ends_with(needle.as_str()),
                })
            }
        }
    }

    fn resolve(&mut self, header: &[String]) -> Result<(), Error> {
        match self {
            BoolExpr::And(es) | BoolExpr::Or(es) => {
                for e in es {
                    e.resolve(header)?;
                }
            }
            BoolExpr::Not(e) => e.resolve(header)?,
            BoolExpr::Cmp(c) => {
                c.lhs.resolve(header)?;
                c.rhs.resolve(header)?;
            }
            BoolExpr::Match { col, .. } => col.resolve(header)?,
            BoolExpr::Affix { col, .. } => col.resolve(header)?,
        }
        Ok(())
    }
}

/// Per-row context for the stateful leaves of a value expression (`prev()`,
/// `rownum()`). [`Default`] is empty — a pure expression never reads it, so the
/// streaming/sharded paths pass the default; the in-memory ordered path fills it.
#[derive(Default)]
pub struct EvalCtx<'a> {
    /// The previous row (`None` on the first row, where `prev()` reads the
    /// current cell instead).
    pub prev_row: Option<&'a [Field<'a>]>,
    /// The 1-based index of the current row.
    pub rownum: u64,
}

/// A cell's value, detached so it can be appended to a row (`add` produces new
/// values rather than borrowing from the input).
#[inline]
fn cell_field(row: &[Field], pos: usize) -> Field<'static> {
    match row.get(pos) {
        Some(f) => f.clone().into_owned(),
        None => Field::Owned(String::new()),
    }
}

impl ValExpr {
    /// Evaluate to a value for the current `row`. Returns an owned field (numbers
    /// and computed strings never borrow the input).
    pub fn eval(&self, row: &[Field], ctx: &EvalCtx) -> Result<Field<'static>, Error> {
        Ok(match self {
            ValExpr::Col(c) => cell_field(row, c.pos),
            ValExpr::Num(n) => Field::Num(*n),
            ValExpr::Str(s) => Field::Owned(s.clone()),
            ValExpr::Neg(e) => Field::Num(-e.eval(row, ctx)?.coerce_num()?),
            ValExpr::Arith { op, lhs, rhs } => {
                let l = lhs.eval(row, ctx)?.coerce_num()?;
                let r = rhs.eval(row, ctx)?.coerce_num()?;
                let v = match op {
                    ArithOp::Add => l + r,
                    ArithOp::Sub => l - r,
                    ArithOp::Mul => l * r,
                    ArithOp::Div => {
                        if r == 0.0 {
                            return Err(Error::Other("division by zero in add expression".into()));
                        }
                        l / r
                    }
                    ArithOp::Mod => {
                        if r == 0.0 {
                            return Err(Error::Other("modulo by zero in add expression".into()));
                        }
                        l % r
                    }
                };
                Field::Num(v)
            }
            ValExpr::Concat(parts) => {
                let mut s = String::new();
                for p in parts {
                    s.push_str(&p.eval(row, ctx)?.as_str());
                }
                Field::Owned(s)
            }
            ValExpr::Func(f, args) => eval_func(*f, args, row, ctx)?,
            ValExpr::Bool(b) => Field::Str(if b.eval(row)? { "t" } else { "f" }),
            ValExpr::Cond { test, then_, else_ } => {
                if test.eval(row)? {
                    then_.eval(row, ctx)?
                } else {
                    else_.eval(row, ctx)?
                }
            }
            ValExpr::Prev(c) => {
                // On the first row there is no previous row; read the current
                // cell so a `col - prev(col)` delta is 0 rather than `col`.
                let src = ctx.prev_row.unwrap_or(row);
                cell_field(src, c.pos)
            }
            ValExpr::Rownum => Field::Num(ctx.rownum as f64),
        })
    }

    fn resolve(&mut self, header: &[String]) -> Result<(), Error> {
        match self {
            ValExpr::Col(c) | ValExpr::Prev(c) => c.resolve(header)?,
            ValExpr::Num(_) | ValExpr::Str(_) | ValExpr::Rownum => {}
            ValExpr::Neg(e) => e.resolve(header)?,
            ValExpr::Arith { lhs, rhs, .. } => {
                lhs.resolve(header)?;
                rhs.resolve(header)?;
            }
            ValExpr::Concat(parts) | ValExpr::Func(_, parts) => {
                for p in parts {
                    p.resolve(header)?;
                }
            }
            ValExpr::Bool(b) => b.resolve(header)?,
            ValExpr::Cond { test, then_, else_ } => {
                test.resolve(header)?;
                then_.resolve(header)?;
                else_.resolve(header)?;
            }
        }
        Ok(())
    }

    /// Whether the expression reads cross-row state (`prev()`/`rownum()`), which
    /// makes it order-dependent (not shardable).
    pub fn is_stateful(&self) -> bool {
        match self {
            ValExpr::Prev(_) | ValExpr::Rownum => true,
            ValExpr::Col(_) | ValExpr::Num(_) | ValExpr::Str(_) | ValExpr::Bool(_) => false,
            ValExpr::Neg(e) => e.is_stateful(),
            ValExpr::Arith { lhs, rhs, .. } => lhs.is_stateful() || rhs.is_stateful(),
            ValExpr::Concat(parts) | ValExpr::Func(_, parts) => {
                parts.iter().any(ValExpr::is_stateful)
            }
            ValExpr::Cond { then_, else_, .. } => then_.is_stateful() || else_.is_stateful(),
        }
    }

    /// A best-effort static type, used by the parser to mark the new column
    /// numeric/text for later implicit comparisons. `None` when it depends on
    /// the data (a bare column, `prev`, or a `?:`).
    pub fn static_numeric(&self) -> Option<bool> {
        match self {
            ValExpr::Num(_) | ValExpr::Neg(_) | ValExpr::Arith { .. } | ValExpr::Rownum => {
                Some(true)
            }
            ValExpr::Str(_) | ValExpr::Concat(_) | ValExpr::Bool(_) => Some(false),
            ValExpr::Func(f, _) => Some(!matches!(f, Func::Upper | Func::Lower | Func::Trim)),
            ValExpr::Col(_) | ValExpr::Prev(_) | ValExpr::Cond { .. } => None,
        }
    }
}

/// Evaluate a built-in function call.
fn eval_func(
    f: Func,
    args: &[ValExpr],
    row: &[Field],
    ctx: &EvalCtx,
) -> Result<Field<'static>, Error> {
    let num = |e: &ValExpr| -> Result<f64, Error> { Ok(e.eval(row, ctx)?.coerce_num()?) };
    Ok(match f {
        Func::Round => Field::Num(num(&args[0])?.round()),
        Func::Floor => Field::Num(num(&args[0])?.floor()),
        Func::Ceil => Field::Num(num(&args[0])?.ceil()),
        Func::Abs => Field::Num(num(&args[0])?.abs()),
        Func::Int => Field::Num(num(&args[0])?.trunc()),
        Func::Len => Field::Num(args[0].eval(row, ctx)?.as_str().chars().count() as f64),
        Func::Upper => Field::Owned(args[0].eval(row, ctx)?.as_str().to_uppercase()),
        Func::Lower => Field::Owned(args[0].eval(row, ctx)?.as_str().to_lowercase()),
        Func::Trim => Field::Owned(args[0].eval(row, ctx)?.as_str().trim().to_owned()),
        Func::Min | Func::Max => {
            let mut acc = num(&args[0])?;
            for a in &args[1..] {
                let v = num(a)?;
                acc = if matches!(f, Func::Min) {
                    acc.min(v)
                } else {
                    acc.max(v)
                };
            }
            Field::Num(acc)
        }
        Func::Coalesce => {
            let mut chosen: Option<Field<'static>> = None;
            for a in args {
                let v = a.eval(row, ctx)?;
                if !v.as_str().is_empty() {
                    chosen = Some(v);
                    break;
                }
            }
            chosen.unwrap_or(Field::Owned(String::new()))
        }
    })
}

/// Project a row to `positions` using a reusable `scratch` buffer (swapped in,
/// so no per-row allocation). Positions may repeat (duplicating a column) or
/// skip; an out-of-range position yields an empty field rather than panicking.
#[inline]
fn project<'a>(row: &mut Vec<Field<'a>>, positions: &[usize], scratch: &mut Vec<Field<'a>>) {
    scratch.clear();
    scratch.reserve(positions.len());
    for &p in positions {
        scratch.push(row.get(p).cloned().unwrap_or(Field::Str("")));
    }
    std::mem::swap(row, scratch);
}

impl Stmt {
    /// Apply to a row, returning whether the row survives (only `Select` can
    /// drop it). `scratch` is a caller-owned buffer reused across rows so
    /// projection (`cols`/`drop-cols`) doesn't allocate per row.
    #[inline]
    pub fn apply<'a>(
        &self,
        row: &mut Vec<Field<'a>>,
        scratch: &mut Vec<Field<'a>>,
        ctx: &EvalCtx,
    ) -> Result<bool, Error> {
        match self {
            Stmt::Cols(p) => {
                project(row, &p.positions, scratch);
                Ok(true)
            }
            Stmt::Select(expr) => expr.eval(row),
            Stmt::Add(a) => {
                let value = a.expr.eval(row, ctx)?;
                match a.pos {
                    // Replace in place; pad with empties if the row is short.
                    Some(i) => {
                        if i >= row.len() {
                            row.resize(i + 1, Field::Str(""));
                        }
                        row[i] = value;
                    }
                    None => row.push(value),
                }
                Ok(true)
            }
            Stmt::ToNum(c) => {
                for &p in &c.positions {
                    if let Some(f) = row.get_mut(p) {
                        *f = Field::Num(f.coerce_num()?);
                    }
                }
                Ok(true)
            }
            Stmt::ToStr(c) => {
                for &p in &c.positions {
                    if let Some(f) = row.get_mut(p)
                        && let Field::Num(n) = *f
                    {
                        *f = Field::Owned(format_num(n));
                    }
                }
                Ok(true)
            }
            // Rename is a header-only change; row data is untouched.
            Stmt::Rename(_) => Ok(true),
        }
    }

    /// Resolve column names to positions against `header`, and reshape `header`
    /// for downstream statements when this statement reshapes the row.
    fn resolve(&mut self, header: &mut Vec<String>) -> Result<(), Error> {
        match self {
            Stmt::Cols(p) => p.resolve(header),
            Stmt::Select(expr) => expr.resolve(header),
            Stmt::ToNum(c) | Stmt::ToStr(c) => c.resolve(header),
            Stmt::Rename(r) => {
                for (from, to) in &r.pairs {
                    let pos = resolve_col(from, header)?;
                    header[pos] = to.clone();
                }
                Ok(())
            }
            Stmt::Add(a) => a.resolve(header),
        }
    }

    /// Whether this statement reads cross-row state (only a stateful `add`),
    /// which routes the plan to the in-memory ordered execution path.
    pub fn is_stateful(&self) -> bool {
        matches!(self, Stmt::Add(a) if a.stateful)
    }
}

impl ProjectStmt {
    fn resolve(&mut self, header: &mut Vec<String>) -> Result<(), Error> {
        if self.exclude {
            // Keep, in order, the columns not named. Unknown names are ignored
            // (csvm's exclude path does the same).
            self.positions = (0..header.len())
                .filter(|&i| !self.names.iter().any(|n| n == &header[i]))
                .collect();
        } else {
            // Keep the named columns, in the given order. A missing name is an
            // error.
            self.positions = self
                .names
                .iter()
                .map(|n| resolve_col(n, header))
                .collect::<Result<_, _>>()?;
        }
        *header = self.positions.iter().map(|&p| header[p].clone()).collect();
        Ok(())
    }
}

impl ConvStmt {
    fn resolve(&mut self, header: &[String]) -> Result<(), Error> {
        self.positions = self
            .names
            .iter()
            .map(|n| resolve_col(n, header))
            .collect::<Result<_, _>>()?;
        Ok(())
    }
}

impl StatsStmt {
    /// Resolve the profiled columns (empty list ⇒ all) and reshape the header to
    /// the profile schema for downstream stages.
    fn resolve(&mut self, header: &mut Vec<String>) -> Result<(), Error> {
        if self.cols.is_empty() {
            self.positions = (0..header.len()).collect();
            self.names = header.clone();
        } else {
            self.positions = self
                .cols
                .iter()
                .map(|n| resolve_col(n, header))
                .collect::<Result<_, _>>()?;
            self.names = self.cols.clone();
        }
        *header = STATS_SCHEMA.iter().map(|s| s.to_string()).collect();
        Ok(())
    }
}

impl SortStmt {
    fn resolve(&mut self, header: &[String]) -> Result<(), Error> {
        for k in &mut self.keys {
            k.pos = resolve_col(&k.name, header)?;
        }
        Ok(())
    }

    /// Positions whose values must be converted to numbers before sorting.
    pub fn numeric_positions(&self) -> impl Iterator<Item = usize> + '_ {
        self.keys.iter().filter(|k| k.numeric).map(|k| k.pos)
    }

    /// Compare two rows by the sort keys. Numeric keys are assumed already
    /// converted to numbers (see [`SortStmt::numeric_positions`]).
    #[inline]
    pub fn compare(&self, a: &[Field], b: &[Field]) -> Ordering {
        for k in &self.keys {
            let ord = if k.numeric {
                num_of(a, k.pos).total_cmp(&num_of(b, k.pos))
            } else {
                cell_str(a, k.pos).cmp(&cell_str(b, k.pos))
            };
            let ord = if k.descending { ord.reverse() } else { ord };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    }
}

#[inline]
fn num_of(row: &[Field], pos: usize) -> f64 {
    match row.get(pos) {
        Some(Field::Num(n)) => *n,
        Some(f) => f.coerce_num().unwrap_or(0.0),
        None => 0.0,
    }
}

/// Apply a sequence of statements to a row, returning whether it survives (only
/// a `Select` can drop it). The per-row hot path — no interpreter involved.
/// `scratch` is a caller-owned buffer reused across rows (see [`Stmt::apply`]).
#[inline]
pub fn apply_stmts<'a>(
    stmts: &[Stmt],
    row: &mut Vec<Field<'a>>,
    scratch: &mut Vec<Field<'a>>,
    ctx: &EvalCtx,
) -> Result<bool, Error> {
    for s in stmts {
        if !s.apply(row, scratch, ctx)? {
            return Ok(false);
        }
    }
    Ok(true)
}

impl Plan {
    /// Resolve every column reference against the input header, returning the
    /// output header (the input header reshaped by any `cols`/`drop-cols`).
    pub fn resolve(&mut self, input_header: &[String]) -> Result<Vec<String>, Error> {
        let mut header = input_header.to_vec();
        for stage in &mut self.stages {
            match stage {
                Stage::Transform(stmts) => {
                    for s in stmts {
                        s.resolve(&mut header)?;
                    }
                }
                Stage::Sort(s) => s.resolve(&header)?,
                Stage::Head(_) | Stage::Tail(_) | Stage::DropLast(_) => {} // no columns to resolve
                Stage::Stats(s) => s.resolve(&mut header)?,
                Stage::Uniq(u) => u.resolve(&header)?, // keeps the row shape
                Stage::Group(g) => g.resolve(&mut header)?,
                Stage::Join(j) => j.resolve(&mut header)?,
            }
        }
        // Colour rules render the output, so resolve them against the final header.
        for rule in &mut self.colors {
            rule.resolve(&header)?;
        }
        // The graph sink draws from the final columns; resolve its references too.
        if let Some(g) = &mut self.graph {
            for c in &mut g.cols {
                c.resolve(&header)?;
            }
        }
        Ok(header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(vals: &[&'static str]) -> Vec<Field<'static>> {
        vals.iter().map(|s| Field::Str(s)).collect()
    }

    fn col(name: &str) -> ColRef {
        ColRef::new(name.into())
    }

    #[test]
    fn numeric_and_string_compares() {
        // (> b 0) numeric: "5" > 0 -> true, "0" > 0 -> false
        let cmp = Cmp {
            op: CmpOp::Gt,
            lhs: Operand::Col(ColRef {
                name: "b".into(),
                pos: 1,
            }),
            rhs: Operand::Num(0.0),
            numeric: true,
        };
        assert!(cmp.eval(&row(&["a", "5"])).unwrap());
        assert!(!cmp.eval(&row(&["a", "0"])).unwrap());

        // (== a "t") string
        let cmp = Cmp {
            op: CmpOp::Eq,
            lhs: Operand::Col(ColRef {
                name: "a".into(),
                pos: 0,
            }),
            rhs: Operand::Str("t".into()),
            numeric: false,
        };
        assert!(cmp.eval(&row(&["t", "5"])).unwrap());
        assert!(!cmp.eval(&row(&["f", "5"])).unwrap());
    }

    #[test]
    fn and_or_short_circuit_and_correctness() {
        // (and (== a "t") (or (> b 0) (> c 0)))
        let expr = BoolExpr::And(vec![
            BoolExpr::Cmp(Cmp {
                op: CmpOp::Eq,
                lhs: Operand::Col(ColRef {
                    name: "a".into(),
                    pos: 0,
                }),
                rhs: Operand::Str("t".into()),
                numeric: false,
            }),
            BoolExpr::Or(vec![
                BoolExpr::Cmp(Cmp {
                    op: CmpOp::Gt,
                    lhs: Operand::Col(ColRef {
                        name: "b".into(),
                        pos: 1,
                    }),
                    rhs: Operand::Num(0.0),
                    numeric: true,
                }),
                BoolExpr::Cmp(Cmp {
                    op: CmpOp::Gt,
                    lhs: Operand::Col(ColRef {
                        name: "c".into(),
                        pos: 2,
                    }),
                    rhs: Operand::Num(0.0),
                    numeric: true,
                }),
            ]),
        ]);
        assert!(expr.eval(&row(&["t", "0", "3"])).unwrap());
        assert!(expr.eval(&row(&["t", "1", "0"])).unwrap());
        assert!(!expr.eval(&row(&["t", "0", "0"])).unwrap());
        assert!(!expr.eval(&row(&["f", "9", "9"])).unwrap());
    }

    #[test]
    fn cols_resolve_reshapes_header() {
        let mut plan = Plan {
            stages: vec![Stage::Transform(vec![
                Stmt::Cols(ProjectStmt {
                    exclude: false,
                    names: vec!["c".into(), "a".into()],
                    positions: vec![],
                }),
                // After the cols above, the row is [c, a]; `select(a == "x")`
                // must resolve `a` to position 1, not its original 0.
                Stmt::Select(BoolExpr::Cmp(Cmp {
                    op: CmpOp::Eq,
                    lhs: Operand::Col(col("a")),
                    rhs: Operand::Str("x".into()),
                    numeric: false,
                })),
            ])],
            output: OutputFormat::Csv,
            input_header: None,
            colors: Vec::new(),
            graph: None,
        };
        let out = plan.resolve(&["a".into(), "b".into(), "c".into()]).unwrap();
        assert_eq!(out, vec!["c", "a"]);
        let Stage::Transform(stmts) = &plan.stages[0] else {
            unreachable!()
        };
        let Stmt::Cols(p) = &stmts[0] else {
            unreachable!()
        };
        assert_eq!(p.positions, vec![2, 0]);
        let Stmt::Select(BoolExpr::Cmp(c)) = &stmts[1] else {
            unreachable!()
        };
        let Operand::Col(cref) = &c.lhs else {
            unreachable!()
        };
        assert_eq!(cref.pos, 1);
    }

    #[test]
    fn drop_cols_keeps_complement() {
        let mut p = ProjectStmt {
            exclude: true,
            names: vec!["b".into(), "missing".into()],
            positions: vec![],
        };
        let mut header = vec!["a".into(), "b".into(), "c".into()];
        p.resolve(&mut header).unwrap();
        assert_eq!(p.positions, vec![0, 2]);
        assert_eq!(header, vec!["a", "c"]);
    }

    #[test]
    fn unknown_column_errors() {
        let mut plan = Plan {
            stages: vec![Stage::Transform(vec![Stmt::ToNum(ConvStmt {
                names: vec!["nope".into()],
                positions: vec![],
            })])],
            output: OutputFormat::Csv,
            input_header: None,
            colors: Vec::new(),
            graph: None,
        };
        let err = plan.resolve(&["a".into()]).unwrap_err();
        assert!(matches!(err, Error::Column { name, .. } if name == "nope"));
    }

    #[test]
    fn sort_compare_numeric_reverse() {
        let s = SortStmt {
            keys: vec![SortKey {
                name: "n".into(),
                pos: 0,
                descending: true,
                numeric: true,
            }],
        };
        // numeric: 10 > 9 (lexically "10" < "9", so this proves numeric mode)
        let a = vec![Field::Num(10.0)];
        let b = vec![Field::Num(9.0)];
        assert_eq!(s.compare(&a, &b), Ordering::Less); // descending: bigger first
    }
}
