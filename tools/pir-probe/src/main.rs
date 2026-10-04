//! Offline, single-server PIR feasibility probe. Not a production transport.
//! Uses upstream test profiles unchanged; no claim of reviewed security parameters.

use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{ensure, Context, Result};
use clap::{Parser, ValueEnum};
use serde::Serialize;
use spiral_rs::client::{Client, PublicParameters, Query};
use spiral_rs::params::Params;
use spiral_rs::server::{load_db_from_seek, process_query};
use spiral_rs::util::{get_fast_expansion_testing_params, params_from_json};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Profile {
    /// Upstream get_fast_expansion_testing_params: 256 rows of 8 KiB.
    UpstreamSmall,
    /// Upstream SDK e2e-tests/params/v1.json: 16,384 rows of 32 KiB.
    #[value(name = "upstream-16k")]
    Upstream16k,
}

impl Profile {
    fn params(self) -> Params {
        match self {
            Self::UpstreamSmall => get_fast_expansion_testing_params(),
            Self::Upstream16k => params_from_json(include_str!("../profiles/upstream-v1.json")),
        }
    }
}

#[derive(Parser)]
#[command(
    about = "Offline research probe for direct single-server PIR; does not change Plumb's search mode"
)]
struct Args {
    #[arg(long, value_enum, default_value = "upstream-small")]
    profile: Profile,
    /// Raw fixed rows, each containing an 8-byte LE payload length followed by bytes and zero padding.
    /// If absent, use synthetic rows. No record contents are printed.
    #[arg(long)]
    database: Option<PathBuf>,
    /// Preflight cap on raw plus preprocessed database storage; query scratch is additional.
    #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u64).range(1..=8192))]
    max_database_mib: u64,
    /// Number of private retrievals, evenly spread over the database, with fresh query randomness.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=32))]
    queries: u32,
}

#[derive(Debug, Serialize)]
struct Report {
    scheme: &'static str,
    library: &'static str,
    profile: String,
    research_only: bool,
    database_source: &'static str,
    rows: usize,
    row_bytes: usize,
    raw_database_bytes: usize,
    preprocessed_database_bytes: usize,
    preprocessing_ms: f64,
    client_setup_ms: f64,
    setup_bytes: usize,
    query_bytes: usize,
    request_bytes: usize,
    response_bytes: usize,
    all_retrievals_correct: bool,
    measurements: Vec<Measurement>,
}

