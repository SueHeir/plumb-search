# Plumb privacy and security review

Plumb has useful local-search and network protections, but the current code does not support an unconditional claim that searches are anonymous or unrecoverable. Normal hosted search reveals the full query to its serving node. Private browser search reduces disclosure under an honest client and separated relay operators, with important fallback, inference and result-authenticity limits. Remote control and trusted-fill URL validation deserve early fixes.

This review records the baseline main commit `7f7f7049bfcf400866b451ccd558174d835a3fa3`, fetched October 4, 2026. It combines source tracing, existing unit tests, an offline adversarial probe and a RustSec dependency scan. It does not establish the configuration or health of deployed nodes and does not constitute a full penetration test or independent cryptographic audit. No production deployment was changed. See the [architecture map](architecture.md) for components, storage and diagrams.

## Fixes following this review

The accompanying changes resolve R1 in the desktop remote-control client and R5 at the seed/fill ingestion boundary. Off-host control now requires verified HTTPS; loopback HTTP supports SSH tunnels. Saved connections are validated before sending credentials. See [transport migration](../remote-control-security.md). Seed and fill transfers strip off-domain or non-HTTP(S) homepage URLs and invalid site-search templates while preserving the rest of the record.

The query-diagnostic portion of R8 is also fixed: search handlers, embedding failures and desktop page-open diagnostics omit queries, URLs and potentially query-bearing error text. History storage, cookies and retention remain unchanged. R2–R4, R6–R7 and R9 remain open. The sections below describe the reviewed baseline; their suggested fixes should be read alongside this status. The offline example now requires unsafe fill URLs to be stripped; its other demonstrations still reproduce the remaining protocol limitations.

Post-fix validation on macOS: 642 workspace tests passed with three existing ignored tests, plus 13 desktop tests. Formatting and clippy passed for the workspace and desktop with warnings denied. The private client passed WASM clippy, an optimized build and pinned wasm-bindgen processing; four node tests passed with the resulting client embedded. The updated offline probe passed. A Linux-only test helper gained a matching platform guard so macOS clippy could run. These checks did not deploy or test the live fleet.

## Threat model

The reviewed boundaries are a visitor versus the serving node; a browser versus its downloaded client code; a requester versus relays and answering peers; a node versus malicious crawl providers; a desktop versus managed remote nodes; and sensitive files versus other operating-system users. Network observers can see timing and sizes even when payloads are encrypted. Someone controlling both relay and answering peer can combine their observations. Browser extensions, compromised operating systems and site-navigation tracking remain outside these mechanisms.

## Priority findings

Priorities describe suggested engineering order, not CVSS scores. A confirmed design limitation is distinguished from a reproduced implementation gap.

| ID | Priority | Finding | Evidence status |
| --- | --- | --- | --- |
| R1 | High | Off-host remote-control bearer tokens can travel over plain HTTP | Confirmed source path and existing HTTP integration test |
| R2 | High for privacy claims | Four bucket numbers can identify a likely query through a dictionary | Offline probe: 148 brand queries narrowed to one |
| R3 | High when telemetry is enabled | STARLite reports permit offline guess matching and fabricated threshold counts | Offline probe: one-report match and ten reports from one process |
| R4 | Medium | Private browser search falls back to direct bucket retrieval after any sealed-path error | Confirmed browser source branch |
| R5 | Medium | Trusted fill keeps URLs outside a record's named domain | Parser gap reproduced; downstream URL path traced |
| R6 | Medium for hostile-peer operation | Browser sealed results ignore crawl proofs; native search permits proofless records | Confirmed source behavior |
| R7 | Medium for privacy claims | Peer signatures do not establish operator independence or trustworthiness of the served client | Confirmed selection inputs; threat-model inference |
| R8 | Medium depending on deployment | Search history and diagnostic logging retain plaintext queries | Confirmed persistence and logging paths |
| R9 | Investigate | Dependency scan includes two memory-soundness warnings | RustSec scan; exploitability in Plumb not established |

## Remote-control transport

`parse_address` adds `http://` when the person omits the scheme and accepts HTTP for any hostname or IP address. `call` sends the stored token as `Authorization: Bearer` over the chosen connection. The remote API checks the token hash and source address, but those checks cannot encrypt a connection. A network observer or active intermediary on an unencrypted LAN/public path can steal the token or modify traffic. An encrypted overlay or tunnel can change that deployment risk; a private IP address alone does not supply encryption.

The client disables redirects and system proxies, which prevents a redirect from forwarding credentials elsewhere. The API is disabled by default, refuses browser Origin headers, and normally rejects public/proxied requests. Those controls are valuable and do not fix plaintext transport. Evidence: `parse_address`, `client` and `call` in [web/nodes.rs](../../crates/plumb-node/src/web/nodes.rs), `authorize` in [web/control.rs](../../crates/plumb-node/src/web/control.rs), and token generation/comparison in [node/control.rs](../../crates/plumb-node/src/node/control.rs).

