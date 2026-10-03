# Running Plumb Search with Docker

The Docker image runs a Plumb Search node on a server or homelab machine.
`plumb run` serves the search page and a JSON API on port 8080. On first start
it downloads seed data and builds its index (searchable within a minute or
two, from the Tranco list; the rest of the seed data follows its first crawl), and from then on it keeps
crawling homepages and rebuilding the index. It also joins the Plumb network
(see [Join the Plumb network](#join-the-plumb-network)), so its crawls help
every other node and theirs help it. Everything it keeps is in one volume
mounted at `/data`.

Images for `linux/amd64` and `linux/arm64` (including 64-bit Raspberry Pi OS)
are published as `ghcr.io/sueheir/plumb-search`.

## Quick start

From a copy of this repository, or of just its `docker-compose.yml`:

```sh
docker compose up -d
```

Then open `http://<server>:8080` (`http://localhost:8080` on the same machine).

The first start downloads the seed data, so the container needs internet
access: the Tranco list comes from tranco-list.eu and the official websites
from query.wikidata.org (plus Common Crawl ranks from data.commoncrawl.org
when you set `--cc-release`). Until the first index is ready, the page shows
what the node is doing and how far it has got, instead of the search box. If
a download fails, the page shows the error and when the node will try again.
It retries by itself, first after 10 minutes and then less often, up to every
6 hours. The log shows the same:

```sh
docker compose logs -f
```

Without Compose:

```sh
docker run -d --name plumb --init --restart unless-stopped --stop-timeout 300 \
  -p 8080:8080 -p 4001:4001/tcp -p 4001:4001/udp \
  -v plumb-data:/data ghcr.io/sueheir/plumb-search:latest
```

`--stop-timeout 300` gives the node time to finish an index build when it is
stopped (Compose sets the same with `stop_grace_period`). Stopping it sooner
is safe, but the build is lost and done again later.

The page has no login, so anyone who can reach the port can search. To use
another port, change the number on the left (`"80:8080"`). To keep the page
to the machine itself, for example behind a reverse proxy, publish it as
`"127.0.0.1:8080:8080"`.

`http://<server>:8080/api/status` reports what the node is doing as JSON
(`phase` is `setting_up` or `ready`), which suits uptime monitors.

## Join the Plumb network

The node joins the Plumb network when it starts: it connects to the
network's first nodes on plumbsearch.org, finds other nodes through them
(and any on your own network by itself), crawls the share of sites the
network assigns it each day, and shares those crawls, signed, with the
others. Nothing needs to be forwarded on the router: the node dials out, and
other nodes reach it through a relay when they cannot reach it directly.
Forwarding port 4001, TCP and UDP, to this machine lets them connect
directly instead, which is faster and spares the relay.

Within a minute of starting, the log says `joined the Plumb network as
12D3Koo...`, and `http://<server>:8080/api/status` shows the node under
`network`: `peer_id`, its `peers`, and the batches of crawls it has published
and received. If it cannot reach any node, `network.problem` says why.

What the node gives and gets:

* It takes in the crawls of the plumbsearch.org node at once, and other
  nodes' crawls once a second crawler agrees with them.
* Other nodes take in its crawls once they agree with their own crawls of
  the same sites, which starts within a day or two: a new node's crawls count
  on another node after they have matched that node's own crawls of 3 sites.
* Confirmed crawls earn it crawl credits, which buy it priority on busy
  nodes (docs/network.md, "Crawl credits").

To keep a node to itself, run it without `--network` (see
[Settings](#settings)). Other network flags, such as `--bootstrap` for
another node to start from, are in docs/network.md.

## Use Plumb as your browser's search engine

Plumb's pages offer it to the browser as a search engine. In Firefox, open
the Plumb page, right-click the address bar and choose **Add "Plumb
Search"**. Chrome lists it under **Settings > Search engine > Manage search
engines and site search** as an inactive shortcut once you have opened the
page; activate it there. Any browser can also take it by hand, with the
address `http://<server>:8080/search?q=%s` (`http://127.0.0.1:8080/search?q=%s`
on the same machine).

The search address the page offers is made from the address the browser
reached Plumb at. Behind a reverse proxy, pass on the original `Host` header,
and set `X-Forwarded-Proto: https` when the proxy serves HTTPS.

## Where the data lives

Everything is in the named volume `plumb-data`, mounted at `/data`:

| Path | What it holds |
| --- | --- |
| `/data/records.jsonl` | One JSON line per site: everything the node has learned. Back it up together with the journal. |
| `/data/records.jsonl.journal` | Crawl results not yet folded into `records.jsonl`. Crawls add to it as they go, and the node folds it in once it reaches a quarter of the size of `records.jsonl` (64 MB at least). Back it up with `records.jsonl`. |
| `/data/indexes/000001/`, ... | The search index, rebuilt from the records. Each rebuild makes a new numbered directory and deletes the old one. |
| `/data/seed/` | The first-start downloads. They can be deleted once `records.jsonl` exists. |
| `/data/state.json` | Progress that survives restarts, such as when the next refresh is due |
| `/data/node.lock` | Held while a node runs, so two nodes never share the directory |

**Mount the volume at `/data` itself**, not at an index directory or a file
inside it. The node replaces its index, `records.jsonl` and `state.json` by
renaming, and Linux cannot rename a mount point.

To keep the data in a host directory instead, bind-mount it at `/data`. The
container runs as the unprivileged user `plumb` (uid and gid 10001), not as
root, so give the directory to that user first:

```sh
mkdir plumb-data && sudo chown 10001:10001 plumb-data
```

```yaml
    volumes:
      - ./plumb-data:/data
```

To back up the records, copy out the journal, when there is one, and then
`records.jsonl` (in that order, so that a journal folded in between the two
copies is not missed):

```sh
docker compose cp plumb:/data/records.jsonl.journal .
docker compose cp plumb:/data/records.jsonl .
```

When a node starts with a `records.jsonl` already in `/data`, it skips the
seed downloads, so the same files can start a new installation; the node
reads a journal next to `records.jsonl` together with it.

`docker compose down -v` deletes the container together with the volume and
everything in it.

## Settings

Open `/app` for the same organized node panel used by the desktop app:
Overview, Search & browser, Resources, Network & privacy, and About.
Network statistics reflect the live node, including peers, crawl agreement,
popularity reports, and relay activity. Forms do not reload while being edited.

The panel keeps the existing local-only write policy: remote connections
see disabled controls and an explanation. Docker bridge networking can make
even a host-local browser appear remote to the container. For those deployments,
configure the node on the host with the startup flags below, or place
`features.json` in the mounted data directory while the node is stopped:

```json
{
  "network": true,
  "search_by_meaning": false,
  "private_search": true,
  "share_popularity": false,
  "bootstrap": [
    "/dns4/plumbsearch.org/tcp/4001/p2p/12D3KooWJ2UWUBsxmPfXTfHa8cBBmzifa6kj5pFZKfJXYNQyJ69a",
    "/ip4/198.211.114.63/tcp/4001/p2p/12D3KooWJ2UWUBsxmPfXTfHa8cBBmzifa6kj5pFZKfJXYNQyJ69a"
  ]
}
```

Feature choices in this file override startup feature flags and take effect
on the next start (`docker compose restart`). Network transport, public
addresses, relay, UPnP, and discovery flags are preserved. The two bootstrap
addresses above are the network's own first nodes on plumbsearch.org; with no
bootstrap addresses at all, the node finds only nodes on its own local
network. Remove `features.json` while stopped to
use only startup flags again. Resource limits stay in `settings.json` and
apply immediately when saved through a local panel.

### Control it from the desktop app

The desktop app can show and change this node's settings from another
computer. Remote control is off until you turn it on, on the node's host:

```sh
docker exec plumb plumb remote-control on
```

It prints a token, once. In the desktop app, choose **+ Connect to a node**
above the panel and enter the node's address (such as
`http://192.168.1.20:8080`) and the token. The app then shows the node next
to **This computer**, with the same Overview, Search & browser, Resources and
Network & privacy sections, and its forms change the node: resource limits at
once, features after `docker compose restart`.

The token can read the node's status and change its settings, features and
refreshes, nothing else. The node keeps only its SHA-256 hash, in
`/data/remote-control.json`. It is taken only from the host itself, local
networks (including Docker's bridge and Tailscale) and requests straight to
the node: requests from public addresses or through a reverse proxy (with a
`Forwarded` or `X-Forwarded-For` header) are refused. So putting the node
behind a proxy on the internet does not expose remote control.
`remote-control on --allow-public` lifts that limit; only use it with HTTPS
in front of the node, since the token is sent with every request.

`plumb remote-control on` again makes a new token and stops the old one
working; `plumb remote-control off` turns remote control off, and
`plumb remote-control status` says which it is. Both take effect at once.


Settings are flags of `plumb run`. The image's default command is
`run --data /data --bind 0.0.0.0:8080`, and a command you set replaces all of
it, so keep those two flags. In `docker-compose.yml`:

```yaml
services:
  plumb:
    command: ["run", "--data", "/data", "--bind", "0.0.0.0:8080", "--sites", "250000"]
```

Then apply it with `docker compose up -d`. With `docker run`, put the command
after the image name:

```sh
docker run -d --name plumb --init --restart unless-stopped --stop-timeout 300 \
  -p 8080:8080 -v plumb-data:/data ghcr.io/sueheir/plumb-search:latest \
  run --data /data --bind 0.0.0.0:8080 --sites 250000
```

| Flag | Default | What it does |
| --- | --- | --- |
| `--sites N` | 1,000,000 | How many of the best-ranked sites to keep from the seed data. First start only. |
| `--cc-release NAME` | none | Also take ranks from a Common Crawl web graph release, such as `cc-main-2025-26-nov-dec-jan` (listed at https://commoncrawl.org/web-graphs). Only the top rows are downloaded. First start only. |
| `--initial-crawl N` | 10,000 | Homepages to crawl once the first index is built; `0` for none. |
| `--refresh-hours H` | 1 | Hours between refreshes, which crawl more homepages and rebuild the index, e.g. `24` or `0.5`. |
| `--crawl-per-refresh N` | 5,000 | Homepages crawled per refresh. |
| `--no-refresh` | | Never refresh: keep the index as the initial crawl leaves it. |
| `--reseed` | | Fold the seed files already in `DIR/seed` into the records again before starting (only files over a week old are downloaded again). For records made before a change to how seed data is read; use it once. |
| `--profile desktop` | `server` | Smaller defaults: 250,000 sites, 2,000 homepages at first and 1,000 more every 12 hours. The flags above still override it. |
| `--alpha A` | the index's default | Weight of the popularity prior in the ranking, from 0 to 1. |
| `--use-system-proxy` | off | Crawl homepages through the proxy in `HTTPS_PROXY`, `HTTP_PROXY` or `ALL_PROXY`; see [Behind a proxy](#behind-a-proxy). |

"First start only" settings take effect when the node sets itself up, that is,
while `/data/records.jsonl` does not exist yet. To change them later, start
over with an empty volume.

`docker run --rm ghcr.io/sueheir/plumb-search:latest run --help` lists every
flag.

The log goes to the container log at the `info` level. Set `RUST_LOG` to
change that, for example:

```yaml
    environment:
      RUST_LOG: debug
```

### Behind a proxy

If the server reaches the internet only through a proxy, pass the proxy's
address to the container in the usual variables and add `--use-system-proxy`
to the command:

```yaml
services:
  plumb:
    command: ["run", "--data", "/data", "--bind", "0.0.0.0:8080", "--use-system-proxy"]
    environment:
      HTTPS_PROXY: http://proxy.example.com:3128
      HTTP_PROXY: http://proxy.example.com:3128
```

With `docker run`, put `-e HTTPS_PROXY=http://proxy.example.com:3128
-e HTTP_PROXY=http://proxy.example.com:3128` before the image name and
`--use-system-proxy` at the end of the command. `NO_PROXY` lists hosts to
reach directly.

The seed downloads use these variables in any case, but without
`--use-system-proxy` the node fetches homepages directly, so behind such a
proxy every crawl fails: the page says the last update failed, and
`/api/status` and the log say the network seems to be down. Through a proxy,
the crawler cannot check that a site's name leads to a public address, as it
does when it connects directly, so a site could point it at hosts on your own
network that the proxy can reach. Use a proxy that refuses private addresses,
or one that cannot reach anything you want kept private.

## Updating

```sh
docker compose pull
docker compose up -d
```

The data stays in the volume. `latest` follows the main branch. Every build
is also tagged `sha-<commit>`, and releases get their version number (`1.2.3`
and `1.2`). Put one of those tags in `docker-compose.yml` to update only when
you choose. `docker image prune` removes the images that were replaced.

With `docker run`, pull the image, then replace the container:

```sh
docker pull ghcr.io/sueheir/plumb-search:latest
docker stop plumb && docker rm plumb
docker run -d --name plumb ...   # the same command as before
```

## Building the image yourself

From a checkout of the repository:

```sh
docker compose build    # or: docker build -t plumb-search .
```

Compose also builds the image when it cannot pull it. A local build is tagged
`ghcr.io/sueheir/plumb-search:latest`, so a later `docker compose pull`
replaces it with the published image.

The build needs BuildKit, the default builder since Docker 23. The first
build compiles everything, which took 2.5 to 4.5 minutes on a 4-core machine.
Later builds keep the downloaded crates and compiled dependencies in BuildKit
cache mounts and recompile only what changed. `--build-arg RUST_VERSION=1.99`
pins the Rust toolchain; the default is the newest 1.x release.

## Disk and memory

Rough figures for the default 1,000,000 sites, measured with the image on a
4-core x86-64 machine using synthetic records shaped like real ones. A new
node has seed ranks for every site but has crawled few homepages, so its
records are small. They grow as it crawls: the second column is a node that
has crawled 90% of its sites, at about 860 bytes per site. At the default
5,000 homepages an hour, with each site crawled at most once every 30 days,
a node gets there within a month or two, sooner in the network, where it
also takes in other nodes' crawls. Real records may come out somewhat
smaller or larger.

| | New node | 90% of homepages crawled |
| --- | --- | --- |
| `records.jsonl` | 125 MB | 860 MB |
| Search index | 100 MB | 500 MB |
| Index build time | 20 to 26 seconds | 66 seconds |
| Peak memory | 1.3 GB | 2.2 GB |
| Memory between refreshes | under 100 MB | under 100 MB |

- **Disk**: the node writes a new index and a new `records.jsonl` before it
  deletes the old ones, so allow for two of each, plus the crawl journal
  (up to a quarter of `records.jsonl`, or 64 MB if that is more), the seed
  downloads (tens of megabytes, estimate) and the image (about 120 MB).
  About 3 GB of free space covers the upper bound.
- **Network**: a node in the network also keeps every crawl batch it sees,
  its own and other nodes', for 35 days in `/data/net/batches`, at roughly
  1 KB per homepage (estimate). A node crawling all day at the default pace
  adds about 120 MB a day of its own, so give a network node 5 to 10 GB more,
  and more as the network grows.
- **Bandwidth**: at most 512 KB is read of each homepage, and most are far
  smaller; crawling 120,000 homepages a day comes to a few GB a day
  (estimate). `--refresh-hours 6` or `--crawl-per-refresh 1000` crawls less.
- **Memory**: memory peaks while the node reads all its records, at the start
  of every crawl and rebuild, and while it builds an index. A crawl keeps
  the records in memory until it ends (about 0.6 GB for a new node), and
  afterwards the node hands what it used back to the system, so between
  refreshes it needs little more than the index pages that searches read
  (measured on a later build than the other figures, with similar
  synthetic records). A new node needs about 1.5 GB of free memory, growing
  toward 2.5 GB as it crawls. With less, use `--profile desktop` or a smaller
  `--sites`; memory grows about in proportion to the number of sites
  (estimate).
- **CPU**: index builds and crawls are short bursts, and the node is idle
  between refreshes. A search took 10 to 25 milliseconds on the larger
  index. Small machines such as a Raspberry Pi take several times longer to
  build an index (estimate).
