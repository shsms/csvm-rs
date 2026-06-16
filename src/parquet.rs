//! Optional `.parquet` input reader (feature `parquet`).
//!
//! Parquet carries its own typed schema, so numeric columns decode straight to
//! [`Field::Num`] — no `to-num` needed — and column names come from the schema
//! (so `hdr`/`--no-header` don't apply). Reading is batch-streaming: each arrow
//! `RecordBatch` (a row group, or a slice of one) is transposed from columnar
//! arrays into rows of owned `Field`s for the row-oriented plan. Only flat
//! primitive columns are supported (int / float / string / bool); nested or
//! other logical types error clearly. The metadata lives in the file footer, so
//! a seekable file is required — stdin is rejected upstream.
//!
//! Row-group-parallel reads (the natural sharding unit, mirroring the CSV
//! byte-range shards) are a follow-up; this first cut streams single-threaded.

use std::fs::File;
use std::path::Path;

use arrow::array::{
    Array, ArrayRef, BooleanArray, Float32Array, Float64Array, Int8Array, Int16Array, Int32Array,
    Int64Array, LargeStringArray, StringArray, UInt8Array, UInt16Array, UInt32Array, UInt64Array,
};
use arrow::datatypes::{DataType, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};

use crate::error::Error;
use crate::field::Field;

/// How many rows the arrow reader decodes per `RecordBatch`. Bounds the
/// streaming path's working memory and makes batching deterministic.
const BATCH_ROWS: usize = 8192;

/// A streaming parquet reader: pulls one `RecordBatch` at a time and transposes
/// it into owned rows.
pub struct ParquetReader {
    inner: ParquetRecordBatchReader,
}

fn open_builder(path: &Path) -> Result<ParquetRecordBatchReaderBuilder<File>, Error> {
    let file = File::open(path)?;
    ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| Error::Other(format!("cannot read parquet '{}': {e}", path.display())))
}

/// Read just the schema's column names, validating every column is a supported
/// flat primitive. Used to resolve the plan before reading data.
pub fn read_header(path: &Path) -> Result<Vec<String>, Error> {
    let builder = open_builder(path)?;
    let schema = builder.schema();
    validate_schema(schema)?;
    Ok(schema.fields().iter().map(|f| f.name().clone()).collect())
}

/// Reject any column whose type the row converter can't map (nested, temporal,
/// decimal, binary, …), naming the offending column and type.
fn validate_schema(schema: &Schema) -> Result<(), Error> {
    for f in schema.fields() {
        if !supported(f.data_type()) {
            return Err(Error::Other(format!(
                "parquet column '{}' has unsupported type {:?} \
                 (only flat int/float/string/bool columns are supported)",
                f.name(),
                f.data_type()
            )));
        }
    }
    Ok(())
}

fn supported(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Boolean
            | DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
            | DataType::Float32
            | DataType::Float64
            | DataType::Utf8
            | DataType::LargeUtf8
    )
}

impl ParquetReader {
    pub fn open(path: &Path) -> Result<ParquetReader, Error> {
        let builder = open_builder(path)?;
        validate_schema(builder.schema())?;
        let inner = builder
            .with_batch_size(BATCH_ROWS)
            .build()
            .map_err(|e| Error::Other(format!("cannot read parquet '{}': {e}", path.display())))?;
        Ok(ParquetReader { inner })
    }

    /// Pull the next batch as owned rows, or `None` at end of file.
    pub fn next_batch(&mut self) -> Option<Result<Vec<Vec<Field<'static>>>, Error>> {
        let batch = self.inner.next()?;
        Some(
            batch
                .map_err(|e| Error::Other(format!("parquet read error: {e}")))
                .and_then(|b| batch_to_rows(&b)),
        )
    }
}

/// Transpose a columnar `RecordBatch` into row-major owned `Field`s.
fn batch_to_rows(batch: &RecordBatch) -> Result<Vec<Vec<Field<'static>>>, Error> {
    let ncols = batch.num_columns();
    let nrows = batch.num_rows();
    let cols: Vec<&ArrayRef> = (0..ncols).map(|c| batch.column(c)).collect();
    let mut rows = Vec::with_capacity(nrows);
    for r in 0..nrows {
        let mut row = Vec::with_capacity(ncols);
        for arr in &cols {
            row.push(field_at(arr, r)?);
        }
        rows.push(row);
    }
    Ok(rows)
}