Suggested fix: require authenticated HTTPS for off-host connections, with a deliberate exception for loopback or an explicitly configured encrypted tunnel. Avoid making HTTP the implicit off-host default. Retain redirect blocking and constant-time token validation.

## Bucket inference

A name key maps to one of 16,384 public SHA-256 buckets. Every query fetches four buckets, including random padding. Hashing is not a secret: an observer with the bucket set can hash likely queries and eliminate those whose required buckets are absent. Padding helps but does not provide private information retrieval.

The offline probe generated a synthetic search for `us bank` and gave the attacker only its four observed bucket numbers. It compared those against the repository's brand-query dictionary, plus the synthetic query, without exposing the padding state. All 148 candidate queries narrowed to `us bank`. This is a demonstration on a small known dictionary, not a measured recovery rate for arbitrary searches. The attack requires observing the relevant bucket set: direct retrieval exposes it to the serving site, and common or colluding answering operators can combine their buckets. One isolated answering peer may see a smaller subset.

Evidence: `bucket_of`, `query_keys` and `pick_buckets` in [core/keys.rs](../../crates/plumb-core/src/keys.rs), [privacy_review.rs](../../crates/plumb-net/examples/privacy_review.rs), and the visibility map below.

![Search privacy boundaries](diagrams/search-privacy.png)

Suggested fix: describe the current guarantee as reducing query disclosure, and measure dictionary inference on realistic query distributions. Stronger guarantees need a different retrieval mechanism or a complete local dataset. More padding and independent relays can mitigate particular observers but do not make the public hash mapping secret.

## Popularity reporting

Popularity sharing is off by default. When enabled, the node stores eligible query/domain picks locally and emits STARLite reports. The report generator derives its randomness from the measurement and epoch. A guessed pair generates the same tag and ciphertext as an observed report, allowing offline confirmation that someone reported that pair even below the ten-report threshold.

The probe matched a single synthetic report to a guessed `us bank` / `usbank.com` pair. It also generated ten distinct reports from one process; each passed `Report::check`, and `tally` counted the pair ten times. The inbound report path checks report shape, age and uniqueness, without a proof that ten independent people or qualified identities contributed. Limits on the honest client's daily submissions do not authenticate hostile reporters. This can inflate ranking signals and defeat the intended interpretation of the threshold. The probe did not transmit reports to any live node.

