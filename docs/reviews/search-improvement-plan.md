# Plumb Search improvement implementation plan

Improve relevance and page retrieval first, repair the production data pipeline next, then expand coverage. Ship each change against a fixed corpus and a separate acceptance suite so data refreshes cannot be mistaken for ranking improvements.

The [generality audit](search-generality-audit.md) records the final evidence rules and supersedes example-shaped proposals below. In particular, mandatory named-paper anchors and literal host, DOI, category or task exceptions were removed from the implementation. Named examples remain regression diagnostics; serving and publication gates require their own indexed or source evidence.

This plan covers the October 9, 2026 search audit and the user's DeepSeek HPC evaluation report from October 10 UTC. The report strengthens the priority of short docs retrieval, whole-query relevance, and reliable identity tools; two additional changes below cover official-site/lookalike behavior and source quality. The local source baseline is `origin/main` at `786eb55a736a91f8dd36fa42ca0be19680dd15f5`. Deployment inspection confirms that **the HPC node runs 0.2.1**. The public website runs a separate 0.2.0 node. The earlier public status snapshot cannot identify the connector's deployment or corpus.

## Verified deployments, branches, and data

| Deployment | Verified evidence | Implication for this plan |
| --- | --- | --- |
| `elizabeth-hpc` node | SSH succeeds. `plumb.service` runs `/home/suehr/projects/plumb-search/target/release/plumb`; both the running executable and local MCP initialization report 0.2.1. Checkout is detached at `068faba33e7caee2d56b003167e60e05abb0e6dc`, matching `origin/claude/project-thread-gh70x1`. Tracked source is clean. | Use this node's corpus and settings for connector regressions. The checkout is a memory-fix branch, not the current main checkout. |
| HPC running artifact | Running `/proc/521774/exe` and the checkout's release binary have identical SHA-256 `4b128f167109f315d3993aaba64e2f66e3ae550b3d5153d53049aa77e1a19c3f`; binary timestamp is 03:55 UTC, service start 03:56 UTC. | Artifact identity is verified. The binary has no embedded Git revision, so the checkout/build association is strong evidence, not an independently embedded commit attestation. |
| Public website and root `/mcp` | `/api/status` and MCP initialization report 0.2.0. The running Docker image's revision label is `004f512570f49f57398c30b6083cbd7ace23b9b1`; that commit declares 0.2.0. | Website-versus-connector comparisons combine transport, code, settings, and data differences. They are not controlled surface-parity tests. |
| Dedicated HPC MCP route | Droplet Caddy's local `hpc.caddy` rewrites a dedicated MCP route to port 18080; `plumb-tunnel.service` forwards it to HPC port 8080. Initializing that route reports 0.2.1. Connector paper/facts probes exactly match the local HPC MCP responses. | This supports the HPC deployment as the connector audit target. Connector registration metadata was not exposed by the tools; record the configured target explicitly in future run manifests. Keep the private route identifier out of public reports. |

Branch ancestry was checked after fetching origin:

| Existing work | Branch / tip | In main? | In HPC checkout? | Decision |
| --- | --- | --- | --- | --- |
| 0.2.1 release bump | `claude/project-thread-b1j25o` / `40d3053` | Yes | Yes | No new version-bump fix is needed. |
| Docs section-heading retrieval | `claude/project-thread-ban6dy` / `c8c6c62` | Yes | Yes | Extend the existing implementation and deployed heading layer. |
| Paper full-title and byline retrieval | `claude/project-thread-mh36mw` / `acbb32b` | Yes | Yes | Separate bad/missing paper records from retrieval failures. |
| Disable findings for MCP evaluations | `claude/project-thread-mymg81` / `f697732` | Yes | No | HPC currently ignores `?findings=off`; a core evaluator or isolated fixture must disable findings independently until this change is deployed. |
| Reference-page relevance gate and set rename | `claude/reference-gate-ban6dy` / `41252bf` | No | No | Evaluate this existing patch before writing overlapping page-gating code. Its `reference2` migration requires a staged dataset and mixed-version checks. |
| Linux allocator/memory fixes | `claude/project-thread-gh70x1` / `068faba` | No | Yes | Preserve the four memory-fix commits when establishing an implementation/deployment base. Do not compare a memory-fixed production process with an unfixed candidate without accounting for this difference. |

The branches share base `f0e5ff1`; current main has six subsequent commits, and HPC has four memory commits. Comparing the two tips shows no changes in the site/page ranking, docs ingestion, paper repair, or facts assembly code examined for this plan. Their material node differences are Linux memory handling and the HTTP MCP findings override. Reconcile these branches and rerun required checks before deploying an implementation built from main.

Read-only HPC corpus inspection and local API/MCP canaries establish:

- Page-index metadata reports **13,727,386 indexed pages**; the separate places index reports **24,014,360 places** and **2,411,210 towns**. These are recorded index totals, not the approximate counts in `SetInfo`.
- `docs.tsv.gz` contains **67,462 records**, with section metadata on **50,047**. A heading-free shared dataset is not the general explanation for the audited docs misses on this node. No retained docs row/extension contains `set_multiplayer_authority`; the method still needs better symbol/page coverage.
- `papers.tsv.gz` contains **199,982 records**. Its Transformer row is still `Paper by Ashish Vaswani et al., 2025`, DOI `10.65215/2q58a426`. RAG's canonical identifier `10.48550/arxiv.2005.11401` is present with Patrick Lewis and 2020, but under the unrelated title “Affordance-Compiled Intelligence: Observable-Only Cognitive Impedance Matching for No-Meta LLM-Integrated Systems”; the original RAG title is absent. The primary arXiv records confirm [Attention Is All You Need, first submitted in 2017](https://arxiv.org/abs/1706.03762) and [Retrieval-Augmented Generation for Knowledge-Intensive NLP Tasks, first submitted in 2020](https://arxiv.org/abs/2005.11401). These are confirmed corpus identity/metadata problems despite canonical repair and full-title retrieval code already existing.
- `reference.tsv.gz` contains **423,029 records** and `subpages2.tsv.gz` **155,563**. Neither has section metadata. Both files exist on HPC; these counts describe files, not independently measured per-set live index counts.
- Japan's Wikipedia record is `Q17` and already contains `f-population=123802000;2024`, capital, currency, and other facts. Typed article search retrieves Japan, while MCP `facts(Japan, population)` returns `found:false`. Ordinary full search displays the WHO subpage titled Japan instead of the Wikipedia entity. This establishes a retrieval/selection-path failure; trace the exact loss before changing ingestion. Treat Japan as the first entity-lookup regression, not a missing-data enrichment case.

