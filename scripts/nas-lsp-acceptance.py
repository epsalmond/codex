#!/usr/bin/env python3
"""Process-real NAS navigation acceptance; see .codex/design/nas-lsp-broker.md.

The driver must call the installed semantic-navigation ToolExecutor, not speak
broker RPC directly. One stdin JSON request produces {"content": "exact tool
response text"} on stdout. Driver metadata is never counted as tool output.
This runner does not install services or change cgroup limits.
"""

from __future__ import annotations

import argparse
import ast
from concurrent.futures import ThreadPoolExecutor
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time
import unittest
import uuid


REPO = Path(__file__).resolve().parent.parent
FIXTURE = REPO / "scripts/fixtures/nas-lsp/corpus.py"
spec = importlib.util.spec_from_file_location("nas_lsp_corpus", FIXTURE)
corpus = importlib.util.module_from_spec(spec)
spec.loader.exec_module(corpus)
MAX_BYTES = 16_384
THRESHOLDS = {
    "exact_location_sets_percent": 100,
    "cold_ms": 10_000,
    "warm_queries": 30,
    "warm_p95_ms": 1_000,
    "refreshed_ms": 10_000,
    "cancel_ms": 500,
    "response_bytes": MAX_BYTES,
    "response_locations": 100,
    "navigation_baseline_byte_ratio": 0.60,
    "memory_max_bytes": 1_073_741_824,
    "pids_max": 64,
}


def compact(value) -> str:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def percentile(values: list[float], quantile: float) -> float:
    return sorted(values)[max(0, math.ceil(len(values) * quantile) - 1)]


def write_json(path: Path, value) -> None:
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n")


def file_identity(path: Path) -> dict:
    return {
        "path": str(path.resolve()),
        "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
    }


def git_head(cwd: Path = REPO) -> str:
    return subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=cwd, text=True
    ).strip()


