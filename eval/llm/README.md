# Small models with Plumb

Does a small local model answer more questions right when it can look things
up in Plumb, and how much context does that cost?

1. `python3 eval/llm/prepare.py --out-dir eval/llm/data` downloads SimpleQA
   and HotpotQA and keeps 500 questions of each.
2. Start a model with an OpenAI-compatible server that can call tools, for
   example `llama-server -m Qwen3-4B-Q4_K_M.gguf --jinja -c 32768 --port 8090`.
3. Ask each set three ways:

   ```sh
   python3 eval/llm/bench.py --server http://127.0.0.1:8090 --questions eval/llm/data/simpleqa.jsonl \
       --mode none --out out/qwen-simpleqa-none.jsonl
   python3 eval/llm/bench.py --server http://127.0.0.1:8090 --questions eval/llm/data/simpleqa.jsonl \
       --mode plumb --plumb "target/release/plumb mcp --node http://127.0.0.1:8080" \
       --out out/qwen-simpleqa-plumb.jsonl
   python3 eval/llm/bench.py ... --mode budget --budget 300 --out out/qwen-simpleqa-budget.jsonl
   python3 eval/llm/bench.py ... --mode outline --out out/qwen-simpleqa-outline.jsonl
   ```

   `outline` is `plumb` with `read_page`'s outline option, which the other
   modes leave out, so comparing it with `plumb` shows what reading a
   page's outline first saves.

Each run prints how many answers were right, the prompt tokens a question
took, right answers per 1,000 tokens, tool calls and time. Results files are
appended to, so a stopped run picks up where it left off.

An answer is right when it contains one of the question's answers after
normalizing case, punctuation and articles. That is stricter than SimpleQA's
own grader (a model) for answers in other words, and looser for answers that
name several things; read a sample of the results before trusting a small
difference.

DISTILL.md trains a small model on a big one's conversations with Plumb,
using `--transcripts`, `--api-key-env` and `--max-tokens` here and `to_sft.py`.
