#!/usr/bin/env python3
"""Native executor -> broker -> pinned TS/Rust acceptance in a bounded user service.

Reuse the unchanged Python runner's byte accounting and /proc observations.
All fixtures, registries, and traces are private; no service is installed.
"""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import importlib.util
import json
import os
from pathlib import Path
import platform
import re
import signal
import socket
import stat
import subprocess
import tempfile
import time
import unittest
import uuid


REPO = Path(__file__).resolve().parent.parent


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


shared = module("nas_python_acceptance", REPO / "scripts/nas-lsp-acceptance.py")
CORPORA = {
    language: module(
        "nas_" + language, REPO / f"scripts/fixtures/nas-lsp-{language}/corpus.py"
    )
    for language in ("typescript", "rust")
}
# Declared before any real-server measurement. Language-specific keys are kept
# even where the current gates coincide, so later experiments cannot drift.
THRESHOLDS = {
    language: {
        **shared.THRESHOLDS,
        "shutdown_ms": 5_000,
        "transitional_followups": 2,
    }
    for language in CORPORA
}


def keys(locations):
    return sorted(
        (loc["path"], loc["line"], loc["column"], loc["line"], loc["end_column"])
        for loc in locations
    )


def save(path, value):
    shared.write_json(path, value)


def registered_roots(setup):
    roots = [setup["root_a"], setup["root_b"], setup["cold_empty_root"]]
    roots += [case["root"] for case in setup["negative_roots"]]
    roots += [
        setup[key]
        for key in ("alias_root", "hostile_root", "source_root")
        if key in setup
    ]
    return [Path(root) for root in roots]


def crash_reports(setup):
    return sorted(
        (
            shared.file_identity(path)
            for root in registered_roots(setup)
            for path in root.glob("rustc-ice-*.txt")
        ),
        key=lambda row: row["path"],
    )


def prepare(directory, language, toolchains, source_root=None):
    directory.mkdir(mode=0o700, parents=True, exist_ok=False)
    corpus = CORPORA[language]
    prepared = corpus.prepare(directory / "corpus")
    roots = prepared["roots"]
    setup = {
        "language": language,
        "source_head": shared.git_head(),
        "corpus": prepared,
        "root_a": roots[0]["root"],
        "root_b": roots[1]["root"],
        "thresholds": THRESHOLDS[language],
        "toolchains": toolchains,
        "socket_path": f"/tmp/codex-daemon-{os.getuid()}/navigation-{language}-{uuid.uuid4().hex[:12]}.sock",
        "negative_roots": [],
    }
    setup["cold_empty_root"] = str(corpus.materialize(directory / "cold-empty"))
    if language == "typescript":
        alias = directory / "typescript-alias"
        (alias / "src").mkdir(parents=True)
        (alias / "package.json").write_text(
            '{"name":"navigation-alias","private":true,"type":"module"}\n'
        )
        for name in ("left", "right"):
            (alias / f"src/{name}.ts").write_text(
                "export function selected() { return 1; }\n"
            )
        (alias / "src/use.ts").write_text(
            'import { selected } from "@selected";\nselected();\n'
        )
        (alias / "tsconfig.json").write_text(
            json.dumps(
                {
                    "compilerOptions": {
                        "target": "ES2022",
                        "module": "ESNext",
                        "moduleResolution": "Bundler",
                        "types": [],
                        "paths": {"@selected": ["./src/left.ts"]},
                    },
                    "include": ["src/**/*.ts"],
                }
            )
        )
        setup["alias_root"] = str(alias)
    recipes = json.loads((corpus.HERE / "negative-cases.json").read_text())["cases"]
    # Additional cases are harness-owned; the independent corpus stays fixed.
    if language == "typescript":
        recipes += [
            {
                "id": "jsdoc_import_type",
                "write": {
                    "src/escape.ts": '/** @type {import("../../outside").Payload} */\nexport const harmless = 1;\n'
                },
            },
            {"id": "missing_root_config", "delete": ["tsconfig.json"]},
            {"id": "missing_root_package", "delete": ["package.json"]},
        ]
    else:
        recipes += [
            {"id": "no_cargo", "delete": ["Cargo.toml"]},
            {
                "id": "macro_include",
                "write": {
                    "src/escape.rs": 'macro_rules! hide { () => { r#include!("../../outside.rs"); }; }\n'
                },
            },
            {
                "id": "proc_macro_model",
                "write": {
                    "rust-project.json": json.dumps(
                        {
                            "crates": [
                                {
                                    "root_module": "src/lib.rs",
                                    "edition": "2021",
                                    "deps": [],
                                    "is_workspace_member": True,
                                    "proc_macro_dylib_path": "hostile.so",
                                }
                            ]
                        }
                    )
                },
            },
        ]
    for recipe in recipes:
        root = corpus.materialize(directory / "negative" / recipe["id"])
        for relative, text in recipe.get("write", {}).items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        for relative in recipe.get("delete", []):
            (root / relative).unlink()
        setup["negative_roots"].append(
            {"id": recipe["id"], "root": str(root), "recipe": recipe}
        )
    # A successful base graph must ignore Cargo/toolchain command selection.
    # Every possible side effect is confined to this new fixture directory.
    if language == "rust":
        root = corpus.materialize(directory / "rust-hostile-tools")
        sentinel = root / "executed-sentinel"
        wrapper = root / "hostile-rustc"
        wrapper.write_text(f"#!/bin/sh\ntouch '{sentinel}'\nexit 91\n")
        wrapper.chmod(0o700)
        (root / ".cargo").mkdir()
        (root / ".cargo/config.toml").write_text(
            '[build]\nrustc = "./hostile-rustc"\nrustc-wrapper = "./hostile-rustc"\n'
        )
        (root / "rust-toolchain.toml").write_text(
            '[toolchain]\nchannel = "invalid-navigation-sentinel"\n'
        )
        (root / "build.rs").write_text(
            'fn main() { std::fs::write("executed-sentinel", "build.rs").unwrap(); }\n'
        )
        setup["hostile_root"] = str(root)
        setup["sentinel"] = str(sentinel)
    if source_root:
        setup["source_root"] = str(source_root.resolve(strict=True))
    save(directory / "setup.json", setup)
    return setup


