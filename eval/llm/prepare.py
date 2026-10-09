#!/usr/bin/env python3
"""Downloads the question sets bench.py asks and writes them as JSON lines
({"id", "question", "answers"}), a fixed sample of each.

  simpleqa.jsonl  OpenAI's SimpleQA: short questions with one right answer,
                  hard for models from memory (MIT licence)
  hotpotqa.jsonl  HotpotQA dev (distractor): questions that need two facts
                  joined (CC BY-SA 4.0)

With --train N it also writes N more questions of each set, none of them in
the sample, as simpleqa-train.jsonl and hotpotqa-train.jsonl: questions for
a big model to answer with Plumb so a small one can be trained on its
answers (see DISTILL.md) without seeing the questions it is tested on.

  python3 eval/llm/prepare.py --out-dir eval/llm/data --sample 500
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


def fetch(url):
    with urllib.request.urlopen(url, timeout=600) as response:
        return response.read().decode()


def write(path, rows, sample, train=0):
    random.Random(7).shuffle(rows)
    if train and sample:
        save(path.replace(".jsonl", "-train.jsonl"), rows[sample : sample + train])
    save(path, rows[:sample] if sample else rows)


def save(path, rows):
    with open(path, "w") as f:
        for row in rows:
            f.write(json.dumps(row) + "\n")
    print(f"{len(rows)} questions in {path}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out-dir", default="eval/llm/data")
    parser.add_argument("--sample", type=int, default=500, help="questions kept of each set (0: all)")
    parser.add_argument("--train", type=int, default=0,
                        help="also keep this many other questions of each set for training")
    args = parser.parse_args()
    os.makedirs(args.out_dir, exist_ok=True)

    rows = [
        {"id": f"simpleqa-{n}", "question": row["problem"], "answers": [row["answer"]]}
        for n, row in enumerate(csv.DictReader(io.StringIO(fetch(SIMPLEQA))))
    ]
    write(os.path.join(args.out_dir, "simpleqa.jsonl"), rows, args.sample, args.train)

    rows = [
        {"id": f"hotpotqa-{item['_id']}", "question": item["question"], "answers": [item["answer"]]}
        for item in json.loads(fetch(HOTPOTQA))
        if item["answer"].lower() not in ("yes", "no")
    ]
    write(os.path.join(args.out_dir, "hotpotqa.jsonl"), rows, args.sample, args.train)


if __name__ == "__main__":
    main()
