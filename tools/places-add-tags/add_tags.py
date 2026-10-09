#!/usr/bin/env python3
"""Adds the tags #275 reads (craft=brewery and sport=) to an existing
places file, without re-reading OpenStreetMap's whole planet (~90 GB).

#275 only adds tags to places the file already holds (pubs that brew,
sports centres), so their ids and tags are enough. They come from the
Overpass API, a tile of the world at a time:

    add_tags.py fetch tags.tsv            # ~1-2M rows, tens of MB
    add_tags.py patch places.tsv.gz tags.tsv places.new.tsv.gz

`patch` streams the file, so it needs little memory. Tags are added as
plumb_ingest::osm::place_of adds them: craft=brewery first (unless the
place's kind is a craft=), then up to two sports, lower-cased.
"""

import gzip
import json
import re
import sys
import time
import urllib.parse
import urllib.request

OVERPASS = "https://overpass-api.de/api/interpreter"
QUERY = """[out:json][timeout:600][maxsize:1073741824];
(
  nwr["name"]["sport"]({s},{w},{n},{e});
  nwr["name"]["microbrewery"="yes"]({s},{w},{n},{e});
  nwr["name"]["craft"="brewery"]({s},{w},{n},{e});
);
out tags;"""
PREFIX = {"node": "n", "way": "w", "relation": "r"}


def overpass(box):
    s, w, n, e = box
    body = urllib.parse.urlencode({"data": QUERY.format(s=s, w=w, n=n, e=e)})
    request = urllib.request.Request(
        OVERPASS, body.encode(), headers={"User-Agent": "plumb-search places tags"}
    )
    with urllib.request.urlopen(request, timeout=900) as response:
        answer = json.load(response)
    if "runtime error" in answer.get("remark", ""):
        raise RuntimeError(answer["remark"])
    return answer["elements"]


def fetch_box(box, out, depth=0):
    """Fetches `box`, splitting it in four when Overpass gives up on it."""
    for attempt in range(3):
        try:
            elements = overpass(box)
            break
        except Exception as err:  # timeouts, 429s, 504s, runtime errors
            print(f"  {box}: {err}", file=sys.stderr)
            if depth < 4 and attempt == 1:
                s, w, n, e = box
                mid_lat, mid_lon = (s + n) / 2, (w + e) / 2
                for part in [
                    (s, w, mid_lat, mid_lon),
                    (s, mid_lon, mid_lat, e),
                    (mid_lat, w, n, mid_lon),
                    (mid_lat, mid_lon, n, e),
                ]:
                    fetch_box(part, out, depth + 1)
                return
            time.sleep(30 * (attempt + 1))
    else:
        raise SystemExit(f"Overpass would not answer for {box}")
    for el in elements:
        tags = el.get("tags", {})
        out.write(
            "\t".join(
                [
                    PREFIX[el["type"]] + str(el["id"]),
                    tags.get("craft", "").strip(),
                    tags.get("microbrewery", "").strip(),
                    tags.get("sport", "").strip().replace("\t", " "),
                ]
            )
            + "\n"
        )
    out.flush()
    print(f"  {box}: {len(elements)}", file=sys.stderr)
    time.sleep(5)


def fetch(path):
    # 30x30 degree tiles; dense ones split themselves.
    with open(path, "w", encoding="utf-8") as out:
        for s in range(-90, 90, 30):
            for w in range(-180, 180, 30):
                fetch_box((s, w, s + 30, w + 30), out)


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
