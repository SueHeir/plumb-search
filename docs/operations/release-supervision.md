# Search release supervision

This runbook covers 0.2.x search-quality releases. LLMs may help evaluate search
results; node management and recommendation product features belong to 0.3.x.
Use subscription-backed agent execution only. Do not enable API-billed model
calls, external paid graders, or new infrastructure without a separate budget.
The legacy 1,000-job model run remains paused.

## Ownership and cycle state

One coordinator owns the candidate, evaluation, pull request and merge. Each
deployment has one named owner. Read the current handoff before starting work;
an active coordinator is not a queue to duplicate. Keep code changes in separate
worktrees and serialize merges and deployments. Recover worthwhile conflicting
PRs in subsequent batches rather than expanding an almost finished candidate.

A durable runner needs a singleton lease and a persisted cycle record containing
the owner, worktree, candidate SHA, evaluation artifact paths and hashes, CI SHA,
published digest, previous and current host image IDs, recovery location,
deployment phase, verification results and next action. On restart, inspect
actual Git, CI and host state before resuming. Unknown state stops deployment.
Never infer a completed deployment from an image publication or checkout SHA.

## Candidate gates

1. Freeze the candidate commit and establish a clean build identity. Run the
   required repository checks and inspect CI for that exact commit. A later
   ranking or serialization change invalidates earlier readiness evidence.
2. Compare baseline and candidate on identical ordered corpus files, hashes,
   model, vectors, clock and query options. Keep corpus refresh experiments
   separate. Record top-one/top-three, MRR, category losses and negative controls;
   passing at rank 100 is not evidence of good top-result quality.
3. Exercise serving contracts, including private/server parity and MCP identity,
   facts, places and docs behavior. Report conservative abstentions separately
   from false confident answers. Model-assisted transcript review is qualitative
   evidence, not a replacement for deterministic checks or independent labels.
4. Establish representative latency and steady-state memory baselines. The
   proposed +10% p95 latency and +20% RSS limits in the search improvement plan
   are not established measurements. A host must remain within its hard limits,
   including startup index rebuilding and swap pressure.
5. Validate persisted-state compatibility and a concrete recovery path. A
   successful parser test alone does not prove a live index can be restored.

Use the bounded existing evaluation runner: one Linux build at a time, a
30-minute core run, and at most 400 identity calls over 10 minutes. Its 8 GiB
RSS / 12 GiB scratch policy needs the coordinator's resource monitor; the
Python runner does not enforce a kernel memory limit. Keep production history,
findings, peer results and mutable data outside controlled comparisons.

## Recovery before rollout

Record the exact running image ID and revision, Compose configuration, selected
page-set settings, source-generation pointers and relevant file hashes. Retain
the previous image locally; do not prune it during the observation period.
For HPC or desktop deployments, retain the old executable or app bundle too.

Built-in backups hold settings, network identity, credit and remote-control
material, not corpus or index data. Treat those files as secrets: keep their
existing protections, never print their contents, and do not transfer credentials
as part of an evaluation or an ad hoc recovery copy. Use an approved private
backup facility for sensitive state. A persistent Docker volume is not a backup.

The page-index schema contributes to the derived index key. After successfully
opening/building an index, `remove_other_indexes` deletes other `pages/index-*`
directories. Therefore a previous schema key does not retain the old index.
Do not keep a full duplicate corpus or derived index on the Droplet: its
user-approved Plumb storage budget is 64 GB, independently of filesystem free
space. Retain the prior image and a small protected configuration manifest
locally. Existing source sets plus their counts, generation metadata and mtimes
can rebuild a compatible old page index; old image rollback alone does not
restore an index that startup removed. Keep a reserve for temporary index builds,
Docker images and normal corpus growth within the allocation.

For this serving-only release, prefer in-place executable rollback with unchanged
compatible source data, followed by an old-schema index rebuild if needed.
Verify old/new readers against the retained source records and settings before
rollout. Measure rebuild time, peak memory and temporary disk in a bounded
scratch experiment; until measured, recovery time is unknown and search may be
unavailable throughout rebuilding. Do not require a full duplicate when this
recovery tradeoff is accepted. Block incompatible destructive source migrations
until their affected irreplaceable records have a verified recovery path.

