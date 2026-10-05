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
* **Known weakness:** the epoch is public, so someone after one particular site can make keys until one is assigned to it (with a share of one eighth, any key gets a given site about every 8 days anyway). Assignment spreads the work and caps how much one key writes; it does not stop a targeted attack by itself. Agreement between crawlers (below) means one such key is not enough, and a fresh key does not count at all until its crawls have matched the checking node's own (see "One person, many keys"), so grinding keys gains little. The drand seed in "Next steps" would close the rest.

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

A batch may also carry a site's recent headlines from its feed, each site's on a line of its own; a node takes those only from its own and trusted crawlers (see [news.md](news.md)).

Batches older than 7 days or dated in the future are refused. A node keeps the batches it holds for 35 days (`DIR/net/batches/`), serves them to others, and uses them to prove its search answers.

## Agreement between crawlers

`plumb_net::agree`. A record from the network counts only once two or more different nodes crawled it and saw the same thing, so one bad node cannot poison the index alone.

* **Homepage facts.** For each site the node holds the latest crawl from each crawler (its own crawls included). Once two crawlers' crawls agree, the newest of them goes to the inbox, with only the aliases both saw. Until then the node keeps what it had: its own crawl or the seed data.
* **"The same."** Homepages change, so this is not byte equality: the URLs must be on the same host, and the titles and descriptions must share at least 75% of their words after the index's own text normalization (case, punctuation and `U.S.`/`US` ignored). When per-site embedding text (title, description, headings, Wikidata description) lands from the search quality work, it can use the same normalization.
* **Link text and new domains.** Held per target site and crawler. A new domain counts once two crawlers named it, a piece of link text once two crawlers reported it, and the count of linking sites is the one at least two crawlers reached.
* **Scoring crawlers.** Each confirmation gives every crawler in the agreeing group an agreement, and every other crawler whose crawl of that site was made within 2 days but did not match a disagreement (further apart, the site may simply have changed). A crawler judged at least 10 times that agrees less than half the time is distrusted: its crawls are still held and scored, but no longer count towards agreement. The node's own crawls always count.
* **State.** Nothing new on disk: the node rebuilds it at start from the batches it holds, oldest first. Crawls older than 14 days are dropped, so a distrusted crawler can earn its way back. `GET /api/status` shows `network.agreement`: sites pending, sites confirmed, crawlers distrusted.
* **How soon a site is confirmed.** Assignment stays independent per node (a site is crawled by about one node in eight each day), so with N nodes a site gets about N/8 crawls a day. Over the 14-day window a site is assigned to two nodes with about 72% odds in a network of 2 nodes, 94% with 3, and almost surely with 5 or more. In a network of one, nothing from the network is ever taken in, which is the point.
* **Own crawls judge straight away.** A crawl made within 2 days of one of the node's own crawls of the same site is scored against it at once, confirmed or not.

* **Trusted nodes.** While the network is a handful of nodes, waiting for two crawlers mostly keeps good crawls out. So a node keeps a list of nodes it trusts (Liz, 2026-10-03): `plumb run --network --trust-peer 12D3Koo...` (repeatable). Every node trusts the plumbsearch.org node (`12D3KooWEDPBv4sacn42shoToAwu62CreVC89QFiAA31HrWv3xrg`) by default (Liz, 2026-10-03), so a new node takes in crawls from the start; `--no-default-trust` turns that off. A crawl signed by a trusted node is taken in at once, like the node's own, and counts towards any quorum. Like the node's own, it is not held to the daily assignment (Liz, 2026-10-03), so one of a person's machines can crawl far more than its share for their other nodes, and its homepages' headings and text are kept too, for search by meaning; every other crawler goes through the rules in this section. Being trusted earns a crawler nothing else: it is scored like any other, and matching its crawls vouches for no one. `GET /api/status` shows `network.agreement.trusted_peers`. When a node starts with a crawler it did not trust at its last start, it takes in that crawler's held batches again as trusted, so their homepages' text is kept too (`DIR/net/trusted-applied` notes which were); a newer crawl of a site still wins.
* **Who a search asks.** A node's network searches, background rounds and the sealed requests it relays for private search in the browser ask only the nodes in its search scope (Liz, 2026-10-05): `trusted` (its trusted nodes), `friends-of-friends` (those and the nodes they trust, the default) or `anyone`. Set with `plumb run --search-from <who>`, in the panel under Network & privacy, or from the desktop app for a connected node. A node asks each trusted node what it trusts on `/plumb/trust/1` when they connect, keeps the answers in `DIR/net/friends.json` and dials those nodes; only one hop is followed. Changing the scope empties the bucket cache. `GET /api/status` shows `network.search_scope`, `network.search_peers` and `network.friends_of_friends`. Connecting a node in the desktop app ("+ Connect to a node") makes the two trust each other: each saves the other's id in its trusted nodes, applied when it restarts.