Corpus counts were obtained from compact metadata and four small set-file scans; file modification times are not treated as verified source observation dates. The running node and the reference fetch process were left untouched.

## DeepSeek evaluation evidence and revised priorities

The supplied report describes 57 chats, 2,918 calls, and 2,435 self-graded calls, with 17% graded yes, 30% partly, and 51% no. Its 89% no rate for typed docs and 41% deep-page share among misses/partial results are useful triage signals. They are not independent-query accuracy estimates: difficult chats produce more retries, the same model writes many prompts and grades its own calls, and ungraded calls are outside the percentages. The report's raw transcripts and tally files were not available in the supplied attachment. Validate labels from full responses before using these proportions as release metrics.

The run also spans a service restart at 03:56 UTC, records 345 findings, and posts page-specific findings into HPC. The summary does not give the count posted. Saved findings can change later responses within the evaluation; record build, corpus, findings state, and time per call. Score end-task success and query-family success separately from individual tool-call success. Use the reported median 0.58 seconds and p95 1.0 second as descriptive figures for this run, not controlled performance baselines. Only 15 calls errored, so quality takes priority over availability or speed work.

Follow-up read-only probes reproduce several examples and distinguish their causes:

| Case | Current HPC result and corpus evidence | Implementation decision |
| --- | --- | --- |
| Typed docs `tomllib` | Empty; `python tomllib` and `tomllib site:docs.python.org` return the existing Python page. Product aliases are already in the file. | Fix short typed-docs retrieval, not duplicate product-name enrichment. |
| Typed docs `padStart`, `javascript padStart` | Both empty despite existing MDN and Kotlin pages, including MDN section metadata. | Add exact symbol/short-name retrieval and trace eligibility/candidate loss before scheduling a recrawl. |
| Typed docs `react hooks`, `python` | Empty; the file has React Hooks pages, 127 React records and 505 Python records. | Separate short topic retrieval from product-only docs browsing. A bare product should lead to its useful overview/reference roots. |
| Typed docs `pytest` | Empty; `docs.pytest.org` has no records in the file and no current docs source profile. | This is a source-coverage gap requiring ingestion as well as retrieval work. |
| Plain `tomllib`, `Amazon River length` | Unrelated domains lead: tomayko.com/tomanbesar.com and amazon.com/amazon.jobs. | Extend change 5's regression set; a partial domain/brand match cannot establish the whole query's subject. |
| `official_site("mypy documentation")` | Returns mapy.com with medium confidence despite the package card supplying mypy.readthedocs.io and mypy-lang.org appearing as an alternative. | Bind confidence and package/docs evidence to the requested entity; spelling and popularity must not validate a different entity. |
| `official_site("Xe currency converter")`, `"Chroma vector database"` | Returns converter.app and vector.dev, while xe.com and trychroma.com are alternatives. | Task/category words must not displace the named entity in the identity tool. |
| `official_site("SAT México")` | Defaults return Guatemala; explicit `country=MX` returns sat.gob.mx at low confidence. | A jurisdiction stated in the query is stronger evidence than a default country preference. Preserve explicit user intent across locale handling. |
| `check_lookalike(console.hetzner.cloud)` | Reports lookalike. The address currently redirects to console.hetzner.com, which [Hetzner's own Cloud page](https://www.hetzner.com/cloud/) links as its console. | Add verified alternate-domain/migration evidence; resemblance or a redirect alone cannot establish affiliation. |
| `check_lookalike(api.semanticscholar.org)` | Reports official in the follow-up. | Keep this as a regression control; do not claim the historical miss is currently reproduced or fixed without its exact run state. |
| Reported health/finance spam | Follow-up `lisinopril side effects` and `IRS contribution limits` do not reproduce the supplied spam examples. Their responses still lean on generic homepages and tangential reference pages. | Recover exact original queries/settings for spam fixtures; extend existing quality controls rather than assuming no controls exist. |
| `Poilâne Paris` | US towns lead and no places collection appears. | Domain-word relevance and MCP's missing places path are separate from geographic disambiguation and business coverage. |

The source already adds product aliases in `plumb-ingest/src/docs.rs` and has `PageSearcher::search_naming_docs`. However, docs topic matching uses the generic three-stem minimum in `question_match`, requires `docs_asked`, and typed search filters sources after retrieval. These are concrete boundaries to trace; the complete contribution to each miss requires the stage diagnostics from change 1. “Only homepages” describes the observed usefulness of many responses, not the absence of all deep-page content: HPC already has over 13 million indexed pages.

After a compact baseline, prioritize the user-visible work as follows. The numbered changes below describe complete deliverables; their first bounded slices can ship without waiting for a large shared-assembly or rich-content rewrite.

1. **Retrieve indexed docs for short queries and product names** — change 6's typed-docs slice. Fix tomllib, padStart and React Hooks, then add pytest and other verified missing docs sources.
2. **Stop domain-word and partial-brand promotion** — changes 4 and 5. Include asyncio, tomllib, K2, Amazon River, Logan Square, budget, deadline and shutdown with real-brand negative controls.
3. **Correct identity-tool decisions and confidence** — change 14. Wrong “official” answers and affiliation accusations need their own precision gates.
4. **Demote verified spam while keeping relevant trusted pages** — change 15. Reuse current health and pill-shop controls and collect exact failing cases; avoid broad TLD or popularity bans.
5. **Expand answer-bearing government, health and travel pages** — initial change 11 batches, after safe subset merging from change 3 and retrieval validation. Use wanted domains as demand evidence, not a guarantee that every page can be fetched.
6. **Ship bounded existing-content fixes alongside that work** — direct entity lookup from change 10, MCP places from change 2, the existing reference gate experiment, and staged paper correction from change 8. They do not require a global facts rebuild, worldwide places overhaul, or new scholarly search architecture.
7. **Expand richer docs, recent research, languages and news** — the measured remaining parts of changes 7, 9, 12 and 13, plus broader places/business and shopping coverage.

## Findings confirmed in the source

| Finding | Source evidence | Consequence |
| --- | --- | --- |
| Relevance checks have exceptions for partial names and nearest semantic candidates | `plumb-index/src/lib.rs`, `rank_traced`: popularity gating uses `name.words() == 0`; partial filtering requires a meaning backend and exempts some named and nearest candidates | A partial brand match can retain its popularity advantage. The exact contribution to each live miss still needs a trace. |
| MCP omits places | `plumb-node/src/mcp.rs`, `search_with`, never calls `SearchBackend::places`; `web.rs`, `api_search`, does | Existing local content is inaccessible through that search tool. |
| Search assembly differs by surface | Website/API call `extras`, `route_sources`, and place handling; MCP has separate answer, package, and result-cap logic | A shared backend does not guarantee the same output. |
| Normal page candidates are narrow | `node/pages.rs`: `PAGES_PER_SEARCH = 10`; `pages.rs`: `MAX_PAGES_LISTED = 2`; learned ranking reorders ten rows | Better retrieval or a ranker cannot recover rows discarded earlier. |
| Typed page search filters late | `node/pages.rs`, `pages_of`, retrieves up to `KIND_PAGES = 200` before filtering kind, operators, and language | Other sources can exhaust the candidate window before requested-kind results are considered. |
| Docs already index headings | `FetchedDoc.sections` becomes `Article.sections`, then `Page::topic`; headings are capped at 64 and 1,000 total characters. HPC has 50,047 heading-bearing docs records. | Extend this implementation and measure uncovered pages/symbols rather than adding a duplicate heading system. |
| Docs extraction misses later prose and API sections | `extract.rs` retains the first 100 body words; ingestion reduces descriptions to 160 characters; section headings have an eight-word extraction cap | Important error names and methods can be absent from searchable text. |
| Cached inner-page fetches never expire | `fetch.rs`, `run_docs` and `fetch_sites`, reuse `read_kept_docs` whenever it parses | Reusing a work directory can preserve stale content or older extraction indefinitely. |
| Selected-site fetches rebuild only the selected input | `run_docs`, `run_reference`, and `run_subpages` build output from the sites selected for that invocation | Targeted refreshes need merge semantics before they are used on the production set. |
| Canonical paper repair already exists but skips present IDs | `paper_names.rs`, `improve` and `add_arxiv_papers`; `run_papers` invokes it. `missing_arxiv_ids` and `add_arxiv_papers` skip identities already in `by_arxiv`. | Rerunning alone cannot repair the RAG record's wrong title. Add existing-ID consistency checks; enrichment failures currently permit writing an unenriched set. |
| Paper coverage favors established works | `openalex.rs`, `fetch_papers`, sorts by citations and filters using a minimum; paper topic retrieval indexes only titles | Fresh work and topical research need a separate ingestion and retrieval path. |
| Facts already carry some dates | Population stores a count year; item facts handle ended statements and latest statements | Preserve existing semantics. Add completeness reporting and retrieval isolation before redesigning facts. |
| Facts lookup depends on displayed search pages | MCP `facts` calls `lookup(subject, 5)` and `answers::fact_pages` consumes placed pages. Japan's Q17 facts exist in the HPC corpus but the live facts tool returns false. | Fix entity lookup independently of display selection; the current response conflates selection loss with missing enrichment. |
| New content types are indistinct to the learned model | `learned.rs` has eight explicit set features, excluding docs, reference, and subpages | Feature expansion requires a new compatible model, not a JSON edit to the old one. |
| News source fallback can change the requested source | `news.rs`, `recent`, falls through to topical matching when the named source has no feed results | A query asking for BBC can return another publisher that mentions BBC. |
| Public recent endpoint is blocked by deployment routing | `/api/recent` is registered in the node but absent from `site/Caddyfile`'s public allowlist | The observed public 404 is explainable without assuming the news store is empty. |
| Page language is mostly inferred from set | `Page::language` returns a language for Wikipedia, GitHub/questions, and some films, but none for docs/reference | Language filters cannot reliably constrain those pages. |
| Docs product names are already stored but short queries are gated | `docs_articles` writes product-prefixed aliases; `question_match` uses a three-stem minimum for docs; `search_naming_docs` has product/site hints | Short-query retrieval needs a docs-specific path; adding the same product words again is insufficient. |
| Official-site evidence can refer to the wrong entity | `mcp.rs`, `official_site`, can retain an unrelated well-known pick despite package docs evidence, and promote a domain matching a task word | “Official” for Mapy or Vector does not mean official for mypy or Chroma. Confidence must be bound to identity. |
| Lookalike detection lacks verified cross-domain affiliation | `check_lookalike` searches a registrable domain and names, then compares it to better-known domains without an affiliation graph | Legitimate alternate domains need evidence; an unknown same-brand domain must not be accepted by resemblance alone. |
| Quality controls already exist | `health.rs` moves health authorities; `is_pill_shop` demotes some low-link-score drug domains | Extend and measure these controls; do not add a second disconnected health whitelist. |

Source paths above are relative to `crates/`. Existing `plumb eval --recall` already distinguishes missing index content, retrieval losses, and placement losses. Extend it instead of building a competing evaluator.

## Delivery order

The priority of a user-facing fix differs from implementation order: reproducibility and safe data refreshes must precede comparisons and corpus rebuilds.

| Change | Deliverable | Depends on | Audit fixes addressed |
| --- | --- | --- | --- |
| 1 | Reproducible baseline and acceptance harness | None | Evaluation and inventory foundations |
| 2 | MCP places and common result assembly | 1 | Surface consistency |
| 3 | Fresh caches and safe corpus publication | 1 | Freshness, inventory, production repair |
| 4 | Shared query intent and package-card gating | 1, 2 | Intent routing |
| 5 | Whole-query relevance evidence | 1, 4 | Wrong-brand results and filler |
| 6 | Candidate selection and page blending | 1, 4, 5 | Recall and ranking limits |
| 7 | Symbols and passages in software docs | 3, 4, 6 | Information inside pages |
| 8 | Canonical paper repair and quality checks | 1, 3 | Scholarly identity |
| 9 | Recent papers and date constraints | 4, 6, 8 | Recent research |
| 10 | Entity lookup and reliable facts enrichment | 1, 3, 4 | Facts availability and dates |
| 11 | Government, company, and reference expansion | 3, 6, 7 | Specific destination pages |
| 12 | Source-aware news and public endpoint | 1, 2, 4 | News quality |
| 13 | Language-aware pages and coverage responses | 3, 6, 7, 10 | Non-English coverage and honest gaps |
| 14 | Entity-bound official-site and affiliation decisions | 1; reuse 4 where available | Wrong identity-tool answers and confidence |
| 15 | Relevance-aware source quality and spam demotion | 1, 5; reuse 4 where available | Health, finance, and shopping spam |

Inventory from changes 1 and 3 ships early. Coverage explanations in change 13 reuse it. These are separate reviewable changes, not a commitment to perform a single large rewrite.

Dependencies above apply to the full deliverables. Short typed-docs retrieval in change 6, direct entity lookup in change 10, and exposing existing MCP places in change 2 require only a pinned baseline and their focused contract tests. Initial targeted HTML coverage in change 11 uses the existing compact format after safe merging; rich passage extraction follows change 7. Do not turn those small fixes into dependents of the entire architectural plan.

## Change 1 Establish a reproducible baseline

Extend `plumb-node/src/eval.rs`, `eval_labels.rs`, `cli.rs`, and `eval/README.md`. Reuse current TSV suites and `--recall`. Add a richer JSONL acceptance format for query options, relevant URLs, explicitly irrelevant domains, expected result type, dates, and cases where abstaining is correct. Keep current TSV behavior compatible.

Create two sets: the known audit regressions for development, and an independently written acceptance set with at least 40 query families and 200 total queries across navigation, ambiguous words, troubleshooting, practical questions, facts, research, dates, languages, and places. Assign paraphrases of the same family to the same split. Existing query-hash splits remain valid for old suites; a new family split prevents nearby paraphrases from leaking across the new suites. Do not train on the acceptance set.

Use DeepSeek transcripts as candidate regression evidence. Keep model grades, manually adjudicated relevance labels, and root-cause classifications in separate fields. Recover exact arguments and full results, including findings provenance, rather than grading only the first 1,500 characters saved in `calls.jsonl`. Deduplicate repeated retries by query family for category reporting. Split the two prompt rounds by family and audit overlap before calling round 2 a held-out set. Reproduce reported official-site/lookalike failures using their own tool contracts, not ordinary-search rank alone.

Record commit/build identifier, rank settings, model identifier, query-instruction mode, enabled sources, corpus checksums, actual indexed counts, target node/transport, and fixed evaluation time with each run. Add an embedded build revision so a clean source checkout and artifact timestamps are no longer needed to infer a running binary's source. Disable findings, personalization, plugins, and external results for the core baseline. Evaluate those features separately. The earlier Serde probe included a cached finding, which must not contaminate a retrieval comparison. The existing findings override is in main but absent from HPC; do not assume passing its URL parameter currently isolates that deployment.

Add machine-readable stages: record present, exact identity lookup, lexical candidate, semantic candidate, source-filtered candidate, page selection, blended row, learned order, and serialized output. Aggregate reports need recall at 10/50/100, top-one, top-three, MRR, NDCG at ten for graded labels, wrong-domain rate, latency, response size, and storage/RSS measurements. Diagnostics run explicitly through the evaluator or local diagnostic mode; ordinary user queries need no new retained logs.

Build surface tests using one in-process fixture backend and identical options. Compare normalized identities, order, types, answers, and provenance across HTML, full JSON, SearXNG, and MCP. Do not demand equality between two different live nodes or intentional browser personalization settings.

**Acceptance:** repeated runs against the same snapshot have identical result identities and scores; a known missing record is classified differently from a retrieved-but-discarded one; an irrelevant high-ranking domain can fail a query even when the expected URL also appears.

## Change 2 Expose places and share result assembly

Modify `mcp.rs`, `web.rs`, `web/places.rs`, and `web/searxng.rs`. Extract the small pure pieces of place suppression, local website linking, utility-source routing, and result ordering into a node search-assembly module. Keep network calls and browser personalization in their existing layers. Avoid a full transport rewrite.

Add a typed assembled-result structure containing ordered rows and optional answers, places, headlines, profiles, and source diagnostics. Existing public fields remain available; adapters render the same underlying objects. Apply the limit once after assembly, with an explicit separate bounded places collection. Do not overload site/page positions to encode place identities.

MCP `search` calls the places backend for an explicit location, emits center, name, address, distance, website when available, OSM identity, and attribution in JSON and concise text. Reuse website behavior that suppresses places for a named non-place entity. For `near me`, return a missing-location indication unless the caller supplied an explicit location; country is not a city. Add an optional location argument only after agreeing its schema across the node tool definition and connector registration.

Keep `kind=site` as sites only and typed page searches as the requested page type. Explicit operators and safe/language constraints apply before and after assembly. `found_before` remains a separate provenance-labelled collection.

**Acceptance:** the same fixture's Denver restaurants appear in MCP and full JSON with matching identities and distances; an empty place search does not claim the category is globally absent; navigation and operator behavior remain intact. Verify connector tool registration as well as node output after deployment.

## Change 3 Make refreshes and publication reliable

Replace bare cached vectors in `fetch.rs` with a versioned envelope: source target/configuration fingerprint, extractor version, fetch time, per-URL outcomes, and completion state. Legacy caches remain readable as stale caches. Reuse only fresh, compatible, completed data. Add explicit cache age and force-refresh CLI options. Initial policies are configurable, with docs/reference refreshed weekly and daily freshness available for fast-changing sections; source timestamps are kept separately from fetch time.

Changing roots, page cap, title cleaning, symbol extraction, or passage settings invalidates the relevant cache. A subset refresh merges updated hosts into the existing set and preserves unselected hosts. A failed or empty host refresh keeps that host's last good data and records the failure. Provide an explicit full replacement mode for intentionally rebuilding a set from scratch.

Write an immutable staged generation plus a manifest identifying the actual source revisions, counts per host/set, successes, failures, caps, enrichment stages, and checksums. Validate the generated set and canary queries before promoting it. Preserve the previous generation and reuse existing staging/index swap mechanisms. Publish the manifest/generation pointer last; readers continue using the old complete generation until its replacement is indexed.

Keep `SetFileNotes.complete` as transfer completeness, not ingestion quality. A fully transferred dataset can still contain a failed enrichment stage. Add independent quality fields. Extend `node/newer.rs` and `plumb-net/src/pages.rs` with optional manifest/capability metadata while preserving legacy peers. Mark rich docs and facts layers explicitly; `layers_of` currently misclassifies unknown profile entries as profiles, and only two sets get layer protection.

Expose a bounded public coverage summary through existing `/api/status`: indexed counts, generation identifiers, source/fetch age, enabled/disabled status, complete/partial enrichment, and truncation. Keep detailed host errors and local cache paths in the management interface. Compute summaries on publication rather than scanning the corpus per search.

**Acceptance:** an unchanged fresh cache skips work; an expired or older extractor cache refetches; a selected Python refresh preserves other docs hosts; failed enrichment is visible and cannot silently replace a required good layer; interruption during build/promotion leaves a usable previous generation. Rich-set growth respects the existing 125% guard and storage settings.

## Change 4 Introduce shared query intent

Add a small deterministic query-plan type in `plumb-core`, used by site/page retrieval and node assembly. Preserve original text, operators, language, and country. Store independent signals for explicit URL/navigation, package lookup/install, docs/troubleshooting, practical task, fact, research, source-specific news, location, and temporal constraints. Source/operator constraints take precedence; ambiguous queries can retain more than one retrieval source.

Reuse `facts::fact_asked`, package/install parsing, `sources::route`, docs product matching, and news/location parsers. Centralize decisions rather than adding another list of keywords to every caller. Bare words such as budget, change, and deadline are not evidence of brand navigation. A recognized entity plus login/docs can be; a product plus an error or method remains an informational query.

Gate `Mcp::guess_package`: explicit package/version/install requests can promote cards; troubleshooting and method queries can show a related card as supplementary information after the answer-bearing rows. A package's popularity alone cannot select the query's primary result type. Preserve scoped names and install commands.

**Acceptance:** Serde installation and scoped npm lookup retain correct cards; Kubernetes CrashLoopBackOff does not lead with a Go install card; Python package index still finds PyPI; Chase login still finds the official site; numeric versions are not mistaken for publication years.

## Change 5 Rank by whole-query evidence

Change `plumb-index/src/lib.rs`, `schema.rs`, and learned row signals. Compute weighted query coverage for all ranked candidates, including candidates with embeddings. Give discriminative subject/task terms more weight than generic modifiers. Keep original lexical score and semantic closeness as separate features; current scores are relative to the strongest BM25 hit and do not by themselves establish complete relevance.

Separate exact full-name evidence, partial-name evidence, explicit typed-domain evidence, and ordinary words. Full-name/typed navigation retains existing anti-lookalike guarantees. Partial-name candidates receive popularity/name boosts only in proportion to evidence for the rest of the query. Remove unconditional relevance exemptions for being in the nearest semantic group; being nearest does not establish relevance.

Use a conservative relevance tier before popularity for informational queries. Require convincing lexical coverage or corroborating semantic evidence. Missing vectors are unknown evidence, not zero relevance. Use lexical retrieval when the meaning model is unavailable. Calibrate any hard removal threshold on the tune set; prefer demotion and a low-confidence fallback until the false-negative rate is measured. Do not globally require every word, which would damage paraphrases and rare vocabulary.

Preserve evidence fields through learned reordering. `reorder` currently trades site scores to maintain downstream order, so diagnostic relevance must not reuse that display/order score.

**Acceptance:** Budget car rental, Deadline Hollywood, and Change.org do not appear in the top three of their audited informational queries; matching brand queries remain correct; synonyms, unknown-vector sites, spelling, country preferences, and official-site protection pass dedicated controls with meaning both on and off.

## Change 6 Retain useful candidates and blend pages by intent

**First slice: short typed-docs retrieval.** Preserve existing product aliases and `search_naming_docs`; introduce a docs-specific match policy for an explicit docs request or constrained docs host. Retrieve eligible docs within the requested kind/host before popularity caps. Match exact qualified symbols and verified short identifiers even when a page's title has a subtitle. Allow bounded one/two-term docs topic queries without lowering the global Stack Overflow question threshold. Product-only queries use source-profile overview/reference roots and explicit product fields rather than requiring a page title equal to the product. Measure ambiguity when a short API name belongs to multiple ecosystems; keep source labels and honor an explicit product/host constraint. Carry the same narrow route into plain search when docs intent or strong indexed symbol evidence exists, while preserving ordinary ambiguous-word navigation.

Pin current-file fixtures for `tomllib`, `python tomllib`, `tomllib site:docs.python.org`, `padStart`, `javascript padStart`, `react hooks`, and product-only `python`. Trace raw candidate eligibility, name/topic scoring, source filtering, truncation, and final serialization. A fixture that shows an existing record discarded must fail before any new crawl is credited with fixing it. Separately add the missing pytest source after merge-safe ingestion is available. This slice can ship before richer passages, a new learned model, or full assembly changes.

First evaluate `origin/claude/reference-gate-ban6dy` at `41252bf` against the fixed audit and held-out practical-query suites. It extends `subpage_asked` to reference pages, stems title matching, improves contiguous-query checks, and renames the set to `reference2` so older readers do not serve it with weaker relevance rules. Reuse it if it improves irrelevant-page rejection without unacceptable loss of task-answering pages; revise its thresholds only with measured evidence. This gate does not fix partial-brand site ranking, source-filter candidate loss, or facts entity lookup.

If adopting the branch, publish a staged `reference2.tsv.gz` with completion/quality metadata and validate it on the new reader before promotion. The branch preserves old setting names but does not automatically migrate the existing `reference.tsv.gz` file. Keep the old generation for rollback and test new-name peer advertisement, old/new settings, reader behavior, and that reference coverage does not disappear on upgraded nodes. Coordinate publication and code deployment separately from the active reference builder.

Add source/kind/host/language filtering within page candidate retrieval before its shared cap. Keep exact identity/name candidates separate from broad topic candidates. Preserve docs, reference, and subpage candidates when other popular sets compete; the current preservation helper excludes reference. Keep per-source diagnostic counts.

Introduce bounded candidate settings rather than changing every cap independently. Starting experiments use up to 50 eligible site rows and 50 eligible page rows for informational assembly, retaining only relevant rows; measure 10/20/30 rows for learned reranking. These are experimental budgets, not final defaults. Keep fast navigation limits and exact-name preservation.

Replace the unconditional two-page display rule with intent-aware selection under the existing overall result limit. An informational query can have several answer-bearing pages; a navigational query can keep one supporting page. Relax `docs_kept_below` only when a docs candidate strongly matches the requested task and the first site is weak or belongs to another subject. Preserve the PyPI negative control.

Add explicit docs/reference/subpages features, query-intent features, coverage, and missing-evidence indicators to `learned.rs`. Retrain `learned_model.json` from new features, regenerating examples with the current query instruction. Existing model loading already rejects feature mismatch; ship schema and compatible model together. Keep the old ranker behind the experiment configuration for rollback.

**Acceptance:** relevant pages survive each retrieval/filter/blend stage; increasing the output limit does not unexpectedly change the earlier ranking; ranking gains are evaluated on a fixed corpus. Choose the smallest budget with useful recall gain that passes the performance gate.

## Change 7 Extend docs with API symbols and passages

Extend the existing streaming extractor in `plumb-crawl/src/extract.rs`; retain its request/body/time bounds. Add a separate opt-in inner-page extraction configuration. Homepage records remain compact. Collect API identifiers and anchors plus bounded text windows around substantive headings, definitions, and code blocks. Preserve complete identifiers such as `set_multiplayer_authority`, `Array.prototype.sort`, and `std::vector`, alongside split tokens; generic underscore splitting alone is insufficient.

Use at most 8 KiB of rich search content per docs page initially, including up to eight passages of 512 characters and an explicit symbol/anchor budget. These are initial bounded design values to validate on Godot, Kubernetes, Python, MDN, and Rust. Favor exact symbols and diverse sections instead of taking the first headings until the character budget expires. Do not scan unbounded content or raise homepage extraction limits globally.

Carry rich content as a versioned optional `search` extension in the existing article metadata line, encoded safely for its tab/pipe separators. Confirm legacy readers ignore the extension and current cutters retain it with the parent article. Add searchable fields for symbols, headings, and passages to the page index; return the matching passage/anchor with provenance, not a generated answer. Keep positional offsets separate: existing normalized token offsets cannot safely highlight original HTML.

Make cache identity and peer layer protection from change 3 cover this extension. Build the new page schema in a new generation. Older nodes can consume the compact fields. Prevent cached heading-free data from being presented as a successful rich rebuild.

Deduplicate by canonical URL/content evidence instead of dropping all repeated cleaned titles. Two legitimate API pages titled Introduction may have different subjects or versions. Keep versions identifiable and collapse genuinely duplicate mirrors.

**Acceptance:** Godot's authority method resolves to the owning Node documentation and anchor; CrashLoopBackOff retrieves its explanatory Kubernetes passage; section matches and exact API symbols work without embeddings; menus, hidden text, huge HTML, and malformed markup retain the extractor's resource protections. Measure actual storage and response-size costs before broad rollout.

## Change 8 Verify and harden canonical scholarly records

The HPC file confirms the audited bad Transformer record and RAG metadata attached to the correct arXiv identifier but an unrelated title. First exercise existing `plumb-ingest/src/paper_names.rs`, `paper_names::improve`, against pinned local fixtures for the 2017 Transformer paper, the original RAG paper, a same-title unrelated work, and an existing canonical ID with a wrong title. `missing_arxiv_ids` and `add_arxiv_papers` both skip IDs already present; this explains why a rerun cannot by itself validate or fix RAG's existing-ID/title mismatch.

Add a bounded authoritative-metadata verification path for already-present landmark IDs, initially a fixed small acceptance list, with a separate resumable background consistency queue. Compare title, authors, and dates against the canonical source even when identity is already in `by_arxiv`. Correct verified conflicts by stable identifier, preserve conflicting upstream metadata in provenance, and keep an unrelated incorrect title out of searchable aliases. Record unresolved conflicts rather than quietly blessing them because their IDs exist. Broad verification remains an explicitly budgeted ingestion operation, not a live-query request.

Build a staged real paper generation with that narrow correction and the existing missing/misdated-paper repair, examining both completion reports. Record the Transformer result as a data repair if the existing logic succeeds; reuse it instead of adding a duplicate patch. Start this staged repair early using an isolated fresh work directory; it need not wait for the full ingestion-manifest implementation, and must not overwrite the live file or another builder's output.

Harden the repair: `add_arxiv_papers` currently matches normalized title and a year gap before replacing the DOI/description. Add explicit author/identity corroboration; a title and age difference are not sufficient proof that two works are the same. Preserve original source IDs, raw dates, canonical identity, and correction reason. Explicit matching IDs take precedence. Do not overwrite a valid journal publication merely because an earlier preprint exists.

Represent paper metadata structurally instead of parsing a year/author back from display prose: DOI, arXiv ID, OpenAlex ID, authors, original-publication date, version date, venue, source, and verified alternate URLs. Group only demonstrably related versions. Emit original and later dates distinctly. Check a bounded corpus of landmark papers and DOI resolution outcomes during generation validation; network resolution failures are recorded and do not automatically erase records.

Required enrichment failures leave the previous good paper generation active. The Papers with Code archive remains supplementary; foundational records must not depend exclusively on an external archive being available during a rebuild.

**Acceptance:** the canonical Transformer and RAG works are retrievable by full title and identifiers; RAG's already-present ID no longer preserves the unrelated title; author/year variants work; same-title works stay separate; published/preprint versions are grouped without falsifying dates; unavailable enrichment cannot silently publish the audited bad state.

## Change 9 Add recent research and explicit dates

Keep citation-based ingestion for established works. Add a separate bounded recent-paper lane using structured metadata and supported provider date pagination, with provider behavior tested before deployment. Start with a configurable 90-day window and 50,000-record budget. Reserve budget by date and field/source coverage so one discipline cannot consume the entire lane. No citation minimum applies to this lane.

Cache/resume state includes query filters, time window, fetch version, and cursor so an old completed citation fetch is not treated as a fresh recent fetch. Share the compact result set after it passes validation; upstream requests occur during background ingestion rather than exposing each user's query to a provider.

Merge established and recent records by confirmed identity and preserve provenance/count type. Papers with Code method-use counts are not citation counts. Add bounded abstract/keyword search metadata where provider terms permit; implement this as a separate measured extension rather than inserting abstracts into display descriptions.

Expose explicit after/before date constraints consistently through CLI, JSON, and MCP. Natural-language years become hard filters only when phrasing clearly requests a date; dates in titles, standards, and software versions are preserved as ordinary terms. Newest order is optional and applies after relevance. Unknown dates cannot satisfy a hard date constraint. Date-filtering must happen before candidate caps.

**Acceptance:** a 2025-restricted research query never silently offers a 2016 paper as satisfying the date; results with unknown dates are identified; recent low-citation works survive ingestion and retrieval; established exact-title lookup is unchanged. An empty constrained result reports indexed coverage, not an assertion that no such research exists.

## Change 10 Resolve entities and make facts dependable

Add direct named-entity lookup in the page backend, returning bounded entity candidates with stable item identity before display blending. Facts retrieval uses this lookup instead of relying on five displayed search rows. Preserve existing company/fruit disambiguation and clarify ambiguous entities rather than selecting a namesake solely because it has the requested property.

Japan's Q17 population and other facts are already present in the HPC file; typed article search finds the country while ordinary search selects the WHO page with the same title. Trace candidate retrieval, selection, learned placement, and `fact_pages` filtering using this pinned record and competing pages. Implement the narrow entity lookup before expanding facts ingestion. A generic reference subpage sharing an entity's title must not suppress the entity used by the facts tool.

Separately extend `plumb-ingest/src/item_facts.rs` completion reporting by property, read stage, targeted batches, endpoint, and affected item counts. Use the existing per-item fallback for genuinely missing enrichment. Change worldwide offset pagination only if a pinned-data reproduction establishes its role; mirror fallback and targeted top-item repair already exist.

Add resumable targeted enrichment for a fixed high-value entity list, including countries and frequently requested organizations. Persist failed item/property pairs for background retries. Missing property, unresolved entity, interrupted enrichment, and stale data are distinct states. No live search-triggered upstream request is required.

Separate unsupported properties from missing data. The current `FactKind` has elevation/height but no river length or borders; those require explicit schema/import/rendering work, not a Japan-style lookup fix. Prioritize properties using validated demand and coverage. Physical constants belong in a small sourced constants-answer path; changing minimum wages requires jurisdiction, effective date, and maintained source data. Neither should be promised as a result of adding arithmetic or generic Wikidata enrichment.

Preserve population count years and current-statement filtering. Add provenance metadata for observation time, retrieval time, valid-from/to when known, units, and underlying statement identity. Do not manufacture an as-of date from the fetch date. Update compact serialization, fact rendering, MCP JSON, and peer layers together with backward compatibility.

**Acceptance:** known-country fixtures resolve the correct Wikidata item regardless of site/page placement; population reports the source's count year; ended CEOs are excluded; missing/stale/ambiguous cases are distinguishable; facts refreshes preserve profiles, leads, and unrelated valid facts. The accepted live corpus includes Japan's requested property or a precise ingestion diagnostic.

## Change 11 Expand specific destination pages

Start with measurable targets: California court tenant guidance, official agency forms/standards, company investor and newsroom sections, and practical repair/recipe instructions. Extend `plumb-core/src/subpages.rs` and `reference.rs` through source profiles with roots, index pages, task types, jurisdiction, language, and version/date policies.

Use the DeepSeek wanted-site evidence to select bounded first batches: verify current coverage for irs.gov, ssa.gov, studentaid.gov, treasurydirect.gov, tools.usps.com, tfl.gov.uk, nhs.uk and cdc.gov. For docs, verify jenkins.io, docs.pytest.org, prometheus.io, typescript-eslint.io, vite.dev, docs.gitlab.com and grafana.com. Distinguish missing host profiles, permitted roots, absent task pages, and present-but-unretrievable records. For each fetched source, require a few useful task pages and reject Site Index/search/error/redirect/empty pages as successful coverage. Forms, health guidance, and travel operations are separate content types; avoid publishing a large shallow sitemap sample and calling the gap closed.

The existing NVIDIA profile targets `www.nvidia.com`; inspect actual investor-host coverage and permitted redirects before expanding roots. For government sources, index relevant document titles and HTML guidance before adding bounded PDF text extraction. PDFs require a separate sandboxed/time-limited extractor and resource measurements; the current inner-page crawler accepts HTML.

Replace shallowest-first-only selection with bounded quotas across useful root sections and explicit index/sitemap hints. Keep robots, host, redirect, and rate restrictions in the existing crawler. Enrich reference pages with headings and task type, reusing the rich-content path, so cost articles, repair instructions, recipes, and editorial articles are distinguishable. A task-type signal boosts a fitting page only if it also matches the topic.

For forums, first audit supported optional plugins and public site-search handoffs. The Reddit plugin already exists and requires owner credentials; its activation is a deployment configuration decision. Support eligible explicit-site operator queries without bypassing the domain constraint. Provider credentials, access, and terms are real dependencies; do not promise universal forum crawling or silently send all queries to external plugins.

**Acceptance:** investor searches return the requested company's verified investor page; tenant searches return jurisdiction-matching guidance; faucet repair prefers instructions to installation cost; cookie recipes prefer usable recipes to celebrity news. If a forum source is unavailable, return an accurate coverage status and site-search link.

## Change 12 Respect news source intent

Modify `news.rs`, node news status, common assembly, and the deployment allowlist. Distinguish source-requested news from news about an entity. Resolve the requested publisher to permitted feed hosts, including legitimate alternate hosts. If BBC is requested but no BBC headlines are available, return that condition instead of substituting another publisher that mentions BBC.

For topic searches, retain time-window constraints and diversity limits. Replace the rigid all-title-words intersection with measured lexical matching over substantive topic terms, preserving quoted phrases and entity constraints. Add synonyms only when validated. Increase retention or ingestion sources only when the corpus report shows a meaningful coverage benefit within the resource budget.

Return absolute publication timestamps and fetch/source status in addition to relative age. Include recent headlines in the common assembled result so MCP, SearXNG, and JSON agree. Add `/api/recent` to `site/Caddyfile`'s public route allowlist and test that exact public route without opening management routes.

**Acceptance:** BBC latest news returns BBC's feeds or a source-specific availability message; NVIDIA earnings matches both subject and event rather than either word alone; expired and duplicate headlines are excluded; API/MCP show matching source identities and dates; public recent endpoint works after deployment.

## Change 13 Add language metadata and explain gaps

Carry actual page language through `PageMeta`, `FetchedDoc`, article extensions, and `Page`. Prefer declared/detected content language over host country. Apply requested language before candidate caps. Preserve typed-domain navigation rules intentionally; unknown language stays identifiable rather than being mislabeled English.

Start with Spanish and German reference/docs sources and Wikipedia sets using the existing language-capable article path. Add language-specific stemming/stop words where justified; keep exact name/symbol fields language-neutral. Benchmark each new language separately and keep multilingual embeddings as a later measured option. The pinned English site embedding model cannot be assumed to solve this change.

Add response status indicating available sources, temporal coverage, disabled or partial sets, missing location, unresolved entity, and weak relevance evidence. Use the manifest from change 3. Show concise user wording only when relevant to the query, alongside an existing site-search handoff or relaxed-filter suggestion. Do not automatically broaden explicit filters or call an upstream search engine. Confidence labels are calibrated categories with reasons, not invented probabilities.

**Acceptance:** the Spanish recipe query returns an actual Spanish recipe where indexed; a German institutional query retains correct official navigation; requested-language pages survive retrieval without English stems; an unsupported long-tail query produces a clear gap and useful handoff instead of unrelated high-confidence filler.

## Change 14 Bind identity-tool decisions to the requested entity

Extend `plumb-node/src/mcp.rs`, the existing `eval/official_site/run.py` suites, and focused MCP tests. Preserve the distinction between a site's official status and evidence that it belongs to the entity requested. Resolve the entity and its task/jurisdiction before selecting a URL. Task words such as converter or database must not defeat the discriminative name Xe or Chroma. Spelling suggestions are alternatives; changing mypy to mapy cannot establish the requested entity's official site.

Reuse the existing package/docs path. A verified package's docs link can outrank an unrelated popular site's medium-confidence guess; popularity protects a matching entity's site, not every popular pick. Return confidence and reasons tied to the selected entity and URL. If affiliation cannot be established, distinguish an unresolved official destination from ordinary search suggestions rather than presenting an unrelated match as `found:true`. Preserve existing correct brand, package, and ambiguous-entity behavior. A jurisdiction explicitly stated in the query, such as México, takes precedence over a default-country boost while an explicit conflicting filter is handled transparently.

For lookalikes, retain current registrable-domain and hostname boundary checks. Add compact, provenance-bearing affiliation evidence for verified alternate domains, product consoles, APIs, and migrations. Prefer authoritative owner references, Wikidata/verified package metadata, and bounded verified redirect chains to a known official destination. A redirect by itself is not proof of affiliation, and the same label on another TLD is not an automatic exception. Preserve literal-host analysis against `brand.com.attacker.tld`, Unicode confusables, suffix tricks, unknown hosting tenants, and actual misspellings. Store or return uncertain cases as unknown/suspected rather than an unsupported definitive accusation.

**Acceptance:** mypy documentation selects verified mypy docs; Xe converter and Chroma database retain their entity; SAT México honors the stated jurisdiction with defaults and explicit country options; verified Hetzner console aliases avoid the reproduced accusation; Semantic Scholar's API remains accepted. Real impersonation fixtures continue to be flagged. Report false medium/high-confidence official-site answers and false lookalike accusations separately from raw tool success rates. Primary ownership evidence is required for each accepted alternate domain.

## Change 15 Extend source quality without hiding relevance failures

Reuse `plumb-index/src/health.rs`, `is_pill_shop` in `lib.rs`, and shared query intent. Pin exact reported spam queries and independently review the named sites before applying domain-specific penalties. The current two follow-up medical/tax probes did not reproduce those examples; approximate queries cannot establish either a repair or the absence of the historical problem.

Combine topic relevance with bounded source evidence: verified institutional identity, jurisdiction, substantive page text, crawl/error state, repeated deceptive drug/shop patterns, and reviewed spam decisions with reasons. Weak popularity, missing metadata, or a particular TLD alone cannot prove spam. A short reviewed penalty list can contain confirmed cases while broader quality features are measured; do not blanket-ban unfamiliar sites. Preserve named navigation for legitimate businesses and sources, subject to actual impersonation/spam evidence.

Health authority routing already inserts three authority homepages. Keep its useful intent detection, but select relevant indexed guidance pages when available and preserve correct jurisdiction. Generic authority homepages are useful fallbacks, not proof of an answered task. For money/tax queries, distinguish official limits/forms from editorial comparisons, fake-ID pages, and topic-sharing businesses. Quality policy applies consistently to sites and deep-page rows and survives learned reordering; page relevance gates remain necessary alongside it.

**Acceptance:** manually verified spam fixtures lose top-five informational placement; relevant official/clinical guidance and tax pages survive; legitimate small-source and explicit-navigation controls retain recall; explainable quality reasons are available in evaluation traces. Evaluate precision and false positives on a fixed corpus, using the same latency/RSS gates as ranking changes. Do not infer a site's legitimacy from an LLM self-grade alone.

## Verification and release gates

The thresholds below are proposed shipping criteria, not measured outcomes. Change 1 establishes the baselines and sampling uncertainty before they are enforced.

1. Every audited regression has a pinned deterministic fixture, expected result identity/type, and at least one negative control. All deterministic contract, date, provenance, and migration checks pass.
2. Existing held-out navigation top-one decreases by no more than one percentage point. Evaluate paired changes and uncertainty; do not hide a weak category behind a pooled score. Informational NDCG/MRR improves, wrong-brand top-three rate falls, and relevant-candidate recall does not fall after adding filters.
3. On representative server and desktop profiles, proposed budgets are no more than 10% relative p95 search-latency regression and 20% relative steady-state RSS regression for ranking-only changes. Corpus expansion and richer extraction require separately measured, explicit storage/download costs. A profile cannot exceed its configured hard resource limit. Absolute baselines and exceptions must be written into the experiment report before rollout.
4. Rebuild fresh staged data for data-affecting changes. Ranking comparisons use the same corpus; corpus comparisons use the same ranker. Report their separate effects, then their combined behavior.
5. Validate old-reader/new-file and new-reader/old-file behavior, bounded malformed extensions, cache invalidation, partial transfers, growth guards, cancelled ingestion, generation swaps, and rollback. Preserve the existing compact six-column article format.
6. Run the repository checks for implementation changes: `cargo fmt --all --check`, `cargo clippy --workspace --exclude plumb-desktop --all-targets -- -D warnings`, and `cargo test --workspace --exclude plumb-desktop`. Exercise relevant Docker tests for publication, restarts, and public routes. No new runtime tests are needed merely to publish this plan.
7. Use existing experiment layers for ranking rollout. Start with controlled fixed-corpus comparison, then a small deployment experiment. Corpus promotion is a separately reversible generation swap. Rerun the same public/MCP canary queries, confirming the build, model, settings, and generation actually serving them. An improved local test without the corresponding deployed generation is not completion.

## First implementation batch

Establish a build from current main that retains the HPC memory fixes and the existing findings-off evaluation support. Pin the HPC corpus and a small manually adjudicated baseline. First ship change 6's short typed-docs slice and changes 4/5's domain-word relevance fixes. Follow with change 14's identity-tool corrections and change 15's verified source-quality controls. Start targeted useful-page batches from change 11 after change 3 supplies safe subset merging. This order reflects the larger DeepSeek evidence and the reproduced follow-up cases.

Alongside those focused changes, retain direct facts entity lookup from change 10, MCP places from change 2, a staged paper repair with the existing-ID consistency correction from change 8, and evaluation of the existing reference gate from change 6. These remain small independent deliverables with specific acceptance gates. Existing places exposure should not wait for worldwide business completeness or same-name-town disambiguation; expose missing/ambiguous location clearly and measure the geographic backend separately.

Complete changes 1 through 5 around those fixes to make comparisons reproducible, share result assembly, prevent stale/partial refreshes, and address package intent and partial-brand promotion. Docs heading presence is already verified; measure heading/symbol retrieval failures rather than scheduling a blanket heading rebuild as the presumed fix.

Proceed to candidates and rich docs only after the baseline can attribute losses and the data pipeline can publish a verifiably fresh generation. Expand papers, facts, and destination-page sources on that foundation. Complete news, language coverage, and gap responses using the shared assembly and manifest rather than separate surface-specific fixes.

The plan is ready for implementation with the verified deployments, branch corrections, and DeepSeek reconciliation above. Remaining decisions concern measured stage attribution, independently validated labels, and rollout thresholds. The recorded corpus and follow-up probes already distinguish several retrieval failures from missing content.