For a recovery copy where needed, use existing HPC capacity for nonsensitive
source records and generation metadata, outside live startup cleanup paths.
Do not transfer identity keys, tokens, credit state or private configuration as
an ad hoc corpus copy. Sensitive state is the nine-file built-in backup scope
in `node/backup.rs`; preserve existing local protections or use an explicitly
approved private backup destination. Corpus, downloaded seed sets and derived
indexes are excluded from that built-in scope. Signed crawl evidence may need
preservation if it cannot be fetched again; do not assume every record is
reproducible merely because its search index is derived.

A running recursive pre-copy alone is not consistent. Where preservation is
necessary, the deployment owner must gracefully stop the node and finalize sync
and verification while quiescent, respecting the five-minute stop allowance.
Preserve independent reference jobs and the production tunnel. Use actual
copies or verified snapshots, never hard links to mutable files; preserve source
mtimes. An authorized HPC pre-copy is preparation, not proof of recoverability.
Invalid staged corpus publication remains blocked; scoped quarantine proof does
not authorize promoting an ineligible full corpus.

## Existing fleet and storage checks

These are observations from October 10, 2026, not fixed capacity guarantees.
Recheck immediately before any copy or rollout.

| Host | Role and recovery path | Observed data / free disk |
| --- | --- | --- |
| Public droplet | Docker node and Caddy under `/opt/plumb-search/site`; data `/var/lib/docker/volumes/plumb-data/_data`; relay is separate | 46 GiB / 81 GiB |
| LA and NY | Docker `plumb`, `/opt/plumb/docker-compose.yml`; same volume path; `--crawl-only` | LA 20 GiB / 12 GiB; NY 22 GiB / 11 GiB |
| HPC | User `plumb.service`; checkout `/home/suehr/projects/plumb-search`; data `plumb-data`; separate tunnel service | 49 GiB / 126 GiB |

The 81 GiB Droplet free-space observation is physical capacity, not permission
to exceed the 64 GB Plumb allocation. HPC is the intended home for larger corpus
experiments, subject to measured disk and memory limits and no new paid
provisioning. Full same-host crawler backups do not fit the observed free space. Do not assume
compression will solve this or copy until the destination and remaining reserve
are checked. Establish a compatible bounded recovery scope or an approved
existing storage destination first. Crawl-only nodes skip serving page-index
workers, but their signed crawl and credit persistence still need compatibility
review. Do not delete retained crawl data to make an unverified backup fit.

## Publish, deploy and verify

Main pushes already publish Docker images through the repository workflow. Use
that pipeline. CI and image publication are separate workflows; main had no
branch protection at inspection. The deployment owner must verify required CI
and the exact published SHA/digest before selecting an image. Do not deploy a
floating tag solely because its name is `main` or `latest`.

Publication does not update running containers. The inspected droplet has no
continuous pull updater. The crawler upgrade scripts perform a specific upgrade;
their watches expire after 24 hours. Docker restart policies do not pull images.
Deploy one host at a time with the retained configuration and persistent volume,
then verify running image/revision, health, restart count, resource headroom and
public/MCP canaries before advancing. A relay with no revision label needs its
own identity evidence and must not be silently included in a node rollout.

On failure, the owner stops the candidate, restores the agreed previous image
and compatible data/index/configuration, and verifies the recovered service.
Crawler policy distinguishes startup deployment failure from later crashes:
the existing watch rolls back during its initial startup window, then stops on
later crashes. Preserve that crash-stop behavior; do not replace it with an
unbounded restart or rollback loop.

## Continuous execution readiness

Registered Mac task execution is available. HPC SSH, an installed Codex CLI and
saved login do not establish a registered remote task host or a working durable
agent runner. Its desktop update-manager service is not a project supervisor.
Before enabling unattended execution, verify a supported authenticated bounded
run, singleton ownership, restart recovery, quota handling and alert delivery.
Use existing credentials through supported authentication only; do not extract
session tokens for an API-compatible benchmark server. Stop when subscription
capacity is unavailable. Do not fall back to billed API usage or purchased
credits. Do not claim 24/7 operation until its scheduler, recovery and execution
have actually been verified.
