# Small models with Plumb

Does a small local model answer more questions right when it can look things up in Plumb, and how much context does that cost? These two scripts measure it, with the Python standard library only. For contributors working on the MCP tools or [local-model setups](../../docs/local-llms.md).

## Steps

1. **Get the questions.** [prepare.py](prepare.py) downloads SimpleQA, HotpotQA (dev, distractor) and Bamboogle, and writes a fixed sample of each as JSON lines (`{"id", "question", "answers"}`):

   ```sh
   python3 eval/llm/prepare.py --out-dir eval/llm/data --sample 500
   ```

   `--sample` is the questions kept of each set (0 keeps all; Bamboogle has 125), and `--sets` picks some of `simpleqa,hotpotqa,bamboogle`. `eval/llm/data/` is ignored by git.

2. **Start a model** behind an OpenAI-compatible server that can call tools, such as llama.cpp's:

   ```sh
   llama-server -m Qwen3-4B-Q4_K_M.gguf --jinja -c 32768 --port 8090
   ```

3. **Ask each set** in the modes you want to compare, with [bench.py](bench.py):

   ```sh
   python3 eval/llm/bench.py --server http://127.0.0.1:8090 --questions eval/llm/data/simpleqa.jsonl \
     --mode none --out out/qwen-simpleqa-none.jsonl
   python3 eval/llm/bench.py --server http://127.0.0.1:8090 --questions eval/llm/data/simpleqa.jsonl \
     --mode plumb --plumb "target/release/plumb mcp --node http://127.0.0.1:8080" \
     --out out/qwen-simpleqa-plumb.jsonl
   ```

| Mode | What the model gets |
| --- | --- |
| `none` | No tools: it answers from what it knows |
| `plumb` | Plumb's MCP tools, through the command `--plumb` names (default `plumb mcp`) |
| `budget` | As `plumb`, with each tool result cut to about `--budget` tokens (default 300) |
| `outline` | As `plumb`, plus `read_page`'s `outline` option, which the other modes leave out; compare it with `plumb` to see what reading a page's outline first saves |

The model never gets `report_finding` or a tool marked destructive.

## Options

| Option | Default | What it does |
| --- | --- | --- |
| `--rounds N` | 6 | Model turns that may call tools |
| `--max-tokens N` | 512 | Tokens a reply may take, thinking included; models that think first need more, such as 2048 |
| `--model NAME` | `local` | The model name sent to the server |
| `--limit N` | all | Ask only the first N questions |
| `--out FILE` | | Results, appended to: a stopped run picks up where it left off |

A model still calling tools after its last round, or whose reply has no answer in it, gets one more turn without tools asking it to answer (`forced` in the results).

## Results

Each run prints how many answers were right, the prompt tokens a question took (and in its last call), right answers per 1,000 prompt tokens, tool calls and seconds a question. Each results line keeps the question, the answer said, whether it was right, the token counts, the number of tool calls and the seconds taken.

An answer is right when it contains one of the question's answers after both are normalized (case, punctuation, articles). That is stricter than SimpleQA's own grader, a model, for answers in other words, and looser for answers that name several things: read a sample of the results before trusting a small difference.
