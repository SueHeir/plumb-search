# Search generality audit

The search quality batch uses indexed names, aliases, content, semantic similarity, structured entity records and provider metadata. Evaluation cases are diagnostics: serving routes do not look up case IDs or load expected answers. The pre-merge audit identified several policies that were too closely shaped around the original examples. Their corrections and combined results must be reviewed together before release.

## Findings and required corrections

| Finding in the assembled candidate | Required general behavior |
| --- | --- |
| Any short or digit-containing unmatched qualifier could protect a partial brand match | Retain substantive qualifiers. Require indexed lexical, alias or semantic support rather than qualifier length or character shape. |
| Refund wording could protect a popular partial name without subject evidence | Apply the same evidence requirements to refund and other task requests. |
| Selected task phrases were stripped before name-only reranking | Preserve substantive constraints; generic navigation syntax must not erase the requested service or subject. |
| Three category suffixes were removed by the official-site parser | Keep category words in candidate validation; insufficient evidence produces suggestions or abstention. |
| API/developer/reference/specification qualifiers were stripped by a second identity parser | Separate documentation navigation while retaining technical identity constraints. An acronym owner does not establish the requested technical resource. |
| A same-name cached package docs URL could establish a company destination without indexed affiliation evidence | Require a matching indexed project homepage or verified destination supporting the complete request; otherwise show an unverified hint. |
| Three literal hosts bypassed backend affiliation checks | Use backend owner evidence. Similar labels and service words alone establish neither affiliation nor impersonation. |
| Founder disambiguation selected company type only when an exact namesake was described as fruit | Founder and founding-date requests can apply to organizations, settlements and other entities. Return ambiguity for shared names without sufficient type evidence; never choose a namesake because it has a fact. |
| One literal DOI had a special repair, and two papers were mandatory publication anchors | Verify corpus-derived IDs and source consistency. Optional caller-supplied canaries remain separate from general publication gates. Title and author corroboration can link a preprint, but cannot overwrite an independent journal claim. |
| A same-name package or owner could receive a confident destination despite competing identities | Preserve competing evidence and report unresolved identity. A package name or popularity does not establish which entity the user requested. |

Corrections are integrated in `3750390` (founding ambiguity), `8bacd94` (substantive query constraints), `f5a1668` / `d79dc4a` (generic paper consistency), `d1b2744` (affiliation and owner ambiguity), and `d4e57f4` (private ranking parity). `b0db107` adds shared subject evidence to native and private ranking after the first generic candidate introduced harmful ordering losses despite unchanged top-100 acceptance. Frozen evaluation labels remain unchanged; unit fixtures expand supported and unsupported evidence explicitly.

The experimental shared subject guard uses indexed aliases and independent names to scope query evidence, then orders supported candidates ahead of weak fallbacks. Synthetic controls pass, but the frozen serving comparison exposes sparse real-world identity and task evidence that those controls do not fully model. A missing task word is unknown rather than proof of irrelevance; an independent link name proves a name association rather than compatibility with the full request.

The first subject guard (`b0db107`) introduced four acceptance losses relative to `bb8b59a`. The corrected `b5258b4` completed all 959 observations with 22 acceptance gains and one loss relative to baseline. It restored several sparse semantic and established-name cases, but an unrelated independent-link anchor still removed a present passport destination. Material ordering losses remained for Amazon tracking, OpenAI API, GitLab CI and other owner queries. Legacy AI MRR fell 0.02190 (95% family-bootstrap interval −0.03915 to −0.00594). Top-100 acceptance alone does not capture this harm.

The bounded release excludes this experimental promotion from the default native and private paths while retaining native evidence diagnostics and explicit opt-in tests. Its generic retention correction uses the existing descriptive semantic floor for noncollision descriptive candidates. It does not restore short-query, digit, API, refund, host or named-example exemptions. The final `96198f5` unchanged-input comparison passed 959 cases with 14 acceptance gains, zero losses and no manual-regression flags. Legacy AI MRR returned to parity (+0.001147; interval crossing zero). Brand acceptance/top-three rates are unchanged, with five top-one demotions and MRR −0.008591 reported as a small ordering tradeoff. Further subject inference and relevance-tier tuning belongs to the next iteration, with sparse indexed evidence represented explicitly.

