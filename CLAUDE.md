# csvm-rs — Claude Code guidelines

A Rust port of [csvm](https://github.com/shsms/csvm), a multithreaded CSV
manipulation tool. The original is C++ (PEGTL + a hand-rolled DSL). This port
keeps the execution model but uses a **pipe command language** parsed into a
plain-Rust plan. (It briefly used `tulisp`/Lisp; that was dropped for the pipe
syntax — see git history. The performance backend is unchanged by that swap;
only `parse.rs` is the frontend.)

## The one hard rule: no interpreter in the hot path

`parse.rs` parses the script into a plain-Rust `Plan` exactly once, at startup.
Per-row processing (potentially billions of rows) runs the compiled `Plan` —
column refs are indices, comparisons are monomorphic. The `Plan` contains only
owned Rust data and is `Send + Sync`, shared across worker threads behind an
`Arc`. Nothing parses or interprets the script while rows are flowing.

## Command language (pipe syntax)

A script is a sequence of stages separated by `|`. Commands take comma- or
space-separated arguments. `select` takes a **bare** infix expression (no
surrounding quotes — only string *literals* are quoted).

```text
cols a,b,c | select amount > 1000 && flag == 't' | sort amount=nr id
```

| csvm DSL                         | csvm-rs pipe                                  |
|----------------------------------|-----------------------------------------------|
| `cols(id, a, b)`                 | `cols id,a,b`  (or `cols id a b`)             |
| `!cols(a, b)`                    | `cols -v a,b`                                 |
| `select(a == 't' && b != '0')`   | `select a == 't' && b != '0'`                 |
| `sort(a, b:r)`                   | `sort a b=r`                                  |
| `to_num(a, b)` / `to_str(a, b)`  | `to-num a,b` / `to-str a,b`                   |

- **`cols`** keeps/reorders the named columns; **`cols -v`** keeps everything
  *except* them (like `cut --complement`). Columns can also be referenced by
  **1-based position**: `resolve_col` (`plan.rs`) matches the header name
  first and only then reads a bare integer as a position (so a column
  literally named `2` is never shadowed; out of range is its own error). That
  fallback lives in the one shared resolver, so it works for every column
  reference — `sort 1=n`, `group 2`, `` select `3` > 0 `` — not just `cols`.
  `cols` alone also takes **ranges** (`resolve_col_spec`): `2-4` between two
  positions, or `a:d` between any two references, expanding in header order;
  an exact header name (even one like `1-2`) always wins over the range
  reading. `cols -v` still ignores an unknown name, but a bad position or
  range is an error (`resolve_col_spec` takes an `Unknown` policy that applies
  to a bare name only, so a range endpoint keeps its did-you-mean hint).
  Resolvers that keep a column's name — group keys, agg names, the stats
  `field` column, sort keys, `uniq`/`to-num`/`to-str` lists, and `ColRef`
  (chart titles, expression refs) — rewrite it to the header name, so
  output and `--print-engine` show `qty`, not `2`. Join keys and `rename`'s
  source name still echo the spec text.
  Backticks are stripped before resolution (`split_list`), so they do not
  force a name reading. A `to-num`/`to-str` given by position pins the same
  column as one given by name: types are re-derived by position in
  `Plan::resolve` (see *Implicit conversions*).
- **`select`** operators: `==` (or `=`), `!=`, `< > <= >=`, `=~` / `!~` (regex),
  `^=` / `*=` / `$=` (begins/contains/ends, literal substring — negate with
  `!(…)`), `&&`, `||`, `!`, parens. No word operators (so columns are never reserved
  words). Operands of the six comparisons are full **value expressions** (the
  `add` grammar below the ternary): bare identifiers are columns (backtick-quote
  a name that isn't a bare identifier, e.g. `` `frequenz-app-edge` ``), numbers
  are numeric literals, `'…'`/`"…"` are string literals, and arithmetic
  (`select price * qty >= 30`), functions (`abs(x) > 1`), and parenthesized
  boolean subexpressions compared as `t`/`f` (`(a >= 0) == (b >= 0)`) all work
  (`Cmp` holds two `ValExpr`s; leaf operands take allocation-free fast paths,
  compound ones outlined `#[inline(never)]` fallbacks). `=~`/`!~` and
  the affixes still take a plain column on the left. A `select` reading
  `prev()`/`rownum()` is stateful and routes to the ordered in-memory path,
  like a stateful `add` (`select val != prev(val)`, `select rownum() % 2 == 1`);
  `color` predicates reject stateful expressions at parse time (they render
  post-run, where a rule whose column is missing from the output, by name or
  by position, is silently dropped; any other resolve error in a rule still
  aborts). A parenthesized
  expression makes `select (…)` fall out for free, and chaining `select`s ANDs
  them. Comparison mode (numeric / lexical / per-row auto) is covered under
  *Implicit conversions* below.
- **`sort`** specs: a bare `col`, or `col=flags` where flags are `n` (numeric),
  `s` (lexical / string), and/or `r` (reverse) — e.g. `amount=nr`. Multi-key,
  stable. Each key has a `SortMode` (`plan.rs`): a bare column is `Auto`
  (decided per *cell*: a cell that parses as a number orders numerically and
  before every non-number, a blank reading as 0 as in `select`'s auto; text
  follows lexically; NaN/inf are numbers — a total
  order, so it streams and shards without sampling types, and a mixed column
  never aborts), `=n` or a `to-num` column is `Numeric` (a non-number aborts),
  `=s` or a `to-str` column is `Lexical`. The external sort's encoded key
  (`encode_key` in `sort.rs`) prefixes an auto key with a one-byte tag
  (number / text), and the in-memory path parses the same cell once through
  `SortStmt::row_key` (`plan::auto_num`) before `SortStmt::compare` orders
  it, so both paths agree byte for byte.
- **`head [N]`** keeps the first N rows reaching it (default 10 when omitted;
  also `head -n N`, `-nN`, `--lines N`, and the obsolete `-N`). Own stage;
  streams + stops early when there's no sort, else truncates in the materialized
  path. A *negative* count (`head -n -N`) keeps all but the last N — a separate
  `Stage::DropLast` on the blocking in-memory path. (Byte mode `-c` isn't
  supported.)
- **`tail [N]`** keeps the last N rows (default 10; same count spellings as
  `head`, via the shared `parse_row_count`). Blocking — it can't stream/stop
  early, so any plan with `tail` takes the in-memory path (`Stage::Tail`,
  applied as a drain in `apply_stages_over_rows`).
- **`uniq [cols]`** (alias `dedup`) drops duplicate rows keeping the first, by
  the whole row or the named key columns. Global (not Unix-adjacent), so no
  pre-sort is needed; blocking, so it uses the in-memory path. The dedup key is
  the CSV-encoded cells (`dedup_rows` in `exec.rs`, a `HashSet`).
- **`stats [cols]`** reduces the input to one summary row per column
  (`field,count,empty,min,max,sum,mean,stddev`); an empty list profiles every
  column. A blocking, *reducing* stage: it streams the input through per-column
  accumulators (`ColStats` in `stats.rs`, O(columns) memory) and rewrites the
  header to the profile schema, so `sort`/`head`/`fmt` compose after it. Type
  detection is lenient (a non-numeric cell ⇒ text column; unlike `to-num` it
  never aborts); min/max are numeric for numeric columns, lexical for text;
  sample stddev via Welford. Non-finite values (`NaN`/`inf`) parse as numbers
  but are skipped from the aggregates (still counted as non-empty) so they don't
  poison sum/mean/stddev; `select`/`sort`/`to-num` still accept them. `ColStats`
  is deliberately presentation-free so `fmt`'s value-colouring can reuse it.
  Over a **seekable file with `-n>1`**, `stats` shards: each worker builds a
  partial `ColStats` over its byte range and `ColStats::merge` (Welford parallel
  combine) folds them. Counts/`min`/`max` are exact; floating `sum`/`mean`/
  `stddev` may differ from the `-n1` result by ~1 ULP (parallel reduction sums
  in a different order). stdin / `-n1` use the single-pass streaming path.
- **`group COLS`** + **`agg FN(col),… [by COLS]`** reduce to one row per distinct
  key — the per-key sibling of `stats` (which reduces globally). `group` sets the
  keys; a following `agg` fuses with it (replacing `group`'s placeholder `count`),
  or `agg … by COLS` carries its own keys, or `agg` with neither emits a single
  global row. Functions are `count/sum/min/max/mean/stddev` (a bare `count`
  counts rows; `count(col)` counts non-empty cells); output cols are named
  `col_func` (`amount_sum`) or `count`. `sum/mean/stddev` are blank for a
  non-numeric column (same policy as `stats`). A blocking, reducing stage
  (`Stage::Group`, `GroupStmt`/`AggSpec`/`AggFunc` in `plan.rs`): the `Grouper`
  in `exec.rs` folds rows into a `HashMap<key, GroupAcc>` keeping first-seen
  order, reusing one `ColStats` (`stats.rs`) per distinct aggregated column —
  O(groups × aggregated-cols) memory. Like `stats` it has streaming and sharded
  fast paths (`group_shape` gates both): stdin / `-n1` **stream** the input
  through one `Grouper` (`run_group_streaming`), and a **seekable file with
  `-n>1`** shards — each worker builds a partial `Grouper` over its byte range
  and `Grouper::merge` folds them in file order (so first-seen group order is
  preserved). Counts/`min`/`max` are exact; floating `sum`/`mean`/`stddev` may
  differ from `-n1` by ~1 ULP (parallel reduction order), the same caveat as
  sharded `stats`. A `group` combined with a `sort` (or another blocking stage)
  still takes the in-memory path.
- **`join [FLAGS] ITEM[, ITEM…]`** (`ITEM := [(SUBPIPELINE)] FILE [on KEYS]`)
  merges one or more right-side CSVs in by key. Items are separated by
  top-level commas (`split_top_commas` — protects quoted and parenthesized
  commas only, so quote a file path containing a comma; a composite key
  list's commas split too, and the key-continuation step stitches those
  fragments back onto their `on` clause). Either every item has its own
  `on`, or a single trailing `on` is shared by all — mixing errors where
  detectable, but a keyless, paren-less fragment after an `on` clause is
  lexically identical to more keys, so a forgotten per-file `on` surfaces
  at resolve time (`JoinStmt::own_keys` counts the keys from an explicit
  `on` — the item's own, or the shared trailing one; `join_key_err` in
  `plan.rs` hints on the continuation-appended ones). Multiple
  items desugar to one `JoinStmt`/`Stage::Join` per file, left to right — pure
  parser-level sugar (like `delta`), identical to chaining single joins; flags
  apply to every item. A blocking stage: the right `FILE` is run through its optional
  parenthesized sub-`Plan` (a full recursive pipeline — this is the "DAG" node),
  materialized, and built into a `HashMap<key, Vec<right_row_idx>>`; the left rows
  probe it. Type flags `-l/--left`, `-r/--right`, `-F/--full` (inner default);
  unmatched cells pad empty (an unmatched right row coalesces the key value into
  the left key column). `on` keys are `name` or `lname=rname`, composite via a
  list; matching is exact string equality on the CSV-encoded key cells (like
  `uniq`). Output = left cols ++ right non-key cols; a clashing right name is
  suffixed `_r` (configurable per side with `--lsuffix`/`--rsuffix` — only
  *clashing* names are touched). `exec::prepare_joins` reads each right file's
  header and resolves its sub-plan *before* the pure `Plan::resolve` (which needs
  the right header to compute the joined schema); `main` calls it between parse
  and resolve. Any plan with a `join` takes the in-memory path (`Stage::Join` in
  `apply_stages_over_rows`). The right side must be a file (never stdin). Paren
  groups make `split_stages` paren-aware so the sub-pipeline's `|` doesn't split
  the outer pipeline.
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
- **`color …`** attaches a colour rule (plan metadata, like `fmt`'s output
  mode): `color COLOUR EXPR` paints a row, `-c COL` a cell, `-g COLS RAMP [LO HI]`
  a value gradient (`-g` takes several columns, emitting one `Gradient` rule each
  with the shared ramp/bounds). Predicate rules reuse the `select` expression parser; rules
  resolve against the *output* header and render in `exec::render`, which pads by
  *visible* width so ANSI escapes don't break alignment. Gradients reuse
  `ColStats::num_range` for default bounds. Colours are truecolour SGR
  (`src/color.rs`); `--color auto|always|never` gates emission (auto = TTY).
- **`rename old=new …`** is a header-only change (resolve renames the header;
  `apply` is a no-op).
- **`add NAME EXPR`** appends a computed column (`Stmt::Add`), or replaces `NAME`
  in place if it already exists (`AddStmt.pos`; `NAME` may also be a 1-based
  position of an existing column, like any other column reference, and the
  header keeps that column's name; inside `EXPR` a bare integer is a number
  literal, so a position there is backticked: ``add 2 num(`2`)``). On a
  ragged row the replaced cell is padded into place (a missing cell reads as
  blank, so `num()` gives 0). `EXPR` is a *value* expression
  (`ValExpr` in `plan.rs`): arithmetic (`+ - * / %`), `++` concat, the function
  set (`round/floor/ceil/abs/int/sqrt/pow/exp/log/log10/log2/sign/min/max/len/
  upper/lower/trim/coalesce/num/str` — the math functions follow IEEE at
  domain edges, `sqrt(-1)` = NaN, no abort; div/mod-by-zero still aborts;
  `num(x)` casts to a number and aborts on a non-number, `str(x)` casts to
  text, and each types its result so `add c num(c)` pins column `c`), a
  `?:` ternary
  (reusing `BoolExpr` for the test), constants, and `prev(col)`/`rownum()`. It reuses the `select` tokenizer
  (`lex_expr`, extended with arithmetic operators + context-sensitive sign) and a
  sibling recursive-descent parser (`ExprParser::parse_value`). `eval` takes an
  `EvalCtx { prev_row, rownum }` and returns an owned `Field`. A pure `add` is
  per-row and **shardable** (rides every path); an `add` reading `prev`/`rownum`
  is `is_stateful()` and routes to the **in-memory ordered path** (the guard
  `plan_has_stateful_expr` in `exec::run_body`/`run_file`, mirroring the
  `tail`/`uniq`/`join` fallback), so its output is `-n`-independent. The new
  column carries the expression's **static type** (numeric / text / untyped,
  `ExprParser::static_type` — including a type inherited from a
  `to-num`/`to-str` column or a `?:` whose branches agree), so later
  comparisons against it are typed the same as against the expression itself.
- **`delta [-s SUF] COLS`** is pure parser-level sugar: `parse_delta` emits one
  stateful `Stmt::Add` per column (`COL<suffix> = COL - prev(COL)`, suffix
  default `_delta`), so it shares all of `add`'s machinery and guarantees.
- **`hdr a,b,c`** supplies column names for headerless input: it sets
  `Plan.input_header` (plan-level metadata, must be the first command), so the
  whole input is data and `main`/the test harness skip reading a header line
  (file shards start at byte 0). The names are prepended on output.
- **`fmt`** sets the output mode to whitespace-aligned (`column -t`); it's a
  `Plan.output` flag, applied by `exec::format_aligned` after the run produces
  CSV (so the executor itself is unchanged). Columns whose data cells are all
  numeric are right-justified (digits line up); text columns are left-justified.
- **`graph KIND COL [flags]`** (alias `plot`) is a terminal-chart **sink**: it
  draws from the columns reaching it instead of emitting CSV, so it must be the
  *last* command (the parser rejects anything after it). Plan metadata
  (`Plan.graph`, `GraphSpec` in `plan.rs`), not a stage — like `fmt`/`color` it
  renders in `exec::render` from the buffered output, reusing the whole executor
  upstream. `exec::render_graph` pulls the charted columns and drops
  non-numeric/empty cells *loudly* (counted and reported below the chart — the
  "strict and loud" policy); the drawing lives in `src/graph.rs`. Charts:
  - `graph hist COL` — `Histogram` bins values (Sturges' default, capped at 50)
    into horizontal block bars with an eighth-block fractional tail.
  - `graph bar LABEL VALUE` — one diverging bar per row anchored at a zero
    baseline (negatives extend left); capped at `MAX_BARS=50` rows, overflow
    reported. Use after group-by.
  - `graph spark COL` — a one-line sparkline (eighth-height blocks), a long
    series bucket-averaged down to the width.
  - `graph scatter X Y[,Y2…]` / `graph line X Y[,Y2…]` — points (line: Bresenham
    segments) on a 2×4 `Braille` canvas in a labelled frame. Multiple y-series
    get distinct colours (`render_xy`, when `--color`) with a legend; one braille
    canvas per series, first-series-wins per shared cell. `collect_xy` picks the
    X mode (a `graph::XAxis`): **numeric** (plotted as-is); else **temporal** — if
    every X cell parses as a timestamp (`crate::datetime::parse_epoch`, no dep),
    plot at true epoch positions; else the **row-index fallback** (categories) —
    plot against the 1-based ordinal and flag `even row spacing`. A
    partially-numeric X keeps the strict drop-bad-rows behaviour. The bottom axis
    is *graduated* with intermediate ticks (`x_label_row`/`place_ticks`): numeric
    uses round 1/2/5×10ⁿ values (`nice_ticks`), time interpolates and drops the
    date to `HH:MM:SS` when every tick is the same day, categories show only the
    end cells; tick count adapts to width (so `--scale` adds more), labels that
    collide are dropped.
  Flags: `--bins N` (hist), `--scale F`, `--title T`, `--svg`. A single `--scale`
  multiplies both base dimensions (`graph::chart_size`: `BASE_W`=80 cols,
  `BASE_H`=15 rows; default 1.0) — there is no separate width/height knob and no
  terminal probing. `--svg` emits a
  standalone SVG document to the normal output instead of the terminal chart
  (`src/svg.rs`, hand-written XML, no dep; reuses the same collected data). PNG
  (needs a raster dep) and a scatter density colour ramp are the remaining
  follow-ups (`todo.org`).
- `to-num`/`to_num` and `to-str`/`to_str` both spellings accepted.
- `parse` first strips `#`-to-EOL comments (quote-aware: `'…'`/`"…"`/`` `…` ``
  protect a literal `#`), then `split_stages` splits on a lone unquoted `|`
  **or a newline** (so a multi-line `-f` script is one stage per line, no
  trailing `|`s); a `||` (or) and a `|`/newline inside a string literal or a
  `join (…)` group are left intact, and blank/comment-only stages are dropped.
  `parse.rs` is a hand-written tokenizer plus a recursive-descent expression
  parser producing the `BoolExpr`/`Cmp` IR (and the `ValExpr` value IR for
  `add`). The grammar is **single-pass**: each position parses once as a
  boolean-or-value (`BV` in `ExprParser`) and lookahead settles which — no
  backtracking, so parse time is linear in the script and errors point at the
  offending token.

## Implicit conversions (`to_num`/`to_str` are implicit)

csvm requires explicit `to_num` before numeric comparison/sort and `to_str`
before output. csvm-rs makes these implicit:

Each comparison resolves to one of three modes at compile time (`CmpMode` in
`plan.rs`, decided by `CmpMode::decide` from the operands' **static types** —
`ValExpr::static_type`, which consults the tracked `to-num`/`to-str`/`add`
column types for `Col`/`prev` leaves and types a `?:` only when both branches
agree; visible per-compare as `:num`/`:str`/`:auto` under `--print-engine`).
The decision runs twice: the parser decides by column *name*, then
`Plan::resolve` re-decides from a **position-keyed** type map it threads
through the statements (`to-num`/`to-str`/`add` set a column's type, `cols`
reorders the map, `group` keeps its keys' types and types its aggregates,
`stats` types its profile columns, a join keeps the left side's), so a type
follows the column however it is spelled — `to-str 2 | sort qty`, a rename in
between, a `cols` reorder, a `group` on the column. Resolve only pins further:
an `Auto` becomes `Numeric`/`String` and a default `String` becomes
`Numeric` (numericizing a literal), never the reverse. The three checks
below run **in order**, so a numeric signal wins over a string one (`to-str c
| select c > 5` is still `Numeric` — the numeric literal decides). `sort`
mirrors this per key with its own `SortMode` (a bare `sort c` auto-detects
per cell; `=n`/`=s` or a typed column pin it, the latter also at resolve time
— see the `sort` bullet above):

- A comparison with a **statically numeric side** (a numeric literal,
  arithmetic, a numeric function, `rownum()`, or a `to-num`-typed column) ⇒
  `Numeric`: both sides parse to `f64` (a non-number aborts the run).
- A comparison with a **statically string side** (a string literal, `++`
  concat, a boolean value, a string function, or a `to-str`-typed column),
  **or an `==`/`!=` between two untyped sides** ⇒ `String`: lexical. (Equality
  stays lexical because numeric equality on floats is fragile.)
- An **ordering (`< > <= >=`) between two untyped sides** ⇒ `Auto`: decided
  *per row* — if both cells parse as numbers, order numerically, else fall back
  to lexical. This kills the old footgun where `select qty > stock` silently
  compared `"100" < "9"` as text. It's reproducible (a function of the two
  values, not a sampled type) and never aborts; the successful `f64` parses are
  reused so the common all-numeric case costs the same as an explicit `to-num`.
  Pin a genuinely-text column back to lexical with `to-str`.
- Numbers always serialize correctly on output — no `to-str` needed to print.
- `to-num c` / `to-str c` remain as explicit **type overrides** affecting later
  column-vs-column comparisons and the default sort mode for that column.

Numeric coercion: trim, empty ⇒ `0.0`, else parse `f64`; non-numeric is an error
that aborts the run with the offending value (matches csvm's `to_num`
strictness) — except in `Auto` mode, where a non-number quietly drops to lexical
for that row instead of aborting.

## CSV spec (the "input spec")

Hand-rolled zero-copy, quote-aware scanner over each chunk (uses `memchr`):
- `,`-separated, `"`-quoted fields; `""` is an escaped quote inside quotes;
  quoted fields may contain commas.
- **No embedded newlines in fields** — a row is a line. This is required for the
  parallel chunked reader (input is split into ~1 MB chunks at line boundaries)
  and matches csvm's model.
- UTF-8 in, UTF-8 out. First line is the header.
- Unchanged borrowed fields are written back verbatim (zero re-encoding); only
  fields that needed unescaping or were converted to numbers allocate.

`csv` / `csv-core` were evaluated; the hand-rolled scanner was chosen because the
parallel-chunk + zero-copy-borrow design needs to slice fields directly out of
the chunk buffer. A `csv`-backed strict mode could be a future option.

## Parquet input (optional `parquet` feature)

The first cut of the pluggable-format roadmap. **Off by default** to keep the
lean dep tree — `--features parquet` pulls `parquet` + `arrow` + codecs (the same
"gate dep-needing formats" rule the SVG-vs-PNG split follows). `src/parquet.rs`
(cfg-gated) wraps the arrow `ParquetRecordBatchReader`:
- Selected by a `.parquet` extension or `--format parquet`; `--format csv`
  forces CSV. Parquet's metadata is in the file footer, so it needs a **seekable
  file** — stdin is rejected, and `hdr`/`--no-header` error (the schema *is* the
  header). Without the feature, a parquet input errors with a build hint.
- The schema gives the header **and types**: numeric columns decode straight to
  `Field::Num`, so the implicit-typing engine works with **no `to-num`**; bool ⇒
  `t`/`f`; null ⇒ empty cell. Only flat primitives (int/uint/float/string/bool)
  are supported — any other column type (temporal/decimal/binary/nested, and a
  dictionary/categorical-encoded column whose arrow type is `Dictionary`) errors
  by name in `validate_schema`. Numbers join the `f64` model, so an int64/uint64
  magnitude above 2^53 loses integer precision on read (the same limit CSV hits).
- `exec::run_parquet` drives it: each arrow `RecordBatch` is transposed columnar
  ⇒ rows of owned `Field`. A lone non-stateful transform **streams** batch by
  batch (O(batch) memory) and, with `-n>1`, **shards across row groups**
  (`run_parquet_sharded`/`process_row_groups`): the row-group indices are split
  into contiguous per-worker blocks (`partition_row_groups`), each worker decodes
  its block via `ParquetReader::open_row_groups`, and the serialized outputs
  concatenate in file order — the parquet mirror of CSV's `run_sharded` (~3.2× on
  4 cores; a single-row-group file can't shard). Anything blocking
  (sort/group/tail/uniq/join, or a stateful `add`) materializes and runs the
  staged in-memory path — mirroring `run_body`'s dispatch.
- Follow-ups (`todo.org`): column/row-group projection push-down (only decode the
  columns the plan touches), more column types (temporal/decimal/dictionary), and
  parquet *output* (the `Sink` half). `gen_parquet` (feature-gated example) writes
  a multi-row-group fixture for trying / benchmarking it.

## Execution model

- Main thread streams the input file in chunks into a bounded MPMC channel.
- A `Plan` is a list of **stages** split at `sort` boundaries (mirrors csvm's
  `tblock`s: statements before a sort form one stage, the sort is its own stage,
  statements after form another).
- No-sort plans over a **seekable file** with `-n>1` are **sharded**: each
  worker reads its own line-aligned byte range (no central reader/channel),
  applies the stage with borrowed rows, and outputs are concatenated in file
  order. stdin (or `-n1`) streams chunk-by-chunk instead. Fully zero-copy.
- **Streaming reads what's available, not a full chunk.** The streaming paths
  (`head` and a lone transform) read via `next_chunk_available` (a single `read`
  completed to a line boundary) and flush output per chunk, so a slow or
  unbounded stream emits promptly instead of stalling until a 1 MB buffer fills
  (which made `head` hang and `select` withhold output). `sort` and the
  in-memory fallback still fill large chunks via `next_chunk` (they must read
  all input before emitting, so batching wins there).
- `sort` is a blocking stage handled by a **parallel external merge sort**
  (`src/sort.rs`, modeled on csvm): the driver reads raw input blocks; `-n`
  workers each parse + apply the pre-sort statements, **serialize each row to
  bytes once**, compute an **order-preserving encoded key**, and sort their
  block into a run (kept in memory, or spilled to a temp file past the budget).
  A single-threaded binary-heap k-way merge then picks the smallest key and
  emits the row's already-serialized line bytes via a callback — no per-field
  allocation, no re-serialization on output. A block is a contiguous input
  range, so its sequence number keeps the merge stable.

`Field<'a>` (`Str(&'a str) | Owned(String) | Num(f64)`) serves both paths: the
streaming path uses `Field<'chunk>` borrows; crossing a stage boundary calls
`into_owned()` to get `Field<'static>`. Compiled statements are generic over the
lifetime, so there is one `apply` implementation.

## CLI

`csvm [-o/--output OUT] [-n/--threads N] [-f/--file FILE] [-t/--temp-dir DIR]
[--chunk-size SIZE] [--sort-buffer SIZE] [--color WHEN] [--print-engine]
[-V/--version] [SCRIPT] [INPUT]`. The script is the first positional; the input
file is an optional **second positional** (awk-style; default stdin, a bare `-`
is stdin). With `-f FILE` the pipeline is read from a file (awk-style) and the
single positional is the input. At most one input is accepted (a second
positional is an error). Long options take `--flag VALUE` or `--flag=VALUE`.
`--chunk-size`/`--sort-buffer` accept K/M/G (binary)
suffixes. `--no-header` treats the input as headerless and auto-names columns
`c1,c2,…` (main peeks the first line to count them — for stdin it reads the line
and chains it back; `hdr` wins if both are given). `--color` honors
`NO_COLOR`/`CLICOLOR_FORCE` under `auto`. Help lives in one registry
(`src/help.rs`): `--help`/`csvm help` print the overview, `csvm help CMD` (name
or alias) a command's forms + example, `csvm help TOPIC` the `operators`/
`colors`/`types`/`sizes` pages; a test cross-checks the registry against
`parse::COMMANDS` so the help can't drift (the overview's command list is
generated from it). Usage errors show only the brief synopsis. Defaults:
stdin/stdout, threads = 1,
chunk = 1 MB, sort buffer = 256 MiB. (csvm used `-f IN` for *input*; this port
reuses `-f` for the *script* file and takes input positionally — flags
otherwise mirror csvm.)

## Performance

Benchmarked against the C++ csvm (`gen_csv` 3M rows / 151 MB, warm cache).
csvm-rs is faster on every case tried — filter, projection, and sort, at both
`-n 1` and `-n 8` (sort by ~3x). The wins came from: a zero-copy scanner +
compiled plan; sharded file reads (no central reader/channel for transforms);
reusing a projection scratch buffer (no per-row alloc); and a sort that
serializes once in the parallel workers, sorts an encoded key, and lets the
serial merge just copy line bytes.

### Known pass-2 opportunities

- Sort run generation is parallel and the intermediate merges are
  **multi-level + parallel** (groups of `fanout=32` runs are consolidated
  across the workers, so huge inputs stay FD-bounded). The **final** merge is
  still single-threaded; parallelizing it (and, for parquet, projection/filter
  push-down into the reader) is what's left.
- The order-preserving string sort key uses a `\0` terminator; a string sort
  key containing a literal NUL byte (never produced by normal CSV) would order
  slightly off. Numeric keys are exact.

## Roadmap

The pipe language is deliberately built to grow: new verbs are a parse arm plus
a `Stmt`/`Stage`. `join` is implemented (`Stage::Join`, a sub-`Plan` right side —
the `Plan` is now a small DAG). The computed `add` column is implemented
(`Stmt::Add`, the `ValExpr` value engine; `prev`/`rownum` take the ordered
in-memory path). Group-by aggregation is implemented (`Stage::Group`,
`group … | agg …`), as is terminal-native graphing (`graph
hist/bar/spark/scatter/line` in `src/graph.rs`, `--svg` export in
`src/svg.rs`). Pluggable input formats have begun: **parquet read** ships behind
the optional `parquet` feature (`src/parquet.rs`, see above). Planned (see
`todo.org` for design notes): parquet projection push-down and *write*,
JSON-lines output, and a scatter density colour ramp. TSV / a delimiter flag,
multiple input files, and join naming sugar were considered and rejected for
good.

## Conventions

- Git identity: `Sahas Subramanian <sahas.subramanian@proton.me>` (author +
  committer), passed per-command. Default branch `main`.
- Commits: imperative subject, no prefix tag, no AI co-author footer.
- Run `cargo fmt` and `cargo clippy` before committing; keep both clean. CI
  (`.github/workflows/ci.yml`) enforces fmt, `clippy --all-targets -D warnings`,
  `cargo test`, a release build, and `cargo bench --no-run` on every push and PR.
- `src/lib.rs` holds the library; `src/main.rs` is a thin CLI shim so internals
  are unit-testable.
