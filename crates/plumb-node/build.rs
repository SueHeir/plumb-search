//! Embeds the private search page's WebAssembly, when it has been built.
//!
//! `crates/plumb-private` compiles to WebAssembly, and `wasm-bindgen` turns
//! that into `plumb_private.js` and `plumb_private_bg.wasm`
//! (docs/private-search.md). This copies both into the build from the
//! folder in `PLUMB_PRIVATE_DIR`, or `target/private/` in the workspace when
//! that is not set. Without them, empty files are embedded and the node
//! says private search is not in this build.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::{env, fs, process::Command};

const FILES: [&str; 2] = ["plumb_private.js", "plumb_private_bg.wasm"];

fn main() {
    build_revision();
    println!("cargo:rerun-if-env-changed=PLUMB_PRIVATE_DIR");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("set by Cargo"));
    source_revision(&manifest);
    let dir = match env::var_os("PLUMB_PRIVATE_DIR") {
        Some(dir) => manifest.join(dir),
        None => manifest.join("../../target/private"),
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("set by Cargo"));
    let present = FILES.iter().all(|name| dir.join(name).is_file());
    for name in FILES {
        let source = dir.join(name);
        println!("cargo:rerun-if-changed={}", source.display());
        let bytes = if present { read(&source) } else { Vec::new() };
        fs::write(out.join(name), bytes).expect("writing to OUT_DIR");
    }
    if !present && env::var_os("PLUMB_PRIVATE_DIR").is_some() {
        panic!(
            "PLUMB_PRIVATE_DIR is set, but {} does not hold {}",
            dir.display(),
            FILES.join(" and ")
        );
    }
}

/// Stamp publication manifests with the source that actually built the binary.
/// Source archives keep this unknown unless the builder supplies a revision.
fn source_revision(manifest: &Path) {
    println!("cargo:rerun-if-env-changed=PLUMB_SOURCE_REVISION");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(manifest)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map(|s| s.trim().to_string())
    };
    for name in ["HEAD", "index"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={}", manifest.join(path).display());
        }
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&["rev-parse", "--git-path", &reference]) {
            println!("cargo:rerun-if-changed={}", manifest.join(path).display());
        }
    }
    let revision = env::var("PLUMB_SOURCE_REVISION")
        .ok()
        .or_else(|| git(&["rev-parse", "HEAD"]));
    if let Some(revision) =
        revision.filter(|r| r.len() == 40 && r.bytes().all(|c| c.is_ascii_hexdigit()))
    {
        let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
        println!(
            "cargo:rustc-env=PLUMB_SOURCE_REVISION={revision}{}",
            if dirty { "+dirty" } else { "" }
        );
    }
}

fn read(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
}

// Git worktrees use a .git *file*. Ask Git for its own paths rather than
// assuming a checkout has .git/HEAD. Archive/container builds can provide
// the revision explicitly; without either source report unknown honestly.
fn build_revision() {
    for name in ["PLUMB_BUILD_REVISION", "PLUMB_BUILD_DIRTY"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let root =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("set by Cargo")).join("../..");
    let git = |args: &[&str]| {
        Command::new("git")
            .current_dir(&root)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
    };
    println!("cargo:rerun-if-changed={}", root.join(".git").display());
    for name in ["HEAD", "index", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={}", root.join(path).display());
        }
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&["rev-parse", "--git-path", &reference]) {
            println!("cargo:rerun-if-changed={}", root.join(path).display());
        }
    }
    // Changes outside this crate affect the artifact too, including untracked
    // fixtures. Cargo watches these directories recursively.
    for path in ["crates", "eval", "Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={}", root.join(path).display());
    }
    let explicit = env::var("PLUMB_BUILD_REVISION").ok();
    let revision = explicit.clone().or_else(|| git(&["rev-parse", "HEAD"]));
    let revision = revision
        .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| "unknown".into());
    let source = if revision == "unknown" {
        "unknown"
    } else if explicit.is_some() {
        "environment"
    } else {
        "git"
    };
    let dirty = match env::var("PLUMB_BUILD_DIRTY").ok().as_deref() {
        Some("true" | "1") => "true",
        Some("false" | "0") => "false",
        Some(_) => "unknown",
        None if explicit.is_some() => "unknown",
        None => match git(&["status", "--porcelain", "--untracked-files=normal"]) {
            Some(status) if status.is_empty() => "false",
            Some(_) => "true",
            None => "unknown",
        },
    };
    let source_digest = git(&[
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "-z",
    ])
    .map(|files| {
        let mut paths: Vec<_> = files.split('\0').filter(|p| !p.is_empty()).collect();
        paths.sort_unstable();
        let mut hash = Sha256::new();
        for path in paths {
            hash.update(path.as_bytes());
            hash.update([0]);
            match fs::read(root.join(path)) {
                Ok(bytes) => {
                    hash.update((bytes.len() as u64).to_le_bytes());
                    hash.update(bytes);
                }
                Err(_) => hash.update(b"missing"),
            }
        }
        format!("{:x}", hash.finalize())
    })
    .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=PLUMB_BUILD_SOURCE_SHA256={source_digest}");
    println!("cargo:rustc-env=PLUMB_BUILD_REVISION={revision}");
    println!("cargo:rustc-env=PLUMB_BUILD_SOURCE={source}");
    println!("cargo:rustc-env=PLUMB_BUILD_DIRTY={dirty}");
}