### One person, many keys

A node key costs nothing to make, so one person could run many keys and agree with themselves. Batches travel by gossip, so a node usually cannot see which IP address a crawler crawls from, and grouping keys by address would not work. Instead each node checks crawlers against fetches it made itself:

* **A key earns its vote.** Once a node holds any crawl of its own, another crawler counts towards agreement only after its crawls matched the node's own crawls of 3 different sites (`VOUCHES_NEEDED`), and while it is not distrusted. Sending the same crawl again does not count twice. A crowd of fresh keys counts for nothing, and the only way to get a vote is to crawl honestly first. A crawl that matches the node's own crawl of that very site needs no vouching, since that match is the check.
* **Disputes are settled by fetching the site.** A quorum without the node's own crawl is held, not released, when another counting crawler saw something else within 2 days, when the node's own crawl saw something else, or when it contradicts the record last confirmed for the site. The site goes on a recheck list (at most 1,000), and the node's next crawl round fetches up to 100 of them first, whether or not it is assigned them that day. Its own crawl then decides: the side it matches is released, and the other side's crawlers each get a disagreement. Those fetches are published like other crawls, but other nodes ignore crawls of sites the crawler was not assigned, and they are never used as proofs in search answers. So keys that earned their votes and then turn on a site lose them, and the site is not changed meanwhile.
* **Network search.** "Two nodes agree" on a network result now means two crawlers the asking node counts, so two fresh keys signing the same crawl only get "signed crawl, checked".
* **Nodes that do not crawl** cannot check anyone, so they count every crawler that is not distrusted, and in a dispute the side with more counting crawlers wins.
* `GET /api/status` shows `network.agreement.vouched_crawlers` and `disputed_sites`.
* **Cost to normal nodes:** none for searching. A new honest node's crawls start counting on other nodes after a few of its crawls overlap theirs, which in a small network happens within days. The rechecks are at most 100 homepage fetches per round, and only when crawlers disagree.
* **Known gaps.** Someone who runs several keys that crawl honestly for a while can still agree on a site no node with a crawl of its own has checked, and that was never confirmed before; random spot checks of confirmed sites would catch that. The epoch is still public (drand seed below), and crawl tokens would put a price on each key.

## Spreading batches

`plumb_net::node`, `plumb_net::proto`.

* A new batch's signed header is announced on the gossipsub topic `plumb/batches/1`. Gossip messages are validated before they are passed on, so a bad header stops at the first honest node.
* A node that hears of a batch it lacks fetches it with `/plumb/batch/1` from the node that passed the header on, or from the crawler, at most 16 at a time.
* When two nodes meet, each asks the other for the headers of the batches it holds from the last 3 epochs, and fetches what it missed. A node that was off for a day catches up this way.
* Records confirmed by a second crawler (see above) go to `DIR/net/inbox.jsonl` as they arrive. Between two pieces of work, the node folds the inbox into its records file through the same journal crawls use. It rebuilds its index once 2,000 records have come in, or at its next refresh.

## Filling free space

