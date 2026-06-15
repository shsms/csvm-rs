# csvm-rs

A fast, multithreaded CSV manipulation tool — a Rust port of
[csvm](https://github.com/shsms/csvm) (originally C++). Pipelines are written in
a small shell-pipe-style language:

```sh
csvm "select fieldA == 't' && countZ > 0 | cols -v fieldA" input.csv
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

`csvm help` prints the overview; `csvm help CMD` shows one command's forms and
an example (`csvm help color`, `csvm help join`, …); `csvm help TOPIC` covers
`operators`, `colors`, `types`, and `sizes`.

## The command language

A script is a sequence of stages separated by `|`. Each stage is a command with
comma- or space-separated arguments:

| Command            | Does                                                       |
|--------------------|------------------------------------------------------------|
| `cols a,b,c`       | keep these columns, in this order                          |
| `cols -v a,b`      | keep everything *except* these columns                     |
| `select EXPR`      | keep rows where `EXPR` is true                             |
| `sort SPEC...`     | sort rows (stable, multi-key)                              |
| `head [N]`         | keep the first `N` rows (default 10; `head -n N`, `head -N`; `head -n -N` keeps all *but* the last N) |
| `tail [N]`         | keep the last `N` rows (default 10; blocking — buffers the tail) |
| `uniq [cols]`      | drop duplicate rows, keeping the first (whole row, or by key `cols`; global) |
| `stats [cols]`     | summary stats per column (count/empty/min/max/sum/mean/stddev) |
| `join [(SUB)] FILE on KEYS` | merge a right-side file in by key (inner/left/right/full) |
| `color …`          | colour output by condition or value gradient (rendered with `fmt`) |
| `rename old=new …` | rename columns (header only; row data unchanged)           |
| `add NAME EXPR`    | append a computed column (replaces `NAME` in place if it exists) |
| `delta [-s SUF] cols` | append `col_delta = col - prev(col)` per column (shorthand) |
| `hdr a,b,c`        | supply column names for headerless input (must come first) |
| `fmt`              | whitespace-aligned table (`column -t`); numbers right-justified |
| `to-num a,b` / `to-str a,b` | mark columns numeric / string (usually unnecessary) |

Arguments may be separated by commas or spaces (`cols a,b,c` ≡ `cols a b c`).
`to_num`/`to_str` (underscore) are accepted too. `where`/`filter` are aliases for
`select`, `cut` for `cols`, and `dedup` for `uniq`. A `#` outside quotes starts a comment to end of
line (handy in a `-f` script file). A column name with a comma or space can be
backtick-quoted in any command — `` cols `first, last`,age `` — as in `select`.

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

### `select` expressions

A bare infix expression (no surrounding quotes — only string *literals* are
quoted). Single-quote the whole script so the shell doesn't eat `>`, `|`,
backticks, or spaces.

- **Column reference**: a bare identifier — `fieldA`. A name that isn't a bare
  identifier (e.g. it contains `-`) is wrapped in backticks: `` `frequenz-app-edge` ``.
- **Literals**: numbers (`0`, `3.14`, `-5`); strings in single or double quotes
  (`'t'`).
- **Comparisons**: `==` (or `=`), `!=`, `<`, `>`, `<=`, `>=`.
- **Substring**: `col ^= 'pre'` (begins with), `col *= 'mid'` (contains),
  `col $= 'suf'` (ends with) — literal, so no regex escaping; negate with `!(…)`.
- **Regex**: `col =~ 'pattern'`, `col !~ 'pattern'` (Rust `regex` syntax).
- **Logic**: `&&`, `||`, `!`, and parentheses. `&&`/`||` short-circuit. `||` is
  *not* mistaken for a stage `|`.

```sh
select fieldA == 't' && (countZ > 0 || countA > 0)
```

Two handy consequences: a parenthesized expression is just `select (…)`, and
chaining `select`s ANDs them — `select a > 0 | select b == 't'`.

A leading `-v` negates the **whole** expression (like `cols -v`): `select -v EXPR`
*drops* the rows matching `EXPR` — i.e. `!(EXPR)`, so `select -v a > 0 || b > 0`
keeps only rows where neither holds.

### `sort` specs

Each spec is a bare column or `col=flags` (`col:flags` also works), where flags
combine:

- `r` — reverse (descending).
- `n` — compare numerically rather than lexically.

```sh
sort fieldA fieldB=r countZ=nr      # by fieldA asc, fieldB desc, countZ numeric desc
```

### `add`

`add NAME EXPR` appends a column computed per row from `EXPR`, a *value*
expression over the other columns. If `NAME` already exists it is replaced in
place (so `add price price * 1.1` works); otherwise it is appended at the end
(reorder afterwards with `cols`). Types are implicit, as in `select`: arithmetic
yields a number, string ops yield text, and column refs coerce by context.

```sh
csvm 'add total amount * qty' data.csv          # arithmetic
csvm 'add margin (price - cost) / price * 100' data.csv
csvm 'add full first ++ " " ++ last' data.csv   # ++ concatenates (never +)
csvm 'add tier amount > 1000 ? "big" : "small"' data.csv   # ternary
csvm 'add ok amount > 0' data.csv               # a bare comparison yields t/f
```

The expression grammar (loosest to tightest precedence): `?:` ternary, `++`
concat, `+ -`, `* / %`, unary `-`, then atoms — a column, a number, a quoted
string, a parenthesised expression, or a function call.

- **Functions**: `round floor ceil abs int` (numeric, one arg), `min max`
  (numeric, one or more), `len` (text length), `upper lower trim` (text),
  `coalesce` (first non-empty). `split`/regex/date functions are not yet
  implemented.
- **Errors**: arithmetic on a non-number, or divide/modulo by zero, aborts the
  run (like `to-num`) rather than emitting a blank.

#### Row-to-row values (`prev`, `rownum`)

Two functions reach beyond the current row:

- `prev(col)` — `col`'s value in the **previous** row (the current row's own
  cell on the first row, so a step delta is `0` there rather than the full
  value).
