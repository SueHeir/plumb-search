# Private search

A Plumb node started with `--private-search` offers a second search page,
`/private`, where the node never learns what people search for. The
browser looks up the results itself:

1. It works out the query's **keys**: the whole query joined and its words
   (`us bank` -> `usbank`, `bank`, `us`), as in `plumb_core::keys`.
2. Each key falls in one of 16,384 **buckets** by its hash. The browser
   picks the buckets of the first keys and pads them with random buckets,
   so it always asks for 4.
3. It fetches those 4 buckets from the node: `GET /api/buckets/{table}/{n}`.
   A bucket holds the best 32 sites of each key that falls in it, a few
   hundred sites in all.
4. It keeps the sites that match its keys and ranks them in the browser
   (`crates/plumb-private`, Rust compiled to WebAssembly), with the same
   formula as the node's own ranking.

The query stays in the page's fragment (`/private#q=us%20bank`), which
browsers never send to a server, so back, reload and bookmarks still work.
The search box has no `name`, so if the script cannot run, the form sends
nothing; the page then says private search needs JavaScript and
WebAssembly, and links to the normal search.

## What the node learns

Four bucket numbers per search, each shared by a few hundred keys. It does
not learn which of the four were real, nor which keys in them were meant.
Two things keep repeated searches from giving that away:

- A bucket's address names its table (the index it came from), and its
  contents never change, so the browser caches it for a year. Searching
  again asks the node for nothing it already sent.
- The padding is not drawn fresh each time. It comes from a secret kept in
  the browser's local storage and the query's keys, so the same query from
  the same browser asks for the same four buckets, and comparing two
  searches does not single out the real ones. Another browser pads
  differently.

The node still sees the visitor's IP address and when they search, as any
web page would. Hiding that takes a relay between the browser and the node
run by someone else, which is planned: browsers would send their bucket
requests through plumbsearch.org, encrypted for other nodes (Oblivious HTTP,
as the network does between nodes).

## Ranking in the browser

The node ranks with a Tantivy index, which does not build for WebAssembly.
A private search only has a few hundred candidates, so
`plumb_private::rank` scores them directly with the node's formula and
defaults: name matches, trust against look-alikes, kinds ("banks") and the
home country work as in the index. The text match is simpler (each query
word scores the boost of every field it is in, without BM25's word
frequencies), and accents are not folded (`nestle` does not find
`Nestlé`). The tests in `crates/plumb-private` check that the first result
agrees with the index on a set of queries.

The home country comes from the node, as for normal search: from the
browser's language unless the node was started with `--country`.

Kind searches ("banks") only find sites that also carry the word in their
names, since buckets hold sites by their names, not their kinds.

## Running it

```sh
plumb run --data DIR --private-search
```

Each index build then also writes its buckets (`indexes/NNNNNN/buckets/`,
about as much disk as the records file). A node whose index has none yet,
because it was built before the flag was turned on, rebuilds it once on
start. The settings gear on the search pages links to `/private` once the
buckets are there.

## Building the page's script

`crates/plumb-private` builds for the browser with the `wasm32` target and
`wasm-bindgen`, whose version must be the one the crate pins (0.2.108):

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.108 --locked
cargo build -p plumb-private --target wasm32-unknown-unknown --profile wasm
wasm-bindgen --target web --no-typescript --out-dir target/private \
    target/wasm32-unknown-unknown/wasm/plumb_private.wasm
cargo build --release -p plumb-node
```

`plumb-node`'s build script embeds `target/private/` (or the folder in
`PLUMB_PRIVATE_DIR`) into the `plumb` binary. Without it, the node still
builds and serves buckets, but `/private` says private search is not
available. The Docker image always builds it. The script is served at an
address that names its own hash, `/private/{hash}/...`, so browsers can
cache it for good too.

## Addresses

| Address | What |
|---|---|
| `GET /private` | The page. Its Content-Security-Policy allows this site's scripts and WebAssembly, and requests to this site only. |
| `GET /private/{hash}/{file}` | The page's script: `boot.js`, `plumb_private.js`, `plumb_private_bg.wasm`. |
| `GET /api/buckets` | `{"table": "2-1f0e...", "buckets": 16384}`, or 503 when the node serves no buckets. Never cached. |
| `GET /api/buckets/{table}/{n}` | Bucket `n` of that table: a JSON list of site records, trimmed to the names, top link texts and signals. Cached for good; 404 once a newer index replaced the table. |
