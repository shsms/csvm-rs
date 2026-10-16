//! Running a script from another program: [`run`] takes a script and an input
//! file, runs it in-process as the CLI would, and returns the output as typed
//! columns instead of CSV text.

use std::borrow::Cow;
use std::path::Path;

use crate::error::Error;
use crate::exec::{self, RunOpts};
use crate::plan::{OutputFormat, Plan, Sources};
use crate::progress::Progress;
use crate::{cli, csv, field, parse, pattern};

/// What a script made of its input.
#[derive(Debug)]
pub struct Output {
    pub columns: Vec<Column>,
    /// One line per thing the run ignored (a `graph` sink).
    pub notices: Vec<String>,
}

/// One output column.
#[derive(Debug)]
pub struct Column {
    pub name: String,
    pub values: Values,
}

#[derive(Debug)]
pub enum Values {
    /// Every non-empty cell parsed as a number; empty cells are NaN.
    Num(Vec<f64>),
    Text(Vec<String>),
}

/// Runs `script` over `input`, a `.parquet` file or else a CSV file with a
/// header line. A blank script passes the input through. A script error keeps
/// its place in the script ([`Error::span`]).
pub fn run(script: &str, input: &Path) -> Result<Output, Error> {
    let mut plan = if script.trim().is_empty() {
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
    exec::prepare_from(&mut plan, None, &mut pattern::RealFs)?;
    exec::prepare_joins(&mut plan)?;
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
    Ok(Output {
        columns: columns(&text),
        notices,
    })
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

/// The CSV `text` (header first) as columns.
fn columns(text: &str) -> Vec<Column> {
    let mut rows: Vec<Vec<Cow<str>>> = Vec::new();
    csv::parse_chunk(text, |r| {
        rows.push(r.drain(..).map(|f| f.into_text()).collect())
    });
    let slices: Vec<&[Cow<str>]> = rows.iter().map(Vec::as_slice).collect();
    let numeric = exec::numeric_columns(&slices);
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
            let values = if numeric.get(i) == Some(&true) || blank {
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
        assert!(out.columns.iter().all(|c| match &c.values {
            Values::Num(v) => v.is_empty(),
            Values::Text(v) => v.is_empty(),
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
