#!/usr/bin/env python3
"""Offline contract/API regressions for trusted validation feedback."""

import copy
import importlib.util
import io
import os
from pathlib import Path
import sys
import unittest
from unittest import mock
import urllib.error
import urllib.request

SPEC = importlib.util.spec_from_file_location(
    "validation_failure",
    Path(__file__).with_name("fork-upstream-validation-failure.py"),
)
MOD = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MOD
SPEC.loader.exec_module(MOD)
FORK, UPSTREAM, CANDIDATE = "a" * 40, "b" * 40, "c" * 40
IDENTITY = MOD.Identity(
    "rust-v0.164.0",
    UPSTREAM,
    FORK,
    CANDIDATE,
    f"fork-recovery/rust-v0.164.0/{FORK[:12]}",
    40297840820,
    1,
)
PREFIX = f"/repos/{MOD.REPOSITORY}"
RUST_LOG = (
    b"2026-10-26T09:00:00Z Compiling codex-core\n"
    b"error[E0053]: method `get` has an incompatible type for trait\n"
    b"  --> core/src/approvals.rs:42:5\n"
    b"  expected ApprovalResult, found Result<ApprovalResult>\n"
    b"error[E0308]: mismatched types\n"
    b"  expected ContentItem, found String\n"
    b"error: could not compile `codex-core` due to 7 previous errors\n"
    b"##[error]Process completed with exit code 101.\n"
)


def step(name, conclusion="success"):
    return {"name": name, "conclusion": conclusion, "status": "completed"}


class FakeAPI:
    def __init__(self, identity=IDENTITY):
        self.identity = identity
        self.run = {
            "id": identity.run_id,
            "run_attempt": identity.run_attempt,
            "repository": {"full_name": MOD.REPOSITORY, "id": MOD.REPOSITORY_ID},
            "event": "workflow_dispatch",
            "head_branch": "main",
            "head_sha": FORK,
            "path": ".github/workflows/fork-upstream-update.yml",
            "html_url": identity.run_url,
            "status": "in_progress",
            "conclusion": None,
        }
        self.prepare = {
            "id": 11,
            "name": MOD.PREPARE_JOB,
            "status": "completed",
            "conclusion": "success",
            "steps": [step("Prepare merge candidate")],
        }
        self.validation = {
            "id": 121488520368,
            "name": MOD.VALIDATION_JOB,
            "status": "completed",
            "conclusion": "failure",
            "steps": [
                step(MOD.IDENTITY_STEP),
                step("Check formatting"),
                step("Test scoped release behavior", "failure"),
            ],
        }
        for job in (self.prepare, self.validation):
            job.update(
                run_id=identity.run_id, run_attempt=identity.run_attempt, head_sha=FORK
            )
        self.jobs = [self.validation, self.prepare]
        self.deployments = []
        self.calls = []
        self.logs = RUST_LOG
        self.main = FORK
        self.candidate = CANDIDATE
        self.parents = [FORK, UPSTREAM]
        self.tag = {"type": "commit", "sha": UPSTREAM}
        self.log_ids = []
        self.pages = None

    def request(self, path, body=None):
        self.calls.append((path, body))
        if path == f"{PREFIX}/actions/runs/{self.identity.run_id}":
            return copy.deepcopy(self.run)
        if path.startswith(
            f"{PREFIX}/actions/runs/{self.identity.run_id}/attempts/1/jobs?"
        ):
            page = int(path.rsplit("=", 1)[1])
            return {
                "jobs": copy.deepcopy(self.pages[page - 1] if self.pages else self.jobs)
            }
        if path == f"{PREFIX}/git/ref/heads/main":
            return {"object": {"sha": self.main}}
        if path == f"{PREFIX}/git/ref/heads/{self.identity.candidate_branch}":
            return {"object": {"sha": self.candidate}}
        if path == f"{PREFIX}/git/commits/{CANDIDATE}":
            return {"sha": CANDIDATE, "parents": [{"sha": sha} for sha in self.parents]}
        if path == f"/repos/openai/codex/git/ref/tags/{self.identity.upstream_tag}":
            return {"object": self.tag}
        if path == f"/repos/openai/codex/git/tags/{'d' * 40}":
            return {"object": {"type": "commit", "sha": UPSTREAM}}
        if path.startswith(f"{PREFIX}/deployments?"):
            return copy.deepcopy(self.deployments)
        if path == f"{PREFIX}/deployments" and body:
            deployment = {
                **body,
                "id": 123,
                "sha": body["ref"],
                "creator": {"login": "github-actions[bot]", "id": 41898282},
            }
            self.deployments.append(deployment)
            return copy.deepcopy(deployment)
        raise AssertionError(f"unexpected API path: {path}")

    def log_chunks(self, job_id):
        self.log_ids.append(job_id)
        yield self.logs


