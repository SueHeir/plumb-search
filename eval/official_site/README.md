# Identity-tool checks

Run the existing official-site suites and the new entity/task/jurisdiction
fixtures against the same isolated candidate and pinned corpus during the
combined evaluation:

```sh
python3 eval/official_site/run.py --mcp http://127.0.0.1:8090/mcp \
  eval/official_site/queries.tsv eval/official_site/heldout.tsv \
  eval/official_site/identity.tsv --out official-site.jsonl
python3 eval/official_site/run.py --mcp http://127.0.0.1:8090/mcp \
  --tool check_lookalike eval/official_site/lookalikes.tsv --out lookalikes.jsonl
```

`--country GT` adds a preference to every case; the optional third TSV column
overrides it per case. The SAT México cases exercise the default, Mexico,
conflicting Guatemala, and `any` preferences. `country` is a preference, not
an identity override; the response reports a conflict when the named
jurisdiction takes precedence. Existing two-column files remain supported.

Official-site scoring uses the selected URL's host. `@unresolved` requires
`found:false`, and a failed call does not count as resolved or unresolved.
The report counts wrong medium/high-confidence official answers separately
from ordinary misses. Lookalike fixtures contain accepted verdicts; their
report separates false definitive accusations from missed impersonations.
`suspected` means affiliation is unverified and is not an official/known-site
verdict. Saved JSONL includes the complete answer, provenance, and case
arguments; comparison keys distinguish country variants and tools.

Exact-host affiliation records in `mcp/identity.rs` are reviewed owner
references observed on October 9, 2026. Hetzner's own status page identifies
its old console host, its current Cloud page links the new destination, and
the old host was observed redirecting there. The redirect corroborates an
owner reference; it is never used on its own to establish ownership. The
Semantic Scholar tutorial documents its API host and remains a control for
an already correct result. Adding another alias requires an owner reference,
a verification date, and boundary/impersonation controls. No TLD, wildcard,
hosting-tenant, or redirect-only exception is accepted.

Run deterministic scoring contracts without a node:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s eval/official_site -p 'test_*.py'
```
