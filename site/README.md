# plumbsearch.org

The website at https://plumbsearch.org and the server setup behind it.

- `public/` is the static site: a page about Plumb Search, with links to the
  code and to the desktop installers on GitHub Releases, plus privacy and
  credits pages.
- `Caddyfile` serves it with HTTPS (Caddy gets and renews the certificates),
  redirects `www.plumbsearch.org` and plain HTTP to `https://plumbsearch.org`,
  and passes the node's public pages (`/search`, `/api/search`, `/api/status`,
  `/opensearch.xml`, the MCP server at `/mcp`, About you at `/about` and
  `/welcome`, private search under `/private` and `/api/buckets`, and
  network search) to a Plumb node on the same machine. Its dashboard, `/app`,
  is not passed on. While no node is running, those addresses
  show a "search isn't available" page instead.
- `docker-compose.yml` runs Caddy, and with the `node` profile, the Plumb node.

No access log is configured, so the server keeps no record of visitors or
searches. Requests that fail, such as searches while the node is down, are
left out of Caddy's log as well, since they would carry the visitor's
address and search.

## The server

A DigitalOcean droplet (Ubuntu 24.04, 4 vCPUs, 8 GB RAM, 160 GB SSD) with
Docker, automatic security updates, a 2 GB swap file and a ufw firewall that
allows 22, 80 and 443 (TCP, plus UDP 443 for HTTP/3). DNS has A and AAAA
records for `plumbsearch.org` and `www.plumbsearch.org` pointing at it.

Caddy uses host networking, so ufw applies to it as usual. The node publishes
port 8080 on 127.0.0.1 only, so it is reachable from the internet only through
Caddy. Its Plumb network port, 4002 (TCP and UDP), is public: Docker opens
published ports itself, past ufw. The network's relay, plumb-relay, runs
separately on the same machine and has port 4001.

## Deploy

```sh
git clone https://github.com/SueHeir/plumb-search.git /opt/plumb-search
cd /opt/plumb-search/site
docker compose up -d                  # website only
```

To add the Plumb node:

```sh
docker compose --profile node up -d
```

That pulls `ghcr.io/sueheir/plumb-search:main`, which is published from
every push to `main` (`latest` is the newest release instead). To build
the node from source on the server instead, name the repository and
branch in `.env` (put another branch in place of `main` to try it):

```sh
echo 'PLUMB_SOURCE=https://github.com/SueHeir/plumb-search.git#main' > .env
docker compose --profile node build plumb
docker compose --profile node up -d
```

On first start the node downloads its seed data and builds its index, which
takes a while; `docker compose logs -f plumb` shows progress. A million sites
need about 2.2 GB of RAM while the index rebuilds.

## Update

```sh
cd /opt/plumb-search && git pull
cd site
docker compose --profile node pull    # newer images, if any
docker compose --profile node up -d
```

Changes to files in `public/` show up right away, since Caddy serves them
from the checkout. After the `Caddyfile` changes, reload it with
`docker compose exec caddy caddy reload --config /site/Caddyfile --adapter caddyfile`.

## Try it locally

With [Caddy](https://caddyserver.com/docs/install) installed, from this
folder:

```sh
sed -e 's|^www.plumbsearch.org {|http://www.localhost:9080 {|' \
    -e 's|^plumbsearch.org {|http://localhost:9080 {|' \
    -e "s|/site/public|$PWD/public|" Caddyfile > /tmp/Caddyfile.local
caddy run --config /tmp/Caddyfile.local --adapter caddyfile
```

Then open http://localhost:9080. Run `plumb serve` on 127.0.0.1:8080 as well
to try the search box.
