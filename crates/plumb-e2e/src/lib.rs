//! Helpers for the end-to-end tests in `tests/`: they run the Plumb Search
//! Docker image the way people do, kill containers in the middle of their
//! work, start them again on the same volume and check what comes out.
//!
//! The tests are `#[ignore]`d, since they need Docker and an image:
//!
//! ```text
//! docker build -t plumb-search:e2e .
//! PLUMB_E2E_IMAGE=plumb-search:e2e cargo test -p plumb-e2e -- --ignored --test-threads 1
//! ```
//!
//! By default the nodes are seeded from the synthetic fixtures and never
//! reach the internet. With `PLUMB_E2E_REAL=1` they download the real seed
//! data and the embedding model, and crawl real homepages, as a node does
//! on its first start; CI runs them that way.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

/// How often a wait looks again.
const POLL: Duration = Duration::from_secs(2);

/// The image under test, from `PLUMB_E2E_IMAGE`.
pub fn image() -> String {
    std::env::var("PLUMB_E2E_IMAGE").expect("PLUMB_E2E_IMAGE names the image to test")
}

/// Whether to use real seed data, the real model and real homepages.
pub fn real() -> bool {
    std::env::var("PLUMB_E2E_REAL").is_ok_and(|v| v == "1")
}

/// The repository's `fixtures/` folder.
pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .canonicalize()
        .expect("the fixtures folder")
}

