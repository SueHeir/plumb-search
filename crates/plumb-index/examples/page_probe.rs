//! Future authorized use: page_probe INDEX_DIR REQUEST_JSON.
//! Building this default-off example does not authorize index access.
use plumb_index::page_probe::{executable_digest, run, Request, MAX_REQUEST_BYTES};
use std::io::Read;
use std::path::PathBuf;

fn main() {
    if execute().is_err() {
        // Keep parse, filesystem and binding errors out of output: they
        // can contain private paths or stored data supplied by an index.
        eprintln!("page probe inconclusive: invalid input or executable/source binding");
        std::process::exit(2);
    }
}

fn execute() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let index = PathBuf::from(args.next().ok_or_else(|| anyhow::anyhow!("index"))?);
    let input = PathBuf::from(args.next().ok_or_else(|| anyhow::anyhow!("request"))?);
    anyhow::ensure!(args.next().is_none(), "arguments");
    let mut bytes = Vec::new();
    std::fs::File::open(input)?
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= MAX_REQUEST_BYTES, "request cap");
    let request: Request = serde_json::from_slice(&bytes)?;
    let source =
        option_env!("PLUMB_DIAGNOSTIC_SOURCE_COMMIT").ok_or_else(|| anyhow::anyhow!("source"))?;
    anyhow::ensure!(source == request.binding.diagnostic_source_commit, "source");
    anyhow::ensure!(
        executable_digest(&std::env::current_exe()?)? == request.binding.diagnostic_binary_sha256,
        "binary"
    );
    let report = run(&index, &request);
    println!("{}", serde_json::to_string(&report)?);
    if !report.complete {
        std::process::exit(2);
    }
    Ok(())
}
