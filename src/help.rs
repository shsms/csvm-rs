//! Help text — a single registry that drives the overview (`--help` / `csvm
//! help`), per-command help (`csvm help CMD`), and topic pages (`csvm help
//! colors`). Keeping one source of truth (rather than a hand-maintained usage
//! string beside the parser) is what stops the help from drifting out of sync
//! with the commands; a test cross-checks it against the parser's command list.

use crate::error::did_you_mean;

/// Help for one command. `synopsis` lists its forms; `detail` is a short
/// paragraph; `examples` are full `csvm '…'` invocations.
pub struct CmdHelp {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub summary: &'static str,
    pub synopsis: &'static [&'static str],
    pub detail: &'static str,
    pub examples: &'static [&'static str],
}

/// A concept page that isn't a single command (operators, colours, …).
pub struct Topic {
    pub name: &'static str,
    pub summary: &'static str,
    pub body: &'static str,
}

/// The synopsis line(s) shown on usage errors and atop the overview.
pub fn usage_line() -> &'static str {
    "usage: csvm [OPTIONS] SCRIPT [INPUT]\n       csvm [OPTIONS] -f FILE [INPUT]"
}

const HEADER: &str = "  SCRIPT   pipe-syntax pipeline; quote it so the shell keeps | > and spaces
  INPUT    input CSV (default: stdin; '-' is stdin); first line is the header

options (--flag VALUE or --flag=VALUE):
  -o, --output FILE    write to FILE (default: stdout)
  -f, --file FILE      read the pipeline from FILE instead of SCRIPT
      --no-header      input has no header; name columns c1, c2, ...
      --format FMT     input format: csv (default) | parquet (auto by extension;
                       parquet needs a build with --features parquet)
  -n, --threads N      worker threads for a seekable file (default: 1)
  -t, --temp-dir DIR   directory for sort spill files (default: system temp)
      --chunk-size SZ  input read chunk; K/M/G suffix ok (default: 1M)
      --sort-buffer SZ in-memory budget before sort spills; K/M/G (default: 256M)
      --color WHEN     auto (TTY only) | always | never
      --print-engine   print the compiled plan and exit
  -h, --help           show this help
  -V, --version        print version and exit";

const EXAMPLES: &str = "\
examples:
  csvm 'select flag == \"t\" && amount > 1000 | cols id,amount' data.csv
  csvm 'sort score=nr | head 5 | fmt' data.csv
  csvm 'stats | sort mean=nr | fmt' data.csv
  csvm 'join prices.csv on sku | select qty > 0' sales.csv";

/// Render help for `topic`: `None` is the overview; a command name/alias or a
/// topic name is its detail page; anything else is an error with a near-match
/// hint (suitable for printing to stderr).
pub fn render(topic: Option<&str>) -> Result<String, String> {
    match topic {
        None => Ok(overview()),
        Some(t) => find_command(t)
            .map(render_command)
            .or_else(|| find_topic(t).map(render_topic))
            .ok_or_else(|| unknown(t)),
    }
}

/// The full `--help` / `csvm help` text: usage, options, the command list and
/// topics (generated from the registry), and examples.
pub fn overview() -> String {
    let width = COMMANDS.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let mut commands = String::from("commands (chain with |; `csvm help CMD` for detail):\n");
    for c in COMMANDS {
        let alias = if c.aliases.is_empty() {
            String::new()
        } else {
            format!("  (alias: {})", c.aliases.join(", "))
        };
        commands.push_str(&format!(
            "  {:<width$}  {}{}\n",
            c.name,
            c.summary,
            alias,
            width = width
        ));
    }
    let topics: Vec<&str> = TOPICS.iter().map(|t| t.name).collect();
    format!(
        "{}\n\n{}\n\n{}\ntopics ({}): see `csvm help TOPIC`\n\n{}",
        usage_line(),
        HEADER,
        commands,
        topics.join(", "),
        EXAMPLES
    )
}

