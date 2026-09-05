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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

/// How a comparison orders its operands, decided at compile time from the
/// operand types and the columns' tracked types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpMode {
    /// At least one operand is statically numeric (a number literal,
    /// arithmetic, a numeric function, `rownum()`, or a `to-num`-typed
    /// column): parse both to `f64` and order numerically (a non-number
    /// aborts the run).
    Numeric,
    /// A statically string operand (a string literal, `++` concat, a boolean
    /// value, a string function, or a `to-str`-typed column), or an `==`/`!=`
    /// between two untyped operands: order lexically.
    String,
    /// An ordering (`< > <= >=`) between two untyped operands: decide per
    /// row — if both sides coerce to numbers, order numerically, else fall
    /// back to lexical. Reproducible (a function of the two values); a leaf
    /// that fails to coerce falls back, but a hard error inside a compound
    /// operand still aborts.
    Auto,
}

/// A single comparison. Operands are full value expressions ([`ValExpr`]), so
/// arithmetic, function calls, and boolean subexpressions (compared as their
/// `t`/`f` rendering) all work. `mode` is decided at compile time from the
/// operands' static types and the columns' tracked types.
#[derive(Clone, Debug)]
pub struct Cmp {
    pub op: CmpOp,
    pub lhs: ValExpr,
    pub rhs: ValExpr,
    pub mode: CmpMode,
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
    Sqrt,
    Pow,
    Exp,
    Log,
    Log10,
    Log2,
    Sign,
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
            Func::Sqrt => "sqrt",
            Func::Pow => "pow",
            Func::Exp => "exp",
            Func::Log => "log",
            Func::Log10 => "log10",
            Func::Log2 => "log2",
            Func::Sign => "sign",
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
            "sqrt" => Func::Sqrt,
            "pow" => Func::Pow,
            "exp" => Func::Exp,
            "log" => Func::Log,
            "log10" => Func::Log10,
            "log2" => Func::Log2,
            "sign" => Func::Sign,
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
        "round", "floor", "ceil", "abs", "int", "sqrt", "pow", "exp", "log", "log10", "log2",
        "sign", "min", "max", "len", "upper", "lower", "trim", "coalesce",
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

/// How one `sort` key orders its cells. The sort-side counterpart of
/// [`CmpMode`]: a bare column defaults to `Auto`; `=n` / a `to-num` column
/// pins `Numeric`, `=s` / a `to-str` column pins `Lexical`. One deliberate
/// difference: `select`'s auto reads a blank cell as 0, while `Auto` here
/// reads it as text, so blanks sort after every number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortMode {
    /// Decided per cell: a cell that parses as a number orders numerically
    /// and before every non-number; the rest (text, empty) order lexically.
    /// A total order that needs no type sampling, so it streams and shards.
    Auto,
    /// Every cell is coerced to a number (a non-number aborts the run).
    Numeric,
    /// Plain byte-wise text order.
    Lexical,
}

/// One key of a `sort`.
#[derive(Clone, Debug)]
pub struct SortKey {
    pub name: String,
    pub pos: usize,
    pub descending: bool,
    pub mode: SortMode,
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
    /// How many leading `keys` came from an explicit `on` clause — this item's
    /// own, or the shared trailing one copied verbatim onto keyless items.
    /// Entries past it were appended by comma-continuation fragments, which
    /// are lexically identical to a file missing its `on` — those drive the
    /// forgotten-`on` hint when they fail to resolve.
    pub own_keys: usize,
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
        let own = self.own_keys;
        self.left_key_pos = self
            .keys
            .iter()
            .enumerate()
            .map(|(i, (l, _))| {
                resolve_col(l, header).map_err(|e| if i < own { e } else { join_key_err(e) })
            })
            .collect::<Result<_, _>>()?;
        self.right_key_pos = self
            .keys
            .iter()
            .enumerate()
            .map(|(i, (_, r))| {
                resolve_col(r, &self.right_header)
                    .map_err(|e| if i < own { e } else { join_key_err(e) })
            })
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

/// What kind of chart `graph` draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphKind {
    /// `graph hist COL`: distribution of one numeric column as binned bars.
    Hist,
    /// `graph bar LABEL VALUE`: one horizontal bar per row (use after group-by).
    Bar,
    /// `graph spark COL`: a one-line sparkline of a column's values.
    Spark,
    /// `graph scatter X Y`: points on a braille canvas.
    Scatter,
    /// `graph line X Y`: points connected on a braille canvas.
    Line,
}

