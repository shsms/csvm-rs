# csvm-rs — Claude Code guidelines

A Rust port of [csvm](https://github.com/shsms/csvm), a multithreaded CSV
manipulation tool. The original is C++ (PEGTL + a hand-rolled DSL). This port
keeps the execution model but replaces the bespoke DSL with **tulisp** (an
embeddable Emacs-Lisp-compatible interpreter) as the command language.

## The one hard rule: no tulisp in the hot path

tulisp parses and **compiles** the script into a plain-Rust `Plan` exactly once,
at startup. Per-row processing (potentially billions of rows) runs the compiled
`Plan` with **zero** Lisp evaluation. Anything that would call into a
`TulispContext` while rows are flowing is a bug. After compilation the
`TulispContext` is dropped; the `Plan` contains only owned Rust data and is
`Send + Sync`, shared across worker threads behind an `Arc`.

## Command language (Lisp surface)

A script is a sequence of tulisp forms. The pipeline verbs are registered with
`ctx.defspecial`, so they receive their **raw, unevaluated** argument forms and
compile them — column names stay symbols, they are never looked up as
variables. Everything else in tulisp (`let`, `dolist`, `mapcar`, string fns…) is
available to *generate* a pipeline, but only the compiled verbs run per row.

| csvm DSL                         | csvm-rs Lisp                                  |
|----------------------------------|-----------------------------------------------|
| `cols(id, a, b)`                 | `(cols id a b)`                               |
| `!cols(a, b)`                    | `(drop-cols a b)`                             |
| `select(a == 't' && b != '0')`   | `(select (and (== a "t") (!= b "0")))`        |
| `sort(a, b:r)`                   | `(sort-by a (b :reverse))`                    |
| `to_num(a, b)` / `to_str(a, b)`  | `(to-num a b)` / `(to-str a b)`               |

- **Operators** in `select`: `== = != /= < > <= >=`, `=~` / `!~` (regex),
  `and`/`&&`, `or`/`||`, `not`/`!`. `and`/`or` are n-ary and short-circuit.
- **Operands**: bare symbols are column references; `"…"` are string literals;
  numbers are numeric literals.
- **sort spec** (the verb is `sort-by`, not `sort` — tulisp's prelude already
  binds `sort`): a bare `col`, or `(col :reverse :numeric)`. Aliases:
  `:r`/`:desc` for reverse, `:n`/`:num` for numeric. Multi-key, stable.
- `to-num`/`to_num` and `to-str`/`to_str` both spellings accepted.

## Implicit conversions (`to_num`/`to_str` are implicit)

csvm requires explicit `to_num` before numeric comparison/sort and `to_str`
before output. csvm-rs makes these implicit:

- A comparison against a **numeric literal** ⇒ numeric compare (the field is
  parsed). Against a **string literal** ⇒ string compare.
- Numbers always serialize correctly on output — no `to-str` needed to print.
- `(to-num c)` / `(to-str c)` remain as explicit **type overrides** affecting
  later column-vs-column comparisons and the default sort mode for that column.

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
- No-sort plans run as a single streaming stage: each worker parses a chunk
  (borrowed rows), applies the stage, serializes to a buffer, tags it with the
  chunk id. A writer thread reassembles output in id order. Fully zero-copy.
- `sort` is a blocking stage: rows are materialized as owned `Field<'static>`,
  sorted (in-memory stable multi-key; external merge via tmp files for large
  inputs), then handed to the next stage.

`Field<'a>` (`Str(&'a str) | Owned(String) | Num(f64)`) serves both paths: the
streaming path uses `Field<'chunk>` borrows; crossing a stage boundary calls
`into_owned()` to get `Field<'static>`. Compiled statements are generic over the
lifetime, so there is one `apply` implementation.

## CLI (parity with csvm)

`csvm [-f IN] [-o OUT] [-n THREADS] [-t TMPDIR] [--chunk-size BYTES]
[--print-engine] SCRIPT`. Defaults: stdin/stdout, threads = 1, chunk = 1 MB.

## Conventions

- Git identity: `Sahas Subramanian <sahas.subramanian@proton.me>` (author +
  committer), passed per-command. Default branch `main`.
- Commits: imperative subject, no prefix tag, no AI co-author footer.
- Run `cargo fmt` and `cargo clippy` before committing; keep both clean.
- `src/lib.rs` holds the library; `src/main.rs` is a thin CLI shim so internals
  are unit-testable.
