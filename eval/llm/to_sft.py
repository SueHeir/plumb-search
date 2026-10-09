#!/usr/bin/env python3
"""Turns the conversations bench.py wrote with --transcripts into training
data for a small model: only questions the big model got right, the shortest
right conversation when a question was asked more than once, and the big
model's reasoning kept as <think>...</think> before each reply, the way
Qwen3 writes its own.

The output is JSON lines of {"messages", "tools"}, which mlx-lm's LoRA
trainer and TRL's SFTTrainer both read.

  python3 eval/llm/to_sft.py out/deepseek-*-transcripts.jsonl --out out/sft.jsonl
"""

import argparse
import json


def size(conversation):
    return sum(len(m.get("content") or "") + len(m.get("reasoning_content") or "")
               for m in conversation["messages"])


def student(message):
    message = dict(message)
    thought = message.pop("reasoning_content", "")
    if message["role"] == "assistant":
        message["content"] = f"<think>\n{thought.strip()}\n</think>\n\n{message.get('content') or ''}"
    return message


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("transcripts", nargs="+", help="files bench.py wrote with --transcripts")
    parser.add_argument("--out", required=True, help="training file (JSON lines)")
    args = parser.parse_args()

    best = {}
    seen = 0
    for path in args.transcripts:
        with open(path) as f:
            for line in f:
                conversation = json.loads(line)
                seen += 1
                if not conversation["right"]:
                    continue
                kept = best.get(conversation["id"])
                if kept is None or size(conversation) < size(kept):
                    best[conversation["id"]] = conversation
    with open(args.out, "w") as out:
        for conversation in best.values():
            row = {"messages": [student(m) for m in conversation["messages"]]}
            if conversation.get("tools"):
                row["tools"] = conversation["tools"]
            out.write(json.dumps(row) + "\n")
    print(f"{len(best)} questions kept of {seen} conversations, in {args.out}")


if __name__ == "__main__":
    main()