def registry(setup, directory):
    parent = Path(setup["socket_path"]).parent
    parent.mkdir(mode=0o700, exist_ok=True)
    info = parent.lstat()
    if (
        not stat.S_ISDIR(info.st_mode)
        or info.st_uid != os.getuid()
        or stat.S_IMODE(info.st_mode) != 0o700
    ):
        raise ValueError(
            "reserved daemon directory must already be owner-owned mode 0700"
        )
    language = setup["language"]
    tools = setup["toolchains"][language]
    roots = [("a", setup["root_a"]), ("b", setup["root_b"])]
    roots += [(entry["id"], entry["root"]) for entry in setup["negative_roots"]]
    roots += [
        (name, setup[key])
        for name, key in (
            ("empty", "cold_empty_root"),
            ("alias", "alias_root"),
            ("hostile", "hostile_root"),
            ("source", "source_root"),
        )
        if key in setup
    ]
    lines = [
        f"socket_path = {json.dumps(setup['socket_path'])}",
        "",
        "[limits]",
        "max_workers = 2",
        "max_queue_per_worker = 4",
        "max_queries = 8",
        "max_connections = 32",
        "idle_ttl_secs = 60",
        "memory_max_bytes = 1073741824",
        "pids_max = 64",
    ]
    for name, root in roots:
        lines += [
            "",
            "[[workspaces]]",
            f'id = "acceptance-{name}"',
            f"root = {json.dumps(root)}",
            f"language = {json.dumps(language)}",
            f"executable = {json.dumps(tools['command'])}",
            f"args = {json.dumps(tools['args'])}",
            'exclude_dirs = [".git"]',
            f'readiness = "{"typescript_project" if language == "typescript" else "rust_analyzer_project"}"',
        ]
        if language == "typescript":
            lines += [f"typescript_tsserver = {json.dumps(tools['tsserver_js'])}"]
        else:
            lines += [f"rustc = {json.dumps(tools['rustc'])}"]
    path = parent / f"registry-languages-{uuid.uuid4().hex[:12]}.toml"
    with path.open("x") as output:
        path.chmod(0o600)
        output.write("\n".join(lines) + "\n")
    return path


