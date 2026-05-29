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
  *except* them (like `cut --complement`).
- **`select`** operators: `==` (or `=`), `!=`, `< > <= >=`, `=~` / `!~` (regex),
  `^=` / `*=` / `$=` (begins/contains/ends, literal substring — negate with
  `!(…)`), `&&`, `||`, `!`, parens. No word operators (so columns are never reserved
  words). Operands: bare identifiers are columns (backtick-quote a name that
  isn't a bare identifier, e.g. `` `frequenz-app-edge` ``), numbers are numeric
  literals, `'…'`/`"…"` are string literals. A parenthesized expression makes `select (…)`
  fall out for free, and chaining `select`s ANDs them.
- **`sort`** specs: a bare `col`, or `col=flags` where flags are `n` (numeric)
  and/or `r` (reverse) — e.g. `amount=nr`. Multi-key, stable.
- **`head [N]`** keeps the first N rows reaching it (default 10 when omitted;
  also `head -n N`, `-nN`, `--lines N`, and the obsolete `-N`). Own stage;
  streams + stops early when there's no sort, else truncates in the materialized
  path. (Negative `-n -N` and byte mode `-c` are not supported.)
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
- **`color …`** attaches a colour rule (plan metadata, like `fmt`'s output
  mode): `color COLOUR EXPR` paints a row, `-c COL` a cell, `-g COL RAMP [LO HI]`
  a value gradient. Predicate rules reuse the `select` expression parser; rules
  resolve against the *output* header and render in `exec::render`, which pads by
  *visible* width so ANSI escapes don't break alignment. Gradients reuse
  `ColStats::num_range` for default bounds. Colours are truecolour SGR
  (`src/color.rs`); `--color auto|always|never` gates emission (auto = TTY).
- **`rename old=new …`** is a header-only change (resolve renames the header;
  `apply` is a no-op).
- **`hdr a,b,c`** supplies column names for headerless input: it sets
  `Plan.input_header` (plan-level metadata, must be the first command), so the
  whole input is data and `main`/the test harness skip reading a header line
  (file shards start at byte 0). The names are prepended on output.
- **`fmt`** sets the output mode to whitespace-aligned (`column -t`); it's a
  `Plan.output` flag, applied by `exec::format_aligned` after the run produces
  CSV (so the executor itself is unchanged). Columns whose data cells are all
  numeric are right-justified (digits line up); text columns are left-justified.
- `to-num`/`to_num` and `to-str`/`to_str` both spellings accepted.
- `parse` first strips `#`-to-EOL comments (quote-aware: `'…'`/`"…"`/`` `…` ``
  protect a literal `#`), then `split_stages` splits on a lone unquoted `|`; a
  `||` (or) and a `|` inside a string literal are left intact, so the bare
  `select` expression needs no quoting of its own. `parse.rs` is a hand-written tokenizer plus a
  recursive-descent expression parser producing the `BoolExpr`/`Cmp` IR.

## Implicit conversions (`to_num`/`to_str` are implicit)

csvm requires explicit `to_num` before numeric comparison/sort and `to_str`
before output. csvm-rs makes these implicit:

- A comparison against a **numeric literal** ⇒ numeric compare (the field is
  parsed). Against a **string literal** ⇒ string compare.
- Numbers always serialize correctly on output — no `to-str` needed to print.
- `to-num c` / `to-str c` remain as explicit **type overrides** affecting later
  column-vs-column comparisons and the default sort mode for that column.

Numeric coercion: trim, empty ⇒ `0.0`, else parse `f64`; non-numeric is an error
that aborts the run with the offending value (matches csvm's `to_num` strictness).

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

`csvm [-o OUT] [-n THREADS] [-t TMPDIR] [--chunk-size BYTES]
[--sort-buffer BYTES] [--print-engine] SCRIPT [INPUT]`. The script is the first
positional; the input file is an optional **second positional** (awk-style;
default stdin, a bare `-` is stdin). Defaults: stdin/stdout, threads = 1,
chunk = 1 MB, sort buffer = 256 MiB. (csvm used `-f IN`; this port takes the
input positionally — flags otherwise mirror csvm.)

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
a `Stmt`/`Stage`. Planned (see `todo.org` for design notes): a computed `add`
column, conditional colouring for `fmt`, pluggable formats via a `Source`/`Sink`
trait (Parquet, TSV), and `join` over multiple files (a sub-pipeline as the
right side; the `Plan` grows a join node and becomes a small DAG).

## Conventions

- Git identity: `Sahas Subramanian <sahas.subramanian@proton.me>` (author +
  committer), passed per-command. Default branch `main`.
- Commits: imperative subject, no prefix tag, no AI co-author footer.
- Run `cargo fmt` and `cargo clippy` before committing; keep both clean. CI
  (`.github/workflows/ci.yml`) enforces fmt, `clippy --all-targets -D warnings`,
  `cargo test`, a release build, and `cargo bench --no-run` on every push and PR.
- `src/lib.rs` holds the library; `src/main.rs` is a thin CLI shim so internals
  are unit-testable.
