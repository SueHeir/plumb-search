#!/usr/bin/env python3
"""Downloads the question sets bench.py asks and writes them as JSON lines
({"id", "question", "answers"}), a fixed sample of each.

  simpleqa.jsonl  OpenAI's SimpleQA: short questions with one right answer,
                  hard for models from memory (MIT licence)
  hotpotqa.jsonl  HotpotQA dev (distractor): questions that need two facts
                  joined (CC BY-SA 4.0)
  bamboogle.jsonl Bamboogle (Press et al., 2022): 125 two-hop questions a
                  search engine does not answer directly, from FlashRAG's copy

  python3 eval/llm/prepare.py --out-dir eval/llm/data --sample 500
  python3 eval/llm/prepare.py --sets bamboogle
"""

import argparse
import csv
import io
import json
import os
import random
import urllib.request

SIMPLEQA = "https://openaipublic.blob.core.windows.net/simple-evals/simple_qa_test_set.csv"
HOTPOTQA = "http://curtis.ml.cmu.edu/datasets/hotpot/hotpot_dev_distractor_v1.json"
BAMBOOGLE = "https://huggingface.co/datasets/RUC-NLPIR/FlashRAG_datasets/resolve/main/bamboogle/test.jsonl"


def fetch(url):
    with urllib.request.urlopen(url, timeout=600) as response:
        return response.read().decode()


def write(path, rows, sample):
    random.Random(7).shuffle(rows)
    rows = rows[:sample] if sample else rows
    with open(path, "w") as f:
        for row in rows:
            f.write(json.dumps(row) + "\n")
    print(f"{len(rows)} questions in {path}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out-dir", default="eval/llm/data")
    parser.add_argument("--sample", type=int, default=500, help="questions kept of each set (0: all)")
    parser.add_argument("--sets", default="simpleqa,hotpotqa,bamboogle", help="sets to download, comma-separated")
    args = parser.parse_args()
    sets = args.sets.split(",")
    os.makedirs(args.out_dir, exist_ok=True)

    if "simpleqa" in sets:
        rows = [
            {"id": f"simpleqa-{n}", "question": row["problem"], "answers": [row["answer"]]}
            for n, row in enumerate(csv.DictReader(io.StringIO(fetch(SIMPLEQA))))
        ]
        write(os.path.join(args.out_dir, "simpleqa.jsonl"), rows, args.sample)

    if "hotpotqa" in sets:
        rows = [
            {"id": f"hotpotqa-{item['_id']}", "question": item["question"], "answers": [item["answer"]]}
            for item in json.loads(fetch(HOTPOTQA))
            if item["answer"].lower() not in ("yes", "no")
        ]
        write(os.path.join(args.out_dir, "hotpotqa.jsonl"), rows, args.sample)

    if "bamboogle" in sets:
        rows = [
            {"id": f"bamboogle-{item['id']}", "question": item["question"], "answers": item["golden_answers"]}
            for item in map(json.loads, fetch(BAMBOOGLE).splitlines())
        ]
        write(os.path.join(args.out_dir, "bamboogle.jsonl"), rows, args.sample)


if __name__ == "__main__":
    main()
