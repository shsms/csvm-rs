# csvm-rs

A fast, multithreaded CSV manipulation tool — a Rust port of
[csvm](https://github.com/shsms/csvm) (originally C++). The bespoke C++ DSL is
replaced by [tulisp](https://github.com/shsms/tulisp), an embeddable Emacs-Lisp,
used as the command language.

The script is **compiled once** by tulisp into a plain-Rust execution plan; the
per-row hot path runs that compiled plan with **no interpreter involved**.

```sh
cat input.csv | csvm '(select (and (== fieldA "t") (> countZ 0))) (drop-cols fieldA)'
```

## Build

```sh
cargo build --release        # binary at target/release/csvm
cargo test                   # unit + integration tests
```

The `tulisp` dependency is a path dependency on the sibling `../tulisp`
checkout.

## Usage

```
csvm [-f IN] [-o OUT] [-n THREADS] [-t TEMPDIR]
     [--chunk-size BYTES] [--sort-buffer BYTES] [--print-engine] SCRIPT
```

| Flag             | Meaning                                                       |
|------------------|---------------------------------------------------------------|
| `-f IN`          | input file (default: stdin)                                   |
| `-o OUT`         | output file (default: stdout)                                 |
| `-n THREADS`     | worker threads (default: 1; `<=0` ⇒ 1)                        |
| `-t, --temp-dir` | directory for sort spill files (default: system temp)         |
| `--chunk-size`   | input chunk size in bytes (default: 1 000 000)                |
| `--sort-buffer`  | in-memory budget before `sort` spills to disk (default 256 MiB) |
| `--print-engine` | print the compiled plan and exit                              |

The first input line is the header; columns are referenced by name.

## The command language

A script is a sequence of Lisp forms. The pipeline verbs are:

| Verb                          | Does                                                       |
|-------------------------------|------------------------------------------------------------|
| `(cols a b c)`                | keep these columns, in this order                          |
| `(drop-cols a b)`             | drop these columns, keep the rest                          |
| `(select EXPR)`               | keep rows where `EXPR` is true                             |
| `(sort-by SPEC...)`           | sort rows (stable, multi-key)                              |
| `(to-num a b)` / `(to-str a b)` | mark columns numeric / string (usually unnecessary, see below) |

`to_num` / `to_str` (underscore) are accepted too.

### `select` expressions

- **Column reference**: a bare symbol — `fieldA`. (Use a string for names that
  aren't valid symbols, e.g. `"first,name"`.)
- **Literals**: strings `"t"`, numbers `0`, `3.14`.
- **Comparisons**: `==`/`=`, `!=`/`/=`, `<`, `>`, `<=`, `>=`.
- **Regex**: `(=~ col "pattern")`, `(!~ col "pattern")` (Rust `regex` syntax).
- **Logic**: `(and …)`, `(or …)`, `(not e)` — also `&&`, `||`, `!`. `and`/`or`
  are n-ary and short-circuit.

```lisp
(select (and (== fieldA "t") (or (> countZ 0) (> countA 0))))
```

### `sort-by` specs

Each spec is a bare column, or `(column :modifier...)`:

- `:reverse` (aliases `:r`, `:desc`) — descending.
- `:numeric` (aliases `:n`, `:num`) — compare numerically rather than lexically.

```lisp
(sort-by fieldA (fieldB :reverse) (countZ :numeric :reverse))
```

> The verb is `sort-by`, not `sort`: tulisp's standard library already binds
> `sort`.

### Conversions are implicit

csvm needs an explicit `to_num` before any numeric comparison or sort and a
`to_str` before output. Here that is automatic:

- A comparison against a **number** is numeric; against a **string** it is
  lexical. So `(> countZ 0)` just works — no `to-num` needed.
- Numbers always print correctly — no `to-str` needed.
- `to-num` / `to-str` remain available as explicit type overrides (they affect
  column-vs-column comparisons and a column's default sort mode).

Numeric coercion treats empty as `0`; a genuinely non-numeric value where a
number is required aborts the run with the offending value (as csvm does).

### Programmable pipelines

Because the verbs run while tulisp evaluates the script, ordinary Lisp control
flow can shape the pipeline at compile time:

```lisp
(when production (to-num price))   ; include a step conditionally
(dotimes (_ 3) (select (> x 0)))   ; emit a step repeatedly
```

Column arguments are read as literal symbols/strings, so a loop variable is
**not** interpolated into a column name.

## Examples

csvm's README examples, translated:

```sh
# keep three columns, in order
csvm '(cols id fieldA countZ)' < input.csv

# drop a column, write to a file
csvm -o out.csv '(drop-cols fieldA)' < input.csv

# filter rows
csvm -f input.csv '(select (and (== fieldA "t") (!= countZ "0")))'

# numeric filter (no to-num needed)
csvm -f input.csv '(select (and (== fieldA "t") (or (> countZ 0) (> countA 0))))'

# filter then drop the filter column
csvm -f input.csv '(select (== fieldA "t")) (drop-cols fieldA)'

# filter, forward sort by fieldA, reverse sort by fieldB
csvm -f input.csv '(select (!= fieldA "t")) (sort-by fieldA (fieldB :reverse))'

# numeric filter and numeric reverse sort
csvm -f input.csv '(select (> countA 0)) (sort-by (countA :reverse :numeric))'
```

`--print-engine` shows the compiled, resolved plan:

```
$ csvm -f input.csv --print-engine \
    '(to-num countZ) (select (> countZ 0)) (sort-by (countZ :r)) (drop-cols fieldB)'
stage 1 (transform):
  1.1 to-num ["countZ"] (positions [4])
  1.2 select (> countZ[4] 0)
stage 2 (sort):
  2.1 sort countZ[4] reverse numeric
stage 3 (transform):
  3.1 drop-cols -> keep positions [0, 1, 2, 3]
```

## How it works

- **Compile once.** tulisp parses the script and runs the pipeline verbs, which
  are special forms (`defspecial`) that receive their arguments *unevaluated* and
  append to a plan. The plan is plain Rust data — no interpreter is retained.
- **Parallel by sharding.** With `-n N` over a seekable file, the data region is
  split into N line-aligned byte ranges and each worker reads and processes its
  own range — no central reader or channel. Shard outputs are concatenated in
  file order, so results are identical regardless of thread count. (stdin can't
  seek, so it streams single-threaded unless `-n` forces a channel pipeline.)
- **Zero-copy.** Fields are sliced straight out of the chunk buffer; only an
  unescaped `""` or a parsed number allocates.
- **Sort runs in parallel and scales past memory.** `sort-by` farms input
  blocks to `-n` workers that parse and stably sort each into a run (kept in
  memory, or spilled to a temp file past `--sort-buffer`); a single-threaded
  binary-heap k-way merge then produces the sorted output.

## Benchmarking

`examples/gen_csv.rs` generates a deterministic 10-column CSV (same row count ⇒
same bytes), so benchmarks are reproducible:

```sh
cargo run --release --example gen_csv -- 3000000 /tmp/huge.csv   # ~151 MB
time ./target/release/csvm -f /tmp/huge.csv -n 8 \
  '(select (and (== flag "t") (> amount 50000) (=~ status "^a"))) (cols id region amount)'
```

## CSV details

- `,`-separated, `"`-quoted fields; `""` is an escaped quote; quoted fields may
  contain commas.
- A row is a line — **fields may not contain newlines** (required for parallel
  chunking; matches csvm).
- UTF-8 in and out. On output, a field is quoted only if it contains a comma,
  quote, or carriage return.

## Differences from csvm

- Command language is Lisp (tulisp), not the C++ DSL; `sort` → `sort-by`,
  `!cols(...)` → `(drop-cols ...)`.
- `to_num`/`to_str` are implicit (see above).
- `=~` regex matching is implemented (csvm left it as a stub).
- Proper RFC-4180 quote handling on input and output.
- `--sort-buffer` is new. `-n` defaults to 1 (like csvm); for a seekable file,
  `-n N` shards the input across N threads.

## License

GPL-3.0.
