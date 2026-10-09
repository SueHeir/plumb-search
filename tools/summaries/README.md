# Summaries for sites with no text

Two thirds of the sites in a node's records have no words of their own: no
homepage description or text, and nothing from Wikidata or Wikipedia. A
search that describes such a site ("cheap flights", "recipes") cannot find
it. This asks Claude for one sentence about each of the best-known ones and
keeps it in the record's `summary` field, which is searched like a
description (only while the site has none) and goes into its vector.

    plumb summaries pick --records records.jsonl --top 10000 --out picks.jsonl
    python3 tools/summaries/summarize.py --picks picks.jsonl --out summaries.jsonl --direct 20
    python3 tools/summaries/summarize.py --picks picks.jsonl --out summaries.jsonl
    plumb summaries apply --summaries summaries.jsonl --records records.jsonl \
        --out records-summaries.jsonl

`--direct 20` asks for 20 sites one at a time and prints them, to read a
sample first. Without it the rest go through the Message Batches API at half
price. The script needs `pip install anthropic` and a key, in
`~/.config/anthropic/api-key` (or `--key-file`) or `ANTHROPIC_API_KEY`.

The model is told to answer UNKNOWN for a site it does not know rather than
guess from the domain, and `apply` drops those, and any answer longer than a
sentence. Then index and embed the new records file as usual and compare
the eval with the old one.
