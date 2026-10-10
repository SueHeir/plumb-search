# Report adapter language scope

The completed harness-only baseline report uses `contracts.rs::observe` for all
959 cases. That path already parses operators and uses scoped
`search_naming_docs` for operator/typed queries. The separate legacy
`eval.rs::evaluate` path still has unrestricted page/profile/fact retrieval;
it does **not** produce this report-mode baseline. Do not use that path as
production language/operator parity evidence.

The candidate report adapter now obtains one language view per case and uses
it for raw page retrieval, `add_other_number`, page demand, legacy fact-subject
lookup and diagnostic candidates. This matches the public language view in
the candidate's production node page path. Scoring, cases, label statuses,
query normalization, source inputs and fixed clock are unchanged. Raw and
diagnostic stages explicitly record where language filtering occurred.

Normal candidate report builds use the language view **unconditionally**, with
no feature flag. This follow-up requires the candidate's PageSearcher APIs;
cherry-pick it only into the integrated candidate. This old harness worktree
cannot compile the final report patch against the baseline APIs, so validation
uses a clean candidate source archive with only this patch overlaid. The
`eval-candidate` feature is only an explicit entry point for the scratch example.
The baseline binary stays unchanged and retains its observed after-cap
language behavior; no baseline rebuild/replay or shared scoring change is
required.

Direct fact answers remain the **legacy subject-search diagnostic** in both
core reports. They do not exercise the production entity resolver and do not
establish its accuracy. The manifest and fact-subject stages now state that
limit explicitly. The candidate MCP example verifies actual entity lookup
before page blending, using production `facts` handling and synthetic evidence.
No new release labels or gates are added.

The candidate-only cap regression fixture places 220 French articles ahead of an
English article in unrestricted retrieval. It verifies that raw search and the
100-page diagnostic window both retrieve the English article using the early
language view. All report tests run in a normal candidate build without an
explicit feature flag. Before applying this candidate-only adjustment, the
seven contract tests also passed against the default baseline APIs.