#[derive(Debug, Serialize)]
struct Measurement {
    query_ms: f64,
    server_ms: f64,
    decode_ms: f64,
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn sizes(params: &Params) -> Result<(usize, usize)> {
    let raw = params.num_items().checked_mul(params.db_item_size);
    let preprocessed = params
        .num_items()
        .checked_mul(params.instances)
        .and_then(|n| n.checked_mul(params.n * params.n))
        .and_then(|n| n.checked_mul(params.poly_len))
        .and_then(|n| n.checked_mul(8));
    Ok((
        raw.context("raw database length overflow")?,
        preprocessed.context("preprocessed database length overflow")?,
    ))
}

fn encode_row(payload: &[u8], row_bytes: usize) -> Result<Vec<u8>> {
    ensure!(
        row_bytes >= 8 && payload.len() <= row_bytes - 8,
        "payload exceeds fixed row capacity; splitting requires a separate privacy design"
    );
    let mut row = vec![0; row_bytes];
    row[..8].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    row[8..8 + payload.len()].copy_from_slice(payload);
    Ok(row)
}

fn decode_row(row: &[u8], row_bytes: usize) -> Result<&[u8]> {
    ensure!(
        row.len() == row_bytes && row_bytes >= 8,
        "incorrect decoded row size"
    );
    let len = usize::try_from(u64::from_le_bytes(row[..8].try_into()?))?;
    ensure!(len <= row_bytes - 8, "invalid row payload length");
    ensure!(
        row[8 + len..].iter().all(|&b| b == 0),
        "nonzero row padding"
    );
    Ok(&row[8..8 + len])
}

fn synthetic(params: &Params) -> Result<Vec<u8>> {
    let (raw_bytes, _) = sizes(params)?;
    let mut data = Vec::with_capacity(raw_bytes);
    for index in 0..params.num_items() {
        // Deliberately variable payload lengths, hidden inside equally sized rows.
        let payload = serde_json::to_vec(&serde_json::json!([
            {"domain": format!("site{index}.example"), "title": format!("Synthetic site {index}")}
        ]))?;
        data.extend_from_slice(&encode_row(&payload, params.db_item_size)?);
    }
    Ok(data)
}

// Both objects contain an independent public randomness seed followed by u64
// coefficients. Reject bad lengths/coefficients before upstream assert/unwrap
// deserializers. This is not a general parser hardening audit.
fn validate_object(data: &[u8], expected: usize, modulus: u64) -> Result<()> {
    ensure!(
        data.len() == expected && expected >= 32 && (expected - 32).is_multiple_of(8),
        "invalid PIR object size"
    );
    // The size check above leaves no remainder.
    let (coefficients, _) = data[32..].as_chunks::<8>();
    for chunk in coefficients {
        ensure!(
            u64::from_ne_bytes(*chunk) < modulus,
            "PIR coefficient exceeds modulus"
        );
    }
    Ok(())
}

/// Server receives public setup and encrypted query bytes only. No index or query text.
fn answer(params: &Params, database: &[u64], request: &[u8]) -> Result<Vec<u8>> {
    let expected = params
        .setup_bytes()
        .checked_add(params.query_bytes())
        .context("request size overflow")?;
    ensure!(request.len() == expected, "incorrect request length");
    let (setup, query) = request.split_at(params.setup_bytes());
    validate_object(setup, params.setup_bytes(), params.modulus)?;
    validate_object(query, params.query_bytes(), params.modulus)?;
    let public = PublicParameters::deserialize(params, setup);
    let query = Query::deserialize(params, query);
    // Offline trusted fixture only. Upstream's server API is not exposed on HTTP.
    Ok(process_query(params, &public, &query, database))
}

fn generate_query(client: &Client<'_>, params: &Params, index: usize) -> Result<Vec<u8>> {
    ensure!(index < params.num_items(), "row index out of range");
    Ok(client.generate_query(index).serialize())
}

fn decode_reply(client: &Client<'_>, params: &Params, response: &[u8]) -> Result<Vec<u8>> {
    // Match the pinned server::encode bit framing before its infallible reader.
    let q1_bits = (4 * params.pt_modulus).ilog2() as usize;
    let bits = params.instances
        * (params.q2_bits as usize * params.n * params.poly_len
            + q1_bits * params.n * params.n * params.poly_len);
    ensure!(
        response.len() == bits.div_ceil(64) * 8,
        "incorrect response length"
    );
    let decoded = client.decode_response(response);
    // Upstream PolyMatrixRaw::to_vec adds 32 zero safety bytes and rounds
    // to 16 bytes. Both pinned test profiles have already aligned rows.
    ensure!(
        decoded.len() == params.db_item_size + 32
            && decoded[params.db_item_size..].iter().all(|&b| b == 0),
        "unexpected upstream decoding tail"
    );
    Ok(decoded[..params.db_item_size].to_vec())
}

fn open_database(path: &std::path::Path) -> Result<File> {
    ensure!(
        std::fs::symlink_metadata(path)?.is_file(),
        "database must be a regular file; symlinks excluded"
    );
    #[cfg(unix)]
    let file = File::from(rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?);
    #[cfg(not(unix))]
    let file = File::open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "database must be a regular file"
    );
    Ok(file)
}

fn run(args: Args) -> Result<Report> {
    ensure!(
        cfg!(target_endian = "little"),
        "upstream wire serialization requires a little-endian host in this probe"
    );
    let params = args.profile.params();
    let (raw_bytes, preprocessed_bytes) = sizes(&params)?;
    let budget = args.max_database_mib * 1024 * 1024;
    ensure!((raw_bytes as u64).saturating_add(preprocessed_bytes as u64) <= budget,
        "database needs {} MiB raw plus {} MiB preprocessed, exceeding --max-database-mib {}; query scratch is additional",
        raw_bytes / 1024 / 1024, preprocessed_bytes / 1024 / 1024, args.max_database_mib);
    let (data, source) = if let Some(path) = args.database {
        let file = open_database(&path).context("opening fixed-row database")?;
        ensure!(
            file.metadata()?.is_file() && file.metadata()?.len() == raw_bytes as u64,
            "database must be a regular file of exactly {raw_bytes} bytes"
        );
        let mut bytes = Vec::with_capacity(raw_bytes);
        file.take(raw_bytes as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() == raw_bytes,
            "database changed size while reading"
        );
        (bytes, "supplied fixed-row file")
    } else {
        (synthetic(&params)?, "synthetic")
    };
    // Freeze the fixture and provide full reads to upstream's seek loader.
    let mut input = Cursor::new(data);
    let started = Instant::now();
    let database = load_db_from_seek(&params, &mut input);
    let preprocessing_ms = elapsed_ms(started);
    let started = Instant::now();
    let mut client = Client::init(&params);
    let setup = client.generate_keys().serialize();
    let client_setup_ms = elapsed_ms(started);
    let mut measurements = Vec::new();
    let mut response_bytes = None;
    for n in 0..args.queries {
        let index = if args.queries == 1 {
            params.num_items() / 2
        } else {
            n as usize * (params.num_items() - 1) / (args.queries as usize - 1)
        };
        let mut expected = vec![0; params.db_item_size];
        input.seek(SeekFrom::Start((index * params.db_item_size) as u64))?;
        input.read_exact(&mut expected)?;
        decode_row(&expected, params.db_item_size)?;
        let started = Instant::now();
        let query = generate_query(&client, &params, index)?;
        let query_ms = elapsed_ms(started);
        ensure!(
            query.len() == params.query_bytes(),
            "query size changed with row"
        );
        let request = [setup.as_slice(), query.as_slice()].concat();
        let started = Instant::now();
        let response = answer(&params, database.as_slice(), &request)?;
        let server_ms = elapsed_ms(started);
        ensure!(
            response_bytes.is_none_or(|size| size == response.len()),
            "response size changed with row"
        );
        response_bytes = Some(response.len());
        let started = Instant::now();
        let decoded = decode_reply(&client, &params, &response)?;
        let decode_ms = elapsed_ms(started);
        ensure!(
            decoded == expected,
            "PIR round-trip failed: decoded {} bytes, expected {} bytes; first mismatch {:?}",
            decoded.len(),
            expected.len(),
            decoded.iter().zip(&expected).position(|(a, b)| a != b)
        );
        decode_row(&decoded, params.db_item_size)?;
        measurements.push(Measurement {
            query_ms,
            server_ms,
            decode_ms,
        });
    }
    Ok(Report {
        scheme: "Spiral single-server PIR",
        library: "spiral-rs =0.2.1-alpha.2",
        profile: format!("{:?}", args.profile),
        research_only: true,
        database_source: source,
        rows: params.num_items(),
        row_bytes: params.db_item_size,
        raw_database_bytes: raw_bytes,
        preprocessed_database_bytes: preprocessed_bytes,
        preprocessing_ms,
        client_setup_ms,
        setup_bytes: setup.len(),
        query_bytes: params.query_bytes(),
        request_bytes: setup.len() + params.query_bytes(),
        response_bytes: response_bytes.context("no retrievals")?,
        all_retrievals_correct: true,
        measurements,
    })
}

