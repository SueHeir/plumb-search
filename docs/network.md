# The Plumb network (Phase 2 design and first prototype)

Plumb nodes can join a peer-to-peer network to split the crawling between them, pass each other what they crawled, and search each other. This page describes the design and what the first prototype (`crates/plumb-net`, switched on with `plumb run --network`) does and does not do yet.

Everything here is opt-in for now. A node started without `--network` works exactly as before.

## In one paragraph

Every day each node is assigned a random eighth of all sites, picked by a hash of the day, its node id and the site. It crawls only those, signs each batch of results with its node key, and announces the batch to the network. Other nodes fetch the batch from whoever passed the announcement on, check the signature and that every homepage in it was really the crawler's to crawl that day, keep only what a homepage crawl can see (title, description, URL, site name, link text), and fold it into their own records. Any node can also search the network without sending its query: it fetches a few hashed buckets of sites (padded with random ones) from other nodes under a throwaway identity, ranks them itself, and checks a Merkle proof on every site taken from a signed crawl, so a node cannot invent or alter a result without being caught. Nodes behind home routers need no port forwarding: they only dial out, and reach each other through relays and hole punching.

## Getting connected without port forwarding

This follows the plan agreed on 2026-10-03.

* **Most nodes only dial out**, like a browser. Fetching batches, announcing them, fetching buckets and serving them all work over connections a node opened itself.
* **Reachable nodes** (a server, a VPS, a homelab with a forwarded port) run with `--public-addr` and `--relay`. They accept connections and relay small messages for nodes behind NAT. `plumbsearch.org` is meant to be the first one.
* **A node behind NAT** takes a reservation on up to two relays it is connected to (circuit relay v2). Other nodes can then reach it at `<relay address>/p2p-circuit/p2p/<its id>`.
* **Hole punching** (DCUtR): when two nodes meet over a relay, they try to open a direct connection through both NATs and move their traffic there. libp2p's own measurements put success at about 70%; the rest stay on the relay, which is fine for searches and announcements (rust-libp2p caps a relayed circuit at 2 minutes and 128 KiB by default).
* **Finding nodes behind NAT**: a node behind a router is known only by its relayed address. Those addresses go into the routing table like any other, so a node that only knows the relay still finds the others and meets them over the relay.
* **Same home network** (mDNS): nodes on one network find each other directly and connect over it. Many routers do not let two machines behind them reach each other through the router's public address, so a hole punch between them usually fails. Turn it off with `--no-local-discovery`.
* **UPnP**: a node asks its home router to forward the port where the router allows it (`--no-upnp` turns that off).
* **Finding nodes**: a node dials its `--bootstrap` nodes, learns about others through Kademlia (`/plumb/kad/1.0.0`), and keeps dialing nodes it knows of until it has 8 connections.
* Transports: TCP and QUIC on port 4001 (`--p2p-port`), IPv4 and IPv6, Noise encryption, Yamux multiplexing.

## Crawl assignments

`plumb_net::assign`. An epoch is one UTC day. A node with id `P` may crawl domain `D` in epoch `E` when the first 8 bytes of `SHA-256("plumb-assign-v1" ‖ E ‖ P ‖ D)` fall below its share. The share is at most one eighth (`MAX_SHARE_PPM`).

* Nobody picks their own sites, the assignment changes every day, and any node can check a crawler's claim from the batch header alone.
* A key can write at most an eighth of the sites per day, and with dozens of nodes most sites have several crawlers a day.
* A node in the network chooses its crawl targets as before (half never-crawled, half due again, best-ranked first), but only among the sites assigned to it today.
* **Known weakness:** the epoch is public, so someone after one particular site can make keys until one is assigned to it (with a share of one eighth, any key gets a given site about every 8 days anyway). Assignment spreads the work and caps how much one key writes; it does not stop a targeted attack. That needs cross-checks between crawlers and spot checks (see "Next steps").

## Crawl batches

`plumb_net::batch`. After each batch of homepages (500 at a time), the node signs the records its crawl produced:

* The batch holds up to 4,096 records, each as the exact JSON bytes that were hashed.
* The signed header holds the crawler's public key, the epoch, its share, the time, the record count and the **Merkle root** over the records (SHA-256, RFC 6962-style leaf and node prefixes).
* The batch id is the hash of the signed header bytes.

What a node keeps from another node's batch (`accept_batch`):

| Part of a record | Kept when |
| --- | --- |
| URL, title, description, site name, crawl time | the record is a crawled homepage the crawler was assigned that epoch, crawled within the epoch, and the URL is on the same site |
| Link text | every linking site it names is one of the batch's own crawled homepages (checked through the 64-bit linker sets) |
| Count of linking sites | capped at the number of homepages the batch crawled |
| New domains the homepages linked to | at most 100 per crawled homepage |
| Popularity ranks, Wikidata status, crawl attempts and failures | never: those come from public seed data or local bookkeeping |