class FeedbackTests(unittest.TestCase):
    def test_observed_rust_errors_and_exact_deployment(self):
        api = FakeAPI()
        deployment, payload, reused = MOD.handoff(api, IDENTITY)
        self.assertFalse(reused)
        self.assertEqual(payload["failure_reason"], "rustc_diagnostics")
        self.assertEqual(payload["validation_result"], "source_failure")
        self.assertEqual(payload["candidate_sha"], CANDIDATE)
        self.assertEqual(payload["validation_job_id"], 121488520368)
        self.assertIn("ContentItem", payload["failure_context"])
        self.assertEqual(api.log_ids, [121488520368])
        self.assertEqual(deployment["ref"], FORK)
        self.assertEqual(deployment["task"], "upstream-release")
        self.assertFalse(deployment["auto_merge"])
        self.assertEqual(deployment["required_contexts"], [])
        self.assertTrue(deployment["transient_environment"])
        self.assertEqual(MOD.handoff(api, IDENTITY)[0]["id"], deployment["id"])
        self.assertTrue(MOD.handoff(api, IDENTITY)[2])
        self.assertEqual(len(api.deployments), 1)

    def test_clean_root_and_bounded_recovery_branches(self):
        root = f"rust-v0.164.0/{FORK[:12]}"
        for branch in [
            f"fork-upstream-candidate/{root}",
            f"fork-recovery/{root}",
            f"fork-recovery/{root}-attempt-1",
            f"fork-recovery/{root}-attempt-2",
        ]:
            identity = MOD.Identity(**{**IDENTITY.__dict__, "candidate_branch": branch})
            self.assertEqual(
                MOD.handoff(FakeAPI(identity), identity)[1]["validation_result"],
                "source_failure",
            )
        for suffix in (
            "-attempt-0",
            "-attempt-3",
            "-attempt-01",
            "-attempt-1/extra",
            "/other",
            "-attempt-1\n",
        ):
            identity = MOD.Identity(
                **{
                    **IDENTITY.__dict__,
                    "candidate_branch": f"fork-recovery/{root}{suffix}",
                }
            )
            with self.assertRaises(MOD.HandoffError):
                identity.validate()

    def test_malformed_identities(self):
        for field, value in [
            ("run_id", True),
            ("run_attempt", 0),
            ("run_id", "123"),
            ("fork_sha", "abc"),
            ("upstream_tag", "rust-v1.2.3-beta"),
            ("candidate_sha", "C" * 40),
        ]:
            with (
                self.subTest(field=field, value=value),
                self.assertRaises(MOD.HandoffError),
            ):
                MOD.Identity(**{**IDENTITY.__dict__, field: value}).validate()

    def test_run_provenance_and_current_attempt(self):
        for field, value in [
            ("run_attempt", 2),
            ("run_attempt", True),
            ("head_sha", CANDIDATE),
            ("head_branch", "other"),
            ("event", "push"),
            ("path", ".github/workflows/fork-release.yml"),
            ("html_url", "https://example.org/run"),
            ("repository", {"full_name": "other/codex", "id": MOD.REPOSITORY_ID}),
            ("repository", {"full_name": MOD.REPOSITORY, "id": 123}),
        ]:
            api = FakeAPI()
            api.run[field] = value
            with (
                self.subTest(field=field, value=value),
                self.assertRaises(MOD.HandoffError),
            ):
                MOD.handoff(api, IDENTITY)
            self.assertFalse(api.deployments)

    def test_missing_ambiguous_or_nonterminal_job(self):
        for mutate in (
            lambda api: api.jobs.pop(0),
            lambda api: api.jobs.append(copy.deepcopy(api.validation)),
            lambda api: api.validation.update(status="in_progress"),
            lambda api: api.validation.update(conclusion="success"),
            lambda api: api.validation.update(id=True),
            lambda api: api.validation["steps"].append(
                step("Second failure", "failure")
            ),
        ):
            api = FakeAPI()
            mutate(api)
            with self.assertRaises(MOD.HandoffError):
                MOD.handoff(api, IDENTITY)

    def test_job_run_binding(self):
        for field, value in [
            ("run_id", 123),
            ("run_attempt", 2),
            ("run_attempt", True),
            ("head_sha", CANDIDATE),
        ]:
            api = FakeAPI()
            api.validation[field] = value
            with self.subTest(field=field), self.assertRaises(MOD.HandoffError):
                MOD.handoff(api, IDENTITY)

    def test_paginated_reordered_jobs_use_exact_attempt(self):
        api = FakeAPI()
        api.pages = [
            [{"id": x, "name": "unrelated"} for x in range(100)],
            [api.prepare, api.validation],
        ]
        self.assertEqual(
            MOD.handoff(api, IDENTITY)[1]["validation_job_id"], 121488520368
        )
        self.assertTrue(
            any("/attempts/1/jobs?per_page=100&page=2" in path for path, _ in api.calls)
        )

    def test_stale_main_candidate_parent_and_tag(self):
        for field, value in [
            ("main", "d" * 40),
            ("candidate", "d" * 40),
            ("parents", [UPSTREAM, FORK]),
            ("parents", [FORK, UPSTREAM, "d" * 40]),
            ("tag", {"type": "commit", "sha": FORK}),
        ]:
            api = FakeAPI()
            setattr(api, field, value)
            with self.subTest(field=field), self.assertRaises(MOD.HandoffError):
                MOD.handoff(api, IDENTITY)
            self.assertFalse(api.deployments)
        api = FakeAPI()
        api.tag = {"type": "tag", "sha": "d" * 40}
        self.assertEqual(
            MOD.handoff(api, IDENTITY)[1]["validation_result"], "source_failure"
        )

    def test_setup_identity_cancel_and_timeout_are_infrastructure(self):
        for kind in ("identity", "prepare", "setup", "cancelled", "timed_out"):
            api = FakeAPI()
            if kind == "identity":
                api.validation["steps"][0]["conclusion"] = "skipped"
            elif kind == "prepare":
                api.prepare["conclusion"] = "failure"
            elif kind == "setup":
                api.validation["steps"][-1]["name"] = "Install uv"
            else:
                api.validation["conclusion"] = kind
                api.validation["steps"][-1]["conclusion"] = kind
            payload = MOD.handoff(api, IDENTITY)[1]
            self.assertEqual(
                payload["validation_result"], "infrastructure_failure", kind
            )

    def test_wrong_deployment_provenance_cannot_dedupe(self):
        for field, value in [
            ("creator", {"login": "other", "id": 41898282}),
            ("task", "deploy"),
            ("ref", "main"),
            ("sha", CANDIDATE),
            ("environment", "other"),
            ("id", True),
        ]:
            api = FakeAPI()
            deployment = MOD.handoff(api, IDENTITY)[0]
            deployment[field] = value
            api.deployments = [deployment]
            with self.subTest(field=field), self.assertRaises(MOD.HandoffError):
                MOD.handoff(api, IDENTITY)

    def test_api_failure_is_not_acknowledged(self):
        api = FakeAPI()
        api.log_chunks = mock.Mock(side_effect=OSError("private transport details"))
        with self.assertRaises(OSError):
            MOD.handoff(api, IDENTITY)
        self.assertFalse(api.deployments)