fn render_command(c: &CmdHelp) -> String {
    let mut out = format!("  {} — {}", c.name, c.summary);
    if !c.aliases.is_empty() {
        out.push_str(&format!("  (alias: {})", c.aliases.join(", ")));
    }
    out.push_str("\n\nforms:\n");
    for form in c.synopsis {
        out.push_str(&format!("  {form}\n"));
    }
    if !c.detail.is_empty() {
        out.push_str(&format!("\n{}\n", c.detail));
    }
    if !c.examples.is_empty() {
        out.push_str("\nexamples:\n");
        for ex in c.examples {
            out.push_str(&format!("  {ex}\n"));
        }
    }
    out.trim_end().to_string()
}

fn render_topic(t: &Topic) -> String {
    format!("  {} — {}\n\n{}", t.name, t.summary, t.body)
}

fn find_command(name: &str) -> Option<&'static CmdHelp> {
    COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name))
}

fn find_topic(name: &str) -> Option<&'static Topic> {
    TOPICS.iter().find(|t| t.name == name)
}

/// Message for an unknown `csvm help` topic, with a near-match suggestion.
fn unknown(t: &str) -> String {
    let mut names: Vec<&str> = Vec::new();
    for c in COMMANDS {
        names.push(c.name);
        names.extend(c.aliases.iter().copied());
    }
    names.extend(TOPICS.iter().map(|t| t.name));
    let hint = match did_you_mean(t, &names) {
        Some(s) => format!(" (did you mean `{s}`?)"),
        None => String::new(),
    };
    format!("no help for `{t}`{hint}\nrun `csvm help` for the list of commands and topics")
}