Evidence: `Report::new`, `with_threshold`, `check` and `tally` in [popularity.rs](../../crates/plumb-net/src/popularity.rs); `take_report` and `take_submitted` in [net/node.rs](../../crates/plumb-net/src/node.rs); local pick collection in [node/network.rs](../../crates/plumb-node/src/node/network.rs). The repository already acknowledges guessable picks and fabricated reporting in [network.md](../network.md). The upstream [sta-rs implementation](https://github.com/brave/sta-rs) also states that its libraries have not been audited.

Suggested fix: keep sharing opt-in and explain the disclosure precisely. Evaluate an oblivious randomness service for offline-guess resistance and an anonymous issuance/spending mechanism tied to qualified contributors for report-count integrity. Those address separate problems; neither transport encryption nor a numerical threshold alone proves independent contributions.

## Direct fallback and traffic analysis

`try_show` in [browser.rs](../../crates/plumb-private/src/browser.rs) tries sealed retrieval, then calls `fetch_buckets` on any error. Target absence, a malformed key or any failed request can therefore cause direct retrieval. The user is told which path supplied the results afterward; there is no strict mode that stops before disclosing all four bucket numbers to the serving site. A malicious relay could refuse requests and induce this downgrade. The raw query remains in the browser fragment, but bucket inference still applies.

Native network search behaves differently: `ask_bucket` in [net/search.rs](../../crates/plumb-net/src/search.rs) uses a direct request when no eligible relay exists, but a failed available relay path does not then fall back to direct. Nodes also send padded background search rounds and cache answers. The browser client has no corresponding background-round scheduler, and node rounds do not cover the browser-to-site HTTP connection.

Requests are not globally mixed, and response sizes are padded into size classes rather than one fixed size. Timing, size classes and low-traffic relays still permit correlation. These are recognized limits in [RFC 9458 section 6.2.3](https://www.rfc-editor.org/rfc/rfc9458.html#section-6.2.3), not evidence of a broken HPKE primitive.

Suggested fix: provide a strict private mode that fails closed, make direct retrieval a deliberate choice, and show the current mode before searching. Document which hop background traffic covers. Test target errors, one failing answer and deliberate relay refusal in the browser path.

## Trusted-fill URL validation

`fill::parse` canonicalizes the record domain and checks size and timestamps, but preserves its URL without a same-domain check. The probe supplied a crawled `usbank.com` record whose URL was `https://attacker.example/`; `accept_filled` accepted it unchanged. The index stores the record's URL and returns it in a hit. The normal web renderer checks only that it is an HTTP(S) URL, so the traced path can produce a named result linking off-domain.

The attacker must control or compromise a trusted fill provider, or persuade the operator to trust one. This is a narrower precondition than an arbitrary network peer. It is nevertheless inconsistent with signed crawl-batch acceptance, which filters off-domain URLs even for trusted crawlers, and with native sealed result handling. Evidence: `parse` in [net/fill.rs](../../crates/plumb-net/src/fill.rs), URL storage in [index/schema.rs](../../crates/plumb-index/src/schema.rs), hit construction in [index/lib.rs](../../crates/plumb-index/src/lib.rs), `safe_href` in [web.rs](../../crates/plumb-node/src/web.rs), and URL filtering in [batch.rs](../../crates/plumb-net/src/batch.rs).

Suggested fix: enforce HTTP(S) and the same registrable domain at the shared record-ingestion boundary, and retain validation when rendering links. Add coverage for both seed and fill transfers. A full poisoned-index-to-browser click reproduction was not run; the parser acceptance was reproduced and the downstream code path was inspected. The node's crawl target uses this URL only as a same-site-checked fallback, so this review does not claim a crawler SSRF through this specific path.

## Result authenticity and trusted operators

The WASM sealed response parser explicitly consumes crawl proofs as ignored data. Encryption authenticates the answering endpoint, but does not prove the records came from a valid signed crawl. Two answering peers can mitigate some ranking manipulation, but one target is allowed, omissions remain possible, and merging copies is not independent agreement verification. Native network search validates supplied proofs but also accepts proofless or expired-proof records as unverified. Evidence: `BucketRecord` and `open` in [private/sealed.rs](../../crates/plumb-private/src/sealed.rs), `fetch_sealed` in [private/browser.rs](../../crates/plumb-private/src/browser.rs), and `check_answer` in [net/search.rs](../../crates/plumb-net/src/search.rs).

Trusted crawl providers bypass the ordinary quorum and assignment checks. Trusted fill has no per-record crawl proofs. Nodes with no own crawls also count unvouched, non-distrusted crawlers toward agreement. Local vouching raises the cost of new identities after a node has its own crawl observations, but does not prove one human per key. These are deliberate trust-model choices, not evidence that signatures are forged. See [agree.rs](../../crates/plumb-net/src/agree.rs), `DEFAULT_TRUSTED_PEERS` in [net/node.rs](../../crates/plumb-net/src/node.rs) and [node/fill.rs](../../crates/plumb-node/src/node/fill.rs).

Signed encryption keys bind a key to a peer id selected from the relay's list; they do not establish that the peer belongs to a different operator. The checked-in site configuration trusts several nodes described as belonging to the same owner. That is compatible with efficient trusted crawling, but distinct machines and keys do not establish independent privacy operators. Furthermore, the serving site supplies the WASM and boot script. A malicious site operator can change that client to transmit the query regardless of the intended protocol. These are threat-model inferences from the code and deployment configuration, not observations of misconduct or of the live fleet.

Suggested fix: verify crawl proofs in the browser, distinguish verified and proofless results, and offer a policy for rejecting unverified answers. Define operator independence separately from peer identity. An independently distributed client with pinned trust information is needed if the serving site itself is in the adversary model. Honest signatures prove provenance and integrity, not that a crawler's account of a website is correct.

## History and logging

The desktop node enables search history by default; servers default to off. With history enabled, browser preferences default to showing and ranking history. The node stores up to 200 searches and 500 opened query/domain pairs per profile in plaintext JSON with timestamps. Profiles are random 128-bit cookie ids. Cookies are HttpOnly and SameSite=Lax, but lack Secure. Files use the generic atomic writer, which supplies no explicit private file mode and relies on the filesystem and umask. There is no time-based expiry of stored profiles, and turning both preferences off stops new collection without deleting the existing file.

Normal search queries also enter browser URLs. The node logs queries at debug level and includes them in search-error messages at error level. The checked-in Caddy configuration has no access log; it is not evidence that other deployed reverse proxies or infrastructure keep none. Evidence: [history.rs](../../crates/plumb-node/src/history.rs), [web/history.rs](../../crates/plumb-node/src/web/history.rs), `run_search` and error paths in [web.rs](../../crates/plumb-node/src/web.rs), generic writer in [node/store.rs](../../crates/plumb-node/src/node/store.rs), and [Caddyfile](../../site/Caddyfile).

Suggested fix: use private directories/files for history, define age-based retention and a purge action, avoid logging query text in routine error paths, and set Secure cookies on HTTPS deployments. Local history can be a useful feature; the interface should make its retention explicit. This review did not establish whether another account can traverse a particular deployed data directory.

## Dependency scan

The saved [cargo-audit output](dependency-audit.json) used a RustSec database last updated October 3, 2026, commit `ef6173cbc5c50ec8166f9a5b28f07834144373ee`. It contained zero vulnerability-class entries and four informational warnings. Zero in that category is not a clean bill of health: two warnings concern memory soundness.

| Package | Advisory | Scope and finding |
| --- | --- | --- |
| lru 0.16.4 | [RUSTSEC-2026-0253](https://rustsec.org/advisories/RUSTSEC-2026-0253.html) | Tantivy dependency, including macOS. Potential use-after-free in pop with a panicking key destructor and caught unwinding; patched in 0.18.2. The inspected Tantivy cache uses usize keys and no pop call, so a reachable Plumb exploit was not established. |
| glib 0.18.5 | [RUSTSEC-2024-0429](https://rustsec.org/advisories/RUSTSEC-2024-0429.html) | Linux desktop GTK/WebKit dependency; absent from the macOS target tree. Unsound VariantStrIter implementations; patched in 0.20.0. Call-path reachability was not established. |
| paste 1.0.15 | [RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436.html) | Unmaintained dependency warning. |
| proc-macro-error 1.0.4 | [RUSTSEC-2024-0370](https://rustsec.org/advisories/RUSTSEC-2024-0370.html) | Unmaintained dependency warning. |

Suggested fix: update through compatible upstream dependency releases, track advisory suppression decisions explicitly, and repeat scans for each shipping target. No dependency versions or lockfile entries were changed in this review.

## Protections verified in source and tests

- The desktop binds its HTTP server to loopback, checks panel navigation destinations and exposes no privileged IPC capabilities to the remotely served panel.
- Local management requires a loopback socket peer and local Host, with Origin and fetch-metadata checks against cross-site requests and DNS rebinding.
- Remote control is off by default, uses 256 random token bits, stores a hash server-side and compares it in constant time. Stored remote tokens, node keys and backups use explicit Unix private permissions.
- Search HTML escapes values, restricts link schemes, sends a CSP, and uses no-referrer. The private client's fetches omit cookies and keep the query in a fragment.
- The crawler uses a filtering DNS resolver tied to the actual connection, refuses offsite redirects, disables system proxies by default, obeys robots, and bounds body/robots processing. Explicit IP targets and proxy-enabled operation need separate assessment; the DNS filter alone does not cover them.
- Peer protocols impose message-size limits, signatures and crawl-field checks. Bucket serving, relaying, fill and node network searches have concurrency limits. These controls do not establish resilience under a coordinated load attack.

## Validation and reproduction

451 existing unit tests passed: core 39, crawler 81, network 95, node 212, private client 11 and desktop 13. The native private-client tests do not execute the browser-only WASM fetch and fallback flow. Public integration tests against live infrastructure, Linux desktop behavior and load/fuzz testing were not run.

Commands used:

```sh
cargo test --locked -p plumb-core -p plumb-crawl -p plumb-net -p plumb-private -p plumb-node --lib
cargo test --locked -p plumb-desktop --bin plumb-desktop
cargo run --locked -p plumb-net --example privacy_review
cargo audit --json
```

The baseline offline probe output was:

```text
Observed bucket numbers: {2985, 5691, 15129, 15898}
Dictionary candidates: 148 -> 1
Remaining candidates: ["us bank"]
One observed popularity report matches the guessed query/domain tag and ciphertext.
One process generated 10 accepted reports: [Popular { query: "us bank", domain: "usbank.com", count: 10 }]
Trusted fill kept an off-domain URL: usbank.com -> https://attacker.example/
```

The example uses synthetic data and makes no network requests. Following the URL fix, its final output is `Trusted fill stripped the off-domain URL for usbank.com.` and its assertion requires removal. The bucket and popularity demonstrations remain unchanged.

## Recommended next work

Next add strict private-mode behavior and test browser failure paths. Keep popularity sharing disabled while its disclosure and contributor-count model are redesigned. Implement browser proof checking, tighten history retention, and document operator assumptions and dependency decisions.

The fleet handoff should resolve deployed commits and flags, actual HTTPS/tunnel paths, operator overlap between relays and targets, trust lists, history/telemetry settings, filesystem permissions and reverse-proxy logging. That evidence can refine deployment risk without changing the source findings above.