class LogTests(unittest.TestCase):
    def collect(self, log, step="Test scoped release behavior"):
        return MOD.collect_log([log], step)

    def test_conservative_source_whitelist_and_infra_precedence(self):
        for log in (
            b"error[E0053]: bad type\nNo space left on device",
            b"failed to download rustc\n" + RUST_LOG,
            b"toolchain 1.99.1 is not installed\n" + RUST_LOG,
            b"Connection reset\n" + RUST_LOG,
            b"command not found\n" + RUST_LOG,
            b"unknown failure",
        ):
            self.assertEqual(
                self.collect(log)["validation_result"], "infrastructure_failure"
            )
        self.assertEqual(
            self.collect(b"Diff in /repo/core.rs:4:\n-old\n+new", "Check formatting")[
                "failure_reason"
            ],
            "formatter_diff",
        )
        self.assertEqual(
            self.collect(b"test suite::approval ... FAILED")["failure_reason"],
            "test_failure",
        )
        self.assertEqual(
            self.collect(b"FAIL [  0.1s] (1/9) codex-core suite::approval")[
                "failure_reason"
            ],
            "test_failure",
        )
        self.assertEqual(
            self.collect(RUST_LOG, "Test merge, metadata, and release helpers")[
                "validation_result"
            ],
            "infrastructure_failure",
        )
        self.assertEqual(
            self.collect(b"error: something went wrong")["validation_result"],
            "infrastructure_failure",
        )

    def test_sanitization_controls_credentials_and_invalid_utf8(self):
        log = (
            b"\x1b[31merror[E0308]: wrong\x1b[0m\r\n\x1b]0;hidden title\x07"
            b"https://user:password@example.org/log?sig=secret#token\n"
            b"::error title=injection::untrusted workflow command\n"
            b"GH_TOKEN=ghp_SECRET123 authorization: Bearer SECRET456 api_key=SECRET789\n"
            b"github_pat_ABC123 sk-secret123 AKIA1234567890123456\n"
            b"custom-raw-token\n\x00\xff\n"
        )
        with mock.patch.dict(os.environ, {"GH_TOKEN": "custom-raw-token"}):
            context = self.collect(log)["failure_context"]
        for forbidden in (
            "\x1b",
            "\x00",
            "hidden title",
            "user:password",
            "sig=",
            "#token",
            "workflow command",
            "ghp_SECRET123",
            "SECRET456",
            "SECRET789",
            "github_pat_ABC123",
            "sk-secret123",
            "AKIA1234567890123456",
            "custom-raw-token",
        ):
            self.assertNotIn(forbidden, context)
        self.assertIn("error[E0308]", context)
        self.assertIn("https://example.org/log", context)

    def test_unicode_line_and_context_bounds_keep_first_diagnostic_and_tail(self):
        chunks = (
            [RUST_LOG]
            + [("界" * 10000 + "\n").encode()] * 60
            + [b"terminal result: exit 101\n"]
        )
        result = MOD.collect_log(chunks, "Test scoped release behavior")
        self.assertLessEqual(len(result["failure_context"].encode()), MOD.MAX_CONTEXT)
        self.assertTrue(result["context_truncated"])
        self.assertLessEqual(
            len(result["failure_context"].splitlines()), MOD.MAX_CONTEXT_LINES
        )
        self.assertIn("error[E0053]", result["failure_context"])
        self.assertIn("terminal result", result["failure_context"])

    def test_context_line_count_bound(self):
        result = self.collect(RUST_LOG + b"x\n" * 10000 + b"terminal\n")
        self.assertLessEqual(
            len(result["failure_context"].splitlines()), MOD.MAX_CONTEXT_LINES
        )
        self.assertIn("error[E0053]", result["failure_context"])
        self.assertIn("terminal", result["failure_context"])

    def test_long_preamble_and_short_tail_preserve_diagnostic_under_both_bounds(self):
        result = self.collect(
            (b"x" * 2000 + b"\n") * 6 + RUST_LOG + b"x\n" * 10000 + b"terminal\n"
        )
        self.assertLessEqual(
            len(result["failure_context"].splitlines()), MOD.MAX_CONTEXT_LINES
        )
        self.assertLessEqual(len(result["failure_context"].encode()), MOD.MAX_CONTEXT)
        self.assertIn("error[E0053]", result["failure_context"])
        self.assertIn("terminal", result["failure_context"])

    def test_download_ceiling_forbids_source_classification(self):
        def chunks():
            yield RUST_LOG
            for _ in range(MOD.MAX_DOWNLOAD // 65536 + 1):
                yield b"plain log line\n" * (65536 // 15 + 1)

        result = MOD.collect_log(chunks(), "Test scoped release behavior")
        self.assertEqual(result["failure_reason"], "log_download_limit")
        self.assertEqual(result["validation_result"], "infrastructure_failure")
        self.assertTrue(result["context_truncated"])

    def test_infrastructure_outside_retained_tail_still_dominates(self):
        result = MOD.collect_log(
            [b"No space left on device\n", b"ordinary\n" * 20000, RUST_LOG],
            "Test scoped release behavior",
        )
        self.assertEqual(result["validation_result"], "infrastructure_failure")
        self.assertIn("No space left", result["failure_context"])

    def test_multibyte_decoding_across_chunks(self):
        log = ("error[E0308]: 界\n" * 3).encode()
        self.assertIn(
            "界",
            MOD.collect_log([bytes([b]) for b in log], "Test scoped release behavior")[
                "failure_context"
            ],
        )

    def test_redirect_download_has_no_authorization_header(self):
        api = MOD.GitHub("private-token")
        error = urllib.error.HTTPError(
            "https://api.github.com/private",
            302,
            "Found",
            {"Location": "https://logs.example.org/private?signature=SECRET"},
            io.BytesIO(),
        )
        response = io.BytesIO(RUST_LOG)
        with (
            mock.patch.object(api.opener, "open", side_effect=error) as first,
            mock.patch.object(
                urllib.request, "urlopen", return_value=response
            ) as second,
        ):
            self.assertEqual(b"".join(api.log_chunks(123)), RUST_LOG)
        self.assertIn("Authorization", first.call_args.args[0].headers)
        self.assertIsInstance(second.call_args.args[0], str)
        self.assertNotIn("headers", second.call_args.kwargs)
        for location in (
            "http://logs.example.org/private",
            "https://user:password@logs.example.org/private",
            "file:///private",
        ):
            error.headers["Location"] = location
            with (
                mock.patch.object(api.opener, "open", side_effect=error),
                self.assertRaises(MOD.HandoffError),
            ):
                list(api.log_chunks(123))


if __name__ == "__main__":
    unittest.main()
