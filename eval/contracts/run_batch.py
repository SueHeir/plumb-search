#!/usr/bin/env python3
"""One bounded combined evaluation after integration; no model calls or builds.

Uses an already-built binary and immutable corpus. Writes only --out-dir.
An optional scratch MCP target exercises real identity tools; it must be
loopback, and its embedded revision must match the executable. This script
never starts/stops production, refreshes data, or resumes an LLM runner.
"""
import argparse
import json
from pathlib import Path
import subprocess
import sys
import time
import urllib.parse

ROOT = Path(__file__).resolve().parents[2]
FIXED_TIME = 1791586800  # Explicitly fixed Unix clock, not host time.


def run(command, out, seconds, *, cwd=ROOT):
    started = time.monotonic()
    with open(out, "w", encoding="utf-8") as log:
        # A process group prevents a timed-out /usr/bin/time wrapper leaving
        # its evaluator alive. No build or external model process is started.
        process = subprocess.Popen([str(c) for c in command], cwd=cwd, stdout=log,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        try:
            code = process.wait(timeout=seconds)
        except subprocess.TimeoutExpired:
            import os
            import signal
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            raise RuntimeError(f"evaluation exceeded {seconds}s; see {out}")
    if code:
        raise RuntimeError(f"command exited {code}; see {out}")
    return {"command": [str(c) for c in command], "elapsed_seconds": time.monotonic() - started,
            "log": str(out)}


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--plumb", type=Path, required=True)
    ap.add_argument("--out-dir", type=Path, required=True)
    ap.add_argument("--compare", type=Path, help="baseline core.jsonl; validate fixed-corpus pairing")
    ap.add_argument("--index", type=Path)
    ap.add_argument("--pages", type=Path, action="append", default=[])
    ap.add_argument("--pages-top", type=int, default=200000, help="bounded records per input set; same for baseline/candidate")
    ap.add_argument("--queries", type=Path, action="append", default=[])
    ap.add_argument("--acceptance", type=Path, action="append", default=[])
    ap.add_argument("--pages-cache", type=Path, help="derived scratch page index cache")
    ap.add_argument("--rank", default="{}")
    ap.add_argument("--model", type=Path)
    ap.add_argument("--vectors", type=Path)
    ap.add_argument("--query-instruction", choices=("off", "on", "mix", "min", "split"), default="split")
    ap.add_argument("--eval-time", type=int, default=FIXED_TIME)
    ap.add_argument("--seconds", type=int, default=1800)
    ap.add_argument("--fixture-baseline", action="store_true")
    ap.add_argument("--mcp", help="optional already-isolated scratch candidate on loopback")
    ap.add_argument("--identity-seconds", type=int, default=600)
    ap.add_argument("--identity-calls", type=int, default=400)
    args = ap.parse_args()
    if bool(args.model) != bool(args.vectors):
        ap.error("--model and --vectors must be supplied together")
    if args.pages_top <= 0 or args.seconds <= 0 or args.identity_seconds <= 0 or args.identity_calls <= 0:
        ap.error("budgets must be positive")
    if args.mcp and urllib.parse.urlsplit(args.mcp).hostname not in ("127.0.0.1", "localhost", "::1"):
        ap.error("--mcp must name a loopback scratch candidate, never production")
    # Refuse overwriting a previous run: every batch has complete provenance.
    args.out_dir = args.out_dir.resolve()
    args.out_dir.mkdir(parents=True, exist_ok=False)
    args.plumb = args.plumb.resolve()
    build = json.loads(subprocess.check_output([str(args.plumb), "build-info"], text=True, timeout=10))
    plan = {"schema": 1, "build": build, "fixed_time": args.eval_time,
            "features": {"findings": False, "personalization": False, "plugins": False,
                         "external_results": False}, "budgets": {"core_seconds": args.seconds,
                         "identity_seconds": args.identity_seconds, "identity_calls": args.identity_calls},
            "steps": [], "complete": False}
    manifest = args.out_dir / "batch.json"
    try:
        if args.fixture_baseline:
            if args.index or args.pages or args.model or args.queries or args.acceptance:
                ap.error("fixture baseline uses only repository fixtures and no model")
            args.index = args.out_dir / "index"
            records = args.out_dir / "records.jsonl"
            plan["steps"].append(run([args.plumb, "ingest", "--tranco", ROOT / "fixtures/tranco.csv",
                "--cc-ranks", ROOT / "fixtures/cc-domain-ranks.txt", "--wat", ROOT / "fixtures/sample.wat",
                "--wikidata", ROOT / "fixtures/wikidata-official-sites.tsv", "--out", records],
                args.out_dir / "ingest.log", 60))
            plan["steps"].append(run([args.plumb, "index", "--records", records, "--index", args.index],
                                     args.out_dir / "index.log", 60))
            args.acceptance = [ROOT / "eval/contracts/offline.jsonl"]
        elif not args.index:
            ap.error("--index or --fixture-baseline is required")
        elif not args.acceptance and not args.queries:
            args.acceptance = [ROOT / "eval/contracts/audit.jsonl", ROOT / "eval/contracts/family_heldout.jsonl"]
        core = [args.plumb, "eval", "--index", args.index, "--limit", "100", "--rank", args.rank,
                "--eval-time", args.eval_time, "--pages-top", args.pages_top, "--report", args.out_dir / "core.jsonl"]
        for flag, paths in (("--pages", args.pages), ("--queries", args.queries), ("--acceptance", args.acceptance)):
            for path in paths:
                core.extend([flag, path.resolve()])
        if args.pages_cache:
            core.extend(["--pages-cache", args.pages_cache.resolve()])
        if args.model:
            core.extend(["--model", args.model.resolve(), "--vectors", args.vectors.resolve(),
                         "--query-instruction", args.query_instruction])
        # Peak process RSS lives in the separate log. It is not steady-state
        # node RSS, and includes diagnostics/index loading. Corpus byte counts
        # are in core.jsonl; both are kept distinct from per-query latency.
        time_flags = ["-l"] if sys.platform == "darwin" else ["-v"]
        plan["steps"].append(run(["/usr/bin/time", *time_flags, *core], args.out_dir / "core-resource.log", args.seconds))
        if args.mcp:
            if build["revision"] == "unknown" or build.get("dirty") is not False:
                raise RuntimeError("identity deployment comparison requires a clean known revision")
            identity = [sys.executable, ROOT / "eval/official_site/run.py", "--mcp", args.mcp,
                        "--require-revision", build["revision"], "--out", args.out_dir / "identity.jsonl",
                        "--max-calls", args.identity_calls, "--time-budget", args.identity_seconds,
                        ROOT / "eval/contracts/audit.jsonl", ROOT / "eval/official_site/queries.tsv",
                        ROOT / "eval/official_site/heldout.tsv"]
            plan["steps"].append(run(identity, args.out_dir / "identity-resource.log", args.identity_seconds + 15))
        if args.compare:
            from compare import compare
            (args.out_dir / "comparison.json").write_text(json.dumps(compare(args.compare, args.out_dir / "core.jsonl"), indent=2) + "\n")
        plan["complete"] = True
    except Exception as err:
        plan["error"] = str(err)
        raise
    finally:
        manifest.write_text(json.dumps(plan, indent=2) + "\n")
    print(manifest)
    return 0


if __name__ == "__main__":
    sys.exit(main())
