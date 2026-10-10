"""Deterministic scoring/fixture contracts; no network or model calls."""
import pathlib
import tempfile
import unittest

import run


class IdentityEvaluation(unittest.TestCase):
    def test_wrong_confident_official_is_separate_from_unresolved(self):
        self.assertEqual(run.adjudicate({"found": True, "domain": "mapy.com",
                                        "confidence": "medium"}, ["mypy.readthedocs.io"],
                                       "official_site"), (False, True, False))
        self.assertEqual(run.adjudicate({"found": False, "confidence": "low"},
                                       ["@unresolved"], "official_site"), (True, False, False))
        self.assertFalse(run.right({"error": "transport failed"}, ["@unresolved"]))

    def test_false_accusations_and_missed_impersonations_are_distinct(self):
        self.assertEqual(run.adjudicate({"verdict": "lookalike"}, ["official"],
                                       "check_lookalike"), (False, True, False))
        self.assertEqual(run.adjudicate({"verdict": "official"}, ["lookalike"],
                                       "check_lookalike"), (False, False, True))
        self.assertEqual(run.adjudicate({"verdict": "suspected"}, ["suspected", "unknown"],
                                       "check_lookalike"), (True, False, False))

    def test_country_cases_have_distinct_comparison_keys(self):
        base = {"file": "identity.tsv", "name": "SAT México"}
        self.assertNotEqual(run.row_key(base), run.row_key({**base, "arguments": {"country": "GT"}}))
        self.assertEqual(run.row_key(base), run.row_key({**base, "tool": "official_site", "arguments": {}}))

    def test_precise_docs_host_does_not_accept_other_tenants(self):
        self.assertFalse(run.right({"found": True, "url": "https://mapy.readthedocs.io/"},
                                   ["mypy.readthedocs.io"]))
        self.assertTrue(run.right({"found": True, "url": "https://mypy.readthedocs.io/en/stable/"},
                                  ["mypy.readthedocs.io"]))
        self.assertFalse(run.right({"found": True, "domain": "xe.com", "url": "https://converter.app/"},
                                   ["xe.com"]))

    def test_existing_and_extended_fixture_formats(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = pathlib.Path(directory) / "cases.tsv"
            fixture.write_text('Xe\txe.com\nSAT México\tsat.gob.mx\t{"country":"MX"}\n', encoding="utf-8")
            self.assertEqual(list(run.read(fixture)), [
                ("Xe", ["xe.com"], {}), ("SAT México", ["sat.gob.mx"], {"country": "MX"})])
        for name in ("queries.tsv", "heldout.tsv", "identity.tsv", "lookalikes.tsv"):
            self.assertTrue(list(run.read(pathlib.Path(__file__).with_name(name))))


if __name__ == "__main__":
    unittest.main()
