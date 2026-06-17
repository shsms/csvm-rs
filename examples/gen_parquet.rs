//! Deterministic parquet generator for trying / benchmarking the `parquet`
//! feature. Requires `--features parquet`.
//!
//! ```sh
//! cargo run --release --features parquet --example gen_parquet -- 100000 /tmp/data.parquet
//! cargo run --release --features parquet -- 'select amount > 50000' /tmp/data.parquet
//! ```
//!
//! Same column shape as `gen_csv` (id, grp, region, amount, price, flag), but
//! typed: ints and a float decode straight to numbers, so no `to-num` is needed.

use std::env;
use std::fs::File;
use std::sync::Arc;

use arrow::array::{Float64Array, Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;

fn main() {
    let mut args = env::args().skip(1);
    let rows: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let path = args
        .next()
        .unwrap_or_else(|| "/tmp/data.parquet".to_string());

    let regions = ["north", "south", "east", "west"];
    let (mut id, mut grp, mut region, mut amount, mut price, mut flag) =
        (vec![], vec![], vec![], vec![], vec![], vec![]);
    for i in 0..rows {
        let h = splitmix64(i);
        id.push(i as i64);
        grp.push((i % 7) as i64);
        region.push(regions[(h % 4) as usize]);
        amount.push((h % 100_000) as i64);
        price.push((h % 100_000) as f64 / 100.0);
        flag.push(if h & 1 == 0 { "t" } else { "f" });
    }

    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("grp", DataType::Int64, false),
        Field::new("region", DataType::Utf8, false),
        Field::new("amount", DataType::Int64, false),
        Field::new("price", DataType::Float64, false),
        Field::new("flag", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from(id)),
            Arc::new(Int64Array::from(grp)),
            Arc::new(StringArray::from(region)),
            Arc::new(Int64Array::from(amount)),
            Arc::new(Float64Array::from(price)),
            Arc::new(StringArray::from(flag)),
        ],
    )
    .expect("build batch");

    // Cap the row-group size so a large file has several groups — the unit of
    // read parallelism (`csvm -n N ... data.parquet`).
    let props = WriterProperties::builder()
        .set_max_row_group_row_count(Some(65_536))
        .build();
    let file = File::create(&path).expect("create output");
    let mut writer = ArrowWriter::try_new(file, schema, Some(props)).expect("open writer");
    writer.write(&batch).expect("write batch");
    writer.close().expect("close writer");
    eprintln!("wrote {rows} rows to {path}");
}

/// splitmix64: a pure function of the index, so the file is reproducible.
fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
