#!/usr/bin/env python3
"""Paired offline comparison with fixed corpus/model/suites and family bootstrap.

Differences are exploratory for candidate/legacy groups. Manual contract
regressions are reported separately. No scorer/model/network is invoked.
"""
import argparse
from collections import defaultdict
import json
from pathlib import Path
import random


def read(path):
    rows = [json.loads(l) for l in Path(path).read_text().splitlines() if l.strip()]
    manifests = [r for r in rows if r.get("type") == "manifest"]
    summaries = [r for r in rows if r.get("type") == "summary"]
    if len(manifests) != 1 or len(summaries) != 1:
        raise ValueError(f"{path}: incomplete report")
    queries = {r["case"]["id"]: r for r in rows if r.get("type") == "query"}
    if len(queries) != manifests[0]["cases"]:
        raise ValueError(f"{path}: missing/duplicated observations")
    if not summaries[0]["corpus_unchanged"]:
        raise ValueError(f"{path}: corpus changed during run")
    return manifests[0], summaries[0], queries


def fingerprints(items):
    # Paths can differ between isolated mounts; content/ordered set identity
    # cannot. The file-level manifest is still retained in each original run.
    return [(item["sha256"], item["bytes"]) for item in items]


def bootstrap(values):
    """Cluster resampling by family (500 bounded rounds, fixed seed)."""
    if not values:
        return None
    means = [sum(group) / len(group) for group in values.values()]
    rng = random.Random(20261009)
    samples = sorted(sum(rng.choice(means) for _ in means) / len(means) for _ in range(500))
    return {"family_mean_delta": sum(means) / len(means),
            "bootstrap_95_percent_interval": [samples[12], samples[487]],
            "families": len(means), "resamples": 500, "seed": 20261009}


def compare(baseline, candidate, mode="ranker"):
    old_manifest, old_summary, old = read(baseline)
    new_manifest, new_summary, new = read(candidate)
    for field in ("eval_time", "query_instruction", "model", "features", "limit", "pages_top", "target", "transport"):
        if old_manifest[field] != new_manifest[field]:
            raise ValueError(f"incomparable {field}")
    if old_manifest["features"] != {"findings": False, "personalization": False,
                                     "plugins": False, "external_results": False, "peers": False}:
        raise ValueError("core isolation features were enabled")
    if fingerprints(old_manifest["suite_fingerprints"]) != fingerprints(new_manifest["suite_fingerprints"]):
        raise ValueError("query/label suites changed")
    if mode == "ranker" and fingerprints(old_manifest["corpus"]) != fingerprints(new_manifest["corpus"]):
        raise ValueError("fixed-corpus comparison requires identical corpus checksums")
    if mode == "corpus" and (old_manifest["rank"] != new_manifest["rank"] or
                             old_manifest["learned_model_sha256"] != new_manifest["learned_model_sha256"]):
        raise ValueError("corpus comparison requires identical rank settings/model")
    old_vectors, new_vectors = old_manifest.get("vectors"), new_manifest.get("vectors")
    if mode == "ranker" and fingerprints([old_vectors] if old_vectors else []) != fingerprints([new_vectors] if new_vectors else []):
        raise ValueError("vectors changed during ranker comparison")
    if old.keys() != new.keys():
        raise ValueError("case sets differ")
    grouped = defaultdict(list)
    changes = []
    for case_id, after in new.items():
        before = old[case_id]
        if before["case"] != after["case"] or before["options"] != after["options"]:
            raise ValueError(f"case/options changed: {case_id}")
        case = after["case"]
        grouped[(case["label_status"], case["category"])].append((before, after))
        if before["score"]["passed"] != after["score"]["passed"]:
            changes.append({"id": case_id, "family": case["family"], "label_status": case["label_status"],
                            "before_passed": before["score"]["passed"], "after_passed": after["score"]["passed"],
                            "violations": after["score"]["violations"]})
    groups = {}
    for (status, category), pairs in grouped.items():
        metrics = {}
        for metric in ("passed", "top1", "top3", "mrr", "ndcg10", "wrong_domain", "wrong_brand_top3"):
            families = defaultdict(list)
            for before, after in pairs:
                if metric in ("top1", "top3", "mrr", "ndcg10") and not after["case"]["relevant"]:
                    continue
                a, b = before["score"][metric], after["score"][metric]
                if a is not None and b is not None:
                    families[after["case"]["family"]].append(float(b) - float(a))
            metrics[metric] = bootstrap(families)
        groups[f"{status}:{category}"] = {"paired_queries": len(pairs), "metrics": metrics}
    old_p95, new_p95 = old_summary["latency_ms"]["p95"], new_summary["latency_ms"]["p95"]
    return {"schema": 1, "mode": mode, "baseline_build": old_manifest["build"], "candidate_build": new_manifest["build"],
            "groups": groups, "changes": changes,
            "manual_regressions": [c for c in changes if c["label_status"] == "manual" and not c["after_passed"]],
            "latency_ms": {"baseline_p95": old_p95, "candidate_p95": new_p95,
                            "relative_p95_change": new_p95 / old_p95 - 1 if old_p95 else None},
            "rss": "compare separate resource logs; peak evaluator RSS is not steady-state server RSS",
            "interpretation": "candidate/legacy labels are exploratory; bootstrap resamples families, not retries"}


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("baseline", type=Path)
    ap.add_argument("candidate", type=Path)
    ap.add_argument("--mode", choices=("ranker", "corpus"), default="ranker")
    ap.add_argument("--out", type=Path, required=True)
    args = ap.parse_args()
    args.out.write_text(json.dumps(compare(args.baseline, args.candidate, args.mode), indent=2) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