class Runner(shared.Runner):
    def __init__(self, setup, driver, executable, receipt):
        super().__init__(setup, driver, 0, executable, receipt)
        self.corpus = CORPORA[setup["language"]]
        self.limits = THRESHOLDS[setup["language"]]

    def snapshot(self, phase):
        snap = {
            "phase": phase,
            **shared.process_snapshot(self.broker_pid, self.executable),
        }
        # TSLS and its tsserver helpers use the same pinned Node executable.
        # Pool capacity counts direct broker children; preserve every Node PID
        # separately for descendant containment, cancellation and shutdown.
        snap["pool_workers"] = [
            row for row in snap["workers"] if row["ppid"] == self.broker_pid
        ]
        snap["runtime_helpers"] = [
            row for row in snap["workers"] if row["ppid"] != self.broker_pid
        ]
        self.snapshots.append(snap)
        self.check(
            f"cgroup/{phase}", snap["bounded"] and snap["descendants_same_cgroup"]
        )
        self.check(
            f"workers/{phase}",
            len(snap["pool_workers"]) <= 2,
            actual=len(snap["pool_workers"]),
            runtime_processes=len(snap["workers"]),
        )
        return snap

    def request(self, query, root_b=False, **overrides):
        at = self.corpus.query_location(query, "shifted" if root_b else "base")
        return {
            "socket_path": self.setup["socket_path"],
            "cwd": self.setup["root_b" if root_b else "root_a"],
            "path": at["path"],
            "operation": query["operation"],
            "position": {"line": at["line"], "column": at["column"]},
            "source": "direct",
            "budget": shared.MAX_BYTES,
            "access": "allowed",
            **overrides,
        }

    def settled(self, request, label):
        rows = [self.call(request, label)]
        while (
            isinstance(rows[-1]["response"], dict)
            and rows[-1]["response"].get("status") in ("indexing", "stale")
            and len(rows) <= self.limits["transitional_followups"]
        ):
            rows.append(self.call(request, f"{label}/followup-{len(rows)}"))
        rows[-1]["total_attempt_latency_ms"] = sum(
            row["wall_latency_ms"] for row in rows
        )
        rows[-1]["transitional_queries"] = [row["label"] for row in rows[:-1]]
        return rows[-1]

    def exact(self, request, wanted, label):
        row = self.settled(request, label)
        response = row["response"]
        correct = (
            isinstance(response, dict)
            and response.get("status") == "ok"
            and not response.get("truncated", False)
            and shared.response_keys(response) == wanted
        )
        row.update(correct=correct, expected=wanted)
        self.check("correct/" + label, correct)
        return row

    def query(self, query, scenario, phase, root_b=False, **overrides):
        wanted = keys(
            self.corpus.expected(query, scenario, "shifted" if root_b else "base")
        )
        return self.exact(
            self.request(query, root_b, **overrides),
            wanted,
            f"{phase}/{'b' if root_b else 'a'}/{query['id']}",
        )

    def rejected(self, request, label):
        row = self.settled(request, label)
        response = row["response"]
        self.check(
            label,
            isinstance(response, dict)
            and response.get("status") in ("unavailable", "invalid_request")
            and not response.get("locations"),
            actual=response,
        )
        return row

    def negative_cases(self):
        query = self.corpus.ORACLE["queries"][0]
        for case in self.setup["negative_roots"]:
            overrides = {"cwd": case["root"]}
            if case["id"] == "orphan_source":
                overrides.update(
                    path="src/orphan.rs", position={"line": 1, "column": 8}
                )
            self.rejected(self.request(query, **overrides), "negative/" + case["id"])
        if "hostile_root" in self.setup:
            self.exact(
                self.request(query, cwd=self.setup["hostile_root"]),
                keys(self.corpus.expected(query)),
                "hostile-tools/local-graph",
            )
            self.rejected(
                self.request(
                    query,
                    cwd=self.setup["hostile_root"],
                    path="build.rs",
                    position={"line": 1, "column": 4},
                ),
                "hostile-tools/build-source",
            )
            self.check(
                "hostile-tools/sentinel_absent",
                not Path(self.setup["sentinel"]).exists(),
            )

    def config_changes(self):
        query = self.corpus.ORACLE["queries"][0]
        root = Path(self.setup["root_a"])
        if self.setup["language"] == "typescript":
            path = root / "tsconfig.json"
            original = path.read_text()
            config = json.loads(original)
            config["compilerOptions"]["plugins"] = [
                {"name": "hostile-navigation-plugin"}
            ]
            path.write_text(json.dumps(config))
            self.rejected(self.request(query), "config/plugin-added")
            path.write_text(original)
            self.query(query, "deleted", "config/restored")
            package = root / "package.json"
            original = package.read_text()
            data = json.loads(original)
            data["types"] = "../outside.d.ts"
            package.write_text(json.dumps(data))
            self.rejected(self.request(query), "config/external-package-metadata")
            package.write_text(original)
            self.query(query, "deleted", "config/package-restored")
            # This new ancestor lookup is outside the registered inventory and
            # must invalidate a warm generation even with unchanged sources.
            ancestor = root.parent / "node_modules"
            ancestor.mkdir()
            try:
                self.rejected(
                    self.request(query), "config/ancestor-node-modules-created"
                )
            finally:
                ancestor.rmdir()
            self.query(query, "deleted", "config/ancestor-restored")
            alias = Path(self.setup["alias_root"])
            config = alias / "tsconfig.json"
            for target in ("left", "right"):
                data = json.loads(config.read_text())
                data["compilerOptions"]["paths"]["@selected"] = [f"./src/{target}.ts"]
                config.write_text(json.dumps(data))
                wanted = [(f"src/{target}.ts", 1, 17, 1, 25)]
                for mode in ("direct", "code_mode"):
                    self.exact(
                        self.request(
                            query,
                            cwd=str(alias),
                            path="src/use.ts",
                            operation="definition",
                            position={"line": 2, "column": 1},
                            source=mode,
                        ),
                        wanted,
                        f"config/alias-{target}/{mode}",
                    )
        else:
            model = root / "rust-project.json"
            model.write_text(
                json.dumps(
                    {
                        "crates": [
                            {
                                "root_module": "../outside.rs",
                                "edition": "2021",
                                "deps": [],
                                "is_workspace_member": True,
                            }
                        ]
                    }
                )
            )
            self.rejected(self.request(query), "config/external-model-created")
            model.unlink()
            self.query(query, "deleted", "config/model-deleted")

    def source_smoke(self):
        # Existing base91 local declarations qualify without a sysroot claim.
        root = Path(self.setup["source_root"])
        path = root / "src/lib.rs"
        before = shared.file_identity(path)
        source = path.read_text()
        symbol = "ENCTAB"
        definition = re.search(r"const ENCTAB\b", source)
        use = re.search(r"\bENCTAB\[", source)
        if not definition or not use:
            raise ValueError(
                "source smoke requires genuine existing base91 ENCTAB definition/use"
            )

        def coordinate(offset):
            return {
                "line": source.count("\n", 0, offset) + 1,
                "column": offset - source.rfind("\n", 0, offset),
            }

        start = coordinate(definition.start() + len("const "))
        wanted = [
            (
                "src/lib.rs",
                start["line"],
                start["column"],
                start["line"],
                start["column"] + len(symbol),
            )
        ]
        for mode in ("direct", "code_mode"):
            self.exact(
                self.request(
                    self.corpus.ORACLE["queries"][0],
                    cwd=str(root),
                    path="src/lib.rs",
                    operation="definition",
                    position=coordinate(use.start()),
                    source=mode,
                ),
                wanted,
                "source-smoke/base91/" + mode,
            )
        self.check(
            "source-smoke/file_unchanged",
            before == shared.file_identity(path),
            source=before,
        )

    def run(self):
        initial = self.snapshot("before_queries")
        if not initial["bounded"] or not initial["descendants_same_cgroup"]:
            raise RuntimeError("finite memory/pid cgroup prerequisite failed")
        self.check("cold/no_workers", not initial["workers"])
        queries = self.corpus.ORACLE["queries"]
        self.check(
            "cold/reference_first", queries[0]["id"] == "cold_primary_references"
        )
        baseline = self.corpus.baseline(1)
        base = [self.query(query, "base", "base") for query in queries]
        self.check(
            "latency/cold",
            base[0]["total_attempt_latency_ms"] <= self.limits["cold_ms"],
        )
        # B is independently cold and also starts with complete reverse refs.
        other = [self.query(query, "base", "base", root_b=True) for query in queries]
        self.check(
            "latency/cold_b",
            other[0]["total_attempt_latency_ms"] <= self.limits["cold_ms"],
        )
        warm = [
            self.query(queries[i % len(queries)], "base", f"warm-{i:02}")
            for i in range(self.limits["warm_queries"])
        ]
        p95 = shared.percentile([row["total_attempt_latency_ms"] for row in warm], 0.95)
        self.check("latency/warm_p95", p95 <= self.limits["warm_p95_ms"], actual_ms=p95)
        before = self.snapshot("before_cancel")
        with ThreadPoolExecutor(max_workers=2) as executor:
            cancelling = executor.submit(
                self.call, self.request(queries[0], cancel_after_ms=1), "cancel/request"
            )
            surviving = executor.submit(
                self.query, queries[0], "base", "cancel/survivor"
            )
            cancelled, survivor = cancelling.result(), surviving.result()
        after = self.snapshot("after_cancel")
        identities = lambda snapshot: {
            (row["pid"], row["start_ticks"]) for row in snapshot["workers"]
        }
        self.check(
            "cancel/status",
            isinstance(cancelled["response"], dict)
            and cancelled["response"].get("status") == "cancelled",
            actual=cancelled["response"],
        )
        self.check(
            "cancel/return", cancelled["wall_latency_ms"] <= self.limits["cancel_ms"]
        )
        self.check(
            "cancel/same_surviving_workers",
            bool(identities(before))
            and identities(before) == identities(after)
            and survivor["correct"],
        )
        for scenario in ("changed", "deleted"):
            self.corpus.set_scenario(Path(self.setup["root_a"]), scenario)
            refreshed = [self.query(query, scenario, scenario) for query in queries]
            self.check(
                "latency/" + scenario,
                max(row["total_attempt_latency_ms"] for row in refreshed)
                <= self.limits["refreshed_ms"],
            )
            for query in queries:
                self.query(query, "base", scenario, root_b=True)
            self.snapshot(scenario)
        for mode in ("direct", "code_mode"):
            for query in queries:
                self.query(query, "deleted", "mode/" + mode, source=mode)
            for budget in (shared.MAX_BYTES, 128, 64, 4, 1, 0):
                self.call(
                    self.request(queries[0], source=mode, budget=budget),
                    f"budget/{mode}/{budget}",
                )
            for access in ("remote", "restricted"):
                self.rejected(
                    self.request(queries[0], source=mode, access=access),
                    f"access/{mode}/{access}",
                )
        for name, overrides in (
            ("unknown_root", {"cwd": str(Path(self.setup["root_a"]).parent)}),
            (
                "traversal",
                {"path": "../worktree-b/src/primary." + self.corpus.EXTENSION},
            ),
            (
                "absent_socket",
                {
                    "socket_path": str(
                        Path(self.setup["socket_path"]).with_name("absent.sock")
                    )
                },
            ),
        ):
            self.rejected(self.request(queries[0], **overrides), name)
        empty = next(query for query in queries if query["symbol"] is None)
        row = self.exact(
            self.request(empty, cwd=self.setup["cold_empty_root"]),
            [],
            "cold-empty/new-worker",
        )
        self.check(
            "latency/cold_empty",
            row["total_attempt_latency_ms"] <= self.limits["cold_ms"],
        )
        self.config_changes()
        self.negative_cases()
        if "source_root" in self.setup:
            self.source_smoke()
        self.snapshot("final")
        # Compare precisely one base variant/round, never both baseline roots
        # against just one navigation root.
        baseline_bytes = sum(
            row["total_bytes"]
            for row in baseline["rows"]
            if row["scenario"] == "base" and row["variant"] == "base"
        )
        navigation_bytes = sum(row["response_bytes"] for row in base)
        ratio = navigation_bytes / baseline_bytes
        self.check(
            "bytes/baseline_ratio",
            ratio <= self.limits["navigation_baseline_byte_ratio"],
            actual=ratio,
        )
        return self.result(
            baseline=baseline,
            summary={
                "warm_p95_ms": p95,
                "base_navigation_bytes": navigation_bytes,
                "base_rg_read_bytes": baseline_bytes,
                "byte_ratio": ratio,
            },
        )

    def result(self, **extra):
        return {
            "scope": "actual native example ToolExecutor + broker + pinned language server; no live model-session E2E",
            "setup": self.setup,
            "harness": shared.file_identity(Path(__file__)),
            "shared_harness": shared.file_identity(
                REPO / "scripts/nas-lsp-acceptance.py"
            ),
            "source_head": shared.git_head(),
            "driver": shared.file_identity(self.driver),
            "server": shared.file_identity(self.executable),
            "python": platform.python_version(),
            "checks": self.checks,
            "checks_passed": sum(row["passed"] for row in self.checks),
            "checks_total": len(self.checks),
            "rows": self.rows,
            "process_snapshots": self.snapshots,
            "token_note": "ceil(UTF-8 response-content bytes/4), not model token counts",
            "additional_required_evidence": [
                "unchanged Python real regression",
                "deterministic malformed/oversized transport, readiness, saturation, eviction and pressure tests",
                "actual production core Direct/Code Mode adapter tests",
                "independent review and exact source/build provenance",
            ],
            **extra,
        }