fn main() -> Result<()> {
    serde_json::to_writer_pretty(std::io::stdout().lock(), &run(Args::parse())?)?;
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_rows_preserve_binary_payloads_and_reject_truncation() {
        for payload in [b"".as_slice(), b"[]", b"\0secret\xff"] {
            let row = encode_row(payload, 24).unwrap();
            assert_eq!(decode_row(&row, 24).unwrap(), payload);
        }
        assert!(encode_row(&[1; 17], 24).is_err());
        assert!(decode_row(&[0; 23], 24).is_err());
        let mut row = encode_row(b"[]", 24).unwrap();
        row[0] = 255;
        assert!(decode_row(&row, 24).is_err());
        row[0] = 2;
        row[23] = 1;
        assert!(decode_row(&row, 24).is_err());
    }

    #[test]
    fn server_rejects_bad_objects_before_upstream_deserialization() {
        let params = Profile::UpstreamSmall.params();
        assert!(answer(&params, &[], &[]).is_err());
        let mut object = vec![0; 40];
        object[32..].copy_from_slice(&params.modulus.to_ne_bytes());
        assert!(validate_object(&object, 40, params.modulus).is_err());
        assert!(validate_object(&[0; 39], 40, params.modulus).is_err());
    }

    #[test]
    fn large_profile_is_refused_before_database_allocation() {
        assert!(run(Args {
            profile: Profile::Upstream16k,
            database: None,
            max_database_mib: 256,
            queries: 1
        })
        .unwrap_err()
        .to_string()
        .contains("exceeding"));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_and_symlink_inputs_are_rejected_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("database.fifo");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        assert!(open_database(&fifo).is_err());
        let regular = dir.path().join("database.bin");
        std::fs::write(&regular, []).unwrap();
        let link = dir.path().join("database.link");
        std::os::unix::fs::symlink(&regular, &link).unwrap();
        assert!(open_database(&link).is_err());
        assert!(open_database(&regular).is_ok());
    }

    #[test]
    fn serialized_single_server_round_trips_have_constant_sizes() {
        let report = run(Args {
            profile: Profile::UpstreamSmall,
            database: None,
            max_database_mib: 32,
            queries: 3,
        })
        .unwrap();
        assert!(report.all_retrievals_correct);
        assert_eq!(report.rows, 256);
        assert_eq!(report.preprocessed_database_bytes, 16 * 1024 * 1024);
        let params = Profile::UpstreamSmall.params();
        let mut client = Client::init(&params);
        let _ = client.generate_keys();
        assert!(generate_query(&client, &params, 256).is_err());
        assert!(decode_reply(&client, &params, &[]).is_err());
        assert!(decode_reply(&client, &params, &[0; 16]).is_err());
        // Fresh encryption randomness, including when selecting the same row.
        assert_ne!(
            generate_query(&client, &params, 42).unwrap(),
            generate_query(&client, &params, 42).unwrap()
        );
    }
}
