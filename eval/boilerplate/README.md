# Homepage boilerplate fixtures

Fixtures for the text a crawl keeps from a homepage, and probes for comparing what two builds' text extraction does to search. For contributors changing [plumb-crawl's extractor](../../crates/plumb-crawl/src/extract.rs).

## Files

| File | What it holds |
| --- | --- |
| [fixtures.json](fixtures.json) | 21 small synthetic pages, each `{id, url, html, must_keep, must_drop}`: phrases the kept text must hold, and boilerplate it must not |
| [retrieval-queries.json](retrieval-queries.json) | 33 searches for 28 pages, each `{id, domain, queries}`: the 21 fixtures under made-up `fixtureNN.test` domains, and 7 public homepages by address |

The fixtures cover cookie and copyright notices, link menus in plain `div`s, repeated blocks, inline formatting, headings, structured names, short business details, products whose description mentions JavaScript, privacy or policies, mathematical prose, and Chinese, Thai and Arabic text.

## The test

`boilerplate_fixtures_preserve_useful_text_and_remove_notices` in [extract.rs](../../crates/plumb-crawl/src/extract.rs) reads `fixtures.json` through the real extractor and checks every `must_keep` phrase is in the page text and no `must_drop` phrase is:

```sh
cargo test -p plumb-crawl boilerplate_fixtures
```

## Comparing two builds

Run the same steps on both builds, on the same saved HTML, and compare the outputs. Never fetch the pages again for the second build.

1. **Extract.** The `boilerplate_probe` example reads a JSON array of `{id, url, html}` and prints one JSON line per page: the extracted metadata, page text and body text, and the docs extraction.

   ```sh
   cargo build -p plumb-crawl --example boilerplate_probe
   target/debug/examples/boilerplate_probe < eval/boilerplate/fixtures.json > fixtures.jsonl
   ```

   The public homepages' HTML is not in the repository. To include them, save each once as `{"id": URL, "url": URL, "html": ...}` in a JSON array and run the probe on it into `live.jsonl`.

2. **Search.** The `boilerplate_retrieval` example indexes the extracted pages as sites and runs every probe, printing each one's rank among the first 10 results. It runs each search twice: at the default `terms_boost` (`mode: default`) and with terms off (`mode: without-terms`). Searches are exact, with no embeddings.

   ```sh
   cargo build -p plumb-index --example boilerplate_retrieval
   cat fixtures.jsonl live.jsonl | target/debug/examples/boilerplate_retrieval \
     eval/boilerplate/retrieval-queries.json > retrieval.jsonl
   ```

This measures word search over a small, hand-picked corpus, not ranking quality. A page's body text feeds its vector rather than word search, so the short body-only and non-Latin fixtures often find nothing here.
