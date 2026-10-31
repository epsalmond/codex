#!/usr/bin/env python3
"""Check exact focused selections without building Rust test binaries."""

from copy import deepcopy
import io
import json
import unittest
from unittest.mock import patch

import check_issue_238_selection as checker


class SelectionTest(unittest.TestCase):
    def listing(self, selection):
        binary_id, names = checker.SELECTIONS[selection]
        return {
            "rust-suites": {
                binary_id: {
                    "testcases": {
                        name: {"filter-match": {"status": "matches"}, "ignored": False}
                        for name in names
                    },
                },
            },
        }

    def check(self, selection, listing):
        with (
            patch("sys.argv", ["checker", selection]),
            patch("sys.stdin", io.StringIO(json.dumps(listing))),
            patch("sys.stdout", new_callable=io.StringIO),
            patch("sys.stderr", new_callable=io.StringIO),
        ):
            checker.main()

    def test_exact_selections_pass(self):
        for selection in checker.SELECTIONS:
            with self.subTest(selection=selection):
                self.check(selection, self.listing(selection))

    def test_missing_ignored_filtered_or_extra_case_fails(self):
        for selection in checker.SELECTIONS:
            binary_id, names = checker.SELECTIONS[selection]
            for mutation in ("missing", "ignored", "filtered", "extra"):
                with self.subTest(selection=selection, mutation=mutation):
                    listing = self.listing(selection)
                    cases = listing["rust-suites"][binary_id]["testcases"]
                    if mutation == "missing":
                        del cases[names[0]]
                    elif mutation == "ignored":
                        cases[names[0]]["ignored"] = True
                    elif mutation == "filtered":
                        cases[names[0]]["filter-match"]["status"] = "mismatch"
                    else:
                        cases["unexpected"] = deepcopy(cases[names[0]])
                    with self.assertRaises(SystemExit) as failure:
                        self.check(selection, listing)
                    self.assertEqual(failure.exception.code, 1)

    def test_wrong_binary_fails(self):
        selection = "code-mode-readiness"
        listing = self.listing(selection)
        listing["rust-suites"]["wrong-binary"] = listing["rust-suites"].pop(
            "codex-core::all"
        )
        with self.assertRaises(SystemExit) as failure:
            self.check(selection, listing)
        self.assertEqual(failure.exception.code, 1)

    def test_unmatched_cases_do_not_expand_selection(self):
        selection = "code-mode-readiness"
        listing = self.listing(selection)
        cases = listing["rust-suites"]["codex-core::all"]["testcases"]
        cases["unrelated"] = {
            "filter-match": {"status": "mismatch"},
            "ignored": False,
        }
        self.check(selection, listing)


if __name__ == "__main__":
    unittest.main()
