# Test fixtures

Everything in this folder is made up for tests. The brands and their domains
are real so that the queries read naturally, but every title, description,
rank, link and Wikidata item id here is invented. Nothing was downloaded, and
none of it describes the real sites. The look-alike domains (such as
`usbank-login-help.com`) are fictional decoys.

| File | Stands in for | Format |
|---|---|---|
| `tranco.csv` | the Tranco top-1M list | `rank,domain` lines, no header |
| `cc-domain-ranks.txt` | Common Crawl web graph domain ranks | tab separated, header `#harmonicc_pos #harmonicc_val #pr_pos #pr_val #host_rev #n_hosts`, hosts reversed (`com.usbank`) |
| `wikidata-official-sites.tsv` | Wikidata official websites (P856) | tab separated, header `item label website` |
| `sample.wat` | a Common Crawl WAT file | WARC records with JSON metadata, not gzipped |
| `brand_queries.tsv` | the brand-name test | `query<TAB>expected_domain[,another_ok_domain]` |

About 60 domains, chosen so that ranking matters:

- **Look-alikes**: phishing-style domains with keyword-stuffed titles and no
  rank signals (`usbank-login-help.com`, `irs-tax-refund-help.com`, ...),
  linked only from one spam page.
- **Real sites that share a name**: `chasecenter.com` for "chase",
  `deltafaucet.com` for "delta", `amazon.jobs` for "amazon",
  `fordfoundation.org` for "ford". The queries go both ways ("chase" and
  "chase center").
- **Names only found in link text or aliases**: "bofa", "amex", "nyt",
  "citibank".
- **Edge cases for the readers**: an http homepage later seen over https, a
  homepage at `/index.html`, 301 and 404 responses, same-site links, generic
  link text ("click here"), a URL used as link text, a domain that is only a
  link target (`github.com`), Wikidata items as bare ids and as entity URLs,
  a shared host claimed by more than five items (`facebook.com`), and a
  website without a registrable domain (an IP address).

## Regenerating sample.wat

`sample.wat` is written by `crates/plumb-node/examples/make_fixtures.rs`.
Edit the pages there, then run, from the workspace root:

```sh
cargo run -p plumb-node --example make_fixtures
```

The output is deterministic. WARC record lengths count the CRLF line endings,
so `.gitattributes` stops git from converting them.

## Running the pipeline on the fixtures

```sh
cargo run -p plumb-node -- ingest --tranco fixtures/tranco.csv \
    --cc-ranks fixtures/cc-domain-ranks.txt --wat fixtures/sample.wat \
    --wikidata fixtures/wikidata-official-sites.tsv --out data/fixtures/records.jsonl
cargo run -p plumb-node -- index --records data/fixtures/records.jsonl --index data/fixtures/index
cargo run -p plumb-node -- search --index data/fixtures/index us bank
cargo run -p plumb-node -- eval --index data/fixtures/index --queries fixtures/brand_queries.tsv --min-top1 0.9
cargo run -p plumb-node -- serve --index data/fixtures/index
```

`cargo test -p plumb-node --test end_to_end` runs the same steps in a
temporary directory.