def prepare(destination: Path, socket_path: Path | None, source_root: Path | None = None) -> dict:
    destination.mkdir(mode=0o700, parents=True, exist_ok=False)
    a = corpus.materialize(destination / "worktree-a")
    b = destination / "worktree-b"
    # Use actual linked worktrees sharing a common Git directory. Independent
    # copies would not detect an incorrect worker key based on git-common-dir.
    def git(*arguments: str) -> str:
        return subprocess.check_output(["git", "-C", str(a), *arguments], text=True, stderr=subprocess.PIPE).strip()
    git("init", "--quiet")
    git("add", "--all")
    git("-c", "user.name=NAS LSP acceptance", "-c", "user.email=nas-lsp-acceptance@localhost",
        "-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "Fixed acceptance corpus")
    git("worktree", "add", "--detach", "--quiet", str(b))
    common_a = Path(git("rev-parse", "--path-format=absolute", "--git-common-dir")).resolve()
    common_b = Path(subprocess.check_output(["git", "-C", str(b), "rev-parse", "--path-format=absolute", "--git-common-dir"], text=True).strip()).resolve()
    if common_a != common_b:
        raise RuntimeError("acceptance roots do not share a common Git directory")
    # A different definition range makes accidental shared indexing observable.
    primary = b / "pkg/primary.py"
    primary.write_bytes(b"# Independent worktree B definition range.\n" + primary.read_bytes())
    # Independent regression: an excluded directory can still contain imports
    # read by the server. Its saved changes must invalidate a warm generation.
    freshness_probe = destination / "server-input-probe"
    (freshness_probe / "hidden").mkdir(parents=True)
    (freshness_probe / "hidden/__init__.py").write_text("")
    (freshness_probe / "a.py").write_text("class A:\n    value: int = 1\n")
    (freshness_probe / "b.py").write_text('class B:\n    value: str = "b"\n')
    (freshness_probe / "hidden/bridge.py").write_text("from a import A\ndef make() -> A:\n    return A()\n")
    (freshness_probe / "main.py").write_text("from hidden.bridge import make\nmake().value\n")
    result = {
        "version": 1,
        "source_head": git_head(),
        "corpus_sha256": corpus.corpus_digest(),
        "root_a": str(a),
        "root_b": str(b),
        "freshness_probe_root": str(freshness_probe),
        "shared_git_common_dir": str(common_a),
        "fixture_git_commit": git("rev-parse", "HEAD"),
        "root_b_transform": {"pkg/primary.py": {"line_offset": 1}},
        "socket_path": str(socket_path or Path(f"/tmp/codex-daemon-{os.getuid()}") / f"navigation-acceptance-{uuid.uuid4().hex[:12]}.sock"),
        "thresholds": THRESHOLDS,
    }
    if source_root is not None:
        result["source_root"] = str(source_root.resolve(strict=True))
        result["source_root_head"] = git_head(source_root)
    write_json(destination / "setup.json", result)
    return result


def write_registry(setup: dict, server: Path) -> Path:
    """Create only a uniquely named private registry; never alter shared modes."""
    parent = Path(setup["socket_path"]).parent
    if parent != Path(f"/tmp/codex-daemon-{os.getuid()}"):
        raise ValueError("socket must be directly inside codex-uds reserved directory")
    try:
        parent.mkdir(mode=0o700)
    except FileExistsError:
        pass
    metadata = parent.lstat()
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.getuid() or stat.S_IMODE(metadata.st_mode) != 0o700:
        raise ValueError("reserved daemon directory is not owner-owned real mode 0700")
    path = parent / f"registry-acceptance-{uuid.uuid4().hex[:12]}.toml"
    lines = [f"socket_path = {json.dumps(setup['socket_path'])}", "", "[limits]",
             "max_workers = 2", "max_queue_per_worker = 4", "max_queries = 8",
             "max_connections = 32", "idle_ttl_secs = 60",
             "memory_max_bytes = 1073741824", "pids_max = 64"]
    roots = [("a", setup["root_a"]), ("b", setup["root_b"])]
    if "freshness_probe_root" in setup:
        roots.append(("freshness", setup["freshness_probe_root"]))
    if "source_root" in setup:
        roots.append(("source", setup["source_root"]))
    for name, root in roots:
        lines += ["", "[[workspaces]]", f'id = "acceptance-{name}"',
                  f"root = {json.dumps(root)}", 'language = "python"',
                  f"executable = {json.dumps(str(server.resolve(strict=True)))}",
                  'args = ["lsp", "--indexing-mode", "lazy-blocking", "--threads", "1"]',
                  'source_extensions = ["py", "pyi"]',
                  'exclude_dirs = [".git", "target", "node_modules", ".venv", "hidden"]' if name == "freshness" else 'exclude_dirs = [".git", "target", "node_modules", ".venv"]',
                  'readiness = "pyrefly_lazy_blocking"']
    with path.open("x") as output:
        path.chmod(0o600)
        output.write("\n".join(lines) + "\n")
    return path


def transformed_key(location: dict, root_b: bool) -> tuple:
    offset = int(root_b and location["path"] == "pkg/primary.py")
    return (
        location["path"], location["line"] + offset, location["column"],
        location["line"] + offset, location["end_column"],
    )


def response_keys(response: dict) -> list[tuple]:
    # Keep duplicates: silently deduplicating here would hide a broker failure.
    return sorted(
        (loc["path"], loc["start"]["line"], loc["start"]["column"],
         loc["end"]["line"], loc["end"]["column"])
        for loc in response["locations"]
    )


def process_snapshot(broker_pid: int, executable: Path) -> dict:
    """One /proc observation; this is evidence, not a process-completion wait."""
    processes = {}
    for path in Path("/proc").iterdir():
        if not path.name.isdecimal():
            continue
        try:
            stat = (path / "stat").read_text()
            tail = stat[stat.rindex(")") + 2:].split()
            processes[int(path.name)] = {
                "pid": int(path.name), "ppid": int(tail[1]),
                "start_ticks": int(tail[19]),
            }
        except (OSError, ValueError, IndexError):
            continue
    if broker_pid not in processes:
        raise RuntimeError(f"broker pid {broker_pid} is absent")
    descendants = {broker_pid}
    while True:
        found = {pid for pid, row in processes.items() if row["ppid"] in descendants}
        if found <= descendants:
            break
        descendants |= found
    rows = []
    for pid in sorted(descendants):
        path = Path("/proc") / str(pid)
        try:
            row = processes[pid] | {
                "executable": str((path / "exe").resolve(strict=True)),
                "cgroup": (path / "cgroup").read_text().strip(),
                "command": (path / "cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace").strip(),
            }
            rows.append(row)
        except OSError:
            continue
    broker = next(row for row in rows if row["pid"] == broker_pid)
    membership = broker["cgroup"].splitlines()
    if len(membership) != 1 or not membership[0].startswith("0::/"):
        raise RuntimeError("broker is not in an unambiguous cgroup v2")
    relative = membership[0].removeprefix("0::")
    cgroup = Path("/sys/fs/cgroup") / relative.lstrip("/")
    memory = (cgroup / "memory.max").read_text().strip()
    pids = (cgroup / "pids.max").read_text().strip()
    bounded = (
        memory != "max" and 0 < int(memory) <= THRESHOLDS["memory_max_bytes"]
        and pids != "max" and 0 < int(pids) <= THRESHOLDS["pids_max"]
    )
    return {
        "cgroup": str(cgroup), "memory_max": memory, "pids_max": pids,
        "memory_peak": (cgroup / "memory.peak").read_text().strip() if (cgroup / "memory.peak").exists() else None,
        "bounded": bounded,
        "descendants_same_cgroup": all(row["cgroup"] == broker["cgroup"] for row in rows),
        "processes": rows,
        "workers": [row for row in rows if row["executable"] == str(executable.resolve())],
    }


class Runner:
    def __init__(self, setup: dict, driver: Path, broker_pid: int, executable: Path, receipt: Path):
        self.setup, self.driver, self.broker_pid = setup, driver, broker_pid
        self.executable, self.receipt = executable, receipt
        self.rows, self.checks, self.snapshots = [], [], []

    def check(self, name: str, passed: bool, **evidence) -> None:
        self.checks.append({"name": name, "passed": passed, **evidence})

    def snapshot(self, phase: str) -> dict:
        snap = {"phase": phase, **process_snapshot(self.broker_pid, self.executable)}
        self.snapshots.append(snap)
        self.check(f"cgroup/{phase}", snap["bounded"] and snap["descendants_same_cgroup"])
        self.check(f"workers/{phase}", len(snap["workers"]) <= 2, actual=len(snap["workers"]))
        return snap

    def call(self, request: dict, label: str) -> dict:
        started = time.perf_counter_ns()
        completed = subprocess.run(
            [str(self.driver)], input=compact(request) + "\n", text=True,
            capture_output=True, timeout=20, check=False,
        )
        elapsed = (time.perf_counter_ns() - started) / 1_000_000
        if completed.returncode:
            raise RuntimeError(f"driver {label} exited {completed.returncode}: {completed.stderr[-4096:]}")
        envelope = json.loads(completed.stdout)
        content = envelope["content"]
        if not isinstance(content, str):
            raise ValueError("driver content must be exact tool response text")
        count = len(content.encode("utf-8"))
        response = json.loads(content) if content else None
        row = {
            "label": label, "request": request, "content": content,
            "response": response, "response_bytes": count,
            "token_estimate_ceil_bytes_div_4": math.ceil(count / 4),
            "wall_latency_ms": elapsed,
            "driver_metadata": {key: value for key, value in envelope.items() if key != "content"},
        }
        self.rows.append(row)
        # Code Mode intentionally ignores host truncation budgets. The native
        # driver computes this using the production response_byte_budget seam.
        effective = min(MAX_BYTES, envelope.get("effective_response_budget", request["budget"]))
        self.check(f"bytes/{label}", count <= effective, actual=count, effective_budget=effective)
        if "serialized_bytes" in envelope:
            self.check(f"byte_metadata/{label}", count == envelope["serialized_bytes"])
        if isinstance(response, dict):
            self.check(f"count/{label}", len(response.get("locations", [])) <= 100)
        return row

    def request(self, query: dict, root_b=False, **overrides) -> dict:
        at = corpus.ORACLE["locations"][query["at"]]
        key = transformed_key(at, root_b)
        return {
            "socket_path": self.setup["socket_path"],
            "cwd": self.setup["root_b" if root_b else "root_a"],
            "path": at["path"], "operation": query["operation"],
            "position": {"line": key[1], "column": key[2]},
            "source": "direct", "budget": MAX_BYTES, "access": "allowed",
            **overrides,
        }

    def query(self, query: dict, scenario: str, phase: str, root_b=False, **overrides) -> dict:
        label = f"{phase}/{'b' if root_b else 'a'}/{query['id']}"
        row = self.call(self.request(query, root_b, **overrides), label)
        transitional = []
        while (
            isinstance(row["response"], dict)
            and row["response"].get("status") in ("stale", "indexing")
            and len(transitional) < 2
        ):
            transitional.append(row["label"])
            row = self.call(self.request(query, root_b, **overrides), f"{label}/followup-{len(transitional)}")
        wanted = sorted(transformed_key(loc, root_b) for loc in corpus.expected(query, scenario))
        response = row["response"]
        correct = (
            isinstance(response, dict) and response.get("status") == "ok"
            and not response.get("truncated", False)
            and response_keys(response) == wanted
        )
        row.update({"correct": correct, "expected": wanted, "scenario": scenario, "transitional_queries": transitional})
        self.check(f"correct/{label}", correct)
        return row

    def source_smoke(self) -> None:
        """Audit real existing source bindings without changing those files."""
        root = Path(self.setup["source_root"])
        layout_path, target_path = root / "codex_package/layout.py", root / "codex_package/targets.py"
        before = [file_identity(path) for path in (layout_path, target_path)]
        layout, targets = layout_path.read_text(), target_path.read_text()
        for symbol in ("TargetSpec", "PackageInputs"):
            use = next(node for node in ast.walk(ast.parse(layout)) if isinstance(node, ast.Name) and node.id == symbol)
            definition = next(node for node in ast.walk(ast.parse(targets)) if isinstance(node, ast.ClassDef) and node.name == symbol)
            use_line = layout.splitlines()[use.lineno - 1]
            column = len(use_line.encode()[:use.col_offset].decode()) + 1
            target_line = targets.splitlines()[definition.lineno - 1]
            target_column = target_line.index(symbol, definition.col_offset) + 1
            wanted = [("codex_package/targets.py", definition.lineno, target_column, definition.lineno, target_column + len(symbol))]
            for source in ("direct", "code_mode"):
                request = self.request(corpus.ORACLE["queries"][0], cwd=str(root), path="codex_package/layout.py", operation="definition", position={"line": use.lineno, "column": column}, source=source)
                row = self.call(request, f"source-smoke/{symbol}/{source}")
                response = row["response"]
                correct = isinstance(response, dict) and response.get("status") == "ok" and not response.get("truncated") and response_keys(response) == wanted
                row.update({"correct": correct, "expected": wanted, "source_files": before})
                self.check(row["label"], correct)
        self.check("source-smoke/files_unchanged", before == [file_identity(path) for path in (layout_path, target_path)])
        self.snapshot("source_smoke")

    def excluded_import_smoke(self) -> None:
        """An import read by Pyrefly remains freshness-relevant when excluded."""
        root = Path(self.setup["freshness_probe_root"])
        main = (root / "main.py").read_text()
        bridge = root / "hidden/bridge.py"
        position = {"line": 2, "column": main.splitlines()[1].index("value") + 1}
        for phase, module, symbol in (("before", "a", "A"), ("after", "b", "B")):
            bridge.write_text(f"from {module} import {symbol}\ndef make() -> {symbol}:\n    return {symbol}()\n")
            target = next(node.target for node in ast.walk(ast.parse((root / f"{module}.py").read_text())) if isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name) and node.target.id == "value")
            wanted = [(f"{module}.py", target.lineno, target.col_offset + 1, target.end_lineno, target.end_col_offset + 1)]
            for source in ("direct", "code_mode"):
                request = self.request(corpus.ORACLE["queries"][0], cwd=str(root), path="main.py", operation="definition", position=position, source=source)
                label = f"excluded-import/{phase}/{source}"
                row = self.call(request, label)
                for attempt in range(2):
                    if not isinstance(row["response"], dict) or row["response"].get("status") not in ("stale", "indexing"):
                        break
                    row = self.call(request, f"{label}/followup-{attempt + 1}")
                response = row["response"]
                correct = isinstance(response, dict) and response.get("status") == "ok" and not response.get("truncated") and response_keys(response) == wanted
                row.update({"correct": correct, "expected": wanted, "saved_import": file_identity(bridge)})
                self.check(label, correct)
                self.check(f"latency/{label}", row["wall_latency_ms"] <= THRESHOLDS["refreshed_ms"])
        self.snapshot("excluded_import")

    def run(self) -> dict:
        initial = self.snapshot("before_queries")
        if not initial["bounded"] or not initial["descendants_same_cgroup"]:
            raise RuntimeError("dedicated memory/pid cgroup prerequisite failed")
        self.check("cold/no_preexisting_workers", not initial["workers"])
        queries = corpus.ORACLE["queries"]
        baseline = corpus.baseline(1)
        base_rows = [self.query(query, "base", "base") for query in queries]
        self.check("latency/cold", base_rows[0]["wall_latency_ms"] <= THRESHOLDS["cold_ms"])
        # Empty answer is probed on a fresh worker, not merely a warmed index.
        empty = next(query for query in queries if query["symbol"] is None)
        cold_empty = self.query(empty, "base", "cold_empty", root_b=True)
        self.check("latency/cold_empty", cold_empty["wall_latency_ms"] <= THRESHOLDS["cold_ms"])
        for query in queries:
            self.query(query, "base", "base", root_b=True)
        warm = [self.query(queries[i % len(queries)], "base", f"warm-{i:02}") for i in range(30)]
        warm_p95 = percentile([row["wall_latency_ms"] for row in warm], .95)
        self.check("latency/warm_p95", warm_p95 <= THRESHOLDS["warm_p95_ms"], actual_ms=warm_p95, queries=len(warm))
        self.snapshot("warm")
        # Cancellation and another session share a registered warm worker.
        references = next(query for query in queries if query["operation"] == "references")
        before = self.snapshot("before_cancel")
        with ThreadPoolExecutor(max_workers=2) as executor:
            cancelling = executor.submit(self.call, self.request(references, cancel_after_ms=1), "cancel/request")
            surviving = executor.submit(self.query, queries[0], "base", "cancel/survivor")
            cancelled, survivor = cancelling.result(), surviving.result()
        after = self.snapshot("after_cancel")
        old_workers = {(row["pid"], row["start_ticks"]) for row in before["workers"]}
        new_workers = {(row["pid"], row["start_ticks"]) for row in after["workers"]}
        cancellation_observed = isinstance(cancelled["response"], dict) and cancelled["response"].get("status") == "cancelled"
        self.check("cancel/status", cancellation_observed, actual=cancelled["response"])
        self.check("cancel/return", cancelled["wall_latency_ms"] <= THRESHOLDS["cancel_ms"])
        self.check("cancel/same_surviving_processes", bool(old_workers) and old_workers == new_workers and survivor["correct"])
        for scenario in ("changed", "deleted"):
            corpus.set_scenario(Path(self.setup["root_a"]), scenario)
            refreshed = [self.query(query, scenario, scenario) for query in queries]
            self.check(f"latency/{scenario}", all(row["wall_latency_ms"] <= THRESHOLDS["refreshed_ms"] for row in refreshed))
            for query in queries:
                self.query(query, "base", scenario, root_b=True)
            self.snapshot(scenario)
        for source in ("direct", "code_mode"):
            self.query(references, "deleted", f"source/{source}", source=source)
            for budget in (MAX_BYTES, 128, 64, 4, 1, 0):
                self.call(self.request(references, source=source, budget=budget), f"budget/{source}/{budget}")
            for access in ("remote", "restricted"):
                row = self.call(self.request(queries[0], source=source, access=access), f"access/{source}/{access}")
                self.check(f"access/{source}/{access}", isinstance(row["response"], dict) and row["response"].get("status") == "unavailable" and not row["response"].get("locations"))
        for name, overrides in (
            ("unknown_root", {"cwd": str(Path(self.setup["root_a"]).parent)}),
            ("traversal", {"path": "../worktree-b/pkg/primary.py"}),
            ("absent_socket", {"socket_path": str(Path(self.setup["socket_path"]).with_name("absent.sock"))}),
        ):
            row = self.call(self.request(queries[0], **overrides), name)
            self.check(name, isinstance(row["response"], dict) and row["response"].get("status") in ("unavailable", "invalid_request") and not row["response"].get("locations"))
        baseline_bytes = sum(row["total_bytes"] for row in baseline["rows"] if row["scenario"] == "base")
        navigation_bytes = sum(row["response_bytes"] for row in base_rows)
        ratio = navigation_bytes / baseline_bytes
        self.check("bytes/baseline_ratio", ratio <= THRESHOLDS["navigation_baseline_byte_ratio"], actual=ratio)
        if "source_root" in self.setup:
            self.source_smoke()
        if "freshness_probe_root" in self.setup:
            self.excluded_import_smoke()
        self.snapshot("final")
        return self.result(baseline=baseline, summary={
            "warm_p95_ms": warm_p95, "base_navigation_bytes": navigation_bytes,
            "base_rg_read_bytes": baseline_bytes, "byte_ratio": ratio,
        })

    def result(self, **extra) -> dict:
        return {
            "scope": "native example ToolExecutor + persistent broker + real Pyrefly integration; no model-session E2E claim",
            "source_head": git_head(), "setup": self.setup,
            "harness": file_identity(Path(__file__)),
            "build_provenance": self.setup.get("build_provenance", {
                "note": "Build commits were not supplied; source_head identifies the harness checkout only.",
            }),
            "driver": file_identity(self.driver), "server": file_identity(self.executable),
            "server_version": subprocess.check_output([str(self.executable), "--version"], text=True).strip(),
            "server_args": ["lsp", "--indexing-mode", "lazy-blocking", "--threads", "1"],
            "os": platform.platform(), "python": platform.python_version(),
            "token_note": "ceil(UTF-8 response-content bytes / 4) estimates; no model tokens measured",
            "latency_note": "wall clock includes native driver process startup; baseline first pass shares OS caches",
            "checks_passed": sum(row["passed"] for row in self.checks),
            "checks_total": len(self.checks), "checks": self.checks,
            "rows": self.rows, "process_snapshots": self.snapshots,
            "additional_required_evidence": [
                "deterministic indexing, timeout and malformed/oversized LSP frames",
                "unavailable executable and registered commands only",
                "worker/queue/pool saturation, bounded shutdown and idle eviction",
                "actual oversized locations and escaped paths under Direct and Code Mode",
                "cgroup prerequisite rejection outside dedicated bounded scope",
                "core registration/host verdict and Code Mode adapter tests",
                "independent review and matching source/binary build provenance",
            ],
            **extra,
        }


