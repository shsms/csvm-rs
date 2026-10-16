//! Running a script from another program: [`run`] takes a script and an input
//! file, runs it in-process as the CLI would, and returns the output as typed
//! columns instead of CSV text.

use std::borrow::Cow;
use std::path::Path;

use crate::color::Style;
use crate::error::Error;
use crate::exec::{self, RunOpts};
use crate::plan::{OutputFormat, Plan, Sources, Stage};
use crate::progress::Progress;
use crate::{cli, csv, field, parse, pattern};

/// What a script made of its input.
#[derive(Debug)]
pub struct Output {
    pub columns: Vec<Column>,
    /// The cells the script's `color` rules paint, row by row.
    pub paints: Vec<Paint>,
    /// One line per thing the run ignored (a `graph` sink).
    pub notices: Vec<String>,
}

/// One output column.
#[derive(Debug)]
pub struct Column {
    pub name: String,
    pub values: Values,
}

/// One painted cell. `row` counts the data rows from 0, the header not counted;
/// a rule that paints a whole row paints each of its cells.
#[derive(Debug)]
pub struct Paint {
    pub row: usize,
    pub col: usize,
    pub style: Style,
}

#[derive(Debug)]
pub enum Values {
    /// Every non-empty cell parsed as a number; empty cells are NaN.
    Num(Vec<f64>),
    Text(Vec<String>),
}

/// Runs `script` over `input`, a `.parquet` file or else a CSV file with a
/// header line. A blank script, or one of comments only, passes the input
/// through. A script error keeps its place in the script ([`Error::span`]).
pub fn run(script: &str, input: &Path) -> Result<Output, Error> {
    run_with(script, input, &|_| true)
}

/// [`run`], reading a file the script joins only when `may_read` allows its
/// canonical path, and only a regular file.
pub fn run_with(
    script: &str,
    input: &Path,
    may_read: &dyn Fn(&Path) -> bool,
) -> Result<Output, Error> {
    // As the CLI does, an error says when a `|` may be missing.
    run_inner(script, input, may_read).map_err(|e| parse::note_missing_pipe(script, e))
}

fn run_inner(
    script: &str,
    input: &Path,
    may_read: &dyn Fn(&Path) -> bool,
) -> Result<Output, Error> {
    let mut plan = if parse::strip_comments(script).trim().is_empty() {
        Plan {
            stages: Vec::new(),
            output: OutputFormat::Csv,
            colors: Vec::new(),
            graph: None,
            sources: Sources::default(),
            from: None,
        }
    } else {
        parse::parse(script)?
    };
    if plan.from.is_some() {
        let e = Error::Compile("from: the input is given by the caller".into());
        return Err(match plan.sources.from.span.clone() {
            Some(span) => e.at(span),
            None => e,
        });
    }
    exec::expand_joins(&mut plan, None, &mut pattern::RealFs)?;
    check_joins(&plan, may_read)?;
    exec::read_join_headers(&mut plan)?;
    let mut notices = Vec::new();
    // The caller draws its own table and charts.
    plan.output = OutputFormat::Csv;
    if plan.graph.take().is_some() {
        notices.push("graph: ignored here; the caller draws its own charts".to_owned());
    }
    let opts = RunOpts {
        chunk_size: 1 << 20,
        threads: 1,
        temp_dir: std::env::temp_dir(),
        sort_buffer: 256 << 20,
        shard_bytes: exec::DEFAULT_SHARD_BYTES,
    };
    let progress = Progress::default();
    let mut buf = Vec::new();
    if cli::is_parquet_path(input) {
        run_parquet(&mut plan, input, &opts, &progress, &mut buf)?;
    } else {
        let (header, data_start, len) = exec::read_header_from_path(input)?;
        let out_header = plan.resolve(&header)?;
        let file = exec::InputFile {
            path: input,
            data_start,
            len,
        };
        exec::run_file(&plan, &out_header, &opts, file, &progress, &mut buf)?;
    }
    let text = String::from_utf8(buf)
        .map_err(|e| Error::Other(format!("output is not valid UTF-8: {}", e.utf8_error())))?;
    let mut rows: Vec<Vec<Cow<str>>> = Vec::new();
    csv::parse_records(&text, |r| {
        rows.push(r.drain(..).map(|f| f.into_text()).collect())
    });
    let rows: Vec<&[Cow<str>]> = rows.iter().map(Vec::as_slice).collect();
    Ok(Output {
        columns: columns(&rows),
        paints: paints(&plan, &rows),
        notices,
    })
}

