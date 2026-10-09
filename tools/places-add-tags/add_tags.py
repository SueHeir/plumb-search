#!/usr/bin/env python3
"""Adds the tags #275 reads (craft=brewery and sport=) to an existing
places file, without re-reading OpenStreetMap's whole planet (~90 GB).

#275 only adds tags to places the file already holds (pubs that brew,
sports centres), so their ids and tags are enough. They come from
QLever's OpenStreetMap planet in two queries:

    add_tags.py fetch tags.tsv            # ~580k rows, ~14 MB
    add_tags.py patch places.tsv.gz tags.tsv places.new.tsv.gz

`patch` streams the file, so it needs little memory. Tags are added as
plumb_ingest::osm::place_of adds them: craft=brewery first (unless the
place's kind is a craft=), then up to two sports, lower-cased.
"""

import gzip
import re
import sys
import time
import urllib.parse
import urllib.request

QLEVER = "https://qlever.dev/api/osm-planet"
PREFIX = "PREFIX osmkey: <https://www.openstreetmap.org/wiki/Key:> "
SPORTS = PREFIX + "SELECT ?o ?s WHERE { ?o osmkey:sport ?s . ?o osmkey:name ?n }"
BREWING = PREFIX + (
    "SELECT DISTINCT ?o WHERE { ?o osmkey:name ?n . "
    '{ ?o osmkey:microbrewery "yes" } UNION { ?o osmkey:craft "brewery" } }'
)
KINDS = {"node": "n", "way": "w", "relation": "r"}


def qlever(query):
    """The rows of `query` on QLever's OpenStreetMap planet, header dropped."""
    body = urllib.parse.urlencode({"query": query}).encode()
    request = urllib.request.Request(
        QLEVER,
        body,
        headers={
            "Accept": "text/tab-separated-values",
            "User-Agent": "plumb-search places tags",
        },
    )
    for attempt in range(4):
        try:
            with urllib.request.urlopen(request, timeout=900) as response:
                lines = response.read().decode("utf-8").splitlines()
            return [line.split("\t") for line in lines[1:]]
        except Exception as err:
            print(f"  QLever: {err}", file=sys.stderr)
            time.sleep(30 * (attempt + 1))
    raise SystemExit("QLever would not answer")


def osm_id(uri):
    """`<https://www.openstreetmap.org/node/123>` as `n123`."""
    match = re.fullmatch(r"<https://www\.openstreetmap\.org/(node|way|relation)/(\d+)>", uri)
    return KINDS[match[1]] + match[2]


def literal(text):
    """A TSV literal (`"climbing"`, maybe typed or tagged) as plain text."""
    match = re.fullmatch(r'"(.*)"(\^\^.*|@.*)?', text.strip())
    text = match[1] if match else text.strip()
    return text.replace('\\"', '"').replace("\\t", " ").replace("\t", " ")


def fetch(path):
    # Whole-world tag queries answer in seconds on QLever; the public
    # Overpass servers time out on them.
    rows = {}
    for uri, sport in qlever(SPORTS):
        rows[osm_id(uri)] = ["", literal(sport)]
    print(f"  {len(rows)} named places with a sport", file=sys.stderr)
    brewing = qlever(BREWING)
    for (uri,) in brewing:
        rows.setdefault(osm_id(uri), ["", ""])[0] = "brewery"
    print(f"  {len(brewing)} named places that brew", file=sys.stderr)
    with open(path, "w", encoding="utf-8") as out:
        for osm, (craft, sport) in rows.items():
            out.write(f"{osm}\t{craft}\t\t{sport}\n")


def field(text):
    return " ".join(re.sub(r"[\t\n\r|;]", " ", text).split())


def patch(places_in, tags_path, places_out):
    tags = {}
    with open(tags_path, encoding="utf-8") as f:
        for line in f:
            osm, craft, microbrewery, sport = line.rstrip("\n").split("\t")
            tags[osm] = (craft == "brewery" or microbrewery == "yes", sport)
    seen = brewing = sporty = climbing = 0
    with gzip.open(places_in, "rt", encoding="utf-8") as src, gzip.open(
        places_out, "wt", encoding="utf-8"
    ) as dst:
        dst.write(src.readline())  # header
        for line in src:
            cols = line.rstrip("\n").split("\t")
            if len(cols) != 13 or cols[11] not in tags:
                dst.write(line)
                continue
            seen += 1
            brews, sport = tags[cols[11]]
            kept = [
                t
                for t in cols[3].split(";")
                if t and t != "craft=brewery" and not t.startswith("sport=")
            ]
            if brews and not cols[2].startswith("craft="):
                kept.append("craft=brewery")
                brewing += 1
            sports = [s.strip() for s in sport.split(";") if s.strip()][:2]
            for s in sports:
                kept.append(field("sport=" + s.lower()))
            sporty += bool(sports)
            climbing += any(s.lower() == "climbing" for s in sports)
            cols[3] = ";".join(kept)
            dst.write("\t".join(cols) + "\n")
    print(
        f"{seen} places matched; {brewing} brew, {sporty} have a sport "
        f"({climbing} climbing)"
    )


if __name__ == "__main__":
    if sys.argv[1:2] == ["fetch"] and len(sys.argv) == 3:
        fetch(sys.argv[2])
    elif sys.argv[1:2] == ["patch"] and len(sys.argv) == 5:
        patch(*sys.argv[2:])
    else:
        raise SystemExit(__doc__)