def exercise(args):
    tools = json.loads(args.toolchains.read_text())
    directory = args.output_dir.resolve()
    setup = prepare(directory, args.language, tools, args.source_root)
    setup["build_provenance"] = {
        "broker_build_head": args.broker_build_head,
        "driver_build_head": args.driver_build_head,
        "receipts": [shared.file_identity(path) for path in args.build_receipt],
    }
    setup["toolchain_manifest"] = shared.file_identity(args.toolchains)
    if args.language == "rust":
        setup["workspace_crash_reports_before"] = crash_reports(setup)
    config = registry(setup, directory)
    setup["registry"] = shared.file_identity(config)
    save(directory / "setup.json", setup)
    runner = Runner(
        setup,
        args.driver.resolve(),
        Path(tools[args.language]["command"]),
        directory / "receipt.json",
    )
    pins = tools[args.language]
    pinned_files = [
        (
            pins["command"],
            pins["node_sha256"]
            if args.language == "typescript"
            else pins["binary_sha256"],
        )
    ]
    if args.language == "typescript":
        pinned_files += [
            (pins["language_server_js"], pins["language_server_js_sha256"]),
            (pins["tsserver_js"], pins["tsserver_js_sha256"]),
        ]
    else:
        pinned_files += [(pins["rustc"], pins["rustc_sha256"])]
    for path, expected in pinned_files:
        runner.check(
            "pin/before/" + path, shared.file_identity(Path(path))["sha256"] == expected
        )
    if not all(check["passed"] for check in runner.checks):
        raise ValueError("pinned artifact hash mismatch before launch")
    process, result = None, {}
    trace = directory / "execve.log"
    try:
        with (directory / "broker.log").open("x") as log:
            # -D keeps the actual broker as our child. The independent tracer
            # observes descendants without proxying registered executables.
            process = subprocess.Popen(
                [
                    "strace",
                    "-D",
                    "-f",
                    "-qq",
                    "-s",
                    "4096",
                    "-e",
                    "trace=execve",
                    "-o",
                    str(trace),
                    str(args.broker.resolve()),
                    "--config",
                    str(config),
                ],
                stdout=log,
                stderr=log,
                start_new_session=True,
            )
            runner.broker_pid = process.pid
            deadline = time.monotonic() + 5
            while True:
                try:
                    with socket.socket(socket.AF_UNIX) as connection:
                        connection.connect(setup["socket_path"])
                    break
                except (FileNotFoundError, ConnectionRefusedError):
                    if process.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError(
                            "broker startup failed; inspect preserved log"
                        )
                    time.sleep(0.02)
            result = runner.run()
    except Exception as error:
        result = runner.result(error=f"{type(error).__name__}: {error}")
    finally:
        if process is not None:
            started = time.perf_counter_ns()
            process.send_signal(signal.SIGTERM)
            forced = False
            try:
                process.wait(timeout=5.25)
            except subprocess.TimeoutExpired:
                forced = True
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            elapsed = (time.perf_counter_ns() - started) / 1_000_000
            runner.check(
                "shutdown/5s", not forced and elapsed <= 5_000, actual_ms=elapsed
            )
            runner.check(
                "shutdown/exit_success",
                process.returncode == 0,
                returncode=process.returncode,
            )
            survivors = []
            for worker in {
                row["pid"]: row for snap in runner.snapshots for row in snap["workers"]
            }.values():
                try:
                    text = Path(f"/proc/{worker['pid']}/stat").read_text()
                    if (
                        int(text[text.rindex(")") + 2 :].split()[19])
                        == worker["start_ticks"]
                    ):
                        survivors.append(worker["pid"])
                except FileNotFoundError:
                    pass
            runner.check(
                "shutdown/no_observed_worker_survivors",
                not survivors,
                survivors=survivors,
            )
            result["shutdown"] = {
                "elapsed_ms": elapsed,
                "forced": forced,
                "returncode": process.returncode,
            }
        if trace.exists():
            text = trace.read_text()
            commands = re.findall(r'execve\("([^"\n]+)"', text)
            allowed = {str(args.broker.resolve()), tools[args.language]["command"]}
            if args.language == "rust":
                allowed.add(tools["rust"]["rustc"])
            unexpected = sorted(set(commands) - allowed)
            runner.check(
                "execve/registered_runtime_only",
                bool(commands) and not unexpected,
                unexpected=unexpected,
                commands=commands,
            )
            result["execve_trace"] = shared.file_identity(trace)
        else:
            runner.check("execve/trace_present", False)
        for path, expected in pinned_files:
            runner.check(
                "pin/after/" + path,
                shared.file_identity(Path(path))["sha256"] == expected,
            )
        if args.language == "rust":
            after = crash_reports(setup)
            runner.check(
                "crash_reports/no_workspace_side_effects",
                after == setup["workspace_crash_reports_before"],
                before=setup["workspace_crash_reports_before"],
                after=after,
            )
        result.update(
            checks=runner.checks,
            checks_passed=sum(row["passed"] for row in runner.checks),
            checks_total=len(runner.checks),
            broker=shared.file_identity(args.broker),
        )
        save(directory / "receipt.json", result)
    passed = "error" not in result and result["checks_passed"] == result["checks_total"]
    print(
        shared.compact(
            {
                "status": "passed" if passed else "failed",
                "checks_passed": result["checks_passed"],
                "checks_total": result["checks_total"],
                "receipt": str(directory / "receipt.json"),
                "error": result.get("error"),
            }
        )
    )
    return passed