/// Refuses a joined file that `may_read` does not allow, or that is not a
/// regular file, in `plan` and the plans it joins.
fn check_joins(plan: &Plan, may_read: &dyn Fn(&Path) -> bool) -> Result<(), Error> {
    for stage in &plan.stages {
        if let Stage::Join(j) = stage {
            check_joins(&j.right_plan, may_read)?;
            let path = Path::new(&j.file)
                .canonicalize()
                .map_err(|e| Error::Other(format!("{}: {e}", j.file)))?;
            if !path.is_file() || !may_read(&path) {
                return Err(Error::Other(format!("{}: may not be read here", j.file)));
            }
        }
    }
    Ok(())
}

#[cfg(feature = "parquet")]
fn run_parquet(
    plan: &mut Plan,
    input: &Path,
    opts: &RunOpts,
    progress: &Progress,
    buf: &mut Vec<u8>,
) -> Result<(), Error> {
    let header = crate::parquet::read_header(input)?;
    let out_header = plan.resolve(&header)?;
    exec::run_parquet(plan, &out_header, opts, input, progress, buf)
}

#[cfg(not(feature = "parquet"))]
fn run_parquet(
    _: &mut Plan,
    _: &Path,
    _: &RunOpts,
    _: &Progress,
    _: &mut Vec<u8>,
) -> Result<(), Error> {
    Err(Error::Other(cli::NO_PARQUET.into()))
}

/// Row `row`'s cell `i`, empty past the row's end.
fn cell<'r>(row: &'r [Cow<str>], i: usize) -> &'r str {
    row.get(i).map_or("", |c| c.as_ref())
}

/// The cells `plan`'s colour rules paint in `rows` (header first).
fn paints(plan: &Plan, rows: &[&[Cow<str>]]) -> Vec<Paint> {
    if plan.colors.is_empty() {
        return Vec::new();
    }
    let grid = exec::compute_styles(&plan.colors, rows);
    // A ragged row's extra cells are not columns.
    let width = rows.first().map_or(0, |header| header.len());
    let mut paints = Vec::new();
    for (ri, row) in grid.into_iter().enumerate().skip(1) {
        for (col, style) in row.into_iter().enumerate().take(width) {
            if !style.is_empty() {
                paints.push(Paint {
                    row: ri - 1,
                    col,
                    style,
                });
            }
        }
    }
    paints
}

/// Whether `cell` would lose its spelling as a number: a leading `+`, or a
/// leading zero before another digit.
fn id_like(cell: &str) -> bool {
    let digits = cell.strip_prefix('-').unwrap_or(cell);
    cell.starts_with('+')
        || (digits.len() > 1 && digits.starts_with('0') && digits.as_bytes()[1].is_ascii_digit())
}