Batches older than 7 days or dated in the future are refused. A node keeps the batches it holds for 35 days (`DIR/net/batches/`), serves them to others, and uses them to prove its search answers.

## Spreading batches

`plumb_net::node`, `plumb_net::proto`.

* A new batch's signed header is announced on the gossipsub topic `plumb/batches/1`. Gossip messages are validated before they are passed on, so a bad header stops at the first honest node.
* A node that hears of a batch it lacks fetches it with `/plumb/batch/1` from the node that passed the header on, or from the crawler, at most 16 at a time.
* When two nodes meet, each asks the other for the headers of the batches it holds from the last 3 epochs, and fetches what it missed. A node that was off for a day catches up this way.
* Records accepted from other nodes go to `DIR/net/inbox.jsonl` as they arrive. Between two pieces of work, the node folds the inbox into its records file through the same journal crawls use. It rebuilds its index once 2,000 records have come in, or at its next refresh.

## Network search

The query never leaves the asking node, and the nodes asked cannot tell which node is asking (Liz's choice, 2026-10-03), nor see its IP address.

* `GET /network?q=` on a node's web page (linked from every results page as "Ask other Plumb nodes too"), and `GET /api/network/search?q=` as JSON.
* **Buckets, not queries.** Every node that answers searches keeps a bucket table next to each index it builds (`indexes/<n>/buckets/`). Each site is filed under its keys: its words and joined names from the domain label, title, aliases and top link texts. A key goes to one of 16,384 buckets by hash, and each key keeps its best 32 sites by link score.
* **Asking.** The asker turns the query into keys the same way, takes the buckets of up to 4 of them, pads that to exactly 4 with random buckets and shuffles them. It asks for each bucket over `/plumb/bucket/1` from up to 2 connected nodes. The node asked sees only bucket numbers, and many unrelated names share each bucket, so it cannot recover the query or tell which buckets were the padding.
* **A throwaway identity for every request.** Each bucket fetch uses a fresh node key and its own short-lived connection, dialed straight to the node or through its relay. The node asked cannot link the request to the asker's network identity, or the 4 bucket requests of one search to each other. 
* **Through a relay, sealed** (Oblivious HTTP, RFC 9458). On its own, a throwaway identity still shows the node asked the IP address the request comes from. So each bucket request goes through another node, picked at random for every request, over `/plumb/oblivious/1`:
  1. The asker asks the relay for the target node's key. The relay fetches it from the target over its own connection and hands the same key to everyone who asks for 10 minutes, so the target cannot give each asker a key of its own and recognize them by it. The key is signed with the target's node key, so the relay cannot swap in its own.
  2. The asker seals the bucket request to that key (HPKE: X25519, HKDF-SHA256, AES-128-GCM, with Mozilla's `ohttp` crate) and hands it to the relay, which passes it to the target and the sealed answer back.
  * The relay sees the asker's IP address and which node it asks, but not the bucket. The target sees the bucket, but only the relay's address. Requests are padded to one size, and answers to the next of 2^k and 1.5 × 2^k bytes (at least 4 KiB), because the relay holds much the same buckets and could otherwise tell the bucket by the answer's size. Only a relay and a target working together can tie an IP address to a bucket, and the asker picks a new pair for every request.
  * Target keys are made in memory, never written to disk, and replaced daily; the previous key is accepted until it expires a day later.
  * Every node that answers searches also relays. If a relay cannot reach the target, one other relay is tried. A request goes straight to the target only when there is no other node to relay it (a network of two). The search page says when that happened, and the JSON answer counts `relayed` and `direct` requests. `GET /api/status` counts `requests_relayed`.
  * What is sealed is Plumb's own CBOR bucket request and answer, not Binary HTTP, so this is RFC 9458's encapsulation rather than full Oblivious HTTP. The same code can serve a browser: `NetHandle::oblivious_keys` and `NetHandle::oblivious_forward` let a public node's web server relay sealed requests from a WASM client to any node in the network.
* **Checking answers.** For every site the answering node holds a signed crawl of, it attaches a **record proof**: the signed batch header, the record and its Merkle path. The asker checks the signature, the path and the assignment. An answer with any proof that does not check out is dropped whole. Sites without a proof (from seed data) are shown as unsigned, their link always goes to `https://<the domain it names>/`, and their popularity signals are taken as the worst any node reported.
* **Ranking locally.** The asker keeps the sites that match the query's keys, builds a small temporary index of them and ranks them with its own ranking, the same as a local search.
* Cost: measured on a 50,000-site index (the plumbsearch.org test node, mostly uncrawled seed sites), the whole bucket table is 3.6 MB and a bucket holds about 3.5 sites on average, so a search moves a few KB (padding adds at most half, and at least 4 KiB per answer). Crawled sites have more keys, so a fully crawled 1M-site index will have bigger buckets; still well under the relay's 128 KiB per circuit. An answer over 20,000 sites is refused.

## Running it

```sh
# A node at home: dials out only.
plumb run --data plumb-data --network \
  --bootstrap /dns4/plumbsearch.org/tcp/4001/p2p/<the server's node id>

# A reachable server that relays for others (open TCP and UDP 4001).
plumb run --data /data --network --relay \
  --public-addr /ip4/198.211.114.63/tcp/4001 \
  --public-addr /ip4/198.211.114.63/udp/4001/quic-v1
```

* The node id is printed at start ("joined the Plumb network as 12D3Koo...") and shown in `GET /api/status` under `network.peer_id`, with the addresses it listens on, its NAT status, its relays, and counts of batches held, published and received.
* The node key is `DIR/net/node.key`. Keep it to keep the same id; a server's id is part of the bootstrap address others use.
* The Docker image exposes 4001; publish it with `-p 4001:4001/tcp -p 4001:4001/udp` on a server that relays.
* No bootstrap node runs yet, so for now nodes are joined by hand with `--bootstrap`. Once plumbsearch.org runs a relay node, its address becomes the default and `--network` the default too.

## What the prototype proves, and what it does not

Tested on one machine (`cargo test -p plumb-net`, `cargo test -p plumb-node a_node_in_the_network`):

* Four nodes and a relay: a batch published by one reaches all the others; a node that joins later catches up; a network search fetches buckets under throwaway identities, each sealed through another node, and returns a verified site with its crawler named; a bucket is fetched through the relay alone; a node behind the relay is reached through it, and hole punching then opens a direct connection.
* Sealed requests: a relay hands out the target's own key, the same one each time; a search's requests all go through relays and none straight to the node answering; a verified site comes back.
* A whole `plumb run` node in the network writes a bucket table with each index, serves it to other nodes, searches the network through `/api/network/search` and `/network`, takes in another node's batch, and searches it from its own index after a rebuild.
* Unit tests: Merkle proofs for every tree size up to 33, tampered records, re-dated headers, swapped keys, unassigned homepages, injected link text, forged proofs in search answers, links that point away from the site they name, sealed requests and answers that round-trip, keys swapped by a relay or expired, an old key accepted only until it expires, and padding.

Tested across machines on 2026-10-03: a relay node on plumbsearch.org (in Docker) and a node on a home Mac behind NAT, no port forwarding. The Mac joined through the relay within 20 seconds, the two shared crawl batches both ways, and private bucket searches were answered in both directions (4 of 4 buckets every time, each fetch from a fresh identity). Two bugs found on the way and fixed: the relay had no usable address for nodes behind NAT (now: identify pushes address changes, and a relay reaches its reserved clients through its own loopback address, since a container may not reach its own public IP), and nodes passed their home-network addresses to the whole network (now kept for nodes on the same network). Then a third node, a Linux machine on the same home network as the Mac, joined. The two home nodes first never met (fixed: relayed addresses now go into Kademlia, so nodes behind NAT find each other), then could not connect through their shared router (fixed: mDNS, see above). With mDNS the Mac found the Linux machine within a second and connected to it directly, and searches from every node got answers to all 8 bucket fetches, most sites confirmed by two nodes. Not yet tested: hole punching between two different home networks, and anything at scale.

## Next steps

Roughly in order; the first two are what the roadmap's Phase 2 gate ("two nodes stay identical using daily changes alone") needs.

1. **Index snapshots.** A Merkle-rooted snapshot of the shared site list, so a new node downloads it from any node instead of each node building its own from seed data, and two nodes can check they agree. Daily changes are then the batches since the snapshot.
2. **Agreement between crawlers.** Keep a homepage's new title only once two crawlers agree (or after a spot-check re-fetch), once the network is big enough for that. Today the newest signed crawl wins.
3. **Abuse limits.** Connection limits, per-node rate limits on requests, peer scoring in gossipsub, and banning keys whose batches fail checks.
4. **An unpredictable epoch seed** from a public randomness beacon (drand), so keys cannot be made in advance for a target site.
5. **Desktop app**: a switch for joining the network, crawling only when idle and on power, with a bandwidth cap.
6. **plumbsearch.org as the first bootstrap and relay node**, then on by default.
7. Phase 3 and 4 pieces from the white paper: homepage fetch receipts, crawl tokens, and private popularity reports.
