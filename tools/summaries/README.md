# Summaries for sites with no text

A script that asks Claude for one sentence about each well-known site that has no words of its own, so searches that describe such a site ("cheap flights", "recipes") can find it. For operators improving a records file.

A site has no words of its own when its record has no description, homepage text, headings or terms, and nothing from Wikidata or Wikipedia; a title alone does not count. The sentence goes into the record's `summary` field. The index searches it as the site's description while the site has no description of its own, and the site's vector is made from it too.

## Steps

```sh
plumb-tools summaries pick --records records.jsonl --top 10000 --out picks.jsonl
python3 tools/summaries/summarize.py --picks picks.jsonl --out summaries.jsonl --direct 20
python3 tools/summaries/summarize.py --picks picks.jsonl --out summaries.jsonl
plumb-tools summaries apply --summaries summaries.jsonl --records records.jsonl \
  --out records-summaries.jsonl
```

1. `summaries pick` writes the best-scored sites with no text (by link score, `--top` of them, 10,000 by default) with what is known of them: domain, title, other names, link texts, kinds and country.
2. `summarize.py --direct 20` asks for the first 20 sites only and prints them, to read a sample first.
3. `summarize.py` asks for the rest. Sites already in `--out` are skipped, so a stopped run carries on.
4. `summaries apply` writes the records with their summaries to a new file; the records file is left as it is.

Then index and embed the new records file as usual and compare the [evals](../../eval/README.md) with the old one.

## How the script asks

| Option | What it does |
| --- | --- |
| `--backend cli` | The default: Claude Code (`claude -p --model haiku`), signed in with a Claude subscription, so no API key is needed |
| `--per-call N`, `--workers N` | With `cli`: sites in one call (default 50) and calls at once (default 4) |
| `--backend api` | The Message Batches API with `claude-haiku-5-5` instead. Needs `pip install anthropic` and an API key |
| `--key-file FILE` | With `api`: the file holding the key, used when it exists (default `~/.config/anthropic/api-key`); otherwise the `anthropic` library finds one, such as `ANTHROPIC_API_KEY` |
| `--direct N` | Ask for the first N sites not yet done, print them, and stop |

The model is told to answer `UNKNOWN` for a site it does not know rather than guess from the domain. `apply` drops those, and any answer longer than 40 words.
