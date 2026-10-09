#!/usr/bin/env python3
"""Checks what a node's official_site tool answers for test names.

    python3 eval/official_site/run.py --mcp http://127.0.0.1:8090/mcp \
        eval/official_site/queries.tsv eval/brand_queries.tsv eval/ai_queries.tsv \
        --out answers.jsonl

Each file holds `name<TAB>answer[,another]` lines (# and blank lines
skipped). An answer that is a registrable domain takes its subdomains too;
one that is a subdomain takes only itself and its www. host. Prints, per
file, how many names got a right site, and how many wrong ones were said
with high or medium confidence. --out keeps every answer, for comparing two
builds line by line (--compare old.jsonl).
"""
import argparse
import json
import sys
import urllib.request


def ask(mcp, name):
    body = json.dumps({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "official_site", "arguments": {"name": name}},
    }).encode()
    req = urllib.request.Request(mcp, body, {"Content-Type": "application/json",
                                             "Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as resp:
        reply = json.load(resp)
    return reply.get("result", {}).get("structuredContent") or {}


def host(answer):
    url = answer.get("url") or ""
    h = url.split("://", 1)[-1].split("/", 1)[0].lower()
    return h[4:] if h.startswith("www.") else h


def right(answer, expected):
    if not answer.get("found"):
        return False
    got = {host(answer), (answer.get("domain") or "").lower()}
    for want in expected:
        want = want.rstrip("*").split("://", 1)[-1].split("/", 1)[0].lower()
        want = want[4:] if want.startswith("www.") else want
        for g in got:
            # A subdomain answer counts only for itself; a registrable one
            # for its subdomains too.
            if g == want or (want.count(".") <= 1 or want.endswith((".co.uk", ".gov.uk"))) and g.endswith("." + want):
                return True
    return False


def read(path):
    for line in open(path, encoding="utf-8"):
        line = line.rstrip("\n")
        if not line.strip() or line.startswith("#"):
            continue
        name, _, answers = line.partition("\t")
        yield name, [a.strip() for a in answers.split(",") if a.strip()]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mcp", required=True)
    ap.add_argument("--out")
    ap.add_argument("--compare")
    ap.add_argument("files", nargs="+")
    args = ap.parse_args()
    old = {}
    if args.compare:
        for line in open(args.compare):
            row = json.loads(line)
            old[(row["file"], row["name"])] = row
    out = open(args.out, "w") if args.out else None
    for path in args.files:
        n = ok = sure_wrong = 0
        for name, expected in read(path):
            try:
                answer = ask(args.mcp, name)
            except Exception as err:  # a failed call counts as wrong
                answer = {"error": str(err)}
            good = right(answer, expected)
            conf = answer.get("confidence")
            n += 1
            ok += good
            sure_wrong += (not good) and conf in ("high", "medium")
            row = {"file": path, "name": name, "expected": expected, "right": good,
                   "domain": answer.get("domain"), "url": answer.get("url"),
                   "confidence": conf}
            if out:
                out.write(json.dumps(row) + "\n")
            before = old.get((path, name))
            if before is not None and before["right"] != good:
                mark = "FIXED" if good else "BROKE"
                print(f"  {mark} {name!r}: {before['url']} -> {row['url']} ({conf})")
        print(f"{path}: {ok}/{n} right, {sure_wrong} wrong said with high or medium confidence")
    return 0


if __name__ == "__main__":
    sys.exit(main())
