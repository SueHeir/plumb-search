# plumbsearch.org

This folder runs the project's website, https://plumbsearch.org: the official
[Caddy](https://caddyserver.com) 2 image serving the static page in
[`site/`](../../site) over HTTPS. Caddy gets and renews the certificates by
itself.

| File | What it does |
| --- | --- |
| `compose.yml` | Runs Caddy with the Caddyfile and `site/` mounted read-only, and named volumes for its certificates (`/data`) and settings (`/config`) |
| `Caddyfile` | Serves the site on plumbsearch.org, redirects www.plumbsearch.org to it, and sets the security and cache headers |

## DNS

Point both names at the server before you start Caddy. It asks for
certificates as soon as it starts, and the certificate authority checks that
the names lead to this server.

| Name | Type | Value |
| --- | --- | --- |
| `plumbsearch.org` | A | the server's IPv4 address |
| `plumbsearch.org` | AAAA | the server's IPv6 address |
| `www.plumbsearch.org` | A | the server's IPv4 address |
| `www.plumbsearch.org` | AAAA | the server's IPv6 address |

If the server has no IPv6 address, leave out the AAAA records. A wrong AAAA
record can make the certificate requests fail, since Let's Encrypt prefers
IPv6 when a name has one. `dig +short plumbsearch.org A` and
`dig +short plumbsearch.org AAAA` show what the world sees.

## Deploy on a fresh Ubuntu server

1. Install Docker Engine with the Compose plugin, following
   [Docker's instructions for Ubuntu](https://docs.docker.com/engine/install/ubuntu/).
   `docker compose version` should then print a version.
2. Clone the repository to `/opt/plumb-search` and start Caddy:

   ```sh
   sudo git clone https://github.com/SueHeir/plumb-search.git /opt/plumb-search
   cd /opt/plumb-search/deploy/plumbsearch.org
   sudo docker compose up -d
   ```

3. Watch Caddy get the certificates with `sudo docker compose logs -f`. Once
   it has logged `certificate obtained successfully` for both names, the site
   is up at https://plumbsearch.org.

The server must be reachable from the internet on port 80 (TCP) and port 443
(TCP and UDP). The certificate authority connects to port 80 or 443 to check
the names, plain HTTP on port 80 is redirected to HTTPS, and HTTP/3 uses UDP
port 443. If the hosting provider has a firewall in front of the server, open
those ports there.

## Updating

```sh
cd /opt/plumb-search
sudo git pull
cd deploy/plumbsearch.org
sudo docker compose up -d
```

Caddy serves changes to `site/` right away, since the folder is mounted;
nothing needs restarting for them. `docker compose up -d` applies changes to
`compose.yml`.

- **If the pull changed the Caddyfile**, also run `sudo docker compose restart`.
  Git replaces the file rather than editing it, and a running container keeps
  seeing the file it was started with.
- **To move to the newest Caddy 2 release**, run `sudo docker compose pull`
  before `sudo docker compose up -d`.

## Logs

Caddy writes its own log, about starting up, certificates and errors, to the
container's log:

```sh
cd /opt/plumb-search/deploy/plumbsearch.org
sudo docker compose logs caddy      # add -f to follow it
```

With Docker's default logging driver the log is a file under
`/var/lib/docker/containers/`;
`sudo docker inspect --format '{{.LogPath}}' $(sudo docker compose ps -q caddy)`
prints its exact path. Two warnings at start-up, `HTTP/2 skipped because it
requires TLS` and the same for HTTP/3, are about port 80, which only
redirects to HTTPS, and are expected.

There is no access log. The Caddyfile has no `log` directive, and without one
Caddy does not record visits.

The certificates and their keys are in the `caddy_data` volume
(`/var/lib/docker/volumes/plumbsearchorg_caddy_data/_data`). Keep it:
`docker compose down -v` deletes it, and Caddy then has to request new
certificates, which the certificate authority limits.

## Docker bypasses ufw

Ports that Docker publishes bypass ufw. Docker writes its own iptables rules,
which take effect before ufw's, so a published port is open to the internet
even when ufw does not allow it. That is fine for Caddy's ports 80 and 443,
which are meant to be public.

Any service added to this server later, such as a Plumb node, should bind to
127.0.0.1 so that only this machine can reach it, and be put behind Caddy if
it needs to be public. In its `compose.yml`:

```yaml
    ports:
      - "127.0.0.1:8080:8080"
```
