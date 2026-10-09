#!/usr/bin/env python3
"""One sentence about each site `plumb summaries pick` wrote, from Claude.

    plumb summaries pick --records records.jsonl --top 10000 --out picks.jsonl
    python3 tools/summaries/summarize.py --picks picks.jsonl --out summaries.jsonl
    plumb summaries apply --summaries summaries.jsonl --records records.jsonl \
        --out records-summaries.jsonl

By default the sites go to Claude Code (`claude -p`, signed in with a
Claude subscription), `--per-call` sites at a time, `--workers` calls at
once: no API key needed. `--backend api` uses the Message Batches API
instead, with a key from `--key-file` (default ~/.config/anthropic/api-key),
ANTHROPIC_API_KEY or an `ant auth login` profile; the key is never printed.
`--direct N` asks for only the first N sites and prints them, to read a
sample first. Sites already in `--out` are skipped, so a stopped run picks
up where it left off.
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor

MODEL = "claude-haiku-5-5"
CLI_MODEL = "haiku"
MAX_TOKENS = 200
BATCH_SIZE = 10_000

SYSTEM = """You write one-sentence descriptions of websites for a search engine's index.
People will find a site by describing it rather than naming it ("cheap flights",
"recipes", "job search", "car parts near me"), so say plainly what the site is and
what people use it for, in the everyday words they would search with.

Rules:
- One sentence, at most 30 words, starting with the site's name.
- Facts only: no praise, no opinions, no numbers or dates that may change.
- Only describe a website you actually know. If you do not know this site, answer
  exactly UNKNOWN. Never guess what a site is from its domain name or the hints."""


def prompt(pick):
    lines = [f"Domain: {pick['domain']}"]
    if pick.get("title"):
        lines.append(f"Homepage title: {pick['title']}")
    if pick.get("aliases"):
        lines.append("Also known as: " + "; ".join(pick["aliases"]))
    if pick.get("link_texts"):
        lines.append("Other sites link to it with: " + "; ".join(pick["link_texts"]))
    if pick.get("kinds"):
        lines.append("Kind of organization: " + "; ".join(pick["kinds"]))
    if pick.get("country"):
        lines.append(f"Country: {pick['country']}")
    return "\n".join(lines)


CLI_SYSTEM = SYSTEM + """