pub const COMMANDS: &[CmdHelp] = &[
    CmdHelp {
        name: "cols",
        aliases: &["cut"],
        summary: "keep or drop columns",
        synopsis: &[
            "cols A,B,C      keep these columns, in this order",
            "cols -v A,B     keep everything except these",
        ],
        detail: "Reorders to the listed order. Names may be comma- or space-separated; \
backtick-quote a name containing a comma or space, e.g. `cols `first, last`,age`.",
        examples: &[
            "csvm 'cols id,amount' data.csv",
            "csvm 'cols -v notes' data.csv",
        ],
    },
    CmdHelp {
        name: "select",
        aliases: &["where", "filter"],
        summary: "keep or drop matching rows",
        synopsis: &[
            "select EXPR     keep rows where EXPR is true",
            "select -v EXPR  drop rows where EXPR is true",
        ],
        detail: "EXPR is a bare infix expression (quote only string literals, not the whole \
expression). See `csvm help operators` for the full operator set. Comparison operands are full \
value expressions — arithmetic, functions (see `csvm help expr`), parenthesized booleans \
(compared as t/f), prev(col), rownum() — not just columns and literals; a stateful comparison \
(prev/rownum) makes the run single-threaded and in input order. A comparison is numeric \
against a statically numeric side (a number literal, arithmetic, a numeric function, a to-num \
column), lexical against a statically string side (a string literal, ++ concat, a bool, a \
to-str column); an ordering (< > <= >=) between two untyped operands auto-detects per row \
(numeric when both sides parse as numbers, else lexical), while == / != between untyped \
operands stay lexical.",
        examples: &[
            "csvm 'select amount > 1000 && flag == \"t\"' data.csv",
            "csvm 'select price * qty >= 1000' data.csv",
            "csvm 'select name =~ \"^A\"' data.csv",
        ],
    },
    CmdHelp {
        name: "sort",
        aliases: &[],
        summary: "stable multi-key sort",
        synopsis: &[
            "sort COL ...           sort by each column (lexical)",
            "sort COL=nr ...        flags: n numeric, r reverse (also COL:nr)",
        ],
        detail: "Keys are applied left to right; the sort is stable. A column marked numeric by \
to-num defaults to a numeric sort.",
        examples: &["csvm 'sort region score=nr' data.csv"],
    },
    CmdHelp {
        name: "head",
        aliases: &[],
        summary: "keep the first N rows",
        synopsis: &[
            "head [N]        first N rows (default 10; also -n N, -nN, --lines N)",
            "head -n -N      keep all but the last N rows",
        ],
        detail: "Streams and stops early when there is no sort before it.",
        examples: &["csvm 'sort score=nr | head 5' data.csv"],
    },
    CmdHelp {
        name: "tail",
        aliases: &[],
        summary: "keep the last N rows",
        synopsis: &["tail [N]        last N rows (default 10; same count spellings as head)"],
        detail: "Blocking: it buffers the tail, so a plan with tail reads all input.",
        examples: &["csvm 'tail 20' data.csv"],
    },
    CmdHelp {
        name: "uniq",
        aliases: &["dedup"],
        summary: "drop duplicate rows, keep the first",
        synopsis: &[
            "uniq            dedup whole rows",
            "uniq COLS       dedup by these key columns",
        ],
        detail: "Global (not adjacent-only like Unix uniq), so the input need not be pre-sorted.",
        examples: &["csvm 'uniq email' contacts.csv"],
    },
    CmdHelp {
        name: "stats",
        aliases: &[],
        summary: "per-column summary statistics",
        synopsis: &[
            "stats           profile every column",
            "stats COLS      profile just these columns",
        ],
        detail: "Reduces the input to one row per column: field, count, empty, min, max, sum, \
mean, stddev. min/max are numeric for numeric columns, lexical for text; sum/mean/stddev are \
numeric-only. Composes with sort/head/fmt after it.",
        examples: &["csvm 'stats | sort mean=nr | fmt' data.csv"],
    },
    CmdHelp {
        name: "group",
        aliases: &[],
        summary: "set group-by keys for agg",
        synopsis: &[
            "group COLS              one row per distinct key",
            "group COLS | agg FNS    aggregate within each group",
        ],
        detail: "On its own, reduces to one row per key with a count of the group's rows. \
Followed by agg, the aggregates replace that count. The per-key sibling of stats.",
        examples: &["csvm 'group region | agg sum(amount) | fmt' sales.csv"],
    },
    CmdHelp {
        name: "agg",
        aliases: &[],
        summary: "aggregate rows per group",
        synopsis: &[
            "agg FN(col),... [by COLS]",
            "  FN: count sum min max mean stddev   (count alone counts rows)",
            "  by COLS: keys, or fuse with a preceding group; neither ⇒ one global row",
        ],
        detail: "Reduces to one row per key: the key columns then one column per aggregate, \
named col_func (e.g. amount_sum), or count. sum/mean/stddev are blank for a non-numeric column.",
        examples: &[
            "csvm 'group region | agg count, mean(amount) | fmt' sales.csv",
            "csvm 'agg sum(amount) by region,product' sales.csv",
        ],
    },
    CmdHelp {
        name: "graph",
        aliases: &["plot"],
        summary: "draw a terminal chart (sink)",
        synopsis: &[
            "graph hist COL              distribution of a numeric column",
            "graph bar LABEL VALUE       one bar per row (use after group-by)",
            "graph spark COL             one-line sparkline of a column",
            "graph scatter X Y[,Y2…]     points on a braille canvas",
            "graph line X Y[,Y2…]        connected points (multi-series)",
            "  flags: --bins N (hist)  --scale F (size ×F)  --title T  --svg",
        ],
        detail: "A sink: draws a chart from the columns reaching it instead of emitting CSV, so \
it must be the last command. Non-numeric/empty cells are dropped from the plot and reported. \
--scale F multiplies the default size; hist bins to Sturges' rule; bar is capped at 50 rows; \
scatter/line take multiple y-series, coloured with --color. A timestamp x is plotted on a true \
time axis; any other non-numeric x plots by row order. --svg emits an SVG document to the output \
instead of a terminal chart.",
        examples: &[
            "csvm 'select region == \"EU\" | graph hist amount' sales.csv",
            "csvm 'group region | agg sum(amount) | graph bar region amount_sum' sales.csv",
            "csvm 'graph line ts open,close --scale 1.5' prices.csv",
            "csvm 'graph hist amount --svg' data.csv -o chart.svg",
        ],
    },
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
    CmdHelp {
        name: "join",
        aliases: &[],
        summary: "merge one or more CSVs by key",
        synopsis: &[
            "join [FLAGS] ITEM[, ITEM...]      ITEM: [(SUB)] FILE [on KEYS]",
            "  FLAGS: -l/--left  -r/--right  -F/--full   (inner by default)",
            "         --lsuffix S  --rsuffix S   (suffix clashing columns)",
            "  KEYS:  name  or  lname=rname,  comma-separated for composite keys",
            "         every item has its own `on`, or one trailing `on` shared by all",
        ],
        detail: "Each FILE (a right side) is loaded fully and probed by the streamed left side, so \
make it the smaller one. (SUB) is an optional csvm sub-pipeline run over its FILE first. Several \
items join left to right, exactly like chaining one join per file. Quote a file path that \
contains a comma. After an `on`, a keyless entry reads as another key column — so a file \
missing its own `on` fails at resolve, with a hint. Output is the left columns \
plus each right's non-key columns; a clashing right name is suffixed _r.",
        examples: &[
            "csvm 'join prices.csv on sku' sales.csv",
            "csvm 'join -l (cols sku,price) prices.csv on sku=item' sales.csv",
            "csvm 'join pv.csv, batt.csv on timestamp' grid.csv",
        ],
    },
    CmdHelp {
        name: "rename",
        aliases: &[],
        summary: "rename columns (header only)",
        synopsis: &["rename OLD=NEW ...     rename one or more columns"],
        detail: "A header-only change; row data is untouched.",
        examples: &["csvm 'rename qty=quantity,amt=amount' data.csv"],
    },
    CmdHelp {
        name: "add",
        aliases: &[],
        summary: "append a computed column",
        synopsis: &["add NAME EXPR   append (or, if NAME exists, replace) a column = EXPR"],
        detail: "EXPR is a value expression over the row: arithmetic (+ - * / %, parens), \
string concat with ++, the functions round/floor/ceil/abs/int/sqrt/pow/exp/log/log10/log2/\
sign/min/max/len/upper/lower/trim/coalesce, a ternary TEST ? A : B, and constants. \
prev(col) is col's value in the previous row \
(the current cell on the first row, so a delta is 0 there) and rownum() is the 1-based row \
index — both make the run single-threaded and in input order. A bare comparison yields t/f. \
Arithmetic on a non-number, or divide/modulo by zero, aborts the run. See `csvm help expr`.",
        examples: &[
            "csvm 'add rate amount - prev(amount)' data.csv",
            "csvm 'add total price * qty | add tier total > 1000 ? \"big\" : \"small\"' data.csv",
        ],
    },
    CmdHelp {
        name: "delta",
        aliases: &[],
        summary: "append per-column step differences",
        synopsis: &[
            "delta COLS         append COL_delta = COL - prev(COL) for each column",
            "delta -s SUF COLS  use suffix SUF instead of _delta",
        ],
        detail: "Shorthand for the common cross-row difference: `delta a b` is exactly \
`add a_delta a - prev(a) | add b_delta b - prev(b)`. Like any prev()-based add it runs \
single-threaded and in input order, so its output is independent of -n; the first row's \
delta is 0.",
        examples: &[
            "csvm 'delta amount' data.csv",
            "csvm 'delta -s _change amount qty | fmt' data.csv",
        ],
    },
    CmdHelp {
        name: "to-num",
        aliases: &[],
        summary: "force columns to numeric",
        synopsis: &["to-num COLS     parse these columns as numbers (alias: to_num)"],
        detail: "Usually unnecessary (comparisons against a number literal coerce implicitly); \
use it to make column-vs-column comparisons or a sort numeric. A non-numeric cell aborts the run.",
        examples: &["csvm 'to-num qty | select qty > stock' data.csv"],
    },
    CmdHelp {
        name: "to-str",
        aliases: &[],
        summary: "force columns to string",
        synopsis: &["to-str COLS     treat these columns as text (alias: to_str)"],
        detail: "The counterpart to to-num; affects later column-vs-column comparisons and the \
default sort mode. Numbers always serialize correctly on output without it.",
        examples: &["csvm 'to-str zip' data.csv"],
    },
    CmdHelp {
        name: "hdr",
        aliases: &[],
        summary: "name columns of headerless input",
        synopsis: &["hdr A,B,C       supply column names (must be the first command)"],
        detail: "For input with no header line: the whole input is data and these names are \
prepended on output. (The CLI's --no-header instead auto-names columns c1, c2, ….)",
        examples: &["csvm 'hdr id,name,amount | select amount > 0' data.csv"],
    },
    CmdHelp {
        name: "color",
        aliases: &["colour"],
        summary: "colourise output",
        synopsis: &[
            "color COLOUR EXPR            paint the whole row where EXPR is true",
            "color -c COL COLOUR EXPR     paint only COL's cell",
            "color -g COLS [RAMP] [LO HI] gradient each COL by value (RAMP is lo:hi)",
        ],
        detail: "Rules render when output is a terminal (or with --color always); most useful \
with fmt. The predicate forms take a full colour spec (bg:, attributes, + to combine). A \
gradient is narrower: RAMP is two plain colour names, it paints the foreground only, defaults \
to green:red, and its range defaults to the column's min/max. See `csvm help colors`.",
        examples: &[
            "csvm 'color red amount < 0 | fmt' data.csv",
            "csvm 'color -g amount green:red 0 5000 | fmt' data.csv",
        ],
    },
    CmdHelp {
        name: "fmt",
        aliases: &[],
        summary: "whitespace-align the table",
        synopsis: &["fmt             align columns like `column -t` (takes no arguments)"],
        detail: "All-numeric columns are right-justified; text columns left-justified. Applied to \
the final output, so it composes after everything else.",
        examples: &["csvm 'stats | fmt' data.csv"],
    },
];

