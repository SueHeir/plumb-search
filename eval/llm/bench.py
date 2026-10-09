#!/usr/bin/env python3
"""Does Plumb make a small model know more, for little context?

Asks a local model (any OpenAI-compatible server, such as llama.cpp's
llama-server started with --jinja so it can call tools) the questions of a
questions file, in one of three ways:

  none     the model answers from what it knows
  plumb    the model may call Plumb's MCP tools (`plumb mcp`) a few times
  budget   as plumb, but each tool result is cut to --budget tokens
  outline  as plumb, with read_page's outline option, which plumb and budget
           leave out so the two can be compared

Each answer is graded by whether it contains one of the question's answers
(normalized: case, punctuation, articles), and every call's token counts are
summed, so the summary can say how many questions were right per 1,000
tokens of context.

Standard library only. Questions are JSON lines: {"id", "question",
"answers": [...]} (see prepare.py).

  python3 eval/llm/bench.py --server http://127.0.0.1:8090 \
      --questions simpleqa.jsonl --mode plumb \
      --plumb "target/release/plumb mcp --node http://127.0.0.1:8080" \
      --out results-qwen-plumb.jsonl
"""

import argparse
import json
import os
import re
import shlex
import string
import subprocess
import sys
import time
import urllib.request

SYSTEM = (
    "Answer the question with a short answer: a name, a date, a number or a few "
    "words. {tools}Finish with a line 'Answer: <answer>'."
)
TOOLS_NOTE = (
    "You can look things up with the tools; use them when you are not sure, "
    "and keep your lookups few. "
)
MAX_ROUNDS = 6


class Mcp:
    """A `plumb mcp` process, spoken to in JSON-RPC over stdin and stdout."""

    def __init__(self, command):
        self.proc = subprocess.Popen(
            shlex.split(command),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        self.next_id = 0
        self.ask("initialize", {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "plumb-bench", "version": "1"},
        })
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        self.tools = self.ask("tools/list", {})["tools"]

    def send(self, message):
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()

    def ask(self, method, params):
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("plumb mcp stopped")
            reply = json.loads(line)
            if reply.get("id") == self.next_id:
                if "error" in reply:
                    raise RuntimeError(reply["error"])
                return reply["result"]

    def call(self, name, arguments):
        try:
            result = self.ask("tools/call", {"name": name, "arguments": arguments})
        except Exception as err:  # the model sees the failure and goes on
            return f"error: {err}"
        return "\n".join(
            part.get("text", "") for part in result.get("content", []) if part.get("type") == "text"
        )

    def openai_tools(self, outline):
        return [
            {
                "type": "function",
                "function": {
                    "name": tool["name"],
                    "description": tool.get("description", "") if outline else without_outline(
                        tool.get("description", "")),
                    "parameters": tool.get("inputSchema", {"type": "object"}) if outline else {
                        **tool.get("inputSchema", {"type": "object"}),
                        "properties": {
                            name: schema
                            for name, schema in tool.get("inputSchema", {}).get("properties", {}).items()
                            if name != "outline"
                        },
                    },
                },
            }
            for tool in self.tools
            if not tool.get("annotations", {}).get("destructiveHint")
            and tool["name"] != "report_finding"
        ]


def without_outline(description):
    """A tool's description without its sentences about outlines."""
    sentences = re.split(r"(?<=\.) ", description)
    return " ".join(s for s in sentences if "outline" not in s)


def chat(args, messages, tools):
    body = {"model": args.model, "messages": messages, "temperature": 0, "max_tokens": args.max_tokens}
    if tools:
        body["tools"] = tools
    headers = {"Content-Type": "application/json"}
    if args.api_key_env:
        headers["Authorization"] = "Bearer " + os.environ[args.api_key_env]
    request = urllib.request.Request(
        args.server.rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers=headers,
    )
    with urllib.request.urlopen(request, timeout=600) as response:
        return json.load(response)


def normalize(text):
    text = text.lower()
    text = "".join(c for c in text if c not in string.punctuation)
    text = re.sub(r"\b(a|an|the)\b", " ", text)
    return " ".join(text.split())


def final_answer(text):
    found = re.findall(r"answer:\s*(.+)", text or "", flags=re.IGNORECASE)
    return (found[-1] if found else (text or "")).strip()


def is_right(answer, answers):
    said = normalize(answer)
    return bool(said) and any(normalize(a) and normalize(a) in said for a in answers)


