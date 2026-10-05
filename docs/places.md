# Places: "pizza in denver" and "coffee near me"

A search that says where, such as "pizza in denver", "coffee shop in boulder", "hotels near red rocks amphitheatre", "denver brewery" or "coffee near me", lists matching places above the sites: their name, kind, distance, street address, website and a link to them on OpenStreetMap, with a small map of where they are.

The map is drawn by Plumb from the places' coordinates as numbered pins with a scale bar. It has no streets: the search page loads nothing from any other server, and map tiles would tell a tile server where every searcher is looking. "Open this area in OpenStreetMap" links to the full map.

Places are © [OpenStreetMap contributors](https://www.openstreetmap.org/copyright), under the Open Database License; every list of places says so.

## How a query is read

- `WHAT in WHERE`, `WHAT near WHERE`, `WHAT around WHERE`, `WHAT close to WHERE`.
- `WHAT near me`, `WHAT nearby`, `nearby WHAT`.
- `WHERE WHAT` or `WHAT WHERE` when WHAT is a kind of place ("denver pizza", "sushi fort collins").

Words like "best", "good" or "cheap" are left out of WHAT. WHAT must be in a place's name or among the words for its kind: a café is found by "cafe", "coffee" and "coffee shop", a supermarket by "grocery store".

WHERE is a town (or village, suburb or neighbourhood) by its name or another name ("nyc"), optionally with its state or country: "portland maine", "paris, france", "denver co". Among towns of one name the bigger wins, and one in the searcher's country a little more. WHERE can also be any named place, such as a stadium or a museum.

Places are listed within a town's size of its centre (12 km for a city, 6 for a town, 3 for a village or around a named place), nearest first, with places that have a website, a brand or a Wikidata item a little ahead. When fewer than three are found, Plumb looks three times as far.

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

Colorado's extract (366 MB) gives 66,102 places in 2 seconds: a 2.7 MB file and a 7 MB index.

## Nodes

Places are one of the page sets: the panel's **Page sets** part has a choice for them, and a node that keeps them takes the file from a node it trusts, only as far as the places it keeps (see [pages.md](pages.md)). Their index is `DIR/pages/places-<key>/`.

To try a places file without a node:

```sh
plumb search --index data/indexes/000123 --places places.tsv.gz --town "Boulder, CO" coffee near me
plumb serve --index data/indexes/000123 --places places.tsv.gz
```