pub const TOPICS: &[Topic] = &[
    Topic {
        name: "operators",
        summary: "select / color expression operators",
        body: "  comparisons:  == (or =)  !=  <  >  <=  >=\n  \
substring:    ^= (begins)  *= (contains)  $= (ends)   — literal, no escaping\n  \
regex:        =~  !~        (Rust regex syntax)\n  \
logic:        &&  ||  !  ()         (&& and || short-circuit)\n  \
operands:     bare word = column; 3.14 = number; 'txt'/\"txt\" = string;\n               \
`name with spaces` = backtick-quoted column;\n               \
any value expression: arithmetic, functions, (bool) as t/f,\n               \
prev(col), rownum()   (see `csvm help expr`)\n  \
comment:      # to end of line (outside quotes)\n\n\
Compare mode: numeric with a statically numeric side (number literal, arithmetic,\n\
numeric function, to-num column); lexical with a statically string side (string\n\
literal, ++ concat, bool, to-str column). Two untyped operands: an ordering (< > <= >=)\n\
auto-detects per row (numeric if both cells are numbers, else lexical); == / != stay\n\
lexical.",
    },
    Topic {
        name: "colors",
        summary: "colour names and attributes for `color`",
        body: "  names:       black red green yellow blue magenta cyan white gray (grey)\n  \
foreground:  NAME              e.g. red\n  \
background:  bg:NAME           e.g. bg:red\n  \
attributes:  bold  dim  underline\n  \
combine:     join parts with +  e.g. bold+red, white+bg:red, underline+bg:blue\n\n\
The combine / bg / attribute forms above are for the PREDICATE rules — \
`color COLOUR EXPR` and `color -c COL COLOUR EXPR`. A GRADIENT (`color -g COL \
lo:hi`) is narrower: lo and hi are two plain colour names (no +, bg, or \
attributes) and it colours the foreground only; the ramp defaults to green:red.\n\n\
Emission is gated by --color auto|always|never (auto = only when stdout is a TTY); \
NO_COLOR and CLICOLOR_FORCE are honoured under auto.",
    },
    Topic {
        name: "expr",
        summary: "value expressions for `add`",
        body: "  arithmetic:  + - * / %  and parens   (numeric; coerces operands)\n  \
unary minus: -x\n  \
concat:      a ++ b ++ c        (string; never use + for text)\n  \
ternary:     TEST ? A : B        (TEST is a select-style comparison)\n  \
boolean:     a bare comparison yields t / f\n  \
functions:   round floor ceil abs int sign   (numeric, 1 arg)\n               \
sqrt exp log log10 log2  (numeric, 1 arg; IEEE at domain edges,\n                                        \
e.g. sqrt(-1) = NaN — no abort)\n               \
pow             (numeric, 2 args)\n               \
min max         (numeric, 1+ args)\n               \
len             (length of text)\n               \
upper lower trim (text, 1 arg)\n               \
coalesce        (first non-empty, 1+ args)\n  \
cross-row:   prev(col)   col in the previous row (current cell on row 1)\n               \
rownum()    1-based row index\n\n\
The same value grammar works as a comparison operand in select / ternary tests, \
so `select price * qty >= 30`, `abs(x) > 1`, and `(a >= 0) == (b >= 0)` all parse. \
prev() / rownum() make the run single-threaded and in input order (also from a select). \
Divide/modulo by zero, or arithmetic on a non-number, aborts the run. \
If NAME already exists, add replaces it in place; otherwise it is appended.",
    },
    Topic {
        name: "types",
        summary: "numeric vs string handling (implicit to-num/to-str)",
        body: "Columns are text by default. A comparison (select / color predicate) is numeric if \
either side is statically numeric (a number literal, arithmetic, a numeric function, a to-num \
column); else lexical if either side is statically string (a string literal, ++ concat, a bool, \
a to-str column); else two untyped operands auto-detect per row for an ordering \
(< > <= >=) — numeric if both sides parse as numbers, else lexical (to-str forces lexical) — \
while == / != stay lexical. A numeric literal or to-num thus wins over a to-str column. A column \
added by `add` carries its expression's static type — including one inherited from a to-num/\
to-str column or from a ternary whose branches agree — so later comparisons against it behave \
the same as against the expression itself. Sort is \
separate: `sort col` is lexical unless you pass =n or to-num the column — it does not \
auto-detect. Empty cells coerce to 0 in numeric context; a non-numeric cell in a strictly-numeric \
op aborts the run (auto falls back to lexical instead). Numbers always serialize correctly on \
output — no to-str needed just to print.",
    },
    Topic {
        name: "sizes",
        summary: "K/M/G suffixes for --chunk-size and --sort-buffer",
        body: "Size options accept a plain integer or a binary suffix: K = 1024, M = 1024², \
G = 1024³ (case-insensitive). E.g. --chunk-size 8M, --sort-buffer 1G. A non-positive value \
falls back to the default.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The help registry and the parser's command list must stay in lockstep:
    /// every command the parser knows has help, and every documented command is
    /// a real command. This is the guard against the help drifting out of sync.
    #[test]
    fn help_matches_parser_commands() {
        for &cmd in crate::parse::COMMANDS {
            assert!(
                find_command(cmd).is_some(),
                "parser command `{cmd}` has no help entry"
            );
        }
        for c in COMMANDS {
            assert!(
                crate::parse::COMMANDS.contains(&c.name),
                "documented command `{}` is not in the parser's command list",
                c.name
            );
        }
    }

    #[test]
    fn overview_lists_every_command() {
        let text = overview();
        for c in COMMANDS {
            assert!(text.contains(c.name), "overview omits `{}`", c.name);
        }
    }

    #[test]
    fn render_resolves_commands_aliases_and_topics() {
        assert!(
            render(Some("join"))
                .unwrap()
                .contains("merge one or more CSVs")
        );
        assert!(render(Some("cut")).unwrap().contains("cols —")); // alias resolves
        assert!(render(Some("colors")).unwrap().contains("magenta")); // topic
        assert!(render(None).unwrap().contains("commands")); // overview
    }

    #[test]
    fn unknown_topic_suggests_and_errors() {
        // `joen` is unambiguously nearest `join`; check the error names the bad
        // topic and offers a suggestion.
        let err = render(Some("joen")).unwrap_err();
        assert!(err.contains("no help for `joen`"), "{err}");
        assert!(err.contains("did you mean `join`"), "{err}");
    }
}
