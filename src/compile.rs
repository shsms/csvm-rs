//! Compiling the Lisp command script into a [`Plan`].
//!
//! The pipeline verbs (`cols`, `drop-cols`, `select`, `sort-by`, `to-num`,
//! `to-str`) are registered with [`TulispContext::defspecial`], so they receive
//! their **raw, unevaluated** argument forms. A column name therefore stays a
//! symbol — it is read structurally, never looked up as a variable — and the
//! verbs only ever *append to a plan*. tulisp runs this once; the resulting
//! [`Plan`] is plain Rust data with no interpreter attached.
//!
//! Column types (`Str`/`Num`) are tracked as the script is walked so that
//! `select` and `sort` can be numeric *implicitly*: a comparison against a
//! number, or against a column that an earlier `to-num` marked numeric, becomes
//! a numeric comparison.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use tulisp::{Error as LispError, TulispContext, TulispObject};

use crate::error::Error;
use crate::plan::{
    BoolExpr, Cmp, CmpOp, ColRef, ConvStmt, Operand, Plan, ProjectStmt, SortKey, SortStmt, Stage,
    Stmt,
};

/// Compile a script string into an executable [`Plan`].
pub fn compile(script: &str) -> Result<Plan, Error> {
    let builder = Rc::new(RefCell::new(Builder::default()));
    let mut ctx = TulispContext::new();
    register_verbs(&mut ctx, &builder);

    if let Err(e) = ctx.eval_string(script) {
        return Err(Error::Compile(e.format(&ctx)));
    }
    let plan = builder.borrow_mut().take_plan();
    Ok(plan)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ColType {
    Str,
    Num,
}

/// A compiled item before it is grouped into stages.
enum Item {
    Stmt(Stmt),
    Sort(SortStmt),
}

#[derive(Default)]
struct Builder {
    items: Vec<Item>,
    col_types: HashMap<String, ColType>,
}

impl Builder {
    fn is_num(&self, name: &str) -> bool {
        self.col_types.get(name) == Some(&ColType::Num)
    }

    /// Group the flat item list into stages: runs of statements become a
    /// `Transform`, and each `sort` is isolated into its own stage (mirrors
    /// csvm's block separation).
    fn take_plan(&mut self) -> Plan {
        let mut stages = Vec::new();
        let mut transform: Vec<Stmt> = Vec::new();
        for item in self.items.drain(..) {
            match item {
                Item::Stmt(s) => transform.push(s),
                Item::Sort(s) => {
                    if !transform.is_empty() {
                        stages.push(Stage::Transform(std::mem::take(&mut transform)));
                    }
                    stages.push(Stage::Sort(s));
                }
            }
        }
        if !transform.is_empty() {
            stages.push(Stage::Transform(transform));
        }
        Plan { stages }
    }
}

/// Register every pipeline verb. Each closure captures a handle to the shared
/// builder and appends one compiled item.
fn register_verbs(ctx: &mut TulispContext, builder: &Rc<RefCell<Builder>>) {
    macro_rules! verb {
        ($name:literal, $b:ident, $args:ident, $body:block) => {{
            let handle = Rc::clone(builder);
            ctx.defspecial($name, move |_ctx, $args| {
                let mut guard = handle.borrow_mut();
                let $b = &mut *guard;
                $body
                Ok(TulispObject::nil())
            });
        }};
    }

    verb!("cols", b, args, {
        b.items.push(Item::Stmt(Stmt::Cols(ProjectStmt {
            exclude: false,
            names: col_names(args, "cols")?,
            positions: Vec::new(),
        })));
    });
    verb!("drop-cols", b, args, {
        b.items.push(Item::Stmt(Stmt::Cols(ProjectStmt {
            exclude: true,
            names: col_names(args, "drop-cols")?,
            positions: Vec::new(),
        })));
    });
    verb!("select", b, args, {
        let form = single_arg(args, "select")?;
        let expr = compile_bool(&form, b)?;
        b.items.push(Item::Stmt(Stmt::Select(expr)));
    });
    // Named `sort-by`, not `sort`: tulisp's prelude already defines `sort`
    // (the compiler resolves it by symbol address before our global override
    // would apply), so reusing the name would silently dispatch to the prelude.
    verb!("sort-by", b, args, {
        let keys = sort_keys(args, b)?;
        b.items.push(Item::Sort(SortStmt { keys }));
    });

    for (name, to_num) in [
        ("to-num", true),
        ("to_num", true),
        ("to-str", false),
        ("to_str", false),
    ] {
        let handle = Rc::clone(builder);
        ctx.defspecial(name, move |_ctx, args| {
            let mut guard = handle.borrow_mut();
            let b = &mut *guard;
            let names = col_names(args, name)?;
            for n in &names {
                b.col_types
                    .insert(n.clone(), if to_num { ColType::Num } else { ColType::Str });
            }
            let conv = ConvStmt {
                names,
                positions: Vec::new(),
            };
            b.items.push(Item::Stmt(if to_num {
                Stmt::ToNum(conv)
            } else {
                Stmt::ToStr(conv)
            }));
            Ok(TulispObject::nil())
        });
    }
}

fn err<T>(msg: impl Into<String>) -> Result<T, LispError> {
    Err(LispError::invalid_argument(msg.into()))
}

/// Read a column name: a bare symbol or a string literal.
fn col_name(form: &TulispObject) -> Result<String, LispError> {
    if form.symbolp() {
        form.as_symbol()
    } else if form.stringp() {
        form.as_string()
    } else {
        err(format!("expected a column name, got: {form}"))
    }
}

/// Read a non-empty list of column names from a verb's arguments.
fn col_names(args: &TulispObject, verb: &str) -> Result<Vec<String>, LispError> {
    let names: Vec<String> = args
        .base_iter()
        .map(|f| col_name(&f))
        .collect::<Result<_, _>>()?;
    if names.is_empty() {
        return err(format!("{verb} expects at least one column"));
    }
    Ok(names)
}

/// Pull exactly one argument form out of a verb's argument list.
fn single_arg(args: &TulispObject, verb: &str) -> Result<TulispObject, LispError> {
    let mut it = args.base_iter();
    let Some(first) = it.next() else {
        return err(format!("{verb} expects one argument"));
    };
    if it.next().is_some() {
        return err(format!("{verb} expects exactly one argument"));
    }
    Ok(first)
}

/// Compile a boolean form: `(and ...)`, `(or ...)`, `(not e)`, a comparison, or
/// a regex match.
fn compile_bool(form: &TulispObject, b: &Builder) -> Result<BoolExpr, LispError> {
    if !form.consp() {
        return err(format!(
            "select expects a comparison or logical form, got: {form}"
        ));
    }
    let head = form.car()?;
    if !head.symbolp() {
        return err(format!("expected an operator, got: {head}"));
    }
    let op = head.as_symbol()?;
    let rest: Vec<TulispObject> = form.cdr()?.base_iter().collect();

    match op.as_str() {
        "and" | "&&" => Ok(BoolExpr::And(compile_bool_list(&rest, b)?)),
        "or" | "||" => Ok(BoolExpr::Or(compile_bool_list(&rest, b)?)),
        "not" | "!" => {
            if rest.len() != 1 {
                return err("not expects exactly one argument");
            }
            Ok(BoolExpr::Not(Box::new(compile_bool(&rest[0], b)?)))
        }
        "==" | "=" => compile_cmp(CmpOp::Eq, &rest, b),
        "!=" | "/=" => compile_cmp(CmpOp::Ne, &rest, b),
        "<" => compile_cmp(CmpOp::Lt, &rest, b),
        ">" => compile_cmp(CmpOp::Gt, &rest, b),
        "<=" => compile_cmp(CmpOp::Le, &rest, b),
        ">=" => compile_cmp(CmpOp::Ge, &rest, b),
        "=~" => compile_match(&rest, false),
        "!~" => compile_match(&rest, true),
        other => err(format!("unknown operator in select: {other}")),
    }
}

fn compile_bool_list(forms: &[TulispObject], b: &Builder) -> Result<Vec<BoolExpr>, LispError> {
    forms.iter().map(|f| compile_bool(f, b)).collect()
}

fn compile_cmp(op: CmpOp, rest: &[TulispObject], b: &Builder) -> Result<BoolExpr, LispError> {
    if rest.len() != 2 {
        return err("comparison expects exactly two operands");
    }
    let mut lhs = compile_operand(&rest[0])?;
    let mut rhs = compile_operand(&rest[1])?;
    let numeric = is_numeric(&lhs, b) || is_numeric(&rhs, b);
    if numeric {
        lhs = numericize(lhs)?;
        rhs = numericize(rhs)?;
    }
    Ok(BoolExpr::Cmp(Cmp {
        op,
        lhs,
        rhs,
        numeric,
    }))
}

fn compile_match(rest: &[TulispObject], negate: bool) -> Result<BoolExpr, LispError> {
    if rest.len() != 2 {
        return err("=~ expects a column and a pattern");
    }
    let col = ColRef::new(col_name(&rest[0])?);
    if !rest[1].stringp() {
        return err(format!("=~ pattern must be a string, got: {}", rest[1]));
    }
    let pattern = rest[1].as_string()?;
    let regex = regex::Regex::new(&pattern)
        .map_err(|e| LispError::invalid_argument(format!("invalid regex '{pattern}': {e}")))?;
    Ok(BoolExpr::Match { col, regex, negate })
}

/// Compile a comparison operand: a symbol is a column, a string/number is a
/// literal.
fn compile_operand(form: &TulispObject) -> Result<Operand, LispError> {
    if form.symbolp() {
        Ok(Operand::Col(ColRef::new(form.as_symbol()?)))
    } else if form.stringp() {
        Ok(Operand::Str(form.as_string()?))
    } else if form.numberp() {
        Ok(Operand::Num(form.try_float()?))
    } else {
        err(format!(
            "operand must be a column, string, or number, got: {form}"
        ))
    }
}

fn is_numeric(op: &Operand, b: &Builder) -> bool {
    match op {
        Operand::Num(_) => true,
        Operand::Col(c) => b.is_num(&c.name),
        Operand::Str(_) => false,
    }
}

/// In a numeric comparison, a string literal is parsed at compile time.
fn numericize(op: Operand) -> Result<Operand, LispError> {
    match op {
        Operand::Str(s) => match s.trim().parse::<f64>() {
            Ok(n) => Ok(Operand::Num(n)),
            Err(_) => err(format!("non-numeric literal '{s}' in numeric comparison")),
        },
        other => Ok(other),
    }
}

/// Parse `sort` specs: a bare column, or `(column :reverse :numeric ...)`.
fn sort_keys(args: &TulispObject, b: &Builder) -> Result<Vec<SortKey>, LispError> {
    let mut keys = Vec::new();
    for spec in args.base_iter() {
        let (name, mods) = if spec.consp() {
            (col_name(&spec.car()?)?, spec.cdr()?.base_iter().collect())
        } else {
            (col_name(&spec)?, Vec::new())
        };
        let mut key = SortKey {
            numeric: b.is_num(&name),
            name,
            pos: 0,
            descending: false,
        };
        for m in mods.iter() {
            let raw = m.as_symbol()?;
            match raw.strip_prefix(':').unwrap_or(&raw) {
                "reverse" | "r" | "desc" | "descending" => key.descending = true,
                "numeric" | "n" | "num" => key.numeric = true,
                _ => return err(format!("unknown sort modifier: {m}")),
            }
        }
        keys.push(key);
    }
    if keys.is_empty() {
        return err("sort expects at least one column");
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Stage;

    #[test]
    fn compiles_cols_and_keeps_symbols_literal() {
        // `id` etc. are unbound symbols; raw-args compilation must not try to
        // evaluate them as variables.
        let plan = compile("(cols id fieldA countZ)").unwrap();
        assert_eq!(plan.stages.len(), 1);
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Cols(p) = &stmts[0] else { panic!() };
        assert!(!p.exclude);
        assert_eq!(p.names, vec!["id", "fieldA", "countZ"]);
    }

    #[test]
    fn select_numeric_vs_string_modes() {
        // `(== a "t")` is a string compare; `(> b 0)` is numeric.
        let plan = compile(r#"(select (and (== a "t") (> b 0)))"#).unwrap();
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
    fn to_num_makes_later_compare_numeric() {
        // After `to-num`, comparing the column against a *string* literal is
        // still numeric, and the literal is parsed at compile time.
        let plan = compile(r#"(to-num c) (select (== c "5"))"#).unwrap();
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
    fn sort_splits_stages_like_csvm() {
        // to_num + select, then sort, then to_str -> three stages.
        let plan = compile("(to-num c) (select (> c 0)) (sort-by (c :r)) (to-str c)").unwrap();
        assert_eq!(plan.stages.len(), 3);
        assert!(matches!(plan.stages[0], Stage::Transform(_)));
        let Stage::Sort(s) = &plan.stages[1] else {
            panic!()
        };
        assert!(s.keys[0].descending);
        assert!(s.keys[0].numeric); // from the earlier to-num
        assert!(matches!(plan.stages[2], Stage::Transform(_)));
    }

    #[test]
    fn drop_cols_and_string_names() {
        let plan = compile(r#"(drop-cols "weird name" fieldA)"#).unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        let Stmt::Cols(p) = &stmts[0] else { panic!() };
        assert!(p.exclude);
        assert_eq!(p.names, vec!["weird name", "fieldA"]);
    }

    #[test]
    fn unknown_operator_is_a_compile_error() {
        let e = compile("(select (xor a b))").unwrap_err();
        assert!(matches!(e, Error::Compile(m) if m.contains("unknown operator")));
    }

    #[test]
    fn regex_match_compiles() {
        let plan = compile(r#"(select (=~ name "^a.*z$"))"#).unwrap();
        let Stage::Transform(stmts) = &plan.stages[0] else {
            panic!()
        };
        assert!(matches!(
            &stmts[0],
            Stmt::Select(BoolExpr::Match { negate: false, .. })
        ));
    }
}
