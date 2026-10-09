# Teaching a small model to search like a big one

A big model answers training questions with Plumb's tools, and a small model
(Qwen3-4B) is trained on the conversations it got right. The small model
learns the big one's habits: when to look something up, which tool to call,
how to word a query Plumb answers well, and when to stop. Then bench.py
compares the two small models on questions neither saw in training.

1. Questions. The training questions come from the same sets as the test
   ones but never overlap them:

   ```sh
   python3 eval/llm/prepare.py --out-dir eval/llm/data --sample 500 --train 1000
   ```

2. Teacher conversations, here with DeepSeek's API (MIT-licensed models;
   DeepSeek allows training other models on their output). The key is read
   from the environment variable named by `--api-key-env`, and `plumb mcp`
   asks plumbsearch.org:

   ```sh
   for set in simpleqa hotpotqa; do
     python3 eval/llm/bench.py --server https://api.deepseek.com --model deepseek-reasoner \
         --api-key-env DEEPSEEK_API_KEY --max-tokens 8000 \
         --questions eval/llm/data/$set-train.jsonl --mode plumb \
         --plumb "target/release/plumb mcp" \
         --out out/deepseek-$set-train.jsonl --transcripts out/deepseek-$set-transcripts.jsonl
   done
   ```

3. Training data: the right answers only, with the teacher's reasoning kept
   as `<think>` blocks:

   ```sh
   python3 eval/llm/to_sft.py out/deepseek-*-transcripts.jsonl --out out/sft.jsonl
   ```

4. Train a LoRA on Qwen3-4B with any trainer that reads `{"messages",
   "tools"}` lines, such as `mlx_lm.lora` on a Mac or TRL's `SFTTrainer` on a
   GPU, then serve the merged model with llama-server.

5. Ask bench.py's test sets (step 3 of README.md) with the base model, the
   trained one and the teacher, and compare right answers and right answers
   per 1,000 tokens.
