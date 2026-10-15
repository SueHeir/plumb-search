# Test fixtures

A small, made-up web for tests: seed files and a crawl of about 60 domains, built so that ranking matters. For contributors writing or changing tests that ingest, index or search.

Everything here is invented. The brands and their domains are real so the queries read naturally, but every title, description, rank, link and Wikidata item id is made up, nothing was downloaded, and none of it describes the real sites. The look-alike domains (such as `usbank-login-help.com`) are fictional decoys.

## Files

| File | Stands in for | Format |
| --- | --- | --- |
| `tranco.csv` | The Tranco top 1M list | `rank,domain` lines, no header |
| `cc-domain-ranks.txt` | Common Crawl's web graph domain ranks | Tab-separated, header `#harmonicc_pos #harmonicc_val #pr_pos #pr_val #host_rev #n_hosts`, hosts reversed (`com.usbank`) |
| `wikidata-official-sites.tsv` | Wikidata's official websites (P856) | Tab-separated, header `item label website` |
| `sample.wat` | A Common Crawl WAT file | WARC records with JSON metadata, not gzipped |
| `brand_queries.tsv` | The brand-name suite | `query<TAB>expected_domain[,another_ok_domain]`, as in [eval/](../eval/README.md#file-format) |

## What the data tests

- **Look-alikes**: phishing-style domains with keyword-stuffed titles and no rank signals (`usbank-login-help.com`, `irs-tax-refund-help.com`), linked only from one spam page.
- **Real sites that share a name**: `chasecenter.com` for "chase", `deltafaucet.com` for "delta", `amazon.jobs` for "amazon", `fordfoundation.org` for "ford". The queries go both ways ("chase" and "chase center").
- **Names found only in link text or aliases**: "bofa", "amex", "nyt", "citibank".
- **Edge cases for the readers**: an http homepage later seen over https, a homepage at `/index.html`, 301 and 404 responses, same-site links, generic link text ("click here"), a URL used as link text, a domain that is only a link target (`github.com`), Wikidata items as bare ids and as entity URLs, a host claimed by more than five items (`facebook.com`), and a website at an IP address, with no registrable domain.

## Who uses them

| Test | What it does with them |
| --- | --- |
| [plumb-node tests/end_to_end.rs](../crates/plumb-node/tests/end_to_end.rs) | Runs `plumb ingest` on every file, builds an index, and drives the web app and the MCP server over it |
| [plumb-tools tests/eval.rs](../crates/plumb-tools/tests/eval.rs) | Runs `plumb-tools index`, `search` and `eval` (with `brand_queries.tsv`) over the ingested records |
| `repository_query_files_are_valid` in [eval.rs](../crates/plumb-tools/src/eval.rs) | Checks `brand_queries.tsv` along with the suites in `eval/` |
| [eval/contracts/offline.jsonl](../eval/contracts/offline.jsonl) | Three contracts for a report over the fixture index |

```sh
cargo test -p plumb-node --test end_to_end
cargo test -p plumb-tools --test eval
```

## Running the pipeline on the fixtures

From the workspace root:

```sh
cargo run -p plumb-node -- ingest --tranco fixtures/tranco.csv \
  --cc-ranks fixtures/cc-domain-ranks.txt --wat fixtures/sample.wat \
  --wikidata fixtures/wikidata-official-sites.tsv --out data/fixtures/records.jsonl
cargo run -p plumb-tools -- index --records data/fixtures/records.jsonl --index data/fixtures/index
cargo run -p plumb-tools -- search --index data/fixtures/index us bank
cargo run -p plumb-tools -- eval --index data/fixtures/index --queries fixtures/brand_queries.tsv --min-top1 0.9
cargo run -p plumb-node -- serve --index data/fixtures/index
```

The last command serves the search page at `http://127.0.0.1:8080`.

## Regenerating sample.wat

[make_fixtures.rs](../crates/plumb-node/examples/make_fixtures.rs) writes `sample.wat`. Edit the pages there, then run from the workspace root:

```sh
cargo run -p plumb-node --example make_fixtures
```

The output is deterministic. WARC record lengths count the CRLF line endings, so [.gitattributes](.gitattributes) stops git from converting them.