`a7fdab9` corrects the remaining identity-parser and cached-docs paths. API/developer/reference/specification qualifiers remain part of validation. Cached package docs require matching indexed owner/destination evidence. Focused MCP checks passed; final serving comparisons remain a release gate. A legitimate author repository that differs from a strict homepage label is reported separately from an actual wrong owner.

## Curated data and routing policies

Some exact names and domains are legitimate source metadata. Docs/reference catalogues select crawl roots, product aliases, scopes, limits and source weights; retrieval still requires indexed records. Publisher aliases and institutional identities describe known sources. These catalogues affect discovery and eligibility, so they should be reviewed as data rather than described as entirely learned ranking.

The existing tool-source router selects fixed destinations for weather, time, currency, calculators, translation, stocks, dictionaries and local directories. The medical policy selects a fixed authority shortlist. Both can move or insert an indexed source homepage ahead of ordinary results, even without task-specific page evidence. The batch extends shared routing to MCP and SearxNG and marks medical homepage fallbacks as missing query words. This remains explicit curated routing, not evidence that each inserted homepage answers the query. Separating those source suggestions from relevance-ranked results is follow-up work.

Known paper titles, authors, IDs and dates belong in regression fixtures or explicitly supplied canary expectations. A verification response must still provide its own metadata; fixtures cannot supply missing runtime facts. The known misdated DOI may remain a retained publication claim when source evidence is insufficient to adjudicate it generically.

## Validation requirements

Use synthetic entities and publications with arbitrary names and IDs, not only the original examples. Cover supported and unsupported qualifiers, candidate ordering, competing owners, missing facts, valid journal dates, incompatible authors, duplicate provider responses and conflicting source identities. Verify that query changes affect relevance through evidence, not through a named exception.

Repeat the fixed-corpus comparison after ranking changes. Keep the same ordered page inputs, cap, model, vectors, clock, query options and label fingerprints. Report acceptance losses and MRR tradeoffs alongside gains. The exploratory suites have been used during diagnosis and are not independently adjudicated, untouched release holdouts.

Check serving contracts separately: direct facts, official-site ambiguity, lookalike uncertainty, docs excerpts, places and dated-paper responses. Keep corpus-refresh canaries separate from retrieval-only comparisons, preserve original hashes, and report missing or unresolved data rather than fabricating coverage.

## Integration evidence and remaining gates

The frozen `f5a1668` run completed 959 observations with 21 pass gains and no pass-to-fail losses relative to `5b96a35`. That acceptance window is top 100 and is insufficient as a release criterion: compared with `bb8b59a`, generic constraint retention introduced 12 MRR losses and one gain, including nine rank-one demotions. Legacy AI MRR fell another 0.01736, with a family-bootstrap 95% interval of −0.03089 to −0.00662. This candidate is held before main while subject/task evidence is corrected generically. Relative semantic scores are normalized among each query's nearest vectors; they are not absolute verification of a requested subject or task.

The full historical paper input (199,982 records) fails the primary-identity gate because six rows have no identifier. Those rows are preserved separately in a scratch quarantine with raw bytes, line numbers, reasons and hashes. The original file is unchanged and remains ineligible; production gates were not relaxed. A separate immutable valid-primary subset (199,976 records) passed the generic repair and serialization gate against the cached two-identity XML feed at `d79dc4a`. One primary arXiv record was corrected; two records received source snapshots/preprint linking. An independent journal DOI/date claim remains unresolved. All 143 legacy variant groups were preserved unchanged. `whole_corpus_verified` remains false, no network request was made, and no candidate was promoted.

The optional named-paper expectation file supplied diagnostics only. Runtime repair used the XML response's title, authors and dates; it did not fill missing facts from the expectation file. Corpus-refresh proof is separate from the fixed retrieval comparison, whose inputs and labels remain unchanged.

`ea9ec2a` prevents new empty provider identities: a blank DOI falls back to a valid supplied OpenAlex identifier. Malformed nonblank DOI claims and invalid provider identifiers fail conversion and are reported before caching. The same normalized DOI/OpenAlex identifiers populate structured metadata. This does not recover the six historical rows whose identifiers are already lost.

Rollback must preserve mutable data as an actual copy or copy-on-write snapshot, including derived page indexes, source generations, identity and configuration. Startup calls `remove_other_indexes` after a successful new page-index build, so a versioned schema key alone does not preserve the previous derived index.
