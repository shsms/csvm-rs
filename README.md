# csvm-rs

A fast, multithreaded CSV manipulation tool. Pipelines are written in a small
shell-pipe-style language:

```sh
csvm "select amount > 1000 && flag == 't' | cols -v flag" input.csv
```

The script is **parsed once** into a plain-Rust execution plan; the per-row hot
path runs that plan with **no interpreter involved**.

## Build

```sh
cargo build --release        # binary at target/release/csvm
cargo test                   # unit + integration tests
make install                 # build --release, then install to your user bin dir
```

No external services or path dependencies; just `cargo build`. `make install`
builds the release binary and copies it to your XDG user-binaries directory
(`systemd-path user-binaries`, typically `~/.local/bin`).

## Usage

```
csvm [-o OUT] [-n THREADS] [-f FILE] [-t TEMPDIR] [--chunk-size SIZE]
     [--sort-buffer SIZE] [--color WHEN] [--print-engine] [SCRIPT] [INPUT]
```

The input file is an optional **second positional**, like `awk 'prog' file`:
`csvm 'select x > 1' data.csv`. Omit it (or pass `-`) to read stdin.

| Argument / flag    | Meaning                                                     |
|--------------------|-------------------------------------------------------------|
| `SCRIPT`           | the pipeline (first positional; required unless `-f`)       |
| `INPUT`            | input file (optional next positional; default stdin, `-` ⇒ stdin) |
| `-o, --output OUT` | output file (default: stdout)                               |
| `-n, --threads N`  | worker threads (default: 1; `<=0` ⇒ 1)                      |
| `-f, --file FILE`  | read the pipeline from `FILE` (awk-style; then `SCRIPT` is omitted) |
| `-t, --temp-dir`   | directory for sort spill files (default: system temp)       |
| `--chunk-size SIZE`| input chunk size; `K`/`M`/`G` suffix ok (default: 1 000 000) |
| `--sort-buffer SIZE`| in-memory budget before `sort` spills; `K`/`M`/`G` ok (default 256 MiB) |
| `--no-header`      | input has no header row; columns are named `c1, c2, …`      |
| `--color WHEN`     | `auto` (TTY only), `always`, `never`; honors `NO_COLOR`/`CLICOLOR_FORCE` |
| `--print-engine`   | print the compiled plan and exit                            |
| `-h, --help`       | usage overview (`csvm help CMD` for one command's detail)   |
| `-V, --version`    | print version and exit                                      |

Long options also accept the `--flag=value` form (e.g. `--color=always`).

The first input line is the header; columns are referenced by name. For a
seekable file, `-n N` shards the work across N threads. A streaming input
(stdin) emits output as rows arrive rather than buffering a full chunk first.

## The command language

A script is a sequence of stages separated by `|` (or by a newline — see below).
Each stage is a command with comma- or space-separated arguments:

| Command            | Does                                                       |
|--------------------|------------------------------------------------------------|
| `cols a,b,c`       | keep these columns, in this order                          |
| `cols -v a,b`      | keep everything *except* these columns                     |
| `select EXPR`      | keep rows where `EXPR` is true                             |
| `sort SPEC...`     | sort rows (stable, multi-key)                              |
| `head [N]`         | keep the first `N` rows (default 10; `head -n -N` keeps all *but* the last N) |
| `tail [N]`         | keep the last `N` rows (default 10; blocking)              |
| `uniq [cols]`      | drop duplicate rows, keeping the first (whole row or by key; global) |
| `stats [cols]`     | summary stats per column (count/empty/min/max/sum/mean/stddev) |
| `group cols`       | group-by keys for a following `agg` (alone: count rows per key) |
| `agg FN(col)… [by cols]` | aggregate per group (count/sum/min/max/mean/stddev) |
| `join [(SUB)] FILE on KEYS` | merge a right-side file in by key (inner/left/right/full) |
| `color …`          | colour output by condition or value gradient (rendered with `fmt`) |
| `rename old=new …` | rename columns (header only; row data unchanged)           |
| `add NAME EXPR`    | append a computed column (replaces `NAME` in place if it exists) |
| `delta [-s SUF] a,b` | append `col_delta = col - prev(col)` per column (shorthand) |
| `hdr a,b,c`        | supply column names for headerless input (must come first) |
| `fmt`              | whitespace-aligned table (`column -t`); numbers right-justified |
| `graph hist COL`   | terminal histogram of a numeric column (sink; must be last) |
| `to-num a,b` / `to-str a,b` | mark columns numeric / string (usually unnecessary) |

Arguments may be separated by commas or spaces (`cols a,b,c` ≡ `cols a b c`).
`to_num`/`to_str` (underscore) are accepted too. `where`/`filter` are aliases for
`select`, `cut` for `cols`, and `dedup` for `uniq`. A `#` outside quotes starts a
comment to end of line. A column name with a comma or space can be
backtick-quoted in any command — `` cols `first, last`,age ``.

### Per-command detail

The table above is the overview. For any command's exact forms, flags, and an
example, run **`csvm help CMD`** (e.g. `csvm help join`, `csvm help add`).
Concept pages: `csvm help operators` (the `select`/value operators), `colors`,
`types`, `expr` (the `add` grammar), and `sizes`. This help is generated from the
same registry the parser uses, so it never drifts from the implementation.

A few things worth knowing up front:

- **`select` takes a bare expression** — `select amount > 1000 && flag == 't'`.
  Single-quote the whole script so the shell keeps `>`/`|`/spaces. Operators:
  `== != < > <= >=`, `=~`/`!~` (regex), `^=`/`*=`/`$=` (begins/contains/ends),
  and `&& || !` with parens; a leading `-v` negates the whole expression.
- **Conversions are implicit.** A comparison against a number is numeric, against
  a string lexical — `amount > 1000` just works, and numbers print correctly with
  no `to-str`. `to-num`/`to-str` remain as explicit overrides. Empty coerces to
  `0`; a non-numeric value where a number is required aborts the run.
- **`add` / `delta`** compute columns: `add total amount * qty`, and
  `add rate amount - prev(amount)` — or the shorthand `delta amount` — for the
  step-to-step difference. An `add` using `prev()`/`rownum()` runs ordered and
  single-threaded, so its output is identical at any `-n`; a pure `add` shards.

### Long pipelines: script files

A long pipeline is hard to read as one quoted string. Stages split on a newline
just as they do on `|`, and `#` starts a comment, so put the pipeline in a file
and run it with `-f` (the input is then the positional argument — no `cat … |`):

```sh
csvm -f pipeline.csvm data.csv
```

```text
# pipeline.csvm — one stage per line, comments allowed
rename value=a
select a > 1000
join (rename value=b) b.csv on key
delta a b                  # a_delta, b_delta
color -g a b a_delta b_delta
cols key a a_delta b b_delta
fmt
```

A trailing `|` at the end of a line is optional — a newline alone separates
stages (it doesn't inside a `join (…)` group, whose own stages split normally).

## Examples

```sh
# keep three columns, in order
csvm 'cols id,region,amount' input.csv

# drop a column, write to a file
csvm -o out.csv 'cols -v flag' input.csv

# filter (numeric — no to-num needed), then drop the filter column
csvm 'select flag == "t" && (amount > 0 || qty > 0) | cols -v flag' input.csv

# filter, then numeric reverse sort
csvm 'select amount > 0 | sort amount=nr' input.csv

# computed column, then a step-to-step delta
csvm 'add total amount * qty | delta total | fmt' input.csv

# inner join on a key, then sort the result
csvm 'join prices.csv on sku | sort price=nr | head' sales.csv

# per-column profile of every column, aligned
csvm 'stats | fmt' input.csv

# group-by: total and mean amount per region, biggest first
csvm 'group region | agg sum(amount),mean(amount) | sort amount_sum=nr | fmt' sales.csv

# terminal histogram of a column's distribution (after a filter)
csvm 'select region == "EU" | graph hist amount --bins 12' sales.csv

# colour negative amounts red, aligned (a TTY, or --color always)
csvm 'color red amount < 0 | fmt' input.csv
```

`--print-engine` shows the compiled, resolved plan:

```
$ csvm --print-engine "to-num amount | select amount > 0 | sort amount=r | cols -v flag" input.csv
stage 1 (transform):
  1.1 to-num ["amount"] (positions [3])
  1.2 select (> amount[3] 0 :num)
stage 2 (sort):
  2.1 sort amount[3] reverse numeric
stage 3 (transform):
  3.1 drop-cols -> keep positions [0, 1, 3]
```

## How it works

- **Parse once.** `parse.rs` turns the script into a plain-Rust `Plan` (a
  quote-aware stage/argument tokenizer plus a recursive-descent parser for the
  `select` and `add` expressions). The hot path runs the plan — no interpreter is
  retained.
- **Parallel by sharding.** With `-n N` over a seekable file, the data region is
  split into N line-aligned byte ranges and each worker reads and processes its
  own range — no central reader or channel. Shard outputs are concatenated in
  file order, so results are identical regardless of thread count. (stdin can't
  seek, so it streams.)
- **Zero-copy.** Fields are sliced straight out of the chunk buffer; only an
  unescaped `""` or a parsed number allocates.
- **Sort runs in parallel and scales past memory.** `sort` farms input blocks to
  `-n` workers that parse, serialize each row once, compute an order-preserving
  key, and sort their block into a run (kept in memory, or spilled to a temp file
  past `--sort-buffer`). Groups of runs are merged in parallel (multi-level, so
  huge inputs stay file-descriptor-bounded), then a binary-heap k-way merge
  copies the already-serialized line bytes out in order.

## Benchmarking

`examples/gen_csv.rs` generates a deterministic 10-column CSV (same row count ⇒
same bytes), so benchmarks are reproducible:

```sh
cargo run --release --example gen_csv -- 3000000 /tmp/huge.csv   # ~151 MB
time ./target/release/csvm -n 8 \
  "select flag == 't' && amount > 50000 && status =~ '^a' | cols id,region,amount" /tmp/huge.csv
```

For repeatable in-process microbenchmarks, `cargo bench` runs a
[criterion](https://github.com/bheisler/criterion.rs) suite (`benches/pipeline.rs`)
over an in-memory dataset — parse, projection, filtering, conversion, sorting,
and alignment — reporting per-operation throughput.

## CSV details

- `,`-separated, `"`-quoted fields; `""` is an escaped quote; quoted fields may
  contain commas. LF and CRLF line endings; trailing newline optional.
- **RFC 4180-conformant with one deviation:** a row is a line, so **fields may
  not contain embedded newlines** (this is what lets the input be chunked and
  sharded in parallel). `tests/csv_conformance.rs` checks field-for-field
  agreement with the `csv` crate on well-formed input and pins the deviation.
- UTF-8 in and out. On output, a field is quoted only if it contains a comma,
  quote, or carriage return.

## Roadmap

The pipe language is built to grow. Planned: conditional colouring for `fmt`,
pluggable formats via a `Source`/`Sink` trait (Parquet, TSV), and more
terminal-native charts (`graph bar/scatter/line/spark`; `graph hist` ships
today). See `todo.org` for design notes.

## License

GPL-3.0.
