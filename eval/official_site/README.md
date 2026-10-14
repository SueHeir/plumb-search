# Identity-tool checks

Checks of the MCP tools `official_site` and `check_lookalike` against a running node, with every request and response saved. For contributors who change how Plumb finds a name's official site or judges a look-alike. What the tools do is in [the MCP docs](../../docs/mcp.md).

## Files

| File | What it holds |
| --- | --- |
| [queries.tsv](queries.tsv) | Names AI agents looked up in test projects whose answer is a subdomain, an inner page or a site whose name is shared |
| [heldout.tsv](heldout.tsv) | Names kept out of every other eval file, to check a change on |
| [identity.tsv](identity.tsv) | Entity, task and jurisdiction cases, some with tool arguments (`SAT México` with `country` MX, GT and `any`) |
| [oct10.tsv](oct10.tsv) | Names an AI agent looked up in a week of replayed tasks, labeled by hand |
| [lookalikes.tsv](lookalikes.tsv) | Addresses for `check_lookalike`, with the verdicts accepted |
| [run.py](run.py) | The runner and scorer |
| [test_run.py](test_run.py) | Tests of the scoring, with no network |

## File format

```text
LifeWiki	conwaylife.com
SAT México	sat.gob.mx	{"country":"GT"}
DefinitelyUnresolvedPlumbEntity9f302c	@unresolved
paypa1.com	lookalike
```

One `name<TAB>answer[,another]` per line, with an optional third column of tool arguments as a JSON object; blank lines and `#` comments are skipped.

- For `official_site`, an answer is a host or a registrable domain. A domain of two labels or fewer (or ending in `.co.uk` or `.gov.uk`) also takes its subdomains; a longer host counts only for itself; `www.` is ignored. `@unresolved` expects `found: false`.
- For `check_lookalike`, the answers are the verdicts accepted: `official`, `known_site`, `lookalike`, `suspected` or `unknown`.

`run.py` also reads the JSONL contracts of [eval/contracts](../README.md#contract-format), and runs their cases whose `tool` is `official_site`, `check_lookalike` or `facts`.

## Run

```sh
python3 eval/official_site/run.py --mcp http://127.0.0.1:8080/mcp --out official-site.jsonl \
  eval/official_site/queries.tsv eval/official_site/heldout.tsv eval/official_site/identity.tsv
python3 eval/official_site/run.py --mcp http://127.0.0.1:8080/mcp --tool check_lookalike \
  --out lookalikes.jsonl eval/official_site/lookalikes.tsv
```

`--mcp` is a node's `/mcp` address; `plumb run` serves one at `127.0.0.1:8080` by default. The runner adds `findings=off` to it, so answers the node saved earlier do not change the results.

| Option | What it does |
| --- | --- |
| `--tool official_site\|check_lookalike` | The tool the TSV files are for (default `official_site`) |
| `--country CODE` | Add a home country to every TSV case that does not set one |
| `--out FILE` | Required: where to write every request, response and score |
| `--compare OLD.jsonl` | Print `FIXED` or `BROKE` for each case whose result changed since an earlier run |
| `--responses FILE` | Score the responses saved in an earlier `--out` again, with no network |
| `--require-revision SHA` | Refuse a node whose embedded revision differs or is not clean |
| `--max-calls N`, `--time-budget S`, `--timeout S` | Stop after 400 calls or 600 seconds, and give each call 10 seconds, by default |

A run that stops at a budget marks its summary `incomplete` and exits with status 2.

## Scoring

- An `official_site` answer is right when it found a site and the host of its URL matches an answer. A wrong answer given with medium or high confidence also counts as a `false_official`, kept apart from plain misses. A call that failed is never right, not even for `@unresolved`.
- A `check_lookalike` answer is right when its verdict is one of those accepted. A `lookalike` verdict for an address that should not get one is a `false_lookalike`; a missed `lookalike` is a `missed_impersonation`.
- JSONL cases are also checked against their `expect` (`site`, `answer_contains`, `date_contains`, `answer_excludes`, `abstain`) and their `negatives`.

The output starts with a manifest (the target, a SHA-256 of each suite, the budgets, and the node's `initialize` answer with its build), then one line per case, then a summary with, for each `label_status:category` group, the cases right, the families right in every case, and the three error counts.

## Tests

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s eval/official_site -p 'test_*.py'
```