def ask(question, args, mcp):
    tools = mcp.openai_tools(args.mode == "outline") if mcp else None
    messages = [
        {"role": "system", "content": SYSTEM.format(tools=TOOLS_NOTE if tools else "")},
        {"role": "user", "content": question["question"]},
    ]
    prompt_tokens = completion_tokens = tool_calls = tool_tokens = 0
    started = time.time()
    text = ""
    for _ in range(MAX_ROUNDS):
        reply = chat(args, messages, tools)
        usage = reply.get("usage", {})
        prompt_tokens += usage.get("prompt_tokens", 0)
        completion_tokens += usage.get("completion_tokens", 0)
        message = reply["choices"][0]["message"]
        text = message.get("content") or ""
        calls = message.get("tool_calls") or []
        if not calls:
            break
        turn = {"role": "assistant", "content": text, "tool_calls": calls}
        if message.get("reasoning_content"):
            # thinking models such as deepseek-reasoner want their reasoning
            # back within one question's tool calls
            turn["reasoning_content"] = message["reasoning_content"]
        messages.append(turn)
        for call in calls:
            tool_calls += 1
            try:
                arguments = json.loads(call["function"].get("arguments") or "{}")
            except json.JSONDecodeError:
                arguments = {}
            result = mcp.call(call["function"]["name"], arguments)
            if args.mode == "budget":
                result = result[: args.budget * 4]
            tool_tokens += len(result) // 4
            messages.append({"role": "tool", "tool_call_id": call.get("id", ""), "content": result})
    answer = final_answer(text)
    final = {"role": "assistant", "content": text}
    if message.get("reasoning_content"):
        final["reasoning_content"] = message["reasoning_content"]
    messages.append(final)
    return {
        "id": question["id"],
        "question": question["question"],
        "answers": question["answers"],
        "said": answer,
        "right": is_right(answer, question["answers"]),
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "last_context": usage.get("prompt_tokens", 0),
        "tool_calls": tool_calls,
        "tool_tokens": tool_tokens,
        "seconds": round(time.time() - started, 2),
        "messages": messages,
        "tools": tools,
    }


def summary(results):
    n = len(results)
    if not n:
        return "no results"
    right = sum(r["right"] for r in results)
    context = sum(r["prompt_tokens"] for r in results) / n
    peak = sum(r["last_context"] for r in results) / n
    calls = sum(r["tool_calls"] for r in results) / n
    seconds = sum(r["seconds"] for r in results) / n
    return (
        f"{right}/{n} right ({100 * right / n:.1f}%), "
        f"{context:.0f} prompt tokens a question ({peak:.0f} in the last call), "
        f"{1000 * right / max(1, sum(r['prompt_tokens'] for r in results)):.2f} right per 1k tokens, "
        f"{calls:.1f} tool calls, {seconds:.1f}s a question"
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--server", required=True, help="OpenAI-compatible server address")
    parser.add_argument("--model", default="local", help="model name sent to the server")
    parser.add_argument("--questions", required=True, help="questions file (JSON lines)")
    parser.add_argument("--mode", choices=["none", "plumb", "budget", "outline"], default="none")
    parser.add_argument("--plumb", default="plumb mcp", help="command that runs plumb mcp")
    parser.add_argument("--budget", type=int, default=300, help="tokens per tool result in budget mode")
    parser.add_argument("--limit", type=int, default=0, help="ask only the first N questions")
    parser.add_argument("--out", required=True, help="results file (JSON lines), appended to")
    parser.add_argument("--api-key-env", default="",
                        help="environment variable holding an API key, for a hosted model")
    parser.add_argument("--max-tokens", type=int, default=512,
                        help="tokens a reply may use; raise it for models that think first")
    parser.add_argument("--transcripts", default="",
                        help="also write each question's whole conversation and tools here "
                             "(JSON lines), for training a model on them")
    args = parser.parse_args()

    with open(args.questions) as f:
        questions = [json.loads(line) for line in f if line.strip()]
    if args.limit:
        questions = questions[: args.limit]
    done = {}
    try:
        with open(args.out) as f:
            for line in f:
                result = json.loads(line)
                done[result["id"]] = result
    except FileNotFoundError:
        pass
    mcp = Mcp(args.plumb) if args.mode != "none" else None
    transcripts = open(args.transcripts, "a") if args.transcripts else None
    with open(args.out, "a") as out:
        for n, question in enumerate(questions, 1):
            if question["id"] in done:
                continue
            try:
                result = ask(question, args, mcp)
            except Exception as err:
                print(f"{question['id']}: {err}", file=sys.stderr)
                continue
            conversation = {"messages": result.pop("messages"), "tools": result.pop("tools")}
            done[result["id"]] = result
            out.write(json.dumps(result) + "\n")
            out.flush()
            if transcripts:
                transcripts.write(json.dumps({**{k: result[k] for k in ("id", "right", "said", "answers")},
                                              **conversation}) + "\n")
                transcripts.flush()
            if n % 25 == 0:
                print(f"{n}/{len(questions)}: {summary(list(done.values()))}", file=sys.stderr)
    asked = [done[q["id"]] for q in questions if q["id"] in done]
    print(summary(asked))


if __name__ == "__main__":
    main()
