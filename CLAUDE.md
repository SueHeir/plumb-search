# Working on Plumb Search

Plumb is a peer-to-peer web search engine in Rust. Most of this code is written by Claude sessions, and each one starts without the last one's memory, so this file holds what every session should know before changing code. CONTRIBUTING.md has the rules for everyone; this file adds how the code is meant to be written.

## Where code goes

- **The node** is what `plumb run` and the Docker image run: plumb-node and the crates it links (plumb-core, plumb-format, plumb-answer, plumb-ingest, plumb-crawl, plumb-index, plumb-embed, plumb-net, plumb-plugin). It should stay small enough for an LLM to read, so keep it compact.
- **plumb-tools** holds whatever a node never runs: building and signing sets, evals, ranking training, fetchers and research commands. If only an operator or an eval runs it, it goes there.
- plumb-private is the browser side of private search (WebAssembly), plumb-desktop is the Tauri app, and `plugins/` holds WASM plugins.
- `zotero/` is the Zotero plugin, in JavaScript since Zotero runs nothing else. It talks to a node only through `/api/search`, so keep that answer's shape (`assembled.rows`, `pages`, `hits`) readable by it, or change both. Its `README.md` says how to test it.

## Before pushing

```sh
cargo fmt --all --check
cargo clippy --workspace --exclude plumb-desktop --all-targets -- -D warnings
cargo test --workspace --exclude plumb-desktop
```

CONTRIBUTING.md lists the extra checks for the WebAssembly crates and the desktop app. One test fails when it runs as root: `exact_fold_rename_failure_preserves_inputs_and_charges_leftover_output`, which needs a read-only directory to refuse a rename.

`crates/plumb-tools/tests/conventions.rs` counts some of what this file asks node code to avoid (raw lock unwraps, direct file writes, bare `tokio::spawn`, `.unwrap()`, long functions and long files) and fails when a count goes up. When a change brings a count down, lower its limit in the same change.

## How node code is written

**Locks.**
- Take std locks with `plumb_core::locks::{Locked, RwLocked}` (`.locked()`, `.read_locked()`, `.write_locked()`), never `.lock().unwrap()`.
- Never hold a std lock across `.await`.
- Don't hold a lock that async code or the network loop needs while doing file I/O, an fsync or a fold. Async code waiting on such a lock blocks a tokio worker, and the droplet has two.

**Blocking work.**
- Disk I/O, parsing whole files and heavy CPU go in `spawn_blocking` (`blocking()` in the node's worker), never on async threads.
- plumb-net's swarm task (`Task::run`) answers every peer, so it never waits on disk, a slow lock or a whole-batch parse. Hand such work to `spawn_blocking` and take the result back as an `Answer`.

**Background jobs.**
- Long-running node jobs start through `supervise` (plumb-node `node.rs`), which restarts one that fails.
- Each job checks `stopping()` / `check_stop()` in its loops.
- A job that can take a GB or so of memory holds `one_big_job()` while it runs, taken before `hold_records()`, never after. plumbsearch.org ran out of memory on 2026-10-13 when three such jobs ran at once.
- A job that holds a big dataset says in its doc comment roughly how much memory it takes.
- Prefer streaming (`build_from_file`, `for_each_record`) to loading a whole record set, and never keep two full copies of something big.

**Files.**
- Write through `plumb_core::storage`: `write_atomic`, `write_atomic_private` for keys and other secrets, `replace_file`, `remove_file`, `create_directory`. Writes are then atomic and charged to the node's storage budget.
- Reading, appending to or folding `records.jsonl` and its journal happens under `hold_records()`. A fold deletes the journal, and an open handle would go on writing into the deleted file.

**Input from peers and the web.**
- Cap every message's size before decoding it, and cap counts per peer.
- Check before keeping, forwarding or cloning: nothing is re-gossiped or passed on unchecked.
- A server-side fetch of a URL a caller or plugin chose goes through plumb-crawl's resolver, which drops private and local addresses.

**What people see.**
- Everything a node shows or returns (its results, plugin results, headlines, leads, MCP answers) goes through the denylist (`StatusSource::denies` in the web layer).
- Pages are built with `format!` and `escape_html`, and links with `safe_href` / `http_url`.
- Pages run no JavaScript and load nothing from other servers.
- Results pages send `Referrer-Policy: no-referrer`, so their forms post with `Origin: null`. `/app` pages send `same-origin`.

**Size and shape.**
- Keep functions under 150 lines and files under 3,000, tests included. A file's tests can go in a `tests/` directory beside it, one file per part.
- When something grows past that, split it by phase or by owner, not just to cut lines.
- Good code here is compact: no wrappers nobody needs, no dead code kept "for later" (git history keeps it), and one helper instead of a copy.
- Comments say why, in the plain style of the docs.

## Search quality

- A change that can move results (ranking, defaults, page or site data, fetching) doesn't merge on its own green CI. It goes through the Batch evals, as the project instructions describe.
- A refactor of ranking code has to leave the frozen eval results identical.
- The node and the evals should run the same results pipeline. Where they don't yet, change both, or the evals measure something the node doesn't serve.

## Docs

- Each crate has a README with its design, modules and tests, [docs/architecture.md](docs/architecture.md) is the map, and [docs/README.md](docs/README.md) lists every page. A change that makes one of them wrong fixes it in the same change.
- [docs/search-quality.md](docs/search-quality.md) reports the 1,000-search review. After a run, add its row to `docs/search-quality/runs.tsv`, run `python3 docs/search-quality/charts.py` and update the numbers in the text. Only runs of builds on main go in.
- Public docs name no private servers, sessions or people, and quote search-quality numbers only there and in the README, so one update keeps them true.

## Review

Pull requests here merge on local checks, often with no human review. Before merging a change to plumb-net, plumb-format, the denylist, the plugin sandbox, or anything the web layer uses to decide what is local or allowed, have a separate reviewer (a fresh agent) read the diff for bugs where two parts meet. The 2026-10-13 review (`code-review/review-2026-10-13.md` in the project's files) found most of its bugs there.

## The wire and the signed format

- Messages between nodes and the signed set format (plumb-format) are read by nodes that are already released. Change them only in ways older nodes accept, unless the project has decided to drop that version.
- plumb-format is meant to stay as it is for a long time. Add only optional fields that older nodes skip, and have a second reviewer read any change to it.
- A signature covers RFC 8785 canonical JSON (`plumb_format::canonical`), so anything signed must come out byte for byte the same on every node.
