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

An inferred search name, a bare domain namesake, a copied title, or an
acronym matching only part of a resource name is insufficient owner evidence.
Those cases return `found:false` with candidates under `alternatives`.
Matching title/domain text without an independent owner reference stays at
low confidence. Category and resource words remain part of the requested
identity; no category suffix is stripped to validate a familiar brand. A
title/identity lacking those words can leave a plausible candidate unresolved,
even if the strict suite expects its domain. An agency's parent site does
not establish the destination of a separately named database or application.
An exact package's homepage
outweighs generated framework documentation on an unrelated host. These
contracts are exercised by deterministic MCP tests for the wrong-owner
evidence shapes observed in the combined candidate appendix; the tests do
not add aliases or fill corpus gaps.

Historical verification facts observed on October 9, 2026 remain regression
provenance, rather than embedded runtime answers. Hetzner's own
[status page](https://status.hetzner.com/incident/62839f8e-073a-4159-87a1-b05d093fe689)
identifies its old console host; its [Cloud page](https://www.hetzner.com/cloud/)
links the new destination, and the old host was observed redirecting there.
The [Semantic Scholar tutorial](https://webflow.semanticscholar.org/product/api/tutorial)
documents its API host. No exact-host exemption table, canned owner map, or
redirect-only ownership rule is used by the identity tools.

Runtime lookalike decisions require backend evidence. An exact brand label
on another suffix or hosting tenant stays `suspected` without affiliation
evidence; service words alone do not establish ownership or impersonation.
Frozen evaluation labels remain unchanged: missing indexed owner evidence
can lower strict `official` coverage and must be reported separately from
false definitive accusations. Typo, suffix-borrowing and homograph controls
continue to exercise impersonation detection.

Run deterministic scoring contracts without a node:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s eval/official_site -p 'test_*.py'
```

When multiple documented owners match the complete requested name, the tool
returns `found:false`, `status:ambiguous`, low confidence and alternatives.
Popularity, result ordering and a same-name package cannot resolve this
identity. An explicit address or a qualifier that distinguishes one owner
can resolve it through the same evidence checks.