/// One cell as an owned `Field`. A null becomes the empty string (matching a CSV
/// empty cell); booleans render as csvm's `t`/`f`. Numeric columns decode to
/// `Field::Num` so the plan's implicit typing needs no `to-num`.
///
/// Numbers join the engine's `f64` model, so an `Int64`/`UInt64` magnitude above
/// 2^53 loses integer precision on read (the same limit CSV numbers hit) — no
/// exact-integer column type exists yet.
fn field_at(arr: &ArrayRef, i: usize) -> Result<Field<'static>, Error> {
    if arr.is_null(i) {
        return Ok(Field::Str(""));
    }
    macro_rules! num {
        ($ty:ty) => {{
            let a = arr
                .as_any()
                .downcast_ref::<$ty>()
                .expect("array type matches data_type");
            Field::Num(a.value(i) as f64)
        }};
    }
    Ok(match arr.data_type() {
        DataType::Boolean => {
            let a = arr
                .as_any()
                .downcast_ref::<BooleanArray>()
                .expect("array type matches data_type");
            Field::Str(if a.value(i) { "t" } else { "f" })
        }
        DataType::Int8 => num!(Int8Array),
        DataType::Int16 => num!(Int16Array),
        DataType::Int32 => num!(Int32Array),
        DataType::Int64 => num!(Int64Array),
        DataType::UInt8 => num!(UInt8Array),
        DataType::UInt16 => num!(UInt16Array),
        DataType::UInt32 => num!(UInt32Array),
        DataType::UInt64 => num!(UInt64Array),
        DataType::Float32 => num!(Float32Array),
        DataType::Float64 => num!(Float64Array),
        DataType::Utf8 => {
            let a = arr
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("array type matches data_type");
            Field::Owned(a.value(i).to_string())
        }
        DataType::LargeUtf8 => {
            let a = arr
                .as_any()
                .downcast_ref::<LargeStringArray>()
                .expect("array type matches data_type");
            Field::Owned(a.value(i).to_string())
        }
        // validate_schema rejects every other type before we reach a batch.
        other => return Err(Error::Other(format!("unsupported parquet type {other:?}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{BooleanArray, Float64Array, Int64Array, StringArray};
    use arrow::datatypes::{Field as AField, Schema};
    use parquet::arrow::ArrowWriter;
    use std::sync::Arc;

    /// Write a small mixed-type parquet fixture (with a null name) and return its
    /// path. Columns: id(Int64), name(Utf8, nullable), amount(Float64),
    /// flag(Boolean).
    fn write_fixture(tag: &str) -> std::path::PathBuf {
        let schema = Arc::new(Schema::new(vec![
            AField::new("id", DataType::Int64, false),
            AField::new("name", DataType::Utf8, true),
            AField::new("amount", DataType::Float64, false),
            AField::new("flag", DataType::Boolean, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![1_i64, 2, 3])),
                Arc::new(StringArray::from(vec![Some("a"), None, Some("c")])),
                Arc::new(Float64Array::from(vec![5.0, 20.0, 15.0])),
                Arc::new(BooleanArray::from(vec![true, false, true])),
            ],
        )
        .unwrap();
        let path =
            std::env::temp_dir().join(format!("csvm-pq-{tag}-{}.parquet", std::process::id()));
        let file = File::create(&path).unwrap();
        let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        path
    }

    /// Write `n` rows of id(Int64)/amount(Float64), for the multi-batch and
    /// empty-file paths. With `n > BATCH_ROWS` the reader yields several batches.
    fn write_n(tag: &str, n: i64) -> std::path::PathBuf {
        let schema = Arc::new(Schema::new(vec![
            AField::new("id", DataType::Int64, false),
            AField::new("amount", DataType::Float64, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from((0..n).collect::<Vec<_>>())),
                Arc::new(Float64Array::from(
                    (0..n).map(|i| i as f64).collect::<Vec<_>>(),
                )),
            ],
        )
        .unwrap();
        let path =
            std::env::temp_dir().join(format!("csvm-pq-{tag}-{}.parquet", std::process::id()));
        let file = File::create(&path).unwrap();
        let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        path
    }

    fn run_script(path: &std::path::Path, script: &str) -> String {
        let opts = crate::exec::RunOpts {
            chunk_size: 1 << 20,
            threads: 1,
            temp_dir: std::env::temp_dir(),
            sort_buffer: 1 << 20,
        };
        let mut plan = crate::parse::parse(script).unwrap();
        let header = read_header(path).unwrap();
        let out_header = plan.resolve(&header).unwrap();
        let mut buf = Vec::new();
        crate::exec::run_parquet(&plan, &out_header, &opts, path, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn cells(rows: &[Vec<Field<'static>>]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|r| r.iter().map(|f| f.as_str().into_owned()).collect())
            .collect()
    }

    #[test]
    fn reads_schema_header_and_typed_rows() {
        let path = write_fixture("hdr");
        assert_eq!(
            read_header(&path).unwrap(),
            ["id", "name", "amount", "flag"]
        );

        let mut reader = ParquetReader::open(&path).unwrap();
        let rows = reader.next_batch().unwrap().unwrap();
        // Numeric columns decode to Field::Num; a null is the empty string;
        // booleans render t/f.
        assert!(matches!(rows[0][0], Field::Num(n) if n == 1.0)); // id
        assert!(matches!(&rows[1][1], Field::Str(s) if s.is_empty())); // null name
        assert!(matches!(rows[1][2], Field::Num(n) if n == 20.0)); // amount
        assert_eq!(rows[0][3].as_str(), "t"); // flag true
        assert_eq!(rows[1][3].as_str(), "f"); // flag false
        assert_eq!(
            cells(&rows),
            vec![
                vec!["1", "a", "5", "t"],
                vec!["2", "", "20", "f"],
                vec!["3", "c", "15", "t"],
            ]
        );
        assert!(reader.next_batch().is_none()); // single batch
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unsupported_column_type_errors_by_name() {
        let schema = Arc::new(Schema::new(vec![AField::new(
            "ts",
            DataType::Timestamp(arrow::datatypes::TimeUnit::Millisecond, None),
            false,
        )]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(arrow::array::TimestampMillisecondArray::from(
                vec![0_i64],
            ))],
        )
        .unwrap();
        let path = std::env::temp_dir().join(format!("csvm-pq-bad-{}.parquet", std::process::id()));
        let file = File::create(&path).unwrap();
        let mut w = ArrowWriter::try_new(file, schema, None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();

        let err = read_header(&path).unwrap_err().to_string();
        assert!(err.contains("ts") && err.contains("unsupported"), "{err}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn run_parquet_streams_and_aggregates() {
        let path = write_fixture("run");
        let opts = crate::exec::RunOpts {
            chunk_size: 1 << 20,
            threads: 1,
            temp_dir: std::env::temp_dir(),
            sort_buffer: 1 << 20,
        };
        let run = |script: &str| -> String {
            let mut plan = crate::parse::parse(script).unwrap();
            let header = read_header(&path).unwrap();
            let out_header = plan.resolve(&header).unwrap();
            let mut buf = Vec::new();
            crate::exec::run_parquet(&plan, &out_header, &opts, &path, &mut buf).unwrap();
            String::from_utf8(buf).unwrap()
        };
        // Streaming transform: numeric compare needs no `to-num` (typed input).
        assert_eq!(
            run("select amount > 10"),
            "id,name,amount,flag\n2,,20,f\n3,c,15,t\n"
        );
        // Blocking stage (materialize path): global aggregate over the column.
        assert_eq!(run("agg sum(amount)"), "amount_sum\n40\n");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn streams_and_aggregates_across_multiple_batches() {
        // 20k rows > BATCH_ROWS (8192), so the reader yields 3 batches; this
        // exercises the per-batch out_buf reuse and the cross-batch materialize.
        let n = 20_000;
        let path = write_n("multi", n);
        // Streaming filter spanning batch boundaries keeps every row.
        assert_eq!(
            run_script(&path, "select id >= 0").lines().count(),
            1 + n as usize
        );
        // A subset that straddles batches (ids 10000..19999 → 10000 rows).
        assert_eq!(
            run_script(&path, "select id >= 10000").lines().count(),
            1 + 10_000
        );
        // Materialize path folds all batches into one aggregate.
        assert_eq!(
            run_script(&path, "agg count, max(id)"),
            "count,id_max\n20000,19999\n"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn empty_file_yields_header_only() {
        let path = write_n("empty", 0);
        assert_eq!(read_header(&path).unwrap(), ["id", "amount"]);
        // No data rows: the streaming path writes just the header.
        assert_eq!(run_script(&path, "select id >= 0"), "id,amount\n");
        let mut reader = ParquetReader::open(&path).unwrap();
        // Arrow may emit zero batches, or batches that are all empty.
        while let Some(batch) = reader.next_batch() {
            assert!(batch.unwrap().is_empty());
        }
        std::fs::remove_file(&path).ok();
    }
}
