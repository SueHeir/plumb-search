//! Embeds the private search page's WebAssembly, when it has been built.
//!
//! `crates/plumb-private` compiles to WebAssembly, and `wasm-bindgen` turns
//! that into `plumb_private.js` and `plumb_private_bg.wasm`
//! (docs/private-search.md). This copies both into the build from the
//! folder in `PLUMB_PRIVATE_DIR`, or `target/private/` in the workspace when
//! that is not set. Without them, empty files are embedded and the node
//! says private search is not in this build.

use std::path::{Path, PathBuf};
use std::{env, fs};

const FILES: [&str; 2] = ["plumb_private.js", "plumb_private_bg.wasm"];

fn main() {
    println!("cargo:rerun-if-env-changed=PLUMB_PRIVATE_DIR");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("set by Cargo"));
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

fn read(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
}