def exercise(args) -> tuple[dict, bool]:
    """Own one broker's complete lifecycle inside the caller's bounded scope."""
    setup = prepare(args.output_dir.resolve(), None, args.source_root)
    directory = args.output_dir.resolve()
    setup["build_provenance"] = {
        "note": "Operator-provided binary build commits; hashes identify the actual executed files. source_head identifies the harness checkout.",
        "broker_build_head": args.broker_build_head,
        "driver_build_head": args.driver_build_head,
        "build_receipts": [file_identity(path) for path in args.build_receipt],
    }
    registry = write_registry(setup, args.server)
    setup["registry"] = file_identity(registry)
    write_json(directory / "setup.json", setup)
    runner = Runner(setup, args.driver.resolve(), 0, args.server.resolve(), directory / "receipt.json")
    process = None
    result = {}
    try:
        with (directory / "broker.log").open("x") as log:
            process = subprocess.Popen([str(args.broker.resolve()), "--config", str(registry)], stdout=log, stderr=log, start_new_session=True)
            runner.broker_pid = process.pid
            # This reusable supervisor owns bounded startup/backoff. The agent
            # launches it once and waits for its native terminal completion.
            deadline = time.monotonic() + 5
            while True:
                try:
                    with socket.socket(socket.AF_UNIX) as connection:
                        connection.connect(setup["socket_path"])
                    break
                except (FileNotFoundError, ConnectionRefusedError):
                    if process.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError("broker failed startup; inspect broker.log")
                    time.sleep(.02)
            result = runner.run()
    except Exception as exc:
        result = runner.result(error=f"{type(exc).__name__}: {exc}")
    finally:
        if process is not None:
            started = time.perf_counter_ns()
            process.send_signal(signal.SIGTERM)
            forced = False
            try:
                process.wait(timeout=5.25)
            except subprocess.TimeoutExpired:
                forced = True
                process.kill()
                process.wait()
            elapsed = (time.perf_counter_ns() - started) / 1_000_000
            result["shutdown"] = {"elapsed_ms": elapsed, "forced_kill": forced, "returncode": process.returncode}
            runner.check("shutdown/broker_5s", not forced and elapsed <= 5_000)
            runner.check("shutdown/exit_success", process.returncode == 0, returncode=process.returncode)
            observed = [row for snap in runner.snapshots for row in snap["workers"]]
            survivors = []
            for worker in {row["pid"]: row for row in observed}.values():
                try:
                    text = Path(f"/proc/{worker['pid']}/stat").read_text()
                    ticks = int(text[text.rindex(")") + 2:].split()[19])
                    if ticks == worker["start_ticks"]:
                        survivors.append(worker["pid"])
                except FileNotFoundError:
                    pass
            runner.check("shutdown/no_observed_worker_survivors", not survivors, survivors=survivors)
            result["checks"], result["checks_total"] = runner.checks, len(runner.checks)
            result["checks_passed"] = sum(row["passed"] for row in runner.checks)
        result["broker"] = file_identity(args.broker)
        write_json(directory / "receipt.json", result)
    passed = "error" not in result and result["checks_passed"] == result["checks_total"]
    return result, passed


