#!/usr/bin/env python3
"""Bounded official_site/lookalike/facts contracts; full RPCs, never LLM grades.

TSV callers retain their old format. JSONL uses plumb eval's family contracts.
--responses re-scores saved full responses offline. --out saves complete request,
response, target build, response bytes and timing, so truncation cannot hide a
bad decision. A JSON summary separates labels and false identity accusations.
"""
import argparse
from collections import defaultdict
import hashlib
import json
from pathlib import Path
import sys
import time
import urllib.parse
import urllib.request


def rpc(mcp, method, params, timeout=10):
    request = {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
    body = json.dumps(request).encode()
    req = urllib.request.Request(mcp, body, {"Content-Type": "application/json",
                                            "Accept": "application/json"})
    start = time.monotonic()
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        raw = resp.read()
    reply = json.loads(raw)
    return request, reply, (time.monotonic() - start) * 1000, len(raw)


def ask(mcp, name):
    """Compatibility for callers importing the previous helper."""
    _, reply, _, _ = rpc(mcp, "tools/call", {
        "name": "official_site", "arguments": {"name": name}}, 60)
    return reply.get("result", {}).get("structuredContent") or {}


def host(answer):
    h = urllib.parse.urlsplit(answer.get("url") or "").hostname or ""
    return h.lower().removeprefix("www.")


def domain_matches(want, got):
    want = want.rstrip("*")
    want = (urllib.parse.urlsplit(want).hostname if "://" in want else want.split("/", 1)[0]) or ""
    want = want.lower().removeprefix("www.")
    got = got.lower().removeprefix("www.")
    # Preserve the existing TSV suite's distinction: a specifically named
    # subdomain takes itself, while a registrable label takes descendants.
    descendants = want.count(".") <= 1 or want.endswith((".co.uk", ".gov.uk"))
    return got == want or descendants and got.endswith("." + want)


def right(answer, expected):
    if not answer.get("found"):
        return False
    got = {host(answer), (answer.get("domain") or "").lower()}
    return any(domain_matches(want, g) for want in expected for g in got)


def read(path):
    """Old TSV iterator, also used by downstream scripts."""
    for line in Path(path).read_text(encoding="utf-8-sig").splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        name, sep, answers = line.partition("\t")
        if not sep or not name.strip() or not answers.strip():
            raise ValueError(f"invalid TSV line in {path}")
        yield name, [a.strip() for a in answers.split(",") if a.strip()]


def cases(path):
    if Path(path).suffix == ".jsonl":
        for line in Path(path).read_text(encoding="utf-8-sig").splitlines():
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            case = json.loads(line)
            if case.get("tool", "search") in ("official_site", "check_lookalike", "facts"):
                yield case
    else:
        for i, (name, expected) in enumerate(read(path)):
            yield {"id": f"{path}:{i}", "family": name, "category": "official_site",
                   "query": name, "tool": "official_site", "label_status": "legacy",
                   "relevant": [{"identity": e, "grade": 1} for e in expected],
                   "expect": {}, "negatives": []}


def arguments(case):
    args = dict(case.get("arguments") or {})
    if not args:
        args = {"url" if case["tool"] == "check_lookalike" else "name": case["query"]}
    options = case.get("options") or {}
    for key, value in (("country", options.get("country")),
                       ("lang", options.get("language")),
                       ("only", options.get("only_country"))):
        if value:
            args.setdefault(key, value)
    return args


def judge(case, reply):
    answer = reply.get("result", {}).get("structuredContent") or {}
    violations = []
    if "error" in reply or reply.get("result", {}).get("isError"):
        violations.append("RPC error")
    expected = [r["identity"] for r in case.get("relevant", [])]
    expect = case.get("expect", {})
    if case["tool"] == "official_site":
        good = right(answer, expected) if expected else not answer.get("found")
        if not good:
            violations.append("wrong official destination")
    elif case["tool"] == "check_lookalike":
        good = answer.get("verdict") == expect.get("verdict")
        if not good:
            violations.append("wrong affiliation verdict")
    else:
        good = bool(answer.get("found"))
        if not good and not expect.get("abstain"):
            violations.append("facts unresolved")
    if expect.get("abstain") and answer.get("found"):
        violations.append("expected abstention")
    if expect.get("site") and not domain_matches(expect["site"], host(answer)):
        violations.append("wrong site contract")
    # Facts payloads carry property/item provenance; inspect all answer values,
    # never the query or the case label, and retain the complete RPC alongside.
    text = json.dumps(answer, ensure_ascii=False).lower()
    for expected_text in expect.get("answer_contains", []) + expect.get("date_contains", []):
        if expected_text.lower() not in text:
            violations.append(f"answer missing {expected_text!r}")
    for excluded in expect.get("answer_excludes", []):
        if excluded.lower() in text:
            violations.append(f"answer contains excluded {excluded!r}")
    for negative in case.get("negatives", []):
        if domain_matches(negative["identity"], host(answer)):
            violations.append(f"negative destination {negative['identity']}")
    passed = not violations
    return {"right": passed, "violations": violations,
            "false_official": case["tool"] == "official_site" and not passed and
                bool(answer.get("found")) and answer.get("confidence") in ("high", "medium"),
            "false_lookalike": case["tool"] == "check_lookalike" and
                expect.get("verdict") != "lookalike" and answer.get("verdict") == "lookalike",
            "domain": answer.get("domain"), "url": answer.get("url"),
            "confidence": answer.get("confidence"), "answer": answer}


def findings_off(mcp):
    parts = urllib.parse.urlsplit(mcp)
    query = dict(urllib.parse.parse_qsl(parts.query))
    query["findings"] = "off"
    return urllib.parse.urlunsplit(parts._replace(query=urllib.parse.urlencode(query)))


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    mode = ap.add_mutually_exclusive_group(required=True)
    mode.add_argument("--mcp")
    mode.add_argument("--responses", help="saved complete RPC JSONL; no network")
    ap.add_argument("--out", required=True)
    ap.add_argument("--compare")
    ap.add_argument("--timeout", type=float, default=10)
    ap.add_argument("--max-calls", type=int, default=400)
    ap.add_argument("--time-budget", type=float, default=600)
    ap.add_argument("--require-revision", help="refuse a node with a different embedded SHA")
    ap.add_argument("files", nargs="+")
    args = ap.parse_args()
    if args.max_calls <= 0 or args.time_budget <= 0 or args.timeout <= 0:
        ap.error("budgets must be positive")
    saved = {}
    if args.responses:
        for line in Path(args.responses).read_text().splitlines():
            row = json.loads(line)
            if "case" in row:
                saved[row["case"]["id"]] = row
    old = {}
    if args.compare:
        for line in Path(args.compare).read_text().splitlines():
            row = json.loads(line)
            if "name" in row:
                old[(row["file"], row["name"])] = row
    selected = [(path, c) for path in args.files for c in cases(path)]
    if len({c["id"] for _, c in selected}) != len(selected):
        ap.error("duplicate case IDs")
    manifest = {"type": "manifest", "schema": 1, "mode": "offline-replay" if args.responses else "mcp",
                "target": "local-mcp" if args.mcp else "saved-responses",
                "findings": "off requested; scratch serve required for isolation",
                "suite_sha256": {p: hashlib.sha256(Path(p).read_bytes()).hexdigest() for p in args.files},
                "budgets": {"calls": args.max_calls, "seconds": args.time_budget, "timeout": args.timeout}}
    target = findings_off(args.mcp) if args.mcp else None
    if target:
        request, reply, _, _ = rpc(target, "initialize", {"protocolVersion": "2025-03-26",
                                  "capabilities": {}, "clientInfo": {"name": "plumb-contracts", "version": "1"}}, args.timeout)
        manifest["initialize_request"] = request
        manifest["initialize_response"] = reply
        build = reply.get("result", {}).get("_meta", {}).get("plumb.build", {})
        if args.require_revision and build.get("revision") != args.require_revision:
            ap.error("node embedded revision does not match --require-revision")
    groups = defaultdict(list)
    start = time.monotonic()
    incomplete = False
    with open(args.out, "w", encoding="utf-8") as out:
        out.write(json.dumps(manifest, ensure_ascii=False) + "\n")
        for n, (path, case) in enumerate(selected):
            if n >= args.max_calls or time.monotonic() - start >= args.time_budget:
                incomplete = True
                break
            request = {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                       "params": {"name": case["tool"], "arguments": arguments(case)}}
            try:
                if target:
                    request, reply, elapsed, size = rpc(target, "tools/call", request["params"], args.timeout)
                else:
                    replay = saved[case["id"]]
                    if replay["request"] != request:
                        raise ValueError("replay request differs from fixture arguments")
                    reply, elapsed, size = replay["response"], None, replay["response_bytes"]
            except Exception as err:
                reply, elapsed, size = {"error": {"message": str(err)}}, None, 0
            judgement = judge(case, reply)
            row = {"type": "query", "file": path, "name": case["query"], "case": case,
                   "request": request, "response": reply, "latency_ms": elapsed,
                   "response_bytes": size, **judgement}
            out.write(json.dumps(row, ensure_ascii=False) + "\n")
            out.flush()
            groups[(case["label_status"], case["category"])].append(row)
            before = old.get((path, case["query"]))
            if before is not None and before["right"] != row["right"]:
                print(f"  {'FIXED' if row['right'] else 'BROKE'} {case['query']!r}")
        summary = {"type": "summary", "incomplete": incomplete, "selected": len(selected), "groups": {}}
        for (status, category), rows in groups.items():
            families = {r["case"]["family"] for r in rows}
            summary["groups"][f"{status}:{category}"] = {
                "queries": len(rows), "families": len(families),
                "families_all_passed": sum(all(r["right"] for r in rows if r["case"]["family"] == f) for f in families),
                **{k: sum(r[k] for r in rows) for k in ("right", "false_official", "false_lookalike")}}
        out.write(json.dumps(summary) + "\n")
    print(json.dumps(summary))
    return 2 if incomplete else 0


if __name__ == "__main__":
    sys.exit(main())