You get several sites, one JSON object per line. Answer with exactly one JSON
object per site, one per line and nothing else: {"domain": ..., "summary": ...},
where summary is the sentence or "UNKNOWN"."""


def cli_batch(picks, workdir):
    """Summaries of `picks` from one `claude -p` call: {domain: summary}."""
    lines = "\n".join(json.dumps({"site": prompt(p)}) for p in picks)
    run = subprocess.run(
        ["claude", "-p", "--model", CLI_MODEL, "--output-format", "json",
         "--system-prompt", CLI_SYSTEM, "--tools", "", "--max-turns", "1",
         "--setting-sources", "", "--strict-mcp-config", "--no-session-persistence"],
        input=lines, capture_output=True, text=True, cwd=workdir, timeout=600,
    )
    if run.returncode != 0:
        print(f"  claude failed ({run.returncode}): {run.stderr.strip()[:300]}", file=sys.stderr)
        return {}
    result = json.loads(run.stdout).get("result", "")
    wanted = {p["domain"] for p in picks}
    found = {}
    for line in result.splitlines():
        line = line.strip().strip("`")
        if not line.startswith("{"):
            continue
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue
        if item.get("domain") in wanted and isinstance(item.get("summary"), str):
            found[item["domain"]] = item["summary"].strip() or None
    return found


def params(pick):
    from anthropic.types.message_create_params import MessageCreateParamsNonStreaming

    return MessageCreateParamsNonStreaming(
        model=MODEL,
        max_tokens=MAX_TOKENS,
        # A one-line description needs no reasoning; thinking off keeps it cheap.
        thinking={"type": "disabled"},
        system=SYSTEM,
        messages=[{"role": "user", "content": prompt(pick)}],
    )


def text_of(message):
    if message.stop_reason not in ("end_turn", "stop_sequence"):
        return None
    return next((b.text for b in message.content if b.type == "text"), "").strip() or None


def read_jsonl(path):
    try:
        with open(path, encoding="utf-8") as f:
            return [json.loads(line) for line in f if line.strip()]
    except FileNotFoundError:
        return []


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--picks", required=True, help="file `plumb summaries pick` wrote")
    parser.add_argument("--out", required=True, help="summaries file, appended to")
    parser.add_argument("--direct", type=int, metavar="N",
                        help="ask for the first N sites one at a time and print them")
    parser.add_argument("--backend", choices=["cli", "api"], default="cli",
                        help="Claude Code with a subscription (cli) or the API with a key (api)")
    parser.add_argument("--per-call", type=int, default=50,
                        help="cli: sites in one claude call")
    parser.add_argument("--workers", type=int, default=4, help="cli: claude calls at once")
    parser.add_argument("--key-file", default="~/.config/anthropic/api-key",
                        help="api: file holding the API key, used when it exists")
    args = parser.parse_args()

    picks = read_jsonl(args.picks)
    done = {line["domain"] for line in read_jsonl(args.out)}
    todo = [p for p in picks if p["domain"] not in done]
    print(f"{len(picks)} sites picked, {len(done)} already summarized, {len(todo)} to go",
          file=sys.stderr)
    if args.direct:
        todo = todo[: args.direct]
    usage = [0, 0]

    with open(args.out, "a", encoding="utf-8") as out:
        def keep(domain, summary):
            out.write(json.dumps({"domain": domain, "summary": summary}) + "\n")
            out.flush()

        if args.backend == "cli":
            chunks = [todo[i: i + args.per_call] for i in range(0, len(todo), args.per_call)]
            # An empty folder, so Claude Code reads no project files or settings.
            with tempfile.TemporaryDirectory() as workdir, \
                    ThreadPoolExecutor(args.workers) as pool:
                calls = [pool.submit(cli_batch, chunk, workdir) for chunk in chunks]
                for n, (chunk, call) in enumerate(zip(chunks, calls), 1):
                    found = call.result()
                    for pick in chunk:
                        # A site the answer left out is not kept, so a second run asks again.
                        if pick["domain"] in found:
                            keep(pick["domain"], found[pick["domain"]])
                            if args.direct:
                                print(f"{pick['domain']}\t{found[pick['domain']]}")
                    print(f"  call {n} of {len(chunks)}: {len(found)} of {len(chunk)} answered",
                          file=sys.stderr)
            client = None
        else:
            import anthropic

            key_file = os.path.expanduser(args.key_file)
            if os.path.exists(key_file):
                with open(key_file, encoding="utf-8") as f:
                    client = anthropic.Anthropic(api_key=f.read().strip())
            else:
                client = anthropic.Anthropic()

        if client and args.direct:
            for pick in todo:
                message = client.messages.create(**params(pick))
                usage[0] += message.usage.input_tokens
                usage[1] += message.usage.output_tokens
                summary = text_of(message)
                print(f"{pick['domain']}\t{summary}")
                keep(pick["domain"], summary)
        elif client:
            from anthropic.types.messages.batch_create_params import Request

            by_id = {}
            for start in range(0, len(todo), BATCH_SIZE):
                chunk = todo[start: start + BATCH_SIZE]
                requests = []
                for i, pick in enumerate(chunk):
                    custom_id = f"s{start + i}"
                    by_id[custom_id] = pick["domain"]
                    requests.append(Request(custom_id=custom_id, params=params(pick)))
                batch = client.messages.batches.create(requests=requests)
                print(f"batch {batch.id}: {len(requests)} requests", file=sys.stderr)
                while batch.processing_status != "ended":
                    time.sleep(30)
                    batch = client.messages.batches.retrieve(batch.id)
                    counts = batch.request_counts
                    print(f"  {counts.succeeded} done, {counts.processing} processing, "
                          f"{counts.errored} errored", file=sys.stderr)
                for result in client.messages.batches.results(batch.id):
                    if result.result.type != "succeeded":
                        # Left out of --out, so a second run asks again.
                        continue
                    message = result.result.message
                    usage[0] += message.usage.input_tokens
                    usage[1] += message.usage.output_tokens
                    keep(by_id[result.custom_id], text_of(message))

    lines = read_jsonl(args.out)
    known = sum(1 for line in lines if line["summary"] and line["summary"] != "UNKNOWN")
    print(f"{len(lines)} sites in {args.out}, {known} with a summary", file=sys.stderr)
    if args.backend == "api":
        print(f"this run used {usage[0]:,} input and {usage[1]:,} output tokens", file=sys.stderr)


if __name__ == "__main__":
    main()
