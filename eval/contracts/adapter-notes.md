# How a report observes a search

What `plumb-tools eval --report` does with each search contract, and what its report rows can and cannot show. For contributors reading reports or changing report mode in [contracts.rs](../../crates/plumb-tools/src/eval/contracts.rs). How to run a report is in [eval/README.md](../README.md#contracts-and-reproducible-reports).

## Two ways eval assembles results

| Mode | Function | Operators and typed searches | Page sets searched |
| --- | --- | --- | --- |
| `eval --queries` | `eval::evaluate` in [eval.rs](../../crates/plumb-tools/src/eval.rs) | Not parsed | In every language |
| `eval --report` | `contracts::observe` | Parsed, as a node does | In the search's language, before any candidate cap |

Report mode follows the node's page path in [node/pages.rs](../../crates/plumb-node/src/node/pages.rs) more closely, so use it when a change touches operators, typed searches (`kind`) or language. TSV suites given with `--report` run through `observe` too.

## The steps of one search

`observe` runs a case through the same plumb-index and plumb-node functions the node's results pages use:

1. **Options**: the case's `country`, `language`, `exact` and `only_country`, falling back to `--country`, `--lang` and `--exact`. A `site` option is added to the query as a `site:` operator.
2. **Language view**: the page sets are cut to the search's language with `PageSearcher::in_language`.
3. **Sites**: `Searcher::search_meaning`, by meaning too when `--vectors` and `--model` are given.
4. **Pages**: a typed search (a `kind` option, or a paper query with a date bound or a sort order) or one with operators goes through `search_papers` or `search_naming_docs`; any other through `search` and `add_other_number`. Pages the options, operators or kind rule out are dropped.
5. **Placing**: a typed search lists its pages alone, operators use `place_operator_pages`, and anything else gets the named-site steps and `place_pages`.
6. **Learned order**: `plumb_index::learned::reorder`, when the ranking has it on and the search is neither typed nor uses operators.
7. **Answer**: `plumb_answer::answer` at the fixed `--eval-time` clock, then the fact lookup below.
8. **Sources**: `plumb_node::sources::route` and `lead_with`, or `plumb_index::health::sources_lead`.
9. **Output**: the rows, cut to `--limit`. Latency is measured up to here.

After that, outside the latency, it runs diagnostic searches: whether each expected answer is in the records or a page set, the site order by words to 100, the nearest sites by meaning, and a page search to 100.

## Where the answer was lost

Each row's `stages.loss` names the first step the expected answer did not get past:

| `loss` | The expected answer was |
| --- | --- |
| `serialized` | In the output rows (found) |
| `serialization` | In the learned order, but not in the output |
| `learned_order` | In the rows before the learned order, not after |
| `blending` | Among the pages selected for placing, not in the rows |
| `page_selection` | Among the pages kept after filtering, not selected |
| `source_filter` | Among the pages retrieved, dropped by options, operators or kind |
| `candidate_not_selected` | In a diagnostic candidate list, never retrieved |
| `retrieval` | In the records or a page set, in no candidate list |
| `record_missing` | Nowhere in the corpus |

## What a report does not show

- **The node's entity lookup.** A fact question's answer comes from the eval's own lookup: it searches the fact's subject among sites and pages, places them, and reads the answer with `plumb_node::web::answers::fact_answer`. The report marks this `legacy_eval_subject_lookup` with `production_entity_lookup_observed: false`.
- **The HTTP and MCP routes.** A report runs this evaluator's assembly in-process. To check what an API or MCP client sees, serve the same indexes through the node's routes with the [eval_mcp example](scratch-mcp.md).
- **The search's own candidate window.** Candidate recall at 10, 50 and 100 comes from the separate diagnostic searches. Recall of the output is `null` at depths above `--limit`.