class HarnessTests(unittest.TestCase):
    def test_exact_oracles_and_first_query(self):
        for corpus in CORPORA.values():
            self.assertEqual(corpus.validate()["audited_queries"], 112)
            self.assertEqual(
                corpus.ORACLE["queries"][0]["id"], "cold_primary_references"
            )

    def test_variant_ranges_and_duplicates(self):
        for corpus in CORPORA.values():
            query = corpus.ORACLE["queries"][0]
            a, b = (
                keys(corpus.expected(query)),
                keys(corpus.expected(query, variant="shifted")),
            )
            self.assertEqual(b, sorted((p, l + 2, c, e + 2, d) for p, l, c, e, d in a))
        self.assertEqual(
            len(keys([{"path": "a", "line": 1, "column": 1, "end_column": 2}] * 2)), 2
        )

    def test_thresholds_fixed(self):
        for language in CORPORA:
            self.assertEqual(THRESHOLDS[language]["cold_ms"], 10_000)
            self.assertEqual(THRESHOLDS[language]["response_bytes"], 16_384)
            self.assertEqual(THRESHOLDS[language]["transitional_followups"], 2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("self-test")
    launch = commands.add_parser("exercise")
    launch.add_argument("--language", choices=CORPORA, required=True)
    for name in ("broker", "driver", "toolchains", "output-dir"):
        launch.add_argument("--" + name, type=Path, required=True)
    launch.add_argument(
        "--source-root", type=Path, help="existing base91 root, Rust only"
    )
    launch.add_argument("--broker-build-head", required=True)
    launch.add_argument("--driver-build-head", required=True)
    launch.add_argument("--build-receipt", type=Path, action="append", default=[])
    args = parser.parse_args()
    if args.command == "self-test":
        raise SystemExit(
            not unittest.TextTestRunner(verbosity=2)
            .run(unittest.defaultTestLoader.loadTestsFromTestCase(HarnessTests))
            .wasSuccessful()
        )
    if args.source_root and args.language != "rust":
        parser.error("source smoke currently covers Rust base91 local symbols only")
    raise SystemExit(0 if exercise(args) else 1)


if __name__ == "__main__":
    main()
