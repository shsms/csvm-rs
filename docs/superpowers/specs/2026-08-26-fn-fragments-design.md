# `fn` pipeline fragments

Design for user-defined pipeline fragments in the csvm-rs pipe language.
Decided 2026-08-26.

## Motivation

Real scripts repeat the same stage patterns with only names changed. The
motivating command joined six long-format metric files and computed three
power-factor column pairs:

```text
rename value=pv_active | cols -v metric,microgrid_id
| join (rename value=pv_reactive | cols -v metric,microgrid_id) 116.reactive.csv on timestamp
| join (rename value=grid_active | cols -v metric,microgrid_id) 116.grid.active.csv on timestamp
... (three more joins) ...
| add pv_pf sqrt(pv_active*pv_active)/sqrt(pv_active*pv_active+pv_reactive*pv_reactive)
| add pf_side ((pv_active >= 0.0) == (pv_reactive >= 0.0)) ? "lag" : "lead"
... (two more pairs) ...
```

Two kinds of repetition: a per-file join-prep fragment, and a per-prefix
formula pair. One mechanism covers both: a textual macro over stages,
parameterized by tokens (file names, column names, expression operands).

Expression-level functions (`def pf(a,r) = ...` callable inside `select`)
were considered and dropped: fragments with `add` cover the actual use
cases, and one mechanism is simpler than two. This is the price accepted:
a fragment cannot be called inside an expression (see Errors for the hint).

## Grammar

A script is `PROLOGUE STAGES`. The prologue is zero or more definitions:

```text
fn NAME(PARAM[, PARAM...]) { BODY }
```

- `NAME` and each `PARAM` are bare identifiers. `NAME` must not collide
  with a built-in command or alias. Params must be distinct. Zero params
  is allowed (`fn tidy() { cols -v metric }`, called as `tidy()`).
- `BODY` is raw text, scanned to the matching `}` with the same
  quote-aware balanced scan used for paren groups. `|`, newlines, and
  `join (...)` groups inside the body need no escaping; a multi-line body
  in a `-f` file needs no continuation marks. `#` comments are stripped
  first, as everywhere.
- Definitions come before the first stage only. `hdr`'s "must be first
  command" rule is unchanged — defs are not stages.
- A call is a stage that is **exactly** `NAME(ARGS)`. Arguments are split
  on top-level commas (the join item splitter's machinery); each argument
  is raw token text pasted verbatim, so `prep('my col')` pastes the
  quotes, and a quoted path containing commas is one argument.

Example:

```text
fn prep(n) { rename value=n | cols -v metric,microgrid_id }
fn pf(t, a, r) { add t abs(a) / sqrt(a*a + r*r) }

prep(pv_active)
| join (prep(pv_reactive)) 116.reactive.csv on timestamp
| pf(pv_pf, pv_active, pv_reactive)
```

## Expansion semantics

- The fn table is built from the prologue first; then stages parse. A
  stage that is exactly `NAME(ARGS)` — in the main pipeline or inside any
  `join (...)` sub-pipeline — is instantiated and its stages spliced in
  at that position.
- **Substitution is textual with identifier boundaries**: a parameter
  name is replaced wherever it appears in the body as a whole identifier
  outside quoted literals (`value_f` in `value=value_f`, `a` in `a*a`,
  but not the `a` in `abs`). Quoted strings in the body are never
  substituted into (`"value_f"` stays literal). This is what lets one
  mechanism parameterize file operands, column names, rename halves, and
  expression operands alike.
- Arity is checked at the call.
- Fragments can call fragments: the instantiated body is parsed like any
  script text, so calls inside it expand recursively. A depth cap (64)
  turns accidental recursion into an error naming the call chain.
- Expansion is purely front-end. By the time a `Plan` exists there are no
  fragments: the compiled-plan hot-path rule, `--print-engine`, and every
  executor path are untouched. A stateful `add` inside a fragment routes
  to the ordered path exactly as if written by hand.

## Errors

Definition-time (each names the offending fn):

- collision with a built-in command or alias (checked against
  `parse::COMMANDS`, the list the help registry cross-checks);
- duplicate fn name; duplicate parameter;
- unterminated body (missing `}`); malformed header;
- a `fn` after the first stage ("fn definitions must come before the
  first stage").

Call-time:

- arity mismatch ("`prep` expects 1 argument, got 3");
- unknown name in stage position gets the existing did-you-mean
  treatment, against fn names and commands together;
- recursion past the depth cap shows the call chain;
- a fragment name used mid-expression (`add x pf(a,b)`) fails in the
  expression parser as an unknown function, with one extra hint line when
  the name matches a defined fn: "`pf` is a fragment — fragments expand
  only as whole stages".

Inside an expanded body, any stage or expression error is prefixed with
its origin ("in fn `prep`: ..."), so a message pointing at generated text
is traceable to the definition. Runtime errors need no prefix — they name
columns, which after substitution are the real ones.

## Implementation shape

All in `parse.rs` except docs and help:

- **Prologue pass**: `parse_prologue` peels leading defs into an
  `FnTable` (name → params + body text) using a new `take_brace_group`,
  a sibling of `take_paren_group` with the same quote-aware balanced
  scan. The remainder is the script proper.
- **Threading**: the public `parse(script)` signature is unchanged;
  internally the parser carries `&FnTable` plus an expansion depth, and
  `parse_join`'s sub-pipeline recursion passes both through (that is what
  makes calls inside `join (...)` work).
- **Expansion hook**: in the stage loop, a stage matching `ident(...)` in
  its entirety with `ident` in the table is instantiated (`subst_params`
  does the word-boundary, quote-aware replacement) and the resulting text
  is parsed as stages at that spot (same table, depth+1), errors wrapped
  with the "in fn `X`:" prefix.
- **Expression hint**: the expression parser's unknown-function error
  path receives the fn names for the whole-stages hint.
- **Help/docs**: a `fn` entry in the help registry (the cross-check test
  forces it), README example, CLAUDE.md section, todo.org cleanup.

## Testing

TDD throughout. Parse-level: def + call splice; call inside `join (...)`;
fn calling fn; arity, duplicate, collision, unterminated-body, and
late-`fn` errors; recursion cap; quoted-literal non-substitution; the
mid-expression hint. End-to-end in `tests/`: the six-file power-factor
pipeline written with fragments, asserting the widened output.

## Out of scope

- Expression-level functions callable inside `select`/`add` expressions
  (dropped — see Motivation).
- The join naming sugar (`as NAME` / `--auto` stem prefix) stays a
  separate todo.org item; fragments reduce its urgency but do not replace
  it.
