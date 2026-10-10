# Search quality batch — October 9, 2026

The user requested parallel implementation chats using GPT-6.1 Sol with extra-high reasoning, focused checks during implementation, and one expensive combined evaluation after integration.

Integration branch: `codex/search-quality-batch-20261009`.
Pinned implementation base: `e33582cd8e30c5c288629b58f68348e56c9e91d7`.
This base combines main's evaluation support, the HPC memory fixes, and the [verified implementation plan](search-improvement-plan.md).

## Active work

Each implementation chat has its own managed worktree and topic branch. The parent integrates committed changes; production services and live corpus files stay intact while candidate code and datasets are evaluated.

| Chat | Task ID | Dispatch status |
| --- | --- | --- |
| Fix Plumb short docs retrieval | `01a12460-1900-7701-859f-58fcc89cdf8b` | Started |
| Improve Plumb docs symbols and passages | `01a12460-1bf9-7521-8e88-07e4d29f0e65` | Started |
| Fix Plumb domain collisions and spam ranking | `01a12460-1e06-7311-b86e-6fb416da8760` | Started |
| Fix Plumb official site and lookalike tools | `01a12460-2013-7e00-8e24-bd1a78b97182` | Started |
| Expose Plumb places and improve news results | `01a12460-21b3-7b93-b90e-fc5d7670eba0` | Started |
| Fix Plumb entity facts lookup and provenance | `01a12460-2330-7e73-90ba-b46ead007f90` | Started |
| Repair Plumb paper identity and recent research | `01a12460-25d4-7552-a7b0-f6e7deef8bc4` | Started |
| Fix Plumb refreshes and useful page coverage | `01a12460-277d-7d80-b355-18c1f0bf65c5` | Started |
| Improve Plumb language and location matching | `01a12460-2989-7033-9a3f-25d957274ec8` | Started |
| Prepare Plumb combined batch evaluation | `01a12460-2b54-7011-8183-586fcab4d92a` | Started |
| Reclaim old DIRT SOIL GRASS storage on HPC | `01a12461-cfa1-7dc2-b308-f6f23b9dbf7f` | Complete: 111.4 GiB net reclaimed |

All eleven chats use `gpt-6.1-sol` and `xhigh`. Storage cleanup is projectless and operates through SSH. Validation moved to separate Cargo target directories after a worker observed a foreign schema artifact in the initially shared directory. Worker builds use one job, no incremental compilation, and no development/test debug symbols to fit the Mac's 24 GiB RAM.

## Integration checks

- Review each final commit range against its declared scope and focused test results.
- Reconcile the compact article extension used by rich docs extraction and declared language. Preserve legacy decoding and delimiters.
- Reconcile direct entity resolution, MCP places, news assembly, and official-site evidence in MCP/backend code.
- Reconcile cache versioning and rich extraction settings before generating candidate docs/reference sets.
- Verify whole-query relevance retains exact brands, typed domains, short queries, and useful semantic results.
- Review safe subset merging, failed-host preservation, staging, and promotion before any corpus update.
- Run workspace formatting and the repository's applicable checks on the integrated tree.
- Pin build identity, corpus checksums, query options, and findings state for both baseline and candidate.
- Run one combined evaluation using family-separated acceptance labels and explicit negative controls.
- Report retrieval improvements separately from dataset refresh improvements and list coverage gaps still requiring data.

## Baseline and operations

The current connector/HPC node is 0.2.1 at the verified memory-fix checkout. The public website is a separate 0.2.0 deployment and is not a controlled baseline. Private routing identifiers are excluded from reports.

Cleanup removed 240 verified Cargo cache roots from 234 inactive DIRT/SOIL/GRASS trees. Net reclaimed space was 111.41 GiB; user-available space rose from 23.06 to 134.47 GiB at cleanup completion. Source/Git/preserved-output checks passed, and the two running Plumb processes retained their PIDs and start times throughout deletion. The exact inventory and deleted paths are in the cleanup chat's local report.

The HPC service restarted externally during dispatch, changing its checkout from `068faba` to `739dd11`, which includes findings-off support. The parent recorded the transition and kept the original probes. Runtime code state must not be inferred from the earlier checkout snapshot.

A frozen search snapshot at `/home/suehr/scratch/plumb-quality-20261009/baseline-snapshot` contains site generation `000145` search/spelling files, all sixteen gzip page sets, the embedding model, and vectors. It uses 3,964,584,112 bytes across 30 files. Each copied file matched the source SHA-256, and site metadata matched before/after copying. Optional privacy/PIR buckets and source lockfiles are omitted; the snapshot is scoped to search evaluation. Future locks and derived indexes belong only in scratch. The snapshot consumes additional space after the cleanup measurement.

The old 1,000-job agent evaluation was explicitly paused by the user. This batch does not resume it. Evaluation findings must not write to production.