/// The output `rows` (header first) as columns.
fn columns(rows: &[&[Cow<str>]]) -> Vec<Column> {
    let numeric = exec::numeric_columns(rows);
    let Some((header, data)) = rows.split_first() else {
        return Vec::new();
    };
    header
        .iter()
        .enumerate()
        .map(|(i, name)| {
            // A column whose cells are all empty is a series with no data:
            // numbers. A header-only output has no cells to tell, so text.
            let blank = !data.is_empty()
                && data
                    .iter()
                    .all(|row| field::trim_cell(cell(row, i)).is_empty());
            // `02134` and `+5` read as numbers, but they are names: text.
            let named = data
                .iter()
                .any(|row| id_like(field::trim_cell(cell(row, i))));
            let values = if (numeric.get(i) == Some(&true) && !named) || blank {
                Values::Num(
                    data.iter()
                        .map(|row| field::trim_cell(cell(row, i)).parse().unwrap_or(f64::NAN))
                        .collect(),
                )
            } else {
                Values::Text(data.iter().map(|row| cell(row, i).to_owned()).collect())
            };
            Column {
                name: name.to_string(),
                values,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{Base, Color};

    /// Writes `text` to a file named after `tag` in the temp dir.
    fn csv(tag: &str, text: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("csvm-embed-{tag}-{}.csv", std::process::id()));
        std::fs::write(&path, text).unwrap();
        path
    }

    fn nums(out: &Output, name: &str) -> Vec<f64> {
        let col = out.columns.iter().find(|c| c.name == name).expect(name);
        match &col.values {
            Values::Num(v) => v.clone(),
            Values::Text(v) => panic!("{name} is text: {v:?}"),
        }
    }

    fn same(a: &[f64], b: &[f64]) -> bool {
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .all(|(x, y)| x == y || (x.is_nan() && y.is_nan()))
    }

    #[test]
    fn blank_script_is_identity() {
        let out = run("  \n", &csv("blank", "timestamp,x\nt1,1\nt2,\n")).unwrap();
        let names: Vec<_> = out.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["timestamp", "x"]);
        match &out.columns[0].values {
            Values::Text(v) => assert_eq!(v, &["t1", "t2"]),
            Values::Num(v) => panic!("timestamp is numeric: {v:?}"),
        }
        assert!(same(&nums(&out, "x"), &[1.0, f64::NAN]));
    }

    #[test]
    fn add_and_select_run() {
        let out = run("add y = x * 2 | select y > 1", &csv("add", "x\n0\n1\n")).unwrap();
        assert!(same(&nums(&out, "y"), &[2.0]));
    }

    #[test]
    fn header_only_output_keeps_columns() {
        let out = run("select x > 100", &csv("none", "t,x\na,1\n")).unwrap();
        assert_eq!(out.columns.len(), 2);
        // No cell tells the type: text.
        assert!(out.columns.iter().all(|c| match &c.values {
            Values::Text(v) => v.is_empty(),
            Values::Num(v) => panic!("{} is numeric: {v:?}", c.name),
        }));
    }

    #[test]
    fn numeric_column_with_gaps_is_num() {
        let out = run("select x != 'no'", &csv("gaps", "x\n1\n\n3\n")).unwrap();
        assert!(same(&nums(&out, "x"), &[1.0, f64::NAN, 3.0]));
    }

    #[test]
    fn all_empty_column_is_num() {
        // A series with no data: numbers, not text.
        let out = run("cols x", &csv("allempty", "t,x\na,\nb,\n")).unwrap();
        assert!(same(&nums(&out, "x"), &[f64::NAN, f64::NAN]));
    }

    #[test]
    fn a_newline_in_a_cell_keeps_the_rows() {
        let out = run("add z = 'p\nq'", &csv("newline", "x\n1\n2\n")).unwrap();
        assert!(same(&nums(&out, "x"), &[1.0, 2.0]));
        match &out.columns[1].values {
            Values::Text(v) => assert_eq!(v, &["p\nq", "p\nq"]),
            Values::Num(v) => panic!("numeric: {v:?}"),
        }
    }

    #[test]
    fn paints_stay_inside_the_columns() {
        let out = run("color red x > 0", &csv("ragged", "x\n1\n2,3\n")).unwrap();
        assert!(!out.paints.is_empty());
        assert!(out.paints.iter().all(|p| p.col < out.columns.len()));
    }

    #[test]
    fn a_script_of_comments_is_blank() {
        let out = run("# nothing yet\n", &csv("comment", "x\n1\n")).unwrap();
        assert!(same(&nums(&out, "x"), &[1.0]));
    }

    #[test]
    fn a_missing_pipe_is_named() {
        let e = run("add y = x\nselect y > 1", &csv("pipe", "x\n1\n")).unwrap_err();
        assert!(e.to_string().contains("missing `|`"), "{e}");
    }

    #[test]
    fn id_like_columns_stay_text() {
        let out = run("", &csv("ids", "id,plus,n\n02134,+5,0\n00001,+6,-0.5\n")).unwrap();
        for name in ["id", "plus"] {
            let col = out.columns.iter().find(|c| c.name == name).unwrap();
            assert!(matches!(col.values, Values::Text(_)), "{name}");
        }
        assert!(same(&nums(&out, "n"), &[0.0, -0.5]));
    }

    #[test]
    fn joined_files_are_checked() {
        let dir = std::env::temp_dir();
        let right = dir.join(format!("csvm-embed-right-{}.csv", std::process::id()));
        std::fs::write(&right, "k,v\n1,a\n").unwrap();
        let script = format!("join '{}' on k", right.display());
        let input = csv("joinpolicy", "k\n1\n");
        let e = run_with(&script, &input, &|_| false).unwrap_err();
        assert!(e.to_string().contains("may not be read"), "{e}");
        let out = run_with(&script, &input, &|p| p == right.canonicalize().unwrap()).unwrap();
        assert_eq!(out.columns.len(), 2);
    }

    #[test]
    fn text_column_stays_text() {
        let out = run("cols side", &csv("text", "side\nlag\nlead\n")).unwrap();
        match &out.columns[0].values {
            Values::Text(v) => assert_eq!(v, &["lag", "lead"]),
            Values::Num(v) => panic!("numeric: {v:?}"),
        }
    }

    #[test]
    fn fmt_dropped_graph_noted() {
        let input = csv("sinks", "t,x\n1,2\n2,3\n");
        let out = run("fmt -s", &input).unwrap();
        assert!(same(&nums(&out, "x"), &[2.0, 3.0]));
        assert!(out.notices.is_empty(), "{:?}", out.notices);
        let out = run("graph line t x", &input).unwrap();
        assert!(same(&nums(&out, "x"), &[2.0, 3.0]));
        assert_eq!(
            out.notices,
            ["graph: ignored here; the caller draws its own charts"]
        );
    }

    #[test]
    fn unknown_column_error_has_span() {
        let e = run("add y = nope * 2", &csv("unknown", "x\n1\n")).unwrap_err();
        assert_eq!(e.span(), Some(8..12), "{e}");
    }

    #[test]
    fn parse_error_has_span() {
        let e = run("add y = (", &csv("parse", "x\n1\n")).unwrap_err();
        assert!(e.span().is_some(), "{e}");
    }

    #[test]
    fn from_is_refused_at_its_place() {
        let e = run("from other.csv | cols x", &csv("from", "x\n1\n")).unwrap_err();
        assert_eq!(e.span().map(|s| s.start), Some(0), "{e}");
    }

    #[test]
    fn gradient_paints_numeric_cells() {
        let out = run("color -g x", &csv("grad", "x\n0\n5\n10\n")).unwrap();
        let fg: Vec<_> = out
            .paints
            .iter()
            .map(|p| (p.row, p.col, p.style.fg))
            .collect();
        assert_eq!(fg.len(), 3, "{fg:?}");
        assert!(
            fg.iter()
                .all(|&(_, col, c)| col == 0 && matches!(c, Some(Color::Rgb(_))))
        );
        assert_eq!(fg.iter().map(|p| p.0).collect::<Vec<_>>(), [0, 1, 2]);
        assert_ne!(fg[0].2, fg[2].2);
    }

    #[test]
    fn gradient_skips_empty_cells() {
        let out = run("color -g x", &csv("gradgap", "x\n0\n\n10\n")).unwrap();
        let rows: Vec<_> = out.paints.iter().map(|p| p.row).collect();
        assert_eq!(rows, [0, 2]);
    }

    #[test]
    fn predicate_with_prev_paints_whole_rows() {
        // `b` falls by more than 100 into the second row and the fourth.
        let input = csv("prev", "a,b\n1,500\n2,300\n3,290\n4,100\n5,90\n");
        let out = run("color red b - prev(b) < -100", &input).unwrap();
        let cells: Vec<_> = out.paints.iter().map(|p| (p.row, p.col)).collect();
        assert_eq!(cells, [(1, 0), (1, 1), (3, 0), (3, 1)]);
        assert!(
            out.paints
                .iter()
                .all(|p| p.style.fg == Some(Color::Base(Base::Red)))
        );
    }

    #[test]
    fn no_color_rules_no_paints() {
        let out = run("add y = x", &csv("plain", "x\n1\n")).unwrap();
        assert!(out.paints.is_empty());
    }

    #[cfg(feature = "parquet")]
    #[test]
    fn parquet_input_runs() {
        use arrow::array::{Float64Array, RecordBatch, StringArray};
        use arrow::datatypes::{DataType, Field, Schema};
        use parquet::arrow::ArrowWriter;
        use std::sync::Arc;

        let schema = Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::Utf8, false),
            Field::new("x", DataType::Float64, true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(vec!["t1", "t2"])),
                Arc::new(Float64Array::from(vec![Some(1.5), None])),
            ],
        )
        .unwrap();
        let path =
            std::env::temp_dir().join(format!("csvm-embed-pq-{}.parquet", std::process::id()));
        let mut writer =
            ArrowWriter::try_new(std::fs::File::create(&path).unwrap(), schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        let out = run("add y = x + 1", &path).unwrap();
        assert!(same(&nums(&out, "y"), &[2.5, 1.0]));
        assert!(same(&nums(&out, "x"), &[1.5, f64::NAN]));
    }
}