/// Presentation options shared across chart kinds.
#[derive(Clone, Debug)]
pub struct GraphOpts {
    pub bins: Option<usize>,
    /// A single size multiplier over the base chart dimensions (1.0 = default).
    pub scale: f64,
    pub title: Option<String>,
    /// Emit an SVG document (to the normal output) instead of a terminal chart.
    pub svg: bool,
}

impl Default for GraphOpts {
    fn default() -> Self {
        GraphOpts {
            bins: None,
            scale: 1.0,
            title: None,
            svg: false,
        }
    }
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

/// A continuation key (appended after an item's own `on` clause) that fails to
/// resolve may really be a file missing its own `on` — the two are lexically
/// identical (`join a.csv on x, b.csv`). Add that hint to the error.
fn join_key_err(e: Error) -> Error {
    match &e {
        Error::Column { name, .. } => Error::Other(format!(
            "{e} (`{name}` was read as an extra join key — did you forget its `on`?)"
        )),
        _ => e,
    }
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
pub(crate) fn cell_num(row: &[Field], pos: usize) -> Result<f64, Error> {
    match row.get(pos) {
        Some(f) => Ok(f.coerce_num()?),
        None => Ok(0.0),
    }
}

impl Cmp {
    #[inline]
    fn eval(&self, row: &[Field], ctx: &EvalCtx) -> Result<bool, Error> {
        let ord = match self.mode {
            CmpMode::Numeric => {
                let l = self.lhs.cmp_num(row, ctx)?;
                let r = self.rhs.cmp_num(row, ctx)?;
                l.partial_cmp(&r)
            }
            CmpMode::String => Some(
                self.lhs
                    .cmp_str(row, ctx)?
                    .cmp(&self.rhs.cmp_str(row, ctx)?),
            ),
            // Per-row: order numerically when both cells parse as numbers, else
            // lexically. A non-number quietly falls back to text rather than
            // aborting (but a hard error inside a compound operand still aborts).
            CmpMode::Auto => match (
                self.lhs.cmp_num_soft(row, ctx)?,
                self.rhs.cmp_num_soft(row, ctx)?,
            ) {
                (Some(l), Some(r)) => l.partial_cmp(&r),
                _ => Some(
                    self.lhs
                        .cmp_str(row, ctx)?
                        .cmp(&self.rhs.cmp_str(row, ctx)?),
                ),
            },
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
    pub fn eval(&self, row: &[Field], ctx: &EvalCtx) -> Result<bool, Error> {
        match self {
            BoolExpr::And(es) => {
                for e in es {
                    if !e.eval(row, ctx)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            BoolExpr::Or(es) => {
                for e in es {
                    if e.eval(row, ctx)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            BoolExpr::Not(e) => Ok(!e.eval(row, ctx)?),
            BoolExpr::Cmp(c) => c.eval(row, ctx),
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

    /// Whether the expression reads cross-row state (`prev()`/`rownum()`) via a
    /// comparison operand, which routes the plan to the ordered in-memory path.
    pub fn is_stateful(&self) -> bool {
        match self {
            BoolExpr::And(es) | BoolExpr::Or(es) => es.iter().any(BoolExpr::is_stateful),
            BoolExpr::Not(e) => e.is_stateful(),
            BoolExpr::Cmp(c) => c.lhs.is_stateful() || c.rhs.is_stateful(),
            BoolExpr::Match { .. } | BoolExpr::Affix { .. } => false,
        }
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
                            return Err(Error::Other("division by zero in expression".into()));
                        }
                        l / r
                    }
                    ArithOp::Mod => {
                        if r == 0.0 {
                            return Err(Error::Other("modulo by zero in expression".into()));
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
            ValExpr::Bool(b) => Field::Str(if b.eval(row, ctx)? { "t" } else { "f" }),
            ValExpr::Cond { test, then_, else_ } => {
                if test.eval(row, ctx)? {
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

    /// A comparison operand as a number. Leaf shortcuts keep the hot path
    /// allocation-free (a bare column or literal never builds a `Field`); the
    /// compound fallback is outlined so these stay small enough to inline.
    #[inline]
    fn cmp_num(&self, row: &[Field], ctx: &EvalCtx) -> Result<f64, Error> {
        match self {
            ValExpr::Col(c) => cell_num(row, c.pos),
            ValExpr::Num(n) => Ok(*n),
            // A numeric comparison never carries a `Str` operand (compile-time
            // normalization parses string literals), but coerce defensively.
            ValExpr::Str(s) => Field::Str(s).coerce_num().map_err(Error::Num),
            e => e.cmp_num_compound(row, ctx),
        }
    }

    #[inline(never)]
    fn cmp_num_compound(&self, row: &[Field], ctx: &EvalCtx) -> Result<f64, Error> {
        Ok(self.eval(row, ctx)?.coerce_num()?)
    }

    /// A comparison operand as text (borrowed for the leaf cases).
    #[inline]
    fn cmp_str<'r>(
        &'r self,
        row: &'r [Field],
        ctx: &EvalCtx,
    ) -> Result<std::borrow::Cow<'r, str>, Error> {
        Ok(match self {
            ValExpr::Col(c) => cell_str(row, c.pos),
            ValExpr::Str(s) => std::borrow::Cow::Borrowed(s.as_str()),
            ValExpr::Num(n) => std::borrow::Cow::Owned(format_num(*n)),
            e => std::borrow::Cow::Owned(e.cmp_str_compound(row, ctx)?),
        })
    }

    #[inline(never)]
    fn cmp_str_compound(&self, row: &[Field], ctx: &EvalCtx) -> Result<String, Error> {
        Ok(self.eval(row, ctx)?.as_str().into_owned())
    }

    /// A comparison operand as a number for `Auto` mode: `None` means "not a
    /// number, fall back to lexical" — but a hard error inside a compound
    /// operand (e.g. arithmetic over text) still propagates.
    #[inline]
    fn cmp_num_soft(&self, row: &[Field], ctx: &EvalCtx) -> Result<Option<f64>, Error> {
        match self {
            // Leaves share cmp_num's coercion (including the missing-cell-is-0
            // rule); only the error-to-None softening differs.
            ValExpr::Col(_) | ValExpr::Num(_) | ValExpr::Str(_) => Ok(self.cmp_num(row, ctx).ok()),
            e => e.cmp_num_soft_compound(row, ctx),
        }
    }

    #[inline(never)]
    fn cmp_num_soft_compound(&self, row: &[Field], ctx: &EvalCtx) -> Result<Option<f64>, Error> {
        // `?` on eval: a hard error (bad arithmetic, div by zero) still aborts;
        // only the final coercion is soft.
        Ok(self.eval(row, ctx)?.coerce_num().ok())
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
            ValExpr::Col(_) | ValExpr::Num(_) | ValExpr::Str(_) => false,
            ValExpr::Bool(b) => b.is_stateful(),
            ValExpr::Neg(e) => e.is_stateful(),
            ValExpr::Arith { lhs, rhs, .. } => lhs.is_stateful() || rhs.is_stateful(),
            ValExpr::Concat(parts) | ValExpr::Func(_, parts) => {
                parts.iter().any(ValExpr::is_stateful)
            }
            ValExpr::Cond { test, then_, else_ } => {
                test.is_stateful() || then_.is_stateful() || else_.is_stateful()
            }
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
        // Domain edges follow IEEE (sqrt(-1) = NaN, log(0) = -inf) rather than
        // aborting — matching how stats and comparisons treat non-finite values.
        Func::Sqrt => Field::Num(num(&args[0])?.sqrt()),
        Func::Pow => Field::Num(num(&args[0])?.powf(num(&args[1])?)),
        Func::Exp => Field::Num(num(&args[0])?.exp()),
        Func::Log => Field::Num(num(&args[0])?.ln()),
        Func::Log10 => Field::Num(num(&args[0])?.log10()),
        Func::Log2 => Field::Num(num(&args[0])?.log2()),
        Func::Sign => {
            let v = num(&args[0])?;
            // f64::signum(0.0) is 1.0; a mathematical sign wants 0 there.
            Field::Num(if v == 0.0 { 0.0 } else { v.signum() })
        }
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
            Stmt::Select(expr) => expr.eval(row, ctx),
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

    /// Whether this statement reads cross-row state (a stateful `add` or a
    /// `select` comparing against `prev()`/`rownum()`), which routes the plan
    /// to the in-memory ordered execution path.
    pub fn is_stateful(&self) -> bool {
        match self {
            Stmt::Add(a) => a.stateful,
            Stmt::Select(e) => e.is_stateful(),
            _ => false,
        }
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
        self.keys
            .iter()
            .filter(|k| k.mode == SortMode::Numeric)
            .map(|k| k.pos)
    }

    /// Compare two rows by the sort keys. Numeric keys are assumed already
    /// converted to numbers (see [`SortStmt::numeric_positions`]).
    #[inline]
    pub fn compare(&self, a: &[Field], b: &[Field]) -> Ordering {
        for k in &self.keys {
            let ord = match k.mode {
                SortMode::Numeric => num_of(a, k.pos).total_cmp(&num_of(b, k.pos)),
                SortMode::Lexical => cell_str(a, k.pos).cmp(&cell_str(b, k.pos)),
                SortMode::Auto => match (auto_num(a, k.pos), auto_num(b, k.pos)) {
                    (Some(x), Some(y)) => x.total_cmp(&y),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => cell_str(a, k.pos).cmp(&cell_str(b, k.pos)),
                },
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

/// A cell as a number under [`SortMode::Auto`]: `None` for anything that is
/// not a number — including an empty or missing cell, which `coerce_num`
/// would read as 0 but auto treats as text (so blanks sort after numbers).
#[inline]
pub(crate) fn auto_num(row: &[Field], pos: usize) -> Option<f64> {
    row.get(pos)?.num_opt()
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
        // Colour is cosmetic and resolves against the *output* header, so a rule
        // naming a column that didn't survive to the output (e.g. dropped by a
        // later `cols`) is simply inert — skip it rather than aborting the run.
        // (Row-level colour errors are already ignored in `compute_styles`.)
        self.colors.retain_mut(|rule| rule.resolve(&header).is_ok());
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
            lhs: ValExpr::Col(ColRef {
                name: "b".into(),
                pos: 1,
            }),
            rhs: ValExpr::Num(0.0),
            mode: CmpMode::Numeric,
        };
        assert!(cmp.eval(&row(&["a", "5"]), &EvalCtx::default()).unwrap());
        assert!(!cmp.eval(&row(&["a", "0"]), &EvalCtx::default()).unwrap());

        // (== a "t") string
        let cmp = Cmp {
            op: CmpOp::Eq,
            lhs: ValExpr::Col(ColRef {
                name: "a".into(),
                pos: 0,
            }),
            rhs: ValExpr::Str("t".into()),
            mode: CmpMode::String,
        };
        assert!(cmp.eval(&row(&["t", "5"]), &EvalCtx::default()).unwrap());
        assert!(!cmp.eval(&row(&["f", "5"]), &EvalCtx::default()).unwrap());
    }

    #[test]
    fn auto_compare_is_numeric_then_lexical_per_row() {
        // (> a b) auto: both cells parse as numbers -> numeric order; a
        // non-number on either side falls back to lexical, never aborting.
        let cmp = Cmp {
            op: CmpOp::Gt,
            lhs: ValExpr::Col(ColRef {
                name: "a".into(),
                pos: 0,
            }),
            rhs: ValExpr::Col(ColRef {
                name: "b".into(),
                pos: 1,
            }),
            mode: CmpMode::Auto,
        };
        // Numeric: 100 > 9 is true (lexically "100" < "9" would be false).
        assert!(cmp.eval(&row(&["100", "9"]), &EvalCtx::default()).unwrap());
        assert!(!cmp.eval(&row(&["9", "100"]), &EvalCtx::default()).unwrap());
        // A non-numeric cell drops to lexical instead of erroring: "banana" >
        // "apple" lexically.
        assert!(
            cmp.eval(&row(&["banana", "apple"]), &EvalCtx::default())
                .unwrap()
        );
        assert!(
            !cmp.eval(&row(&["apple", "banana"]), &EvalCtx::default())
                .unwrap()
        );
    }

    #[test]
    fn and_or_short_circuit_and_correctness() {
        // (and (== a "t") (or (> b 0) (> c 0)))
        let expr = BoolExpr::And(vec![
            BoolExpr::Cmp(Cmp {
                op: CmpOp::Eq,
                lhs: ValExpr::Col(ColRef {
                    name: "a".into(),
                    pos: 0,
                }),
                rhs: ValExpr::Str("t".into()),
                mode: CmpMode::String,
            }),
            BoolExpr::Or(vec![
                BoolExpr::Cmp(Cmp {
                    op: CmpOp::Gt,
                    lhs: ValExpr::Col(ColRef {
                        name: "b".into(),
                        pos: 1,
                    }),
                    rhs: ValExpr::Num(0.0),
                    mode: CmpMode::Numeric,
                }),
                BoolExpr::Cmp(Cmp {
                    op: CmpOp::Gt,
                    lhs: ValExpr::Col(ColRef {
                        name: "c".into(),
                        pos: 2,
                    }),
                    rhs: ValExpr::Num(0.0),
                    mode: CmpMode::Numeric,
                }),
            ]),
        ]);
        assert!(
            expr.eval(&row(&["t", "0", "3"]), &EvalCtx::default())
                .unwrap()
        );
        assert!(
            expr.eval(&row(&["t", "1", "0"]), &EvalCtx::default())
                .unwrap()
        );
        assert!(
            !expr
                .eval(&row(&["t", "0", "0"]), &EvalCtx::default())
                .unwrap()
        );
        assert!(
            !expr
                .eval(&row(&["f", "9", "9"]), &EvalCtx::default())
                .unwrap()
        );
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
                    lhs: ValExpr::Col(col("a")),
                    rhs: ValExpr::Str("x".into()),
                    mode: CmpMode::String,
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
        let ValExpr::Col(cref) = &c.lhs else {
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
    fn sort_compare_auto_numbers_first_then_text() {
        let s = SortStmt {
            keys: vec![SortKey {
                name: "n".into(),
                pos: 0,
                descending: false,
                mode: SortMode::Auto,
            }],
        };
        let row = |s: &str| vec![Field::Owned(s.to_string())];
        // Two numbers: numeric (lexically "10" < "9").
        assert_eq!(s.compare(&row("10"), &row("9")), Ordering::Greater);
        // A number sorts before any text, including the empty cell.
        assert_eq!(s.compare(&row("5"), &row("abc")), Ordering::Less);
        assert_eq!(s.compare(&row("5"), &row("")), Ordering::Less);
        // Two non-numbers: lexical.
        assert_eq!(s.compare(&row(""), &row("abc")), Ordering::Less);
        assert_eq!(s.compare(&row("b"), &row("a")), Ordering::Greater);
        // A pre-converted number is still a number.
        assert_eq!(s.compare(&[Field::Num(2.0)], &row("10")), Ordering::Less);
    }

    #[test]
    fn sort_compare_numeric_reverse() {
        let s = SortStmt {
            keys: vec![SortKey {
                name: "n".into(),
                pos: 0,
                descending: true,
                mode: SortMode::Numeric,
            }],
        };
        // numeric: 10 > 9 (lexically "10" < "9", so this proves numeric mode)
        let a = vec![Field::Num(10.0)];
        let b = vec![Field::Num(9.0)];
        assert_eq!(s.compare(&a, &b), Ordering::Less); // descending: bigger first
    }
}
