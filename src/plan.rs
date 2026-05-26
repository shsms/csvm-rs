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

use crate::error::Error;
use crate::field::{Field, format_num};
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

/// Rename one or more columns (a header-only change; row data is untouched).
#[derive(Clone, Debug)]
pub struct RenameStmt {
    pub pairs: Vec<(String, String)>,
}

/// A row-by-row statement.
#[derive(Clone, Debug)]
pub enum Stmt {
    Cols(ProjectStmt),
    Select(BoolExpr),
    ToNum(ConvStmt),
    ToStr(ConvStmt),
    Rename(RenameStmt),
}

/// A pipeline stage. Transforms run streaming; a sort blocks on all its rows;
/// head keeps the first `n` rows reaching it.
#[derive(Clone, Debug)]
pub enum Stage {
    Transform(Vec<Stmt>),
    Sort(SortStmt),
    Head(usize),
}

/// How the final output is rendered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Csv,
    /// Whitespace-aligned columns, like `column -t` (set by the `fmt` command).
    Aligned,
}

/// A fully compiled pipeline.
#[derive(Clone, Debug)]
pub struct Plan {
    pub stages: Vec<Stage>,
    pub output: OutputFormat,
}

fn resolve_col(name: &str, header: &[String]) -> Result<usize, Error> {
    header
        .iter()
        .position(|h| h == name)
        .ok_or_else(|| Error::Column(name.to_owned()))
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
        }
        Ok(())
    }
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
    ) -> Result<bool, Error> {
        match self {
            Stmt::Cols(p) => {
                project(row, &p.positions, scratch);
                Ok(true)
            }
            Stmt::Select(expr) => expr.eval(row),
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
) -> Result<bool, Error> {
    for s in stmts {
        if !s.apply(row, scratch)? {
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
                Stage::Head(_) => {} // no columns to resolve
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
        };
        let err = plan.resolve(&["a".into()]).unwrap_err();
        assert!(matches!(err, Error::Column(n) if n == "nope"));
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
