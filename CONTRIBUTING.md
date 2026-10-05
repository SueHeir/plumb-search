# Contributing to Plumb Search

Issues and pull requests are welcome. For a bigger change, open an issue first so we can agree on the approach.

To report a security problem, don't open an issue: see [SECURITY.md](SECURITY.md).

## Rules

- All code is Rust. The browser side of private search and plugins are Rust compiled to WebAssembly. HTML, CSS and Markdown are fine; no JavaScript is added to pages.
- Pages load nothing from other servers, and nodes log no searches. Keep it that way.
- Keep docs in step with the code, in the same plain style: short sentences, what it does and how to use it.

## Checks

Before opening a pull request, run:

```sh
cargo fmt --all --check
cargo clippy --workspace --exclude plumb-desktop --all-targets -- -D warnings
cargo test --workspace --exclude plumb-desktop
```

CI runs these, and the checks below for the parts that build on their own. Run those too when you change those parts.

**Private search and plugins** (WebAssembly), with the `wasm32-unknown-unknown` target installed (`rustup target add wasm32-unknown-unknown`):

```sh
cargo clippy -p plumb-private --target wasm32-unknown-unknown -- -D warnings
cargo clippy -p plumb-plugin -p plumb-plugin-hacker-news --target wasm32-unknown-unknown -- -D warnings
cargo build --locked --release -p plumb-plugin-hacker-news --target wasm32-unknown-unknown
cargo build --locked -p plumb-private --target wasm32-unknown-unknown --profile wasm
```

To test the node with the private search page built in, process the module with `wasm-bindgen-cli` 0.2.108 and point the tests at it:

```sh
cargo install wasm-bindgen-cli --version 0.2.108 --locked
wasm-bindgen --target web --no-typescript --out-dir target/private \
  target/wasm32-unknown-unknown/wasm/plumb_private.wasm
PLUMB_PRIVATE_DIR=../../target/private cargo test -p plumb-node private
```

**Desktop app**, which needs the GUI libraries Tauri builds against (on Debian or Ubuntu, `libwebkit2gtk-4.1-dev`; see [docs/desktop.md](docs/desktop.md) for other systems):

```sh
cargo clippy -p plumb-desktop --all-targets -- -D warnings
cargo test -p plumb-desktop
```

**PIR probe** (`tools/pir-probe`), a research tool outside the workspace:

```sh
cargo fmt --manifest-path tools/pir-probe/Cargo.toml --check
cargo clippy --release --locked --manifest-path tools/pir-probe/Cargo.toml --all-targets -- -D warnings
cargo test --release --locked --manifest-path tools/pir-probe/Cargo.toml
```

## License

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed under the MIT license and the Apache License, Version 2.0, at the licensee's option (MIT OR Apache-2.0), without any additional terms or conditions.