A node takes in crawls as they are shared, but one that just joined
holds only the last few days of them. So every 10 minutes a node with
room asks a node it trusts (plumbsearch.org's by default) for its crawled
sites over `/plumb/fill/1`, 1,000 at a time, best-ranked first, and takes
them in like any shared crawl (`plumb_net::fill`, `plumb-node`'s
`node/fill.rs`). It stops at 90% of the storage limit set on the panel,
so its own crawling keeps room, and at the day's download limit; with no
storage limit (the server default) it takes the whole list, a full copy
of the shared index, unless that index would be too big to rebuild: it
stops before an index build would need more than half the machine's
memory (or its container's limit), about 2.5 KB a site, so a 4 GB server
stops at about 800,000 sites. How far it got is in `DIR/net/fill.json` and in
`GET /api/status` under `fill`, and the panel's Storage card shows it.

**Staying under the storage limit** (Liz, 2026-10-05: "my macbook seems to
gone over the limit on storage, maybe we find a interesting way to remove
data that is not of interest"). Crawls other nodes publish keep arriving
after filling stops, and page sets, places and vectors sit next to the
sites, so a node with a limit also holds itself back (`node/trim.rs`):

- At 90% of the limit, a shared crawl only refreshes a site the node
  already holds. A new site is taken in only when it is about one of the
  node's topics (its focus topics and the interests on its About pages) or
  is an official website (Wikidata).
- Still over the limit half an hour later (after the page sets and places
  below are cut), the least useful sites go, lowest link score first, until
  the data folder is back at 85%, and the index is built without them.
  Never dropped: the best 100,000 sites, sites about the node's topics,
  sites an About page always puts first or a searcher opened from the
  results, official websites, and plumbsearch.org. Filling then rests a
  week, so it does not take lower-ranked sites in their place. The node's
  journal says how many went; it looks again an hour later.

- Page set files hold no more pages than the node keeps: a file taken
  whole when the limit was higher is cut to the pages kept, and taken again
  from a trusted node if more are wanted later.
- Places past the first million (every city, town and place with a
  Wikidata item) are kept only within 100 km of a town given on one of the
  node's About pages; only a node with no limit keeps every café and shop
  everywhere (see [places.md](places.md#how-many)).
- Signed batches are kept 14 days (as long as crawls are checked against
  each other) rather than 35, unless `--keep-batches-days` says otherwise.
  Other nodes take none older than a week.

Dropping a site only takes it out of that node's index: the batches the
node signed and published stay in `net/batches/` for those 14 days, so the
network loses none of the crawls it still uses.

The records come from the trusted node's index, not signed batches, so
only trusted nodes are asked. A node answers at most 2 fill requests at
once and 6 a minute from one node, so a new node cannot swamp a small
server. The panel's "Fill free space with the network's crawls" box (on
by default, in the desktop app too) and `--no-fill` turn it off.

**Blackhole** (Liz, 2026-10-05: "a blackhole setting for docker nodes that
just tries to get all the data possible"). `plumb run --network
--blackhole` is for a server with disk and memory to spare. It:

- fills from every connected trusted node, not just one: once through one
  node's list it goes on to the next, and goes through each again a day
  after it last did (`blackhole_peer` in `node/fill.rs`; the panel's Storage
  card and `fill.lists_done` in `/api/status` show how many lists it holds);
- keeps every page set left on Automatic in full (Wikipedia, GitHub, Stack
  Overflow, books, papers), whatever the storage limit; sets turned off or
  cut to a number on the panel stay so;
- keeps the network's crawl batches for good, unless `--keep-batches-days`
  is given, and asks each node it meets for all the batches still taken
  (the last 7 days) rather than the last 3.

The storage limit, the day's download limit and the memory an index build
may take still hold, and it follows the node's trust rules: fill records and
page set files only come from trusted nodes, and other crawls still need a
second crawler to agree.

**Setting up from the network** (Liz, 2026-10-04: "new nodes don't need
to pull from wiki or anywhere anymore"). A new node in the network that
trusts a node sets up from it instead of downloading the seed data: it asks
with `all` set, and the trusted node sends every site of its list, crawled
or not, up to 5,000 a page. Each record carries what the trusted node's own
seed gave it (Tranco and Common Crawl ranks, Wikidata names, countries,
kinds and descriptions, Wikipedia intros), so nothing is downloaded from
Tranco, Common Crawl, Wikidata or Wikipedia. The best 50,000 make the first
index; filling then takes the rest, a round a minute, until the node holds
`--sites` of them (or as many as its storage and memory allow), and goes on
filling as usual from there. When no trusted node answers within two
minutes, or it sends fewer than 1,000 sites, the node downloads the seed
data as before; so does `--seed-from-outside`, `--no-fill`, or a node that
trusts no one. Nodes from before this ignore `all` and send crawled sites
only, which still sets a node up, without the uncrawled ones. The ranks and
facts are as fresh as the trusted node's seed: refreshing them means
reseeding that node.

## Network search

The query text stays on the asking node. Relaying separates the asker's address from the answering node when an eligible relay exists; direct requests expose that address. Bucket numbers, timing and operator collusion remain privacy limitations. See the [privacy review](reviews/privacy-security.md).

* `GET /network?q=` on a node's web page (linked from every results page as "Ask other Plumb nodes too"), and `GET /api/network/search?q=` as JSON.
* **Buckets, not queries.** Every node that answers searches keeps a bucket table next to each index it builds (`indexes/<n>/buckets/`). Each site is filed under its keys: its words and joined names from the domain label, title, aliases and top link texts. A key goes to one of 16,384 buckets by hash, and each key keeps its best 32 sites by link score.
* **Asking.** The asker turns the query into keys the same way and first checks its retained bucket cache. With background rounds enabled, missing or overdue buckets enter an in-memory queue for the next scheduled round. Each round contains exactly 4 buckets, padded with background buckets and shuffled, asked over `/plumb/bucket/1` from up to 2 connected nodes. Answering nodes see bucket numbers. Public hashes allow likely queries to be tested against observed bucket sets; padding does not eliminate that inference.
* **A throwaway identity for every request.** Each bucket fetch uses a fresh node key and its own short-lived connection, dialed straight to the node or through its relay. This avoids reusing the asker's permanent key; addresses and timing can still link requests, particularly with direct retrieval or colluding operators.
* **Through a relay, sealed** (Oblivious HTTP, RFC 9458). On its own, a throwaway identity still shows the node asked the IP address the request comes from. So each bucket request goes through another node, picked at random for every request, over `/plumb/oblivious/1`:
  1. The asker asks the relay for the target node's key. The relay fetches it from the target over its own connection and hands the same key to everyone who asks for 10 minutes, so the target cannot give each asker a key of its own and recognize them by it. The key is signed with the target's node key, so the relay cannot swap in its own.
  2. The asker seals the bucket request to that key (HPKE: X25519, HKDF-SHA256, AES-128-GCM, with Mozilla's `ohttp` crate) and hands it to the relay, which passes it to the target and the sealed answer back.
  * The relay sees the asker's IP address and which node it asks, but not the bucket. The target sees the bucket, but only the relay's address. Requests are padded to one size, and answers to the next of 2^k and 1.5 × 2^k bytes (at least 4 KiB), because the relay holds much the same buckets and could otherwise tell the bucket by the answer's size. Only a relay and a target working together can tie an IP address to a bucket, and the asker picks a new pair for every request.
  * Target keys are made in memory, never written to disk, and replaced daily; the previous key is accepted until it expires a day later.
  * Every node that answers searches also relays. If a relay cannot reach the target, one other relay is tried. A request goes straight to the target only when there is no other node to relay it (a network of two). The search page says when that happened, and the JSON answer counts `relayed` and `direct` requests. `GET /api/status` counts `requests_relayed` (sealed bucket requests and popularity reports passed on) and `relaying_peers` (connected nodes that can relay).
  * What is sealed is Plumb's own CBOR bucket request and answer (or a popularity report, see below), not Binary HTTP, so this is RFC 9458's encapsulation rather than full Oblivious HTTP. The same code can serve a browser: the asker's half (checking keys, sealing, opening, padding) is `plumb_core::oblivious` behind the `oblivious` feature, which builds for `wasm32-unknown-unknown` (with `getrandom` 0.4's `wasm_js` feature in the WASM crate), and `NetHandle::oblivious_keys` and `NetHandle::oblivious_forward` let a public node's web server relay sealed requests from a WASM client to any node in the network.
* **Checking answers.** For every site the answering node holds a signed crawl of, it attaches a **record proof**: the signed batch header, the record and its Merkle path. The asker checks the signature, the path and the assignment. An answer with any proof that does not check out is dropped whole. Sites without a proof (from seed data) are shown as unsigned, their link always goes to `https://<the domain it names>/`, and their popularity signals are taken as the worst any node reported.
* **Two crawlers' proofs.** When the answering node holds crawls of a site from more than one crawler, it attaches the proof of the newest crawl that another crawler agrees with, plus that other crawler's proof (the `also` field; older nodes send none and ignore it). The asker checks every proof and marks a site **confirmed** once signed crawls from two different crawlers agree, whether one answer carried both or two answers carried one each. A confirmed copy wins over a single signed crawl from another answer. The results page says "signed crawls, two nodes agree", and `/api/network/search` has `confirmed` per hit.
* **Scheduled rounds** (`plumb_net::rounds`). Rounds run at fixed deadlines, 10 minutes apart by default (30 in the desktop app; `plumb run --round-minutes`). Searches use cached data and enqueue misses; they cannot trigger a round or skip a future slot. A due round takes up to 4 queued buckets and fills remaining positions with background buckets. Failed work waits for another slot; missed slots do not create catch-up bursts. With `--round-minutes 0`, background rounds are off and network searches explicitly retain immediate retrieval. Peer availability, size classes and relay/target collusion still affect observable traffic, so this is not a guarantee of unobservability. `GET /api/status` shows `network.rounds`: cadence, rounds sent, answers and bytes fetched; it does not count searches separately.
* **Persistent cache.** Whole buckets, including empty answers, survive restarts. Twelve hours marks them overdue for a background refresh rather than deleting them. Retained data can answer searches offline; proofs are revalidated and overdue results are labeled. Storage is bounded to 4,096 buckets and 512 MiB, with 16 MiB per serialized bucket, oldest fetch first. See [cache behavior and corpus sizing](cache-first-search.md).
* **Ranking locally.** The asker keeps the sites that match the query's keys, builds a small temporary index of them and ranks them with its own ranking, the same as a local search.
* Cost: measured on a 50,000-site index (the plumbsearch.org test node, mostly uncrawled seed sites), the whole bucket table is 3.6 MB and a bucket holds about 3.5 sites on average, so a search moves a few KB (padding adds at most half, and at least 4 KiB per answer). Crawled sites have more keys, so a fully crawled 1M-site index will have bigger buckets; still well under the relay's 128 KiB per circuit. An answer over 20,000 sites is refused.

## Popularity sharing

`plumb_net::popularity`, `plumb_net::reports`, switched on with `plumb run --network --share-popularity` (off by default). Rankings learn which site people actually pick for a search, without any node learning who searched for what. This follows the design discussed on 2026-10-03, with a simple per-node cap in place of crawl tokens for now.

**What a sharing node notes.** Its result pages link through `/go?q=&d=`, which redirects to the site and notes the pick ("searched `us bank`, picked usbank.com") in `DIR/net/picks.json`, on that node only. The file holds the current week and nothing older. The result page says, under the results, that picks are shared. `/go` only redirects to a site the same search returns, so it cannot be used to send people elsewhere. Only short, plain queries are ever noted: normalized, at most 6 words and 64 characters, no `@`, and no run of 4 or more digits (phone, account and street numbers, dates).

**What it sends.** At random times, 20 to 100 minutes apart, it turns its most-made pick not yet reported this week into a report, at most 8 a day and each pick at most once a week. A report is **threshold-encrypted** with STAR (Brave's [`sta-rs`](https://github.com/brave/sta-rs), MPL-2.0): it carries a tag, the encrypted pick and one secret share of the key. Reports of the same pick in the same week share the tag, and the key comes back only from 10 shares (`REPORT_THRESHOLD`). Below that, a report reveals nothing but its tag. A report is a few hundred bytes.

**How it travels.** The node hands the report to a random connected node under a throwaway identity and a connection of its own, the same way network searches fetch buckets: sealed to that node's key and passed on by a third node picked at random, the relay (`/plumb/oblivious/1`, as in "Network search" above). Every sealed report is padded to 4 KiB (`REPORT_REQUEST_SIZE`), so the relay cannot tell one pick from another by size, and the node it reaches sees only the relay's address. Only in a network of two, where no other node can relay, does it go straight over `/plumb/report/1`. That node keeps it and passes it on over the gossip topic `plumb/reports/1`, so it reaches every node as that node's message, not the sender's. A node that meets another asks it for this week's and last week's reports, so a node that was away catches up.

**How it is counted.** Every node keeps two weeks of reports (`DIR/net/reports/<week>.jsonl`, at most 200,000 a week) and counts them itself, so any node can count or recount and all get the same table. Reports are grouped by tag and ciphertext. A group of 10 or more gives up its key: the node decrypts the pick and checks it by making a report of that pick afresh, which must give the same tag and ciphertext, so forged shares cannot slip in a different pick. A group with a bad share in it is tried again with other subsets of its shares. Shares made for a lower threshold are refused. The result is written to `DIR/net/popularity.json` and recounted every 10 minutes while new reports arrive.

**How it ranks.** Every node in the network, sharing or not, blends the table into its own searches: it ranks 20 results, adds a bonus of up to 0.15 to each site picked for the query (the full amount for the site picked most, less in proportion for others), and sorts again. 0.15 is about a fifth of what naming a site exactly earns, so popularity settles close calls without overruling a name match.

**What this protects, and what it does not yet.**

* No node, the receiving one included, can read a pick until 10 reports of it were sent, and the reports carry no node id.
* Raw queries never leave the node, and the local log forgets them after a week.
* **Guessable picks.** This is STARLite: a report's randomness comes from the pick itself, so anyone can guess a pick ("us bank, usbank.com"), compute its tag and see whether it was reported. They learn that somebody reported it, not who. Full STAR fixes this with a randomness server (an oblivious PRF, `ppoprf`) whose key is rotated weekly; run by a group of nodes, that is a later step. The query filters above keep reportable picks short and navigational, which is what makes this tolerable for now.
* **IP addresses.** The node a report is handed to sees only the relay's IP address; the relay sees the sender's but not the report. Only a relay and a receiving node working together could link the two, and the sender picks a new pair for every report.
* **Bots.** Nothing yet stops one machine from sending many reports of one pick under many throwaway identities, and so pushing a site up for a query. The per-node cap only binds honest nodes. Limits now: the bonus is small and bounded, a pick only counts for sites the asking node's own search returns, and stuffed reports cost the attacker 10 identities per pick per week. The real fix is anonymous crawl tokens (Privacy Pass style): a report must spend a token earned by verified crawling. Tokens exist now (see "Crawl credits"), but a report reaches every node, so it needs a token any node can check, which these are not yet.

Tested on one machine (`cargo test -p plumb-net popularity`, `cargo test -p plumb-node popularity`): reports below the threshold stay unreadable, other picks and other weeks do not help, copies count once, forged shares and junk do not break counting, low-threshold shares are refused; across three nodes a report handed in under a throwaway identity, sealed through a relay, reaches the others, a pick becomes readable at the tenth report, and a node that joins later catches up and counts the same; a whole `plumb run` node notes a pick through `/go`, sends its report to another node, and after nine more reports of the same pick ranks that site higher by the bonus.

## Crawl credits

`plumb_net::credits`, on for every network node. Crawling for the network earns credits, and a node turns them into anonymous one-time tokens it can spend at the node that issued them. This follows the credits design Liz approved on 2026-10-03: credits cannot be given away or sold, early adopters get a head start, and people who only search on a website never see any of it.

**Earning.** Each node keeps its own ledger of every crawler it hears from (`DIR/net/credits/ledger.json`). A crawler earns 1 credit for each homepage crawl that a crawler the node trusts strictly also made: the node itself, or one vouched for by matching the node's own crawls of 3 sites. A crawl that counted on its own, or that only fresh keys agree with, earns nothing, whatever agreement's quorum rules are. Crawls earn 2 for crawls made before 2027-10-01, the network's first year. A crawl made close in time to one two such crawlers made that does not match it costs 5, so making pages up loses more than honest crawling earns. A node the ledger's node trusts (`--trust-peer`, plumbsearch.org by default) earns 1 for each new crawl taken in from it (2 in the first year), since trusted crawls skip agreement and would otherwise earn nothing. And a node earns 1 for each bucket request of the ledger's node it answered with records that checked out: only the node that asked can vouch for that work, so only its ledger counts it. Every node sees the same signed batches, so ledgers come out much the same, but each node goes only by its own. Credits counted while rebuilding agreement at start are not counted twice.

**Tokens.** A node asks another node, the issuer, for tokens over `/plumb/credits/1`, under its own node id, since its balance pays. The issuer gives tokens only to a node whose work counts there: a crawler vouched for by matching its own crawls of 3 sites (whatever agreement's quorum rules) and judged at least 10 times, a node it trusts, or a node that answered at least 10 of its bucket requests, at most as many as its balance pays for, 1 credit each and at most 64 a request. Tokens are signed blind with a VOPRF over ristretto255 (the [`voprf`](https://crates.io/crates/voprf) crate, as in Privacy Pass, RFC 9578): the issuer never sees the token it signs. Handed back later under a throwaway identity, a token shows the issuer it is one of its own and not yet spent, but not which node it went to. Each issuer proves every batch was signed with the same key, and the wallet (`DIR/net/credits/wallet.json`) keeps the first key it sees for each issuer and refuses tokens under another, so an issuer cannot give one node a key of its own to recognize it by. The issuer's key is `DIR/net/credits/token.key`, and the tokens handed back to it are in `DIR/net/credits/spent`.

**What tokens buy: priority when busy** (Liz's pick, 2026-10-03). A node answers 8 bucket requests at once for free (`NetConfig::max_answering`) and turns more away as busy (`busy` in the answer). It also shares out its free answers (`plumb_net::allowance`, 2026-10-05): any one address, or any one relay passing on sealed requests, gets 120 a minute on average with bursts of 240 (a relay the node trusts is not held to a rate), and the owner can cap free answers a day with `--answer-per-day` or on the panel (no cap by default). Bucket requests come under throwaway identities, so an address is all a node can go by, and a token is the only way a request can show it comes from a node that did its part. Searches through the node's own front end are never limited. A request that carries one of its tokens still gets in, up to 8 more at once, and the token is spent. A searching node asks again with a token only after a node said it was busy, so tokens go only where they help, and a free search of a busy node simply comes back without that node's answer. Sealed requests that carry a token are padded to 256 bytes rather than 64: the relay can tell a paid request from a free one, but not the bucket. A node keeps at least 4 tokens from each node it searches, asking for 16 more at most every 30 minutes (`NetConfig::collect_tokens`, on by default, off with `--no-spend-credits` or on the panel); a node whose ledger has nothing for it says no. Nodes that predate tokens ignore them. Nobody who searches on a website ever deals with tokens: the site's own node spends them.

**Limits now.**

* A token is good only at its issuer, so a node's credits at one issuer are what that issuer counted for it, and a node can spend its balance once at every issuer. That is fine while tokens buy only extra work from the node that issued them.
* The issuer sees a node ask it for tokens and, later, paid requests arrive. A node buys tokens ahead of time and in batches, so this links little, but it is not nothing.
* An issuer that hands a node a key of its own from the very first token would go unnoticed; the wallet only catches a key that changes. Checking keys through a relay, as sealed requests already do, would close that.
* Credits follow agreement's rules on who counts, so its limits on one person running many keys apply here too.

`GET /api/status` shows `network.credits`: this node's own balance as it counts it, its confirmed and mismatched crawls, the accounts it keeps, the tokens it issued, had handed back and holds, its credits at each node it searches as that node last said (`at_peers`, asked every 30 minutes), free answers today and the daily cap, requests turned away, and how many nodes have credits with it. The panel's Network section shows the same as a Credits card, with the daily cap and the spending switch under the network settings.

Tested on one machine (`cargo test -p plumb-net credits`): confirmed crawls earn and mismatches cost, early crawls earn double, tokens go only to crawlers that count and can pay, the ledger, the issuing key and spent tokens survive a restart, a token is spent once, forged and other issuers' tokens are refused, a proof for other tokens does not check out, and the wallet refuses a changed key; a sealed request carrying a token opens at its own size; across two nodes, a node that answers nothing for free turns a search away and then answers it for tokens, one spent per request; across three nodes, two crawlers that agree on 10 sites each earn 20 credits in both ledgers, one buys 8 tokens and then what is left, and a node that never crawled gets none.

## Running it

```sh
# A node at home: dials out only, and starts from plumbsearch.org.
plumb run --data plumb-data --network

# Also share which result is opened, anonymously (off by default).
plumb run --data plumb-data --network --share-popularity

# Also share what a separate `plumb crawl` is filling in (every half hour,
# crawls from the last 6 days), and crawl 128 homepages at once.
plumb run --data plumb-data --network --crawl-concurrency 128 \
  --publish-records /path/to/crawl/records.jsonl

# One person's crawlers covering every site between them: each crawls
# any site, not just its daily share, and they split the sites by hash
# (each names the others; one that stops crawling for a day hands its
# share to the rest). Nodes that trust them take all their crawls.
plumb run --data plumb-data --network --crawl-any-site \
  --crawl-with <other node's id> --crawl-with <third node's id> \
  --refresh-hours 1 --crawl-per-refresh 50000 --crawl-concurrency 64 \
  --keep-batches-days 14

# A test network kept apart from the real one.
plumb run --data test-data --network --no-default-bootstrap --no-default-trust \
  --bootstrap /ip4/192.168.1.20/tcp/4001/p2p/<that node's id>

# A reachable server that relays for others (open TCP and UDP 4001).
plumb run --data /data --network --relay \
  --public-addr /ip4/198.211.114.63/tcp/4001 \
  --public-addr /ip4/198.211.114.63/udp/4001/quic-v1
```

* `GET /api/status` also shows `network.reports_held`, `reports_sent` and `popular_picks`.
* The node id is printed at start ("joined the Plumb network as 12D3Koo...") and shown in `GET /api/status` under `network.peer_id`, with the addresses it listens on, its NAT status, its relays, and counts of batches held, published and received.
* The node key is `DIR/net/node.key`. Keep it to keep the same id; a server's id is part of the bootstrap address others use.
* The Docker image exposes 4001; publish it with `-p 4001:4001/tcp -p 4001:4001/udp` on a server that relays.
* Every node starts from the relay on plumbsearch.org (`/dns4/plumbsearch.org/tcp/4001/p2p/12D3KooWJ2UWUBsxmPfXTfHa8cBBmzifa6kj5pFZKfJXYNQyJ69a`, `plumb_net::DEFAULT_BOOTSTRAP`) as well as any `--bootstrap` nodes, unless given `--no-default-bootstrap`. The desktop app joins the network when it starts; `plumb run` joins with `--network`, which the Docker image and `docker-compose.yml` pass (docs/docker.md).

## What the prototype proves, and what it does not

Tested on one machine (`cargo test -p plumb-net`, `cargo test -p plumb-node a_node_in_the_network`):

* Four nodes and a relay: a batch published by one reaches all the others, and its site counts only once a second crawler (the relay) publishes a matching crawl; a node that joins later catches up; a network search fetches buckets under throwaway identities, each sealed through another node, and returns a verified site with its crawler named; a bucket is fetched through the relay alone; a node behind the relay is reached through it, and hole punching then opens a direct connection.
* Sealed requests: a relay hands out the target's own key, the same one each time; a search's requests all go through relays and none straight to the node answering; a verified site comes back.
* A whole `plumb run` node in the network writes a bucket table with each index, serves it to other nodes, searches the network through `/api/network/search` and `/network`, holds another node's crawl until a second node's crawl agrees, then takes it in and searches it from its own index after a rebuild.
* Unit tests: Merkle proofs for every tree size up to 33, tampered records, re-dated headers, swapped keys, unassigned homepages, injected link text, forged proofs in search answers, links that point away from the site they name, sealed requests and answers that round-trip, keys swapped by a relay or expired, an old key accepted only until it expires, and padding.

Tested across machines on 2026-10-03: a relay node on plumbsearch.org (in Docker) and a node on a home Mac behind NAT, no port forwarding. The Mac joined through the relay within 20 seconds, the two shared crawl batches both ways, and private bucket searches were answered in both directions (4 of 4 buckets every time, each fetch from a fresh identity). Two bugs found on the way and fixed: the relay had no usable address for nodes behind NAT (now: identify pushes address changes, and a relay reaches its reserved clients through its own loopback address, since a container may not reach its own public IP), and nodes passed their home-network addresses to the whole network (now kept for nodes on the same network). Then a third node, a Linux machine on the same home network as the Mac, joined. The two home nodes first never met (fixed: relayed addresses now go into Kademlia, so nodes behind NAT find each other), then could not connect through their shared router (fixed: mDNS, see above). With mDNS the Mac found the Linux machine within a second and connected to it directly, and searches from every node got answers to all 8 bucket fetches, most sites confirmed by two nodes. Not yet tested: hole punching between two different home networks, and anything at scale.

## Next steps

Roughly in order; the first two are what the roadmap's Phase 2 gate ("two nodes stay identical using daily changes alone") needs.

1. **Index snapshots.** A Merkle-rooted snapshot of the shared site list, so a new node downloads it from any node instead of each node building its own from seed data, and two nodes can check they agree. Daily changes are then the batches since the snapshot.
2. **Agreement between crawlers: built** (see above). Network search answers carry proofs from two agreeing crawlers. Fresh keys must earn a vote and disputed sites are re-fetched (see "One person, many keys"). Still to do: random spot checks of confirmed sites, and a check in snapshots that each record was confirmed.
3. **Abuse limits.** Connection limits, per-node rate limits on requests, peer scoring in gossipsub, and banning keys whose batches fail checks.
4. **An unpredictable epoch seed** from a public randomness beacon (drand), so keys cannot be made in advance for a target site.
5. **Desktop app**: a switch for joining the network, crawling only when idle and on power, with a bandwidth cap.
6. **plumbsearch.org as the first bootstrap and relay node: built.** Every node starts from it, and the Docker image joins the network by default.
7. Phase 3 and 4 pieces from the white paper: homepage fetch receipts, and tokens any node can check to pay for popularity reports (crawl credits and tokens good at their issuer are in, see above). Popularity reports themselves are in (see above); next for them is a randomness server run by a group of nodes, so picks cannot be guessed.