class HarnessTests(unittest.TestCase):
    def test_oracle_and_independent_root_transform(self):
        self.assertEqual(corpus.validate()["audited_queries"], 36)
        query = next(q for q in corpus.ORACLE["queries"] if q["id"] == "cross_file_definition")
        loc = corpus.expected(query)[0]
        a, b = transformed_key(loc, False), transformed_key(loc, True)
        self.assertEqual(b, (a[0], a[1] + 1, a[2], a[3] + 1, a[4]))

    def test_response_duplicates_and_unicode_ranges_are_not_hidden(self):
        loc = {"path": "astral-🛰.py", "start": {"line": 1, "column": 8}, "end": {"line": 1, "column": 14}}
        self.assertEqual(len(response_keys({"locations": [loc, loc]})), 2)
        self.assertEqual(response_keys({"locations": [loc]})[0][2:], (8, 1, 14))

    def test_prepare_is_exclusive_and_preserves_fixed_corpus(self):
        with tempfile.TemporaryDirectory() as parent:
            path = Path(parent) / "run"
            prepared = prepare(path, None)
            with self.assertRaises(FileExistsError):
                prepare(path, None)
            a, b = Path(prepared["root_a"]), Path(prepared["root_b"])
            self.assertTrue((a / ".git").is_dir())
            self.assertTrue((b / ".git").is_file())
            self.assertEqual(prepared["shared_git_common_dir"], str(a / ".git"))
            original = (b / "pkg/primary.py").read_bytes()
            corpus.set_scenario(a, "deleted")
            self.assertEqual((b / "pkg/primary.py").read_bytes(), original)
            self.assertTrue((b / "pkg/transient.py").exists())

    def test_nearest_rank_p95(self):
        self.assertEqual(percentile(list(range(1, 31)), .95), 29)

    def test_exact_emitted_bytes_and_empty_output(self):
        with tempfile.TemporaryDirectory() as parent:
            driver = Path(parent) / "driver"
            driver.write_text('#!/usr/bin/env python3\nimport json,sys\nr=json.load(sys.stdin)\ns="" if r["budget"]==0 else "0"\nprint(json.dumps({"content":s}))\n')
            driver.chmod(0o700)
            runner = Runner({}, driver, 0, driver, Path(parent) / "receipt.json")
            for budget in (0, 1):
                row = runner.call({"budget": budget}, str(budget))
                self.assertEqual(row["response_bytes"], budget)
            self.assertTrue(all(check["passed"] for check in runner.checks))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("self-test")
    prep = commands.add_parser("prepare")
    prep.add_argument("--output", type=Path, required=True, help="new private directory")
    prep.add_argument("--socket-path", type=Path)
    prep.add_argument("--source-root", type=Path, help="optional real Codex scripts root, read-only smoke")
    run = commands.add_parser("run")
    run.add_argument("--setup", type=Path, required=True)
    run.add_argument("--driver", type=Path, required=True)
    run.add_argument("--broker-pid", type=int, required=True)
    run.add_argument("--server", type=Path, required=True)
    run.add_argument("--output", type=Path, required=True)
    launch = commands.add_parser("exercise", help="prepare roots, supervise one broker, save receipt; run inside dedicated scope")
    launch.add_argument("--broker", type=Path, required=True)
    launch.add_argument("--driver", type=Path, required=True)
    launch.add_argument("--server", type=Path, required=True)
    launch.add_argument("--output-dir", type=Path, required=True, help="must not exist")
    launch.add_argument("--source-root", type=Path, help="optional real Codex scripts root, read-only smoke")
    launch.add_argument("--broker-build-head", help="source commit used to build the broker, independent of the harness checkout")
    launch.add_argument("--driver-build-head", help="source commit used to build the native driver")
    launch.add_argument("--build-receipt", type=Path, action="append", default=[], help="preserved build log or receipt to hash; repeatable")
    args = parser.parse_args()
    if args.command == "self-test":
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(HarnessTests)
        raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
    if args.command == "prepare":
        print(json.dumps(prepare(args.output.resolve(), args.socket_path, args.source_root), indent=2))
        return
    if args.command == "exercise":
        result, passed = exercise(args)
        print(compact({"status": "passed" if passed else "failed", "receipt": str(args.output_dir.resolve() / "receipt.json"), "checks_passed": result.get("checks_passed", 0), "checks_total": result.get("checks_total", 0), "error": result.get("error")}))
        raise SystemExit(0 if passed else 1)
    setup = json.loads(args.setup.read_text())
    if setup["corpus_sha256"] != corpus.corpus_digest() or setup["thresholds"] != THRESHOLDS:
        parser.error("corpus or fixed thresholds changed; prepare fresh roots")
    if args.output.exists():
        parser.error("output already exists; preserve prior receipt")
    runner = Runner(setup, args.driver.resolve(), args.broker_pid, args.server.resolve(), args.output)
    try:
        result = runner.run()
    except Exception as exc:
        result = runner.result(error=f"{type(exc).__name__}: {exc}")
        write_json(args.output, result)
        print(compact({"status": "failed", "error": result["error"], "receipt": str(args.output)}))
        raise SystemExit(1)
    write_json(args.output, result)
    passed = result["checks_passed"] == result["checks_total"]
    print(compact({"status": "passed" if passed else "failed", "checks_passed": result["checks_passed"], "checks_total": result["checks_total"], "receipt": str(args.output), "additional_required_evidence": result["additional_required_evidence"]}))
    raise SystemExit(0 if passed else 1)


if __name__ == "__main__":
    main()
