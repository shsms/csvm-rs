# `fn` Pipeline Fragments Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** User-defined pipeline fragments — `fn NAME(PARAMS) { BODY }` in a script prologue, expanded when a stage is exactly `NAME(ARGS)`.

**Architecture:** Purely front-end textual macros in `parse.rs`: a prologue pass builds an `FnTable` before stage splitting; a stage matching `ident(...)` expands by word-boundary parameter substitution and parses the result into the same `Builder`. The compiled `Plan` and executor are untouched.

**Tech Stack:** Rust; no new dependencies. Everything lives in `src/parse.rs` except help/docs.

**Spec:** `docs/superpowers/specs/2026-08-26-fn-fragments-design.md`

## Global Constraints

- Repo conventions (CLAUDE.md): commits have an imperative subject, **no prefix tag** (no `feat:`), no AI co-author trailer, and a `Signed-off-by: Sahas Subramanian <sahas.subramanian@proton.me>` trailer. Commit with:
  `git -c user.name="Sahas Subramanian" -c user.email="sahas.subramanian@proton.me" commit --signoff -m "..."`
- Stage files explicitly by name; never `git add -A`/`-u`/`.`; never add `.nfs*` files.
- `cargo fmt` and `cargo clippy --all-targets -- -D warnings` must be clean before every commit.
- The parse-time-only rule: nothing may interpret fragments while rows flow. All expansion happens before `Plan` construction.
- Work on branch `fn-fragments` (already created; the spec is committed on it).

---

### Task 1: Brace scanner and prologue parser

**Files:**
- Modify: `src/parse.rs` (helpers near `take_paren_group` at ~line 940, and new items near `COMMANDS` at line 60; tests in the existing `mod tests` at the end of the file)

**Interfaces:**
- Produces: `struct FnDef { params: Vec<String>, body: String }`, `type FnTable = HashMap<String, FnDef>`, `fn parse_prologue(script: &str) -> Result<(FnTable, &str), Error>`, `fn take_brace_group(s: &str) -> Result<(&str, &str), Error>`, `fn is_ident(s: &str) -> bool`, `const RESERVED: &[&str]`. Task 3 consumes `parse_prologue` and `FnTable`; Task 2 consumes nothing from here.

- [ ] **Step 1: Write the failing tests** (append inside `mod tests` in `src/parse.rs`):

```rust
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
        assert!(m("fn cols(a) { head }").contains("collides"), "{}", m("fn cols(a) { head }"));
        assert!(m("fn f(a) { head }\nfn f(b) { tail }").contains("defined twice"));
        assert!(m("fn f(a, a) { head }").contains("duplicate parameter"));
        assert!(m("fn f(a) { head").contains("missing `}`"));
        assert!(m("fn f a { head }").contains("malformed"));
        assert!(m("fn 9x(a) { head }").contains("not a valid name"));
        assert!(m("fn f(a-b) { head }").contains("not a valid parameter"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib prologue 2>&1 | tail -20`