/// Runs `docker` with `args`, returning its stdout; panics with its output
/// when it fails.
pub fn docker(args: &[&str]) -> String {
    let output = Command::new("docker")
        .args(args)
        .output()
        .expect("running docker");
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        output.status.success(),
        "docker {args:?} failed with {}\n--- stdout\n{stdout}\n--- stderr\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// Runs `docker` with `args`, ignoring failures (for cleaning up).
pub fn docker_quiet(args: &[&str]) {
    let _ = Command::new("docker").args(args).output();
}

/// A name no other test run uses: `plumb-e2e-<pid>-<n>-<what>`.
pub fn unique(what: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "plumb-e2e-{}-{}-{what}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// A Docker network for nodes to find each other on, removed on drop.
pub struct Network {
    pub name: String,
}

impl Network {
    pub fn create() -> Self {
        let name = unique("net");
        docker(&["network", "create", &name]);
        Network { name }
    }
}

impl Drop for Network {
    fn drop(&mut self) {
        docker_quiet(&["network", "rm", &self.name]);
    }
}

/// A node: a data volume and, while it runs, a container of the image
/// serving on a free port of 127.0.0.1. The container and the volume are
/// removed on drop, after printing the container's log.
pub struct Node {
    pub name: String,
    pub volume: String,
    pub network: Option<String>,
    /// The flags of `plumb run` after `--data /data --bind 0.0.0.0:8080`.
    pub flags: Vec<String>,
    addr: Option<SocketAddr>,
    /// Logs of containers stopped so far, printed on drop.
    logs: String,
}

impl Node {
    /// A node with a new, empty volume, not started yet.
    pub fn new(what: &str, network: Option<&Network>, flags: &[&str]) -> Self {
        let name = unique(what);
        let volume = format!("{name}-data");
        docker(&["volume", "create", &volume]);
        Node {
            name,
            volume,
            network: network.map(|n| n.name.clone()),
            flags: flags.iter().map(|f| f.to_string()).collect(),
            addr: None,
            logs: String::new(),
        }
    }

    /// Runs `plumb` in a throwaway container with the volume at `/data`
    /// and the fixtures at `/fixtures`, returning its stdout.
    pub fn plumb(&self, args: &[&str]) -> String {
        let fixtures = format!("{}:/fixtures:ro", fixtures().display());
        let volume = format!("{}:/data", self.volume);
        let image = image();
        let mut all = vec!["run", "--rm", "-v", &volume, "-v", &fixtures, &image];
        all.extend_from_slice(args);
        docker(&all)
    }

    /// Writes `/data/records.jsonl` from the fixtures, so the node starts
    /// without downloading seed data.
    pub fn seed_from_fixtures(&self) {
        self.plumb(&[
            "ingest",
            "--tranco",
            "/fixtures/tranco.csv",
            "--cc-ranks",
            "/fixtures/cc-domain-ranks.txt",
            "--wat",
            "/fixtures/sample.wat",
            "--wikidata",
            "/fixtures/wikidata-official-sites.tsv",
            "--out",
            "/data/records.jsonl",
        ]);
    }

    /// Starts the container.
    pub fn start(&mut self) {
        assert!(self.addr.is_none(), "{} is running", self.name);
        let volume = format!("{}:/data", self.volume);
        let image = image();
        let mut args = vec![
            "run",
            "-d",
            "--init",
            "--name",
            &self.name,
            "-p",
            "127.0.0.1::8080",
            "-v",
            &volume,
            "-e",
            "RUST_LOG=info,tantivy=warn",
        ];
        if let Some(network) = &self.network {
            args.extend(["--network", network]);
        }
        args.extend([
            image.as_str(),
            "run",
            "--data",
            "/data",
            "--bind",
            "0.0.0.0:8080",
        ]);
        args.extend(self.flags.iter().map(String::as_str));
        docker(&args);
        let port = docker(&["port", &self.name, "8080/tcp"]);
        let addr = port
            .lines()
            .find_map(|line| line.parse().ok())
            .unwrap_or_else(|| panic!("no published port in {port:?}"));
        self.addr = Some(addr);
    }

    /// Kills the container at once (SIGKILL, like a power cut) and removes
    /// it, keeping its log.
    pub fn kill(&mut self) {
        self.remove(&["kill", "--signal", "KILL"]);
    }

    /// Stops the container as `docker stop` does, and removes it.
    pub fn stop(&mut self) {
        self.remove(&["stop", "--timeout", "300"]);
    }

    fn remove(&mut self, how: &[&str]) {
        let mut args = how.to_vec();
        args.push(&self.name);
        docker_quiet(&args);
        self.save_log();
        docker_quiet(&["rm", "-f", &self.name]);
        self.addr = None;
    }

    fn save_log(&mut self) {
        if let Ok(output) = Command::new("docker")
            .args(["logs", "--timestamps", &self.name])
            .output()
        {
            self.logs.push_str(&format!("=== {} ===\n", self.name));
            self.logs.push_str(&String::from_utf8_lossy(&output.stdout));
            self.logs.push_str(&String::from_utf8_lossy(&output.stderr));
        }
    }

    /// The container's address on its Docker network.
    pub fn ip(&self) -> String {
        docker(&[
            "inspect",
            "--format",
            "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
            &self.name,
        ])
    }

    /// `GET path` on the node: the status code and the body.
    pub fn get(&self, path: &str) -> (u16, String) {
        let addr = self
            .addr
            .unwrap_or_else(|| panic!("{} is not running", self.name));
        http_get(addr, path).unwrap_or_else(|err| panic!("GET {path} on {}: {err}", self.name))
    }

    /// `GET path` on the node, read as JSON; panics unless it answers 200.
    pub fn get_json(&self, path: &str) -> Value {
        let (code, body) = self.get(path);
        assert_eq!(code, 200, "GET {path} on {}: {body}", self.name);
        serde_json::from_str(&body).unwrap_or_else(|err| panic!("GET {path}: {err}: {body}"))
    }

    /// `/api/status`, or `None` while the node does not answer yet.
    pub fn status(&self) -> Option<Value> {
        let addr = self.addr?;
        let (code, body) = http_get(addr, "/api/status").ok()?;
        (code == 200).then(|| serde_json::from_str(&body).ok())?
    }

    /// Waits up to `limit` for `done` to hold for the status, returning it.
    pub fn wait_for(&self, what: &str, limit: Duration, done: impl Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        let mut last = None;
        loop {
            if let Some(status) = self.status() {
                if done(&status) {
                    eprintln!(
                        "{}: {what} after {:.0} s",
                        self.name,
                        start.elapsed().as_secs_f64()
                    );
                    return status;
                }
                last = Some(status);
            }
            if start.elapsed() > limit {
                panic!(
                    "{}: no {what} after {} s; last status: {}",
                    self.name,
                    limit.as_secs(),
                    last.map_or("none".into(), |s| s.to_string())
                );
            }
            std::thread::sleep(POLL);
        }
    }

    /// Runs `program` with `args` in a throwaway container of the image
    /// with the volume at `/data`, returning its stdout.
    pub fn run_in_volume(&self, program: &str, args: &[&str]) -> String {
        let volume = format!("{}:/data", self.volume);
        let image = image();
        let mut all = vec![
            "run",
            "--rm",
            "-v",
            &volume,
            "--entrypoint",
            program,
            &image,
        ];
        all.extend_from_slice(args);
        docker(&all)
    }

    /// The paths under `/data`, one per line.
    pub fn files(&self) -> Vec<String> {
        self.run_in_volume("find", &["/data"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Top results of `/api/search?q=query`, as domains.
    pub fn search(&self, query: &str) -> Vec<String> {
        let hits = self.get_json(&format!("/api/search?q={}", encode(query)));
        domains(&hits)
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if self.addr.is_some() {
            self.save_log();
        }
        docker_quiet(&["rm", "-f", &self.name]);
        docker_quiet(&["volume", "rm", "-f", &self.volume]);
        eprintln!("{}", self.logs);
    }
}

/// The `domain` of each hit in a JSON list (or in its `hits`).
pub fn domains(hits: &Value) -> Vec<String> {
    let list = hits.get("hits").unwrap_or(hits);
    list.as_array()
        .map(|hits| {
            hits.iter()
                .filter_map(|h| h["domain"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// `s` percent-encoded for a query string.
pub fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A plain HTTP/1.0 GET: the status code and the body.
pub fn http_get(addr: SocketAddr, path: &str) -> std::io::Result<(u16, String)> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let response = String::from_utf8_lossy(&response);
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("no end of headers"))?;
    let code = head
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| std::io::Error::other(format!("no status in {head:?}")))?;
    Ok((code, body.to_string()))
}