- `rownum()` — the **1-based** index of the current row.

So the difference between successive rows of a column — the rate of change at
each step — is just:

```sh
csvm 'add rate amount - prev(amount)' data.csv
```

Because they depend on row order, an `add` using `prev()`/`rownum()` runs
**single-threaded and in input order** (it can't be sharded), so its output is
identical at any `-n`. `rownum()` reflects input order even before a later
`sort`. A pure `add` (no `prev`/`rownum`) still shards and streams like any other
transform.

#### `delta` — step differences in one word

Computing a delta for several columns with `add` is repetitive, so `delta` is
shorthand: `delta a b` appends `a_delta`/`b_delta` exactly as
`add a_delta a - prev(a) | add b_delta b - prev(b)` would. `-s SUFFIX` overrides
the `_delta` suffix.

```sh
csvm 'delta a b | fmt' data.csv             # a_delta, b_delta
csvm 'delta -s _change a | fmt' data.csv    # a_change
```

### `stats`

`stats` reduces the input to one row per column — a quick profile, like
`describe`. With no arguments it profiles every column; `stats a,b` limits it to
the named columns.

| Output column         | Meaning                                                |
|-----------------------|--------------------------------------------------------|
| `field`               | the column name                                        |
| `count` / `empty`     | non-empty / empty cells                                |
| `min` / `max`         | numeric range for numeric columns, lexical for text    |
| `sum` / `mean` / `stddev` | numeric columns only (sample stddev); blank for text |

A column counts as numeric when every non-empty cell parses as a number. `stats`
is a blocking, reducing stage, so it composes: filter first, then sort/limit the
profile after it. Over a file with `-n` threads it is computed in parallel and
merged; `count`/`min`/`max` are exact, while floating `sum`/`mean`/`stddev` may
differ from a single-threaded run in the last digit (parallel-reduction order).

```sh
csvm 'stats | fmt' data.csv                          # profile every column, aligned
csvm 'select region == "EU" | stats amount' data.csv # one column, after a filter
csvm 'stats | sort mean=nr | head 5 | fmt' data.csv  # the 5 columns with the largest mean
```

### `join`

`join [FLAGS] [(SUBPIPELINE)] FILE on KEYS` merges a second CSV (the **right**
side) into the stream by matching key columns. The main input is the **left**
side and keeps streaming; the right `FILE` is loaded fully into a hash table (so
make it the smaller of the two). It's an ordinary stage, so more stages can
follow it.

```sh
csvm 'join prices.csv on sku' sales.csv                  # inner join on `sku`
csvm 'join prices.csv on sku=item_id' sales.csv          # keys named differently
csvm 'join prices.csv on region,sku' sales.csv           # composite key
csvm 'join prices.csv on sku | sort price=nr | head' sales.csv  # ops after the join
```

- **Join type** (default inner): `-l`/`--left` keeps every left row, `-r`/`--right`
  every right row, `-F`/`--full` both; unmatched cells are filled empty (an
  unmatched right row carries the key value in the left key column).
- **Keys** (`on …`): a comma/space list; each entry is `name` (same on both
  sides) or `lname=rname` (different names). One-to-many matches fan out (a left
  row matching N right rows yields N output rows).
- **Output columns**: the left columns, then the right's non-key columns. The
  right key columns are dropped (redundant). A non-key name that clashes is
  auto-suffixed `_r` (`price` → `price_r`).
- **Suffixes**: `--lsuffix S` / `--rsuffix S` set the suffix applied to *clashing*
  names on each side (default: left none, right `_r`); non-clashing names are
  untouched. Or pre-`rename` either side for full control.
- **Right sub-pipeline**: an optional parenthesized csvm pipeline run over the
  right file before joining — `join (cols sku,price | select price > 0) prices.csv
  on sku`. It's the full language (filter, project, rename, even a nested join),
  so the right side can be shaped without a temp file.
- Key matching is exact string equality on the key cells (empty matches empty;
  `"1"` ≠ `"1.0"`). The right side must be a named file, not stdin.

### Colouring (`color`)

`color` attaches a colour rule to the output. Rules render when the output is a
terminal (or with `--color always`), and are most useful alongside `fmt`. Two
forms:

```sh
color COLOUR EXPR             # paint the whole row where EXPR holds
color -c COL COLOUR EXPR      # paint only COL's cell
color -g COLS [RAMP] [LO HI]  # gradient each COL by value (RAMP is lo:hi; defaults to green:red)
```

`color -g` accepts several columns at once, applying the same ramp and bounds to
each (`color -g a b c` is three gradient rules), so you don't repeat the stage
per column.

Colours are the eight ANSI names (plus `gray`), `bg:NAME` backgrounds, and the
`bold`/`dim`/`underline` attributes, combined with `+` (e.g. `bold+bg:red`).
This full spec applies to the **predicate** forms (`color COLOUR EXPR` and
`color -c`); a predicate reuses the `select` expression. A **gradient** is
narrower: its `RAMP` is two *plain* colour names (no `+`, `bg:`, or attributes)
and it paints the **foreground only**. The `RAMP` defaults to `green:red`, and
its range defaults to the column's min/max; pass `LO HI` to fix the range
(values outside take the endpoint colours), and `LO>HI` inverts it. Multiple
`color`s stack (last wins per attribute), and `--color auto|always|never`
controls emission (`auto` = only when stdout is a TTY).

```sh
csvm 'color red amount < 0 | color -c qty bold qty == 0 | fmt' data.csv
csvm 'color -g amount green:red 0 5000 | fmt' data.csv
```

### Headerless input (`hdr`)

Some CSVs have no header line — every line is data. `hdr` supplies the column
names: the whole input is treated as data, and the names are prepended on
output. It is plan-level metadata, so it must be the first command.

```sh
# input has no header; name the columns, then work with them
csvm 'hdr id,name,amount | select amount > 1000 | cols id,amount' raw.csv
```

Without `hdr`, the first input line is taken as the header (the default). If you
don't want to name the columns, the `--no-header` flag treats the whole input as
data and auto-names the columns `c1, c2, …` (referenced like any other name):

```sh
csvm --no-header 'select c3 > 1000 | cols c1,c3' raw.csv
```

### Conversions are implicit

csvm needs an explicit `to_num` before any numeric comparison/sort and a
`to_str` before output. Here that is automatic:

- A comparison against a **number** is numeric; against a **string** it is
  lexical. So `countZ > 0` just works — no `to-num` needed.
- Numbers always print correctly — no `to-str` needed.
- `to-num` / `to-str` remain available as explicit type overrides (they affect
  column-vs-column comparisons and a column's default sort mode).

Numeric coercion treats empty as `0`; a genuinely non-numeric value where a
number is required aborts the run with the offending value (as csvm does).

## Examples

csvm's README examples, in the pipe syntax:

```sh
# keep three columns, in order
csvm 'cols id,fieldA,countZ' < input.csv

# drop a column, write to a file
csvm -o out.csv 'cols -v fieldA' < input.csv

# filter rows
csvm "select fieldA == 't' && countZ != '0'" input.csv

# numeric filter (no to-num needed)
csvm "select fieldA == 't' && (countZ > 0 || countA > 0)" input.csv

# filter then drop the filter column
csvm "select fieldA == 't' | cols -v fieldA" input.csv

# filter, forward sort by fieldA, reverse sort by fieldB
csvm "select fieldA != 't' | sort fieldA fieldB=r" input.csv

# numeric filter and numeric reverse sort
csvm "select countA > 0 | sort countA=nr" input.csv
```

`--print-engine` shows the compiled, resolved plan:

```
$ csvm --print-engine "to-num countZ | select countZ > 0 | sort countZ=r | cols -v fieldB" input.csv
stage 1 (transform):
  1.1 to-num ["countZ"] (positions [4])
  1.2 select (> countZ[4] 0)
stage 2 (sort):
  2.1 sort countZ[4] reverse numeric
stage 3 (transform):
  3.1 drop-cols -> keep positions [0, 1, 2, 3]
```

## How it works

- **Parse once.** `parse.rs` turns the script into a plain-Rust `Plan` (a
  quote-aware stage/argument tokenizer plus a recursive-descent parser for the
  `select` expression). The hot path runs the plan — no interpreter is retained.
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

Benchmarked against the C++ csvm on this file (warm cache), csvm-rs is faster on
filter, projection, and sort at both `-n 1` and `-n 8` (sort by ~3×), with
byte-identical output.

For repeatable in-process microbenchmarks, `cargo bench` runs a
[criterion](https://github.com/bheisler/criterion.rs) suite (`benches/pipeline.rs`)
over an in-memory dataset — parse, projection, filtering, conversion, sorting,
and alignment — reporting per-operation throughput.

## CSV details

- `,`-separated, `"`-quoted fields; `""` is an escaped quote; quoted fields may
  contain commas. LF and CRLF line endings; trailing newline optional.
- **RFC 4180-conformant with one deviation:** a row is a line, so **fields may
  not contain embedded newlines** (this is what lets the input be chunked and
  sharded in parallel; csvm has the same constraint). `tests/csv_conformance.rs`
  checks field-for-field agreement with the `csv` crate on well-formed input and
  pins the deviation.
- UTF-8 in and out. On output, a field is quoted only if it contains a comma,
  quote, or carriage return.

## Differences from csvm

- Pipe command language (`|` stages), not the C++ DSL: `!cols(...)` → `cols -v`,
  `sort(a, b:r)` → `sort a b=r`.
- `to_num`/`to_str` are implicit (see above).
- `=~` regex matching is implemented (csvm left it as a stub).
- Proper RFC-4180 quote handling on input and output.
- `--sort-buffer` is new; `-n` defaults to 1 (like csvm) and shards seekable
  files.
- Input is a positional argument (`csvm SCRIPT [INPUT]`, awk-style) rather than
  csvm's `-f IN`; `-o` for output is unchanged.

## Roadmap

The pipe language is built to grow. Planned: conditional colouring for `fmt`,
pluggable formats via a `Source`/`Sink` trait (Parquet, TSV), and group-by
aggregation. See `todo.org` for design notes. (`join` and the computed `add`
column are implemented — see above.)

## License

GPL-3.0.
