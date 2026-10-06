# Places: "pizza in denver" and "coffee near me"

A search that says where, such as "pizza in denver", "coffee shop in boulder", "hotels near red rocks amphitheatre", "denver brewery" or "coffee near me", lists matching places above the sites: their name, kind, distance, street address, website and a link to them on OpenStreetMap, with a small map of where they are.

The map is drawn by Plumb from the places' coordinates as numbered pins with a scale bar. It has no streets: the search page loads nothing from any other server, and map tiles would tell a tile server where every searcher is looking. "Open this area in OpenStreetMap" links to the full map.

Places are © [OpenStreetMap contributors](https://www.openstreetmap.org/copyright), under the Open Database License; every list of places says so.

When a query only might say where ("denver brewery", with no "in" or "near") and is also the name of a site, such as "us bank", the site wins and no places are listed. On a node that keeps search history, places you seldom open for searches like the one you make are folded to one line; opening one brings them back (see the README's search history part).

## How a query is read

- `WHAT in WHERE`, `WHAT near WHERE`, `WHAT around WHERE`, `WHAT close to WHERE`.
- `WHAT near me`, `WHAT nearby`, `nearby WHAT`.
- `WHERE WHAT` or `WHAT WHERE` when WHAT is a kind of place ("denver pizza", "sushi fort collins").

Words like "best", "good" or "cheap" are left out of WHAT. WHAT must be in a place's name or among the words for its kind: a café is found by "cafe", "coffee" and "coffee shop", a supermarket by "grocery store".

WHERE is a town (or village, suburb or neighbourhood) by its name or another name ("nyc"), optionally with its state or country: "portland maine", "paris, france", "denver co". Among towns of one name the bigger wins, and one in the searcher's country a little more. WHERE can also be any named place, such as a stadium or a museum.

Places are listed within a town's size of its centre (12 km for a city, 6 for a town, 3 for a village or around a named place). Places of the kind asked for come first, then those that only have the words in their name (for "sushi", sushi bars before an office called Sushi Tech). Within each, nearest first, with places that have a website, a brand or a Wikidata item a little ahead. When fewer than three are found, Plumb looks three times as far.

## Near me

"Near me" is the town you give on the **About you** page, kept on the node for your browser only. Plumb never works out where you are from your address or anything else. Without a town, a "near me" search says how to give one.

## What is kept

For each place: its name, its kind (an OpenStreetMap tag such as `amenity=cafe`), up to four more kinds such as `cuisine=pizza`, its coordinates, its street address, town, state or region, country, website and OpenStreetMap id. Towns also keep up to four other names. That is about 40 bytes a place in the file and about 100 in the index.

Places are named shops, restaurants, cafés, bars, banks, pharmacies, hospitals, doctors, schools, libraries, hotels, museums, attractions, parks, gyms, stations, airports, offices, workshops, castles and the like. Benches, bins, car park spaces, toilets, vending machines, ATMs, and places marked closed are left out.

## Making the file

```sh
plumb fetch-pages --set places --osm planet-latest.osm.pbf --data /path/to/node-data
```

`--osm` reads an OpenStreetMap extract (`.osm.pbf`): the whole planet (about 90 GB, from planet.openstreetmap.org) or a country or state from download.geofabrik.de. Without `--osm`, `--work DIR` downloads the planet there. The file, `DIR/pages/sets/places.tsv.gz`, is sorted with cities first, then towns, places with a Wikidata item, suburbs, places with a website or brand, villages and neighbourhoods, and then everything else, so the first N places are the best known N.

Colorado's extract (366 MB) gives 66,102 places in 2 seconds: a 2.7 MB file and a 7 MB index. The whole planet (October 2026) gives 24,014,360 places, 2.4 million of them towns, in 21 minutes on 32 cores with 18 GB of memory: a 971 MB file and a 2.4 GB index, built in 3 minutes. A search takes 10 to 50 ms.

## How many

| Automatic, by storage limit | Places kept | About |
| --- | --- | --- |
| under 1 GB | none | 0 |
| 1 GB or more | the first 1,000,000: every city and town and the places with a Wikidata item (museums, sights, stations, stadiums), plus every place within 100 km of a town given on one of the node's About pages | 140 MB, plus the places near you and the whole file (950 MB) to pick them from |
| no limit | all of them, every café and shop | 4.2 GB with the file |

Until 2026-10-05, 8 GB or more kept all of them, which put an 8 GB node over its limit with 4.2 GB of places, most of them far from its owner.

The **Page sets** part of the panel can also turn places off or keep a number of them (`places` in `page_sets`, for example `{"places": "off"}`).

## Nodes

Places are one of the page sets: the panel's **Page sets** part has a choice for them, and a node that keeps them takes the file from a node it trusts, only as far as the places it keeps (see [pages.md](pages.md)). Their index is `DIR/pages/places-<key>/`.

To try a places file without a node:

```sh
plumb search --index data/indexes/000123 --places places.tsv.gz --town "Boulder, CO" coffee near me
plumb serve --index data/indexes/000123 --places places.tsv.gz
```