Expected: compile error — `parse_prologue` not found. (A compile error in the test crate is the failing state here; the functions don't exist yet.)

- [ ] **Step 3: Implement.** Add near `COMMANDS` (line ~60):

```rust
/// Names a `fn` may not take: every command, every alias, and `fn` itself.
const RESERVED: &[&str] = &[
    "cols", "cut", "select", "where", "filter", "sort", "to-num", "to_num", "to-str", "to_str",
    "head", "tail", "stats", "uniq", "dedup", "color", "colour", "rename", "fmt", "hdr", "join",
    "add", "delta", "group", "agg", "graph", "plot", "fn",
];

/// A user-defined pipeline fragment: parameter names plus the raw body text
/// between its braces (comment-stripped, substituted at each call).
struct FnDef {
    params: Vec<String>,
    body: String,
}

type FnTable = std::collections::HashMap<String, FnDef>;
```

Add next to `take_paren_group` (line ~940):

```rust
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
            return Err(err(format!("fn: `{name}` is not a valid name (bare identifier)")));
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
                return Err(err(format!("fn `{name}`: `{p}` is not a valid parameter name")));
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
```

Note: `FnDef`/`FnTable`/`parse_prologue`/`RESERVED` will be dead code until Task 3 wires them into `parse()`. Silence the lint for this intermediate commit with `#[allow(dead_code)]` on `FnDef`, `parse_prologue`, `take_brace_group`, `is_ident`, and `#[allow(dead_code)]` (attribute on the const) for `RESERVED`; Task 3 removes every one of these allows.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib prologue 2>&1 | tail -5`
Expected: `2 passed`. Then `cargo fmt && cargo clippy --all-targets -- -D warnings` clean, and `cargo test --lib 2>&1 | tail -3` all green.

- [ ] **Step 5: Commit**

```bash
git add src/parse.rs
git -c user.name="Sahas Subramanian" -c user.email="sahas.subramanian@proton.me" \
  commit --signoff -m "Add fn-definition prologue parser

Scans leading fn NAME(PARAMS) { BODY } definitions into an FnTable
before stage splitting sees the script. Bodies are raw text delimited
by a quote-aware balanced brace scan (take_brace_group, a sibling of
take_paren_group), so | and newlines inside them need no escaping.
Used in the following commits."
```

---

### Task 2: Parameter substitution

**Files:**
- Modify: `src/parse.rs` (helper near `split_list`; tests in `mod tests`)

**Interfaces:**
- Produces: `fn subst_params(body: &str, params: &[String], args: &[&str]) -> String`. Task 3 consumes it.

- [ ] **Step 1: Write the failing test** (append inside `mod tests`):

```rust
    #[test]
    fn subst_params_is_identifier_bounded_and_quote_aware() {
        let params = vec!["a".to_string(), "value_f".to_string()];
        let args = ["pv_active", "grid_q"];
        // Whole identifiers substitute; substrings (`abs`, `a1`) and quoted
        // literals do not; params inside `old=new` tokens do.
        assert_eq!(
            subst_params("abs(a) + a*a1 ++ 'a' | rename value=value_f", &params, &args),
            "abs(pv_active) + pv_active*a1 ++ 'a' | rename value=grid_q"
        );
        // A digit-led run is one token: `9a` does not substitute its tail.
        assert_eq!(subst_params("add x 9a", &params, &args), "add x 9a");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib subst_params 2>&1 | tail -5`
Expected: compile error — `subst_params` not found.

- [ ] **Step 3: Implement.** Add near `split_list`:

```rust
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
```

(`is_ident` comes from Task 1. Add `#[allow(dead_code)]` until Task 3 uses it.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib subst_params 2>&1 | tail -5` → PASS; `cargo fmt && cargo clippy --all-targets -- -D warnings` clean; full `cargo test --lib` green.

- [ ] **Step 5: Commit**

```bash
git add src/parse.rs
git -c user.name="Sahas Subramanian" -c user.email="sahas.subramanian@proton.me" \
  commit --signoff -m "Add word-boundary parameter substitution for fragments

Replaces a parameter only where it appears as a whole identifier
outside quoted literals, so value_f in value=value_f substitutes while
the a in abs and a quoted 'a' do not. Used in the following commit."
```

---

### Task 3: Wire expansion into the parser

**Files:**
- Modify: `src/parse.rs` — `parse()` (line ~31), `struct Builder` + `impl Builder` (line ~84), `parse_stage` (line ~159), `parse_join`'s sub-pipeline recursion (the `Box::new(parse(inner)?)` call, line ~290); tests in `mod tests`

**Interfaces:**
- Consumes: `FnTable`, `FnDef`, `parse_prologue` (Task 1), `subst_params` (Task 2), plus existing `split_stages`, `split_top_commas`, `take_paren_group`.
- Produces: `fn parse_stages(script: &str, fns: &FnTable, depth: usize) -> Result<Plan, Error>` (internal), `fn fragment_call(stage: &str) -> Option<(&str, &str)>`, `Builder::expand_fragment`, `const MAX_FN_DEPTH: usize = 64`. `Builder` gains a lifetime: `Builder<'a> { fns: &'a FnTable, depth: usize, ... }` with `Builder::new(fns, depth)` replacing `Builder::default()`. Public `parse(&str)` signature unchanged.

- [ ] **Step 1: Write the failing tests** (append inside `mod tests`):

```rust
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
        let plan =
            parse("fn prep(n) { rename value=n }\njoin (prep(x)) r.csv on k").unwrap();
        let [Stage::Join(j)] = plan.stages.as_slice() else {
            panic!("expected a join stage, got {:?}", plan.stages);
        };
        assert_eq!(j.right_plan.stages.len(), 1);
    }

    #[test]
    fn fn_fragment_calls_fragment() {
        let plan =
            parse("fn a(x) { rename v=x }\nfn b(y) { a(y) | cols -v m }\nb(q)").unwrap();
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
        assert!(e.contains("in fn `f`") && e.contains("unknown command"), "{e}");
        assert!(m("head\nfn f(a) { tail }").contains("before the first stage"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib fn_ 2>&1 | tail -20`
Expected: FAIL — `fn prep(n) ...` currently parses as unknown command `fn`, so every test errors differently than asserted (the first three `unwrap` panics, `fn_call_errors` gets the wrong messages).

- [ ] **Step 3: Implement.**

3a. Split `parse()`:

```rust
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
```

3b. `Builder` gains the table and depth (drop `#[derive(Default)]`, keep every other field as-is):

```rust
struct Builder<'a> {
    /// Fragment definitions from the script prologue (shared, read-only).
    fns: &'a FnTable,
    /// Current fragment-expansion depth; `MAX_FN_DEPTH` stops recursion.
    depth: usize,
    items: Vec<Item>,
    col_types: HashMap<String, ColType>,
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
            col_types: HashMap::new(),
            output: OutputFormat::Csv,
            header: None,
            colors: Vec::new(),
            graph: None,
        }
    }
```

(The `impl Builder` block header becomes `impl<'a> Builder<'a>`; method bodies are unchanged.)

3c. In `parse_join`, thread the table into the sub-pipeline (replace `Box::new(parse(inner)?)`):

```rust
                Box::new(parse_stages(inner, self.fns, self.depth)?)
```

3d. Call detection and expansion. Add near `split_first_word`:

```rust
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
```

Add `const MAX_FN_DEPTH: usize = 64;` next to `RESERVED`. Then, at the top of `parse_stage` (after the existing `graph` guard), insert:

```rust
        // A stage that is exactly `NAME(ARGS)` is a fragment call.
        if let Some((name, args)) = fragment_call(stage) {
            let fns = self.fns;
            return match fns.get(name) {
                Some(def) => self.expand_fragment(name, def, args),
                None => {
                    let mut cands: Vec<String> = fns.keys().cloned().collect();
                    cands.extend(COMMANDS.iter().map(|s| s.to_string()));
                    Err(err(match crate::error::did_you_mean(name, &cands) {
                        Some(s) => format!("unknown fragment: {name} (did you mean `{s}`?)"),
                        None => format!("unknown fragment: {name}"),
                    }))
                }
            };
        }
```

and a `"fn"` arm in the command match (anywhere before the `other` arm):

```rust
            "fn" => Err(err("fn definitions must come before the first stage")),
```

Add the expansion method to `impl<'a> Builder<'a>`:

```rust
    /// Instantiate fragment `name` and splice its stages in at this position:
    /// substitute the arguments into the body, then parse the result into
    /// this builder one level deeper (so runaway recursion errors out).
    fn expand_fragment(&mut self, name: &str, def: &'a FnDef, args_text: &str) -> Result<(), Error> {
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
```

3e. Remove every `#[allow(dead_code)]` added in Tasks 1–2.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib 2>&1 | tail -3` — all green (the four new tests plus every existing one; the `Builder` lifetime change must not disturb anything). Then `cargo fmt && cargo clippy --all-targets -- -D warnings` clean and `cargo test 2>&1 | grep -c "test result: ok"` → 8.

- [ ] **Step 5: Commit**

```bash
git add src/parse.rs
git -c user.name="Sahas Subramanian" -c user.email="sahas.subramanian@proton.me" \
  commit --signoff -m "Expand fragment calls into pipeline stages

A stage that is exactly NAME(ARGS) instantiates the named fragment:
arguments substitute into the body by identifier, and the resulting
stages parse into the same builder, so a fragment behaves exactly like
its body written in place — including inside join sub-pipelines, which
now thread the fn table through their recursion. A depth cap turns
accidental recursion into an error carrying the in-fn call chain."
```

---

### Task 4: Expression-position hint

**Files:**
- Modify: `src/parse.rs` — `parse_stage`'s dispatch return, one new method; tests in `mod tests`

**Interfaces:**
- Consumes: `Builder::fns` (Task 3). Produces: `Builder::hint_fragment(e: Error) -> Error`, applied to every stage-dispatch error.

- [ ] **Step 1: Write the failing test** (append inside `mod tests`):

```rust
    #[test]
    fn fn_used_in_expression_gets_a_hint() {
        let e = parse("fn pf(a) { head }\nadd x pf(a)").unwrap_err().to_string();
        assert!(e.contains("whole stages"), "{e}");
        // No fragment of that name: the plain unknown-function error stands.
        let e = parse("add x bogus(a)").unwrap_err().to_string();
        assert!(!e.contains("whole stages"), "{e}");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib fn_used_in_expression 2>&1 | tail -5`
Expected: FAIL — the first assertion (message is plain `unknown function: pf`).

- [ ] **Step 3: Implement.** In `parse_stage`, capture the dispatch result and wrap its error. The existing `match cmd { ... }` is the tail expression; change it to:

```rust
        let (cmd, rest) = split_first_word(stage);
        let result = match cmd {
            // ... every existing arm, unchanged ...
        };
        result.map_err(|e| self.hint_fragment(e))
```

and add to `impl<'a> Builder<'a>`:

```rust
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
```

(`Error` is already imported in `parse.rs`; the `unknown function: ` prefix matches the message built in the expression parser at line ~1829.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib 2>&1 | tail -3` all green; `cargo fmt && cargo clippy --all-targets -- -D warnings` clean.

- [ ] **Step 5: Commit**

```bash
git add src/parse.rs
git -c user.name="Sahas Subramanian" -c user.email="sahas.subramanian@proton.me" \
  commit --signoff -m "Hint when a fragment is called inside an expression

Fragments expand only as whole stages; a call in expression position
fails as an unknown function, which now names the fragment and the
whole-stage rule instead of leaving the user to guess."
```

---

### Task 5: Help entry and docs

**Files:**
- Modify: `src/parse.rs:60` (`COMMANDS`), `src/help.rs` (new `CmdHelp` entry), `README.md` (command table + example), `CLAUDE.md` (command-language section)

**Interfaces:**
- Consumes: the shipped grammar from Tasks 1–4. Produces: nothing for later tasks.

- [ ] **Step 1: Make the drift test fail.** Add `"fn"` to `COMMANDS` in `src/parse.rs`:

```rust
pub(crate) const COMMANDS: &[&str] = &[
    "cols", "cut", "select", "where", "filter", "sort", "to-num", "to-str", "head", "tail",
    "stats", "uniq", "color", "rename", "fmt", "hdr", "join", "add", "delta", "group", "agg",
    "graph", "fn",
];
```

- [ ] **Step 2: Run the help drift test to verify it fails**

Run: `cargo test --lib help 2>&1 | tail -10`
Expected: the registry cross-check test FAILS — `fn` has no help entry.

- [ ] **Step 3: Add the help entry.** In `src/help.rs`, insert a `CmdHelp` in alphabetical/registry order beside the others:

```rust
    CmdHelp {
        name: "fn",
        aliases: &[],
        summary: "define a reusable pipeline fragment",
        synopsis: &[
            "fn NAME(PARAM[, PARAM...]) { STAGES }   (definitions come before the first stage)",
            "  call: NAME(ARG[, ARG...]), as a stage of its own",
        ],
        detail: "A fragment is a reusable run of stages. A call is a stage that is exactly \
NAME(args); the arguments substitute into the body by name (whole identifiers, outside \
quotes), so column names, file names, and expression operands all parameterize. Fragments \
may call other fragments. A fragment cannot be called inside an expression — compute into \
a column with add first.",
        examples: &[
            "csvm 'fn prep(n) { rename value=n | cols -v metric }\nprep(pv) | join (prep(q)) r.csv on ts' data.csv",
        ],
    },
```

Adjust to the registry's actual field set if it differs (open the file; the `join` entry at line ~299 is the template). If the drift test also checks the overview listing or alias table, follow whatever it demands — it is the source of truth.

- [ ] **Step 4: Docs.** In `README.md`, add a table row after the `join` row and an example:

```markdown
| `fn name(a,b) { … }` | define a reusable pipeline fragment; call as `name(x,y)` |
```

```text
# a fragment factors repeated stages; called like name(args)
csvm 'fn prep(n) { rename value=n | cols -v metric }
prep(pv) | join (prep(grid)) grid.csv on timestamp' pv.csv
```

In `CLAUDE.md`, add a bullet to the command-language list (after the `join` bullet):

```markdown
- **`fn NAME(PARAM, …) { STAGES }`** defines a pipeline fragment in the script
  prologue (before the first stage); a stage that is exactly `NAME(ARGS)`
  expands it in place. Purely front-end textual macros (`parse_prologue` /
  `FnTable` / `subst_params` / `Builder::expand_fragment` in `parse.rs`):
  arguments substitute by whole identifier outside quoted literals, fragments
  may call fragments (`MAX_FN_DEPTH` caps recursion), and calls work inside
  `join (…)` sub-pipelines because the fn table threads through the sub-parse.
  By `Plan` time no fragments remain, so the hot path is untouched. A fragment
  name used inside an expression gets a whole-stages hint. Design:
  `docs/superpowers/specs/2026-08-26-fn-fragments-design.md`.
```

- [ ] **Step 5: Run tests to verify everything passes**

Run: `cargo test 2>&1 | grep -c "test result: ok"` → 8; `cargo fmt && cargo clippy --all-targets -- -D warnings` clean.

- [ ] **Step 6: Commit**

```bash
git add src/parse.rs src/help.rs README.md CLAUDE.md
git -c user.name="Sahas Subramanian" -c user.email="sahas.subramanian@proton.me" \
  commit --signoff -m "Document fn fragments in help and docs

fn joins the command registry so csvm help fn works and the drift test
pins the entry; README and CLAUDE.md describe the grammar and the
parse-time expansion model."
```

---

### Task 6: End-to-end test — the motivating pipeline

**Files:**
- Create: `tests/fragments.rs`

**Interfaces:**
- Consumes: the public `csvm::parse::parse` / `csvm::exec` API, same harness shape as `tests/join.rs:13-43`.

- [ ] **Step 1: Write the test**

```rust
//! End-to-end test for `fn` pipeline fragments: the motivating power-factor
//! pipeline from the design spec, written with fragments.

use csvm::exec::{self, RunOpts};
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_csv(content: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "csvm_fragments_{}_{}.csv",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, content).unwrap();
    path
}

fn run(script: &str, input: &str) -> Result<String, String> {
    let mut plan = csvm::parse::parse(script).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(input.as_bytes());
    let header = match plan.input_header.as_deref() {
        Some(h) => h.to_vec(),
        None => exec::read_header(&mut reader).map_err(|e| e.to_string())?,
    };
    exec::prepare_joins(&mut plan).map_err(|e| e.to_string())?;
    let out_header = plan.resolve(&header).map_err(|e| e.to_string())?;
    let opts = RunOpts {
        chunk_size: 64,
        threads: 1,
        temp_dir: std::env::temp_dir(),
        sort_buffer: 1 << 20,
    };
    let mut out = Vec::new();
    exec::run(&plan, &out_header, &opts, &mut reader, &mut out).map_err(|e| e.to_string())?;
    String::from_utf8(out).map_err(|e| e.to_string())
}

#[test]
fn power_factor_pipeline_with_fragments() {
    let reactive = temp_csv("timestamp,metric,value\n1,q,3\n2,q,4\n");
    let script = format!(
        "fn prep(n) {{ rename value=n | cols -v metric }}\n\
         fn pf(t, a, r) {{ add t abs(a) / sqrt(a*a + r*r) }}\n\
         prep(active)\n\
         join (prep(reactive)) {} on timestamp\n\
         pf(pf_col, active, reactive)",
        reactive.display()
    );
    let out = run(&script, "timestamp,metric,value\n1,p,4\n2,p,3\n").unwrap();
    assert_eq!(
        out,
        "timestamp,active,reactive,pf_col\n\
         1,4,3,0.8\n\
         2,3,4,0.6\n"
    );
}
```

(Math check: `abs(4)/sqrt(16+9) = 4/5 = 0.8`; `3/5 = 0.6`.)

- [ ] **Step 2: Run it — it should already pass** (this task pins integration, the features shipped in Tasks 1–5):

Run: `cargo test --test fragments 2>&1 | tail -5`
Expected: PASS. If it fails, the failure is a real integration bug — fix it in `src/parse.rs` (with a narrower parse-level test reproducing it first), not by bending this test.

- [ ] **Step 3: Full suite + lints**

Run: `cargo test 2>&1 | grep -c "test result: ok"` → 9 (the new binary joins the count); `cargo fmt && cargo clippy --all-targets -- -D warnings` clean.

- [ ] **Step 4: Commit**

```bash
git add tests/fragments.rs
git -c user.name="Sahas Subramanian" -c user.email="sahas.subramanian@proton.me" \
  commit --signoff -m "Add end-to-end test for fn fragments

Pins the motivating pipeline: a join-prep fragment used both as a
stage and inside a join sub-pipeline, plus a formula fragment, joined
and computed through the real executor."
```

---

### After the tasks

Run `/pr-prep`-style convergence is NOT part of this plan; when all six tasks are committed, tell the user the branch is ready for the pr-prep gate.
