#!/usr/bin/env python3
"""One sentence about each site `plumb summaries pick` wrote, from Claude.

    plumb summaries pick --records records.jsonl --top 10000 --out picks.jsonl
    python3 tools/summaries/summarize.py --picks picks.jsonl --out summaries.jsonl
    plumb summaries apply --summaries summaries.jsonl --records records.jsonl \
        --out records-summaries.jsonl

The requests go through the Message Batches API (half price; most batches
end within the hour). `--direct N` instead asks for the first N sites one
at a time and prints them, to read a sample before paying for the rest.
Sites already in `--out` are skipped, so a stopped run picks up where it
left off. The key is read from `--key-file` (default
~/.config/anthropic/api-key), else ANTHROPIC_API_KEY or an `ant auth login`
profile; it is never printed.
"""

import argparse
import json
import os
import sys
import time

import anthropic
from anthropic.types.message_create_params import MessageCreateParamsNonStreaming
from anthropic.types.messages.batch_create_params import Request

MODEL = "claude-haiku-5-5"
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


def params(pick):
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
    parser.add_argument("--key-file", default="~/.config/anthropic/api-key",
                        help="file holding the API key, used when it exists")
    args = parser.parse_args()

    picks = read_jsonl(args.picks)
    done = {line["domain"] for line in read_jsonl(args.out)}
    todo = [p for p in picks if p["domain"] not in done]
    print(f"{len(picks)} sites picked, {len(done)} already summarized, {len(todo)} to go",
          file=sys.stderr)
    key_file = os.path.expanduser(args.key_file)
    if os.path.exists(key_file):
        with open(key_file, encoding="utf-8") as f:
            client = anthropic.Anthropic(api_key=f.read().strip())
    else:
        client = anthropic.Anthropic()
    usage = [0, 0]

    with open(args.out, "a", encoding="utf-8") as out:
        def keep(domain, summary):
            out.write(json.dumps({"domain": domain, "summary": summary}) + "\n")
            out.flush()

        if args.direct:
            for pick in todo[: args.direct]:
                message = client.messages.create(**params(pick))
                usage[0] += message.usage.input_tokens
                usage[1] += message.usage.output_tokens
                summary = text_of(message)
                print(f"{pick['domain']}\t{summary}")
                keep(pick["domain"], summary)
        else:
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
    print(f"{len(lines)} sites in {args.out}, {known} with a summary; this run used "
          f"{usage[0]:,} input and {usage[1]:,} output tokens", file=sys.stderr)


if __name__ == "__main__":
    main()
