#!/usr/bin/env python3
"""Frozen parser and simulation checks for the offline savings estimator."""

import importlib.util
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).with_name("shake-savings-estimate.py")
SPEC = importlib.util.spec_from_file_location("shake_savings_estimate", SCRIPT)
ESTIMATOR = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(ESTIMATOR)


class ShakeSavingsEstimateTest(unittest.TestCase):
    def test_fixture_deduplicates_usage_and_resets_call_history_at_compaction(self):
        fixture = Path(__file__).with_name("testdata") / "estimator-history.jsonl"

        record = ESTIMATOR.load_rollout(fixture)

        self.assertIsNotNone(record)
        self.assertEqual(2, len(record["requests"]))
        self.assertEqual(2, len(record["blocks"]))
        self.assertEqual([0, 1], [r["epoch"] for r in record["requests"]])
        self.assertEqual([0, 1], [b["epoch"] for b in record["blocks"]])
        self.assertFalse(record["blocks"][1]["protected"])
        self.assertEqual(872000, record["requests"][0]["context_window"])

    def test_compaction_epochs_keep_savings_independent(self):
        blocks = []
        for epoch, first_ordinal in ((0, 1), (1, 11)):
            for offset in range(3):
                blocks.append(
                    {
                        "ordinal": first_ordinal + offset,
                        "epoch": epoch,
                        "tokens": 10_000,
                        "protected": False,
                    }
                )
        requests = [
            {
                "ordinal": 10,
                "epoch": 0,
                "input_tokens": 30_000,
                "cached_input_tokens": 20_000,
                "output_tokens": 100,
                "model": "gpt-6-astra",
            },
            {
                "ordinal": 20,
                "epoch": 1,
                "input_tokens": 30_000,
                "cached_input_tokens": 20_000,
                "output_tokens": 100,
                "model": "gpt-6-astra",
            },
        ]

        rows, events = ESTIMATOR.simulate_policy(
            {"blocks": blocks, "requests": requests}, 50_000
        )

        self.assertEqual([10_000, 10_000], [event["tokens_freed"] for event in events])
        self.assertEqual([10_000, 10_000], [row["saved_tokens"] for row in rows])
        self.assertEqual(
            [10_000, 10_000], ESTIMATOR.simulate_naive({"blocks": blocks}, requests)
        )

    def test_gpt_5_6_threshold_is_capped_by_small_context_override(self):
        blocks = [
            {"ordinal": ordinal, "epoch": 0, "tokens": 10_000, "protected": False}
            for ordinal in range(1, 7)
        ]
        request = {
            "ordinal": 10,
            "epoch": 0,
            "input_tokens": 100_000,
            "cached_input_tokens": 0,
            "output_tokens": 100,
            "model": "gpt-5.6",
        }

        rows, events = ESTIMATOR.simulate_policy(
            {"blocks": blocks, "requests": [request]}, 100_000
        )

        self.assertEqual(1, len(events))
        self.assertEqual(40_000, events[0]["tokens_freed"])
        self.assertEqual(40_000, rows[0]["saved_tokens"])

    def test_default_root_is_resolved_when_invoked(self):
        with tempfile.TemporaryDirectory() as home:
            sessions = Path(home) / "sessions"
            sessions.mkdir()
            with mock.patch.dict(os.environ, {"CODEX_HOME": home}, clear=True):
                self.assertEqual([str(sessions)], ESTIMATOR.default_roots())


if __name__ == "__main__":
    unittest.main()
