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

* **Trusted nodes.** While the network is a handful of nodes, waiting for two crawlers mostly keeps good crawls out. So a node keeps a list of nodes it trusts (Liz, 2026-10-03): `plumb run --network --trust-peer 12D3Koo...` (repeatable). A crawl signed by a trusted node is taken in at once, like the node's own, and counts towards any quorum; every other crawler goes through the rules in this section. Being trusted earns a crawler nothing else: it is scored like any other, and matching its crawls vouches for no one. `GET /api/status` shows `network.agreement.trusted_peers`.

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
  * Every node that answers searches also relays. If a relay cannot reach the target, one other relay is tried. A request goes straight to the target only when there is no other node to relay it (a network of two). The search page says when that happened, and the JSON answer counts `relayed` and `direct` requests. `GET /api/status` counts `requests_relayed` (sealed bucket requests and popularity reports passed on) and `relaying_peers` (connected nodes that can relay).
  * What is sealed is Plumb's own CBOR bucket request and answer (or a popularity report, see below), not Binary HTTP, so this is RFC 9458's encapsulation rather than full Oblivious HTTP. The same code can serve a browser: the asker's half (checking keys, sealing, opening, padding) is `plumb_core::oblivious` behind the `oblivious` feature, which builds for `wasm32-unknown-unknown` (with `getrandom` 0.4's `wasm_js` feature in the WASM crate), and `NetHandle::oblivious_keys` and `NetHandle::oblivious_forward` let a public node's web server relay sealed requests from a WASM client to any node in the network.
* **Checking answers.** For every site the answering node holds a signed crawl of, it attaches a **record proof**: the signed batch header, the record and its Merkle path. The asker checks the signature, the path and the assignment. An answer with any proof that does not check out is dropped whole. Sites without a proof (from seed data) are shown as unsigned, their link always goes to `https://<the domain it names>/`, and their popularity signals are taken as the worst any node reported.
* **Two crawlers' proofs.** When the answering node holds crawls of a site from more than one crawler, it attaches the proof of the newest crawl that another crawler agrees with, plus that other crawler's proof (the `also` field; older nodes send none and ignore it). The asker checks every proof and marks a site **confirmed** once signed crawls from two different crawlers agree, whether one answer carried both or two answers carried one each. A confirmed copy wins over a single signed crawl from another answer. The results page says "signed crawls, two nodes agree", and `/api/network/search` has `confirmed` per hit.
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

**Earning.** Each node keeps its own ledger of every crawler it hears from (`DIR/net/credits/ledger.json`). A crawler earns 1 credit for each homepage crawl that a crawler the node trusts strictly also made: the node itself, or one vouched for by matching the node's own crawls of 3 sites. A crawl that counted on its own, or that only fresh keys agree with, earns nothing, whatever agreement's quorum rules are. Crawls earn 2 for crawls made before 2027-10-01, the network's first year. A crawl made close in time to one two such crawlers made that does not match it costs 5, so making pages up loses more than honest crawling earns. Nothing else earns credits yet. Every node sees the same signed batches, so ledgers come out much the same, but each node goes only by its own. Credits counted while rebuilding agreement at start are not counted twice.

**Tokens.** A node asks another node, the issuer, for tokens over `/plumb/credits/1`, under its own node id, since its balance pays. The issuer gives tokens only to a crawler whose crawls count there (vouched for by matching its own crawls of 3 sites, whatever agreement's quorum rules, and judged at least 10 times; a node that crawls nothing itself issues none), at most as many as its balance pays for, 1 credit each and at most 64 a request. Tokens are signed blind with a VOPRF over ristretto255 (the [`voprf`](https://crates.io/crates/voprf) crate, as in Privacy Pass, RFC 9578): the issuer never sees the token it signs. Handed back later under a throwaway identity, a token shows the issuer it is one of its own and not yet spent, but not which node it went to. Each issuer proves every batch was signed with the same key, and the wallet (`DIR/net/credits/wallet.json`) keeps the first key it sees for each issuer and refuses tokens under another, so an issuer cannot give one node a key of its own to recognize it by. The issuer's key is `DIR/net/credits/token.key`, and the tokens handed back to it are in `DIR/net/credits/spent`.

**What tokens buy.** Nothing yet: the first perk is Liz's pick.

**Limits now.**

* A token is good only at its issuer, so a node's credits at one issuer are what that issuer counted for it, and a node can spend its balance once at every issuer. That is fine while tokens buy only extra work from the node that issued them.
* An issuer that hands a node a key of its own from the very first token would go unnoticed; the wallet only catches a key that changes. Checking keys through a relay, as sealed requests already do, would close that.
* Credits follow agreement's rules on who counts, so its limits on one person running many keys apply here too.

`GET /api/status` shows `network.credits`: this node's own balance as it counts it, its confirmed and mismatched crawls, the accounts it keeps, and the tokens it issued, had handed back and holds.

Tested on one machine (`cargo test -p plumb-net credits`): confirmed crawls earn and mismatches cost, early crawls earn double, tokens go only to crawlers that count and can pay, the ledger, the issuing key and spent tokens survive a restart, a token is spent once, forged and other issuers' tokens are refused, a proof for other tokens does not check out, and the wallet refuses a changed key; across three nodes, two crawlers that agree on 10 sites each earn 20 credits in both ledgers, one buys 8 tokens and then what is left, and a node that never crawled gets none.

## Running it

```sh
# A node at home: dials out only.
plumb run --data plumb-data --network \
  --bootstrap /dns4/plumbsearch.org/tcp/4001/p2p/<the server's node id>

# Also share which result is opened, anonymously (off by default).
plumb run --data plumb-data --network --share-popularity \
  --bootstrap /dns4/plumbsearch.org/tcp/4001/p2p/<the server's node id>

# A reachable server that relays for others (open TCP and UDP 4001).
plumb run --data /data --network --relay \
  --public-addr /ip4/198.211.114.63/tcp/4001 \
  --public-addr /ip4/198.211.114.63/udp/4001/quic-v1
```

* `GET /api/status` also shows `network.reports_held`, `reports_sent` and `popular_picks`.
* The node id is printed at start ("joined the Plumb network as 12D3Koo...") and shown in `GET /api/status` under `network.peer_id`, with the addresses it listens on, its NAT status, its relays, and counts of batches held, published and received.
* The node key is `DIR/net/node.key`. Keep it to keep the same id; a server's id is part of the bootstrap address others use.
* The Docker image exposes 4001; publish it with `-p 4001:4001/tcp -p 4001:4001/udp` on a server that relays.
* The desktop app joins the network when it starts, through the relay on plumbsearch.org (`/dns4/plumbsearch.org/tcp/4001/p2p/12D3KooWJ2UWUBsxmPfXTfHa8cBBmzifa6kj5pFZKfJXYNQyJ69a`). `plumb run` still joins only with `--network`, and takes its `--bootstrap` nodes by hand.

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
6. **plumbsearch.org as the first bootstrap and relay node**, then on by default.
7. Phase 3 and 4 pieces from the white paper: homepage fetch receipts, and tokens any node can check to pay for popularity reports (crawl credits and tokens good at their issuer are in, see above). Popularity reports themselves are in (see above); next for them is a randomness server run by a group of nodes, so picks cannot be guessed.
