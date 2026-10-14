# Add tags to a places file

A script that adds the `craft=brewery` and `sport=...` tags to a places file made before `fetch-pages --set places` read them, without reading OpenStreetMap's whole planet again (about 90 GB). For operators with an older `places.tsv.gz`; a places file made today already has the tags. How places work is in [places](../../docs/places.md).

## Run it

```sh
python3 tools/places-add-tags/add_tags.py fetch tags.tsv
python3 tools/places-add-tags/add_tags.py patch places.tsv.gz tags.tsv places.new.tsv.gz
```

1. `fetch` asks QLever's copy of the OpenStreetMap planet (`https://qlever.dev/api/osm-planet`) two queries, the named places with a `sport` tag and the named places tagged `craft=brewery` or `microbrewery=yes`, and writes them as `osm<TAB>craft<TAB>microbrewery<TAB>sport` lines, with the place's id written as in a places file (`n123`, `w123`, `r123`). It tries each query up to four times.
2. `patch` streams the places file into a new one, so it needs little memory. For each place whose id is in the tags file it drops its old `craft=brewery` and `sport=` tags, then adds them the way [osm.rs](../../crates/plumb-tools/src/build/osm.rs) does: `craft=brewery` only on a place that serves drinks or food (`plumb_core::place::brews_where_served`), then up to two sports, lowercased. It prints how many places matched, brew and have a sport.

Then replace the old file with the new one; a node keeps it as `pages/sets/places.tsv.gz` in its data folder. The script uses the Python standard library only.
