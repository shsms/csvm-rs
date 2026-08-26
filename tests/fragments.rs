//! End-to-end test for `fn` pipeline fragments: the motivating power-factor
//! pipeline from the design spec, written with fragments.

use csvm::exec::{self, RunOpts};
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_csv(content: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "csvm_fragments_{}_{}.csv",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, content).unwrap();
    path
}

fn run(script: &str, input: &str) -> Result<String, String> {
    let mut plan = csvm::parse::parse(script).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(input.as_bytes());
    let header = match plan.input_header.as_deref() {
        Some(h) => h.to_vec(),
        None => exec::read_header(&mut reader).map_err(|e| e.to_string())?,
    };
    exec::prepare_joins(&mut plan).map_err(|e| e.to_string())?;
    let out_header = plan.resolve(&header).map_err(|e| e.to_string())?;
    let opts = RunOpts {
        chunk_size: 64,
        threads: 1,
        temp_dir: std::env::temp_dir(),
        sort_buffer: 1 << 20,
    };
    let mut out = Vec::new();
    exec::run(&plan, &out_header, &opts, &mut reader, &mut out).map_err(|e| e.to_string())?;
    String::from_utf8(out).map_err(|e| e.to_string())
}

#[test]
fn power_factor_pipeline_with_fragments() {
    let reactive = temp_csv("timestamp,metric,value\n1,q,3\n2,q,4\n");
    let script = format!(
        "fn prep(n) {{ rename value=n | cols -v metric }}\n\
         fn pf(t, a, r) {{ add t abs(a) / sqrt(a*a + r*r) }}\n\
         prep(active)\n\
         join (prep(reactive)) {} on timestamp\n\
         pf(pf_col, active, reactive)",
        reactive.display()
    );
    let out = run(&script, "timestamp,metric,value\n1,p,4\n2,p,3\n").unwrap();
    assert_eq!(
        out,
        "timestamp,active,reactive,pf_col\n\
         1,4,3,0.8\n\
         2,3,4,0.6\n"
    );
}
