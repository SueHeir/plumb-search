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
