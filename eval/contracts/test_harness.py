"""Focused parser, scoring, provenance, leakage and budget checks; no network."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("identity_eval", ROOT / "eval/official_site/run.py")
identity = importlib.util.module_from_spec(spec)
spec.loader.exec_module(identity)
compare_spec = importlib.util.spec_from_file_location("contract_compare", ROOT / "eval/contracts/compare.py")
comparison = importlib.util.module_from_spec(compare_spec)
compare_spec.loader.exec_module(comparison)


class HarnessTests(unittest.TestCase):
    def test_family_acceptance_has_40_families_and_200_queries(self):
        cases = [json.loads(l) for l in (ROOT / "eval/contracts/family_heldout.jsonl").read_text().splitlines()]
        self.assertEqual(len(cases), 200)
        self.assertEqual(len({c["family"] for c in cases}), 40)
        self.assertTrue(all(c["label_status"] == "candidate" for c in cases))
        audit = [json.loads(l) for l in (ROOT / "eval/contracts/audit.jsonl").read_text().splitlines()]
        self.assertFalse({c["family"] for c in audit} & {c["family"] for c in cases})
        self.assertTrue(all(c["negatives"] for c in cases))

    def test_wrong_high_confidence_official_and_false_lookalike_are_separate(self):
        case = {"tool": "official_site", "relevant": [{"identity": "mypy.readthedocs.io"}]}
        scored = identity.judge(case, {"result": {"structuredContent": {
            "found": True, "url": "https://mapy.com", "confidence": "medium"}}})
        self.assertTrue(scored["false_official"])
        self.assertFalse(scored["false_lookalike"])
        case = {"tool": "check_lookalike", "expect": {"verdict": "official"}}
        scored = identity.judge(case, {"result": {"structuredContent": {"verdict": "lookalike"}}})
        self.assertTrue(scored["false_lookalike"])
        self.assertFalse(scored["false_official"])

    def test_domains_have_hostname_boundaries(self):
        self.assertTrue(identity.right({"found": True, "url": "https://api.xe.com/a"}, ["xe.com"]))
        self.assertFalse(identity.right({"found": True, "url": "https://xe.com.attacker.test"}, ["xe.com"]))
        self.assertFalse(identity.right({"found": True, "url": "https://other.readthedocs.io"}, ["mypy.readthedocs.io"]))
        self.assertEqual(identity.findings_off("http://localhost/mcp?findings=on&x=1"),
                         "http://localhost/mcp?findings=off&x=1")

    def test_fixed_corpus_comparison_pairs_families_and_rejects_changed_inputs(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = Path(temp)
            manifest = {"type": "manifest", "cases": 1, "build": {"revision": "baseline"},
                "eval_time": 1, "query_instruction": "Split", "model": None, "limit": 100,
                "pages_top": 999, "target": "in-process-offline", "transport": "core",
                "features": {"findings": False, "personalization": False, "plugins": False,
                    "external_results": False, "peers": False},
                "suite_fingerprints": [{"sha256": "labels", "bytes": 10}],
                "corpus": [{"sha256": "snapshot", "bytes": 100}], "rank": {}, "learned_model_sha256": "model"}
            case = {"id": "one", "family": "family", "category": "navigation", "label_status": "manual",
                    "relevant": [{"identity": "a.org", "grade": 3}]}
            score = {"passed": True, "top1": True, "top3": True, "mrr": 1., "ndcg10": 1.,
                     "wrong_domain": False, "wrong_brand_top3": False, "violations": []}
            query = {"type": "query", "case": case, "options": {}, "score": score}
            summary = {"type": "summary", "corpus_unchanged": True, "latency_ms": {"p95": 10.}}
            def save(path, m, q):
                path.write_text("".join(json.dumps(r) + "\n" for r in [m, q, summary]))
            old, new = temp / "old.jsonl", temp / "new.jsonl"
            save(old, manifest, query)
            save(new, dict(manifest, build={"revision": "candidate"}), query)
            result = comparison.compare(old, new)
            self.assertEqual(result["groups"]["manual:navigation"]["metrics"]["top1"]["family_mean_delta"], 0)
            save(new, dict(manifest, corpus=[{"sha256": "changed", "bytes": 100}]), query)
            with self.assertRaisesRegex(ValueError, "fixed-corpus"):
                comparison.compare(old, new)
            changed = dict(query, score=dict(score, passed=False, violations=["wrong brand"]))
            save(new, manifest, changed)
            self.assertEqual(len(comparison.compare(old, new)["manual_regressions"]), 1)

    def test_offline_replay_retains_complete_response_and_budget_failure(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = Path(temp)
            case = {"id": "a", "family": "a", "category": "identity", "query": "xe",
                    "tool": "official_site", "label_status": "candidate",
                    "relevant": [{"identity": "xe.com"}]}
            fixture = temp / "suite.jsonl"
            fixture.write_text(json.dumps(case) + "\n")
            response = {"result": {"structuredContent": {"found": True, "url": "https://xe.com",
                        "provenance": {"text": "x" * 10000}}}}
            saved = temp / "saved.jsonl"
            saved.write_text(json.dumps({"case": case, "request": {"jsonrpc": "2.0", "id": 1,
                "method": "tools/call", "params": {"name": "official_site", "arguments": {"name": "xe"}}},
                "response": response, "response_bytes": 10100}) + "\n")
            out = temp / "out.jsonl"
            result = subprocess.run([sys.executable, str(ROOT / "eval/official_site/run.py"),
                "--responses", str(saved), "--out", str(out), str(fixture)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            rows = [json.loads(l) for l in out.read_text().splitlines()]
            self.assertEqual(rows[1]["response"], response)
            self.assertTrue(rows[1]["right"])
            case2 = dict(case, id="b")
            fixture.write_text(json.dumps(case) + "\n" + json.dumps(case2) + "\n")
            result = subprocess.run([sys.executable, str(ROOT / "eval/official_site/run.py"),
                "--responses", str(saved), "--out", str(out), "--max-calls", "1", str(fixture)],
                capture_output=True, text=True)
            self.assertEqual(result.returncode, 2)
            self.assertTrue(json.loads(out.read_text().splitlines()[-1])["incomplete"])


if __name__ == "__main__":
    unittest.main()
