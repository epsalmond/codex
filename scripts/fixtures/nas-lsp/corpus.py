#!/usr/bin/env python3
"""Broker-neutral fixture management, source oracle audit, and rg/read baseline."""

from __future__ import annotations

import argparse
import ast
import hashlib
import json
import math
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import time

HERE = Path(__file__).resolve().parent
ORACLE = json.loads((HERE / "oracle.json").read_text(encoding="utf-8"))
MARKER = ".nas-lsp-corpus.json"


def corpus_digest() -> str:
    digest = hashlib.sha256()
    for path in sorted(
        [
            HERE / "oracle.json",
            *(HERE / "workspace").rglob("*.py"),
            *(HERE / "variants").glob("*.py"),
        ]
    ):
        digest.update(str(path.relative_to(HERE)).encode())
        digest.update(b"\0")
        digest.update(path.read_bytes())
    return digest.hexdigest()


def materialize(root: Path) -> Path:
    """Create a fresh, independently writable root; never replace an existing one."""
    root = root.resolve()
    if root.exists():
        raise ValueError(f"fixture destination already exists: {root}")
    shutil.copytree(HERE / "workspace", root)
    (root / MARKER).write_text(
        json.dumps({"digest": corpus_digest(), "scenario": "base"}) + "\n"
    )
    return root


def set_scenario(root: Path, scenario: str) -> None:
    """Apply base -> changed -> deleted monotonically, preserving the warm root."""
    receipt = json.loads((root / MARKER).read_text())
    stages = list(ORACLE["scenarios"])
    if receipt["digest"] != corpus_digest():
        raise ValueError("fixture root was created from a different corpus")
    current, target = stages.index(receipt["scenario"]), stages.index(scenario)
    if target < current:
        raise ValueError("use a fresh root instead of reverting a warm fixture")
    change = ORACLE["scenarios"][scenario]
    for relative, source in change.get("replace", {}).items():
        (root / relative).write_bytes((HERE / source).read_bytes())
    for relative in change.get("delete", []):
        (root / relative).unlink(missing_ok=True)
    receipt["scenario"] = scenario
    (root / MARKER).write_text(json.dumps(receipt) + "\n")


def location_key(location: dict) -> tuple:
    return (
        location["path"],
        location["line"],
        location["column"],
        location["end_column"],
    )


def expected(query: dict, scenario: str = "base") -> list[dict]:
    symbol = query["symbol"]
    if symbol is None:
        return []
    if query["operation"] == "definition":
        ids = [ORACLE["symbols"][symbol]["definition"]]
    else:
        ids = ORACLE["scenarios"][scenario]["references"][symbol]
    return sorted([ORACLE["locations"][key] for key in ids], key=location_key)


def source_location(
    relative: str, line: str, lineno: int, byte_column: int, width: int = 6
) -> dict:
    """AST offsets are zero-based UTF-8 bytes; convert by decoding the prefix."""
    prefix = line.encode("utf-8")[:byte_column].decode("utf-8")
    character = len(prefix.encode("utf-16-le")) // 2
    return {
        "path": relative,
        "line": lineno,
        "column": len(prefix) + 1,
        "end_column": len(prefix) + 1 + width,
        "utf8_column": byte_column + 1,
        "utf8_end_column": byte_column + 1 + width,
        "lsp": {
            "start": {"line": lineno - 1, "character": character},
            "end": {"line": lineno - 1, "character": character + width},
        },
    }


def source_symbols(files: dict[str, str]) -> dict[str, dict]:
    """Resolve only this corpus's explicit `from pkg import module; module.render`.

    This independent AST check is deliberately not a general Python navigator.
    It excludes docstrings/comments and distinguishes the two imported modules.
    """
    symbols = {
        symbol: {"definition": None, "references": []} for symbol in ORACLE["symbols"]
    }
    for relative, text in sorted(files.items()):
        lines, tree = text.splitlines(), ast.parse(text, filename=relative)
        imports = {
            alias.asname or alias.name: f"pkg.{alias.name}"
            for statement in tree.body
            if isinstance(statement, ast.ImportFrom) and statement.module == "pkg"
            for alias in statement.names
        }
        for node in ast.walk(tree):
            if isinstance(node, ast.FunctionDef) and node.name == "render":
                symbol = f"{relative.removesuffix('.py').replace('/', '.')}.render"
                loc = source_location(
                    relative, lines[node.lineno - 1], node.lineno, node.col_offset + 4
                )
                symbols[symbol]["definition"] = loc
                symbols[symbol]["references"].append(loc)
            elif (
                isinstance(node, ast.Attribute)
                and node.attr == "render"
                and isinstance(node.value, ast.Name)
            ):
                symbol = f"{imports[node.value.id]}.render"
                loc = source_location(
                    relative,
                    lines[node.end_lineno - 1],
                    node.end_lineno,
                    node.end_col_offset - 6,
                )
                symbols[symbol]["references"].append(loc)
    return symbols


def resolve(query: dict, symbols: dict) -> list[dict]:
    # Derive the binding from the source at the query position, not its oracle symbol.
    at = location_key(ORACLE["locations"][query["at"]])
    matches = [
        value
        for value in symbols.values()
        if any(location_key(loc) == at for loc in value["references"])
    ]
    if not matches:
        return []
    if len(matches) != 1:
        raise ValueError(f"ambiguous corpus binding at {at}")
    value = matches[0]
    answer = (
        [value["definition"]]
        if query["operation"] == "definition"
        else value["references"]
    )
    return sorted(answer, key=location_key)


def validate() -> dict:
    audited = 0
    with tempfile.TemporaryDirectory(prefix="nas-lsp-oracle-") as temporary:
        root_a = materialize(Path(temporary) / "worktree-a")
        root_b = materialize(Path(temporary) / "worktree-b")
        untouched = {
            p.relative_to(root_b).as_posix(): p.read_bytes()
            for p in root_b.rglob("*.py")
        }
        for scenario in ORACLE["scenarios"]:
            set_scenario(root_a, scenario)
            files = {
                p.relative_to(root_a).as_posix(): p.read_text(encoding="utf-8")
                for p in root_a.rglob("*.py")
            }
            symbols = source_symbols(files)
            for query in ORACLE["queries"]:
                actual = resolve(query, symbols)
                wanted = expected(query, scenario)
                if actual != wanted:
                    raise ValueError(
                        f"oracle mismatch: {scenario}/{query['id']}: {actual} != {wanted}"
                    )
                audited += 1
            # Also audit changed-only positions, which no fixed query uses.
            for symbol, ids in ORACLE["scenarios"][scenario]["references"].items():
                wanted = sorted(
                    [ORACLE["locations"][key] for key in ids], key=location_key
                )
                if sorted(symbols[symbol]["references"], key=location_key) != wanted:
                    raise ValueError(
                        f"reference inventory mismatch: {scenario}/{symbol}"
                    )
        if untouched != {
            p.relative_to(root_b).as_posix(): p.read_bytes()
            for p in root_b.rglob("*.py")
        }:
            raise ValueError("changing root A changed root B")
        # Explicitly audit the zero-result comment coordinates, excluded by AST.
        loc = ORACLE["locations"]["no_symbol"]
        line = untouched[loc["path"]].decode().splitlines()[loc["line"] - 1]
        if line[loc["column"] - 1 : loc["end_column"] - 1] != "no_symbol_here":
            raise ValueError("zero-result comment position drifted")
        if source_location(loc["path"], line, loc["line"], 2, width=14) != loc:
            raise ValueError("zero-result comment coordinate encoding drifted")
    return {
        "status": "passed",
        "audited_queries": audited,
        "scenarios": list(ORACLE["scenarios"]),
        "independent_roots": 2,
        "corpus_sha256": corpus_digest(),
    }


def baseline(rounds: int) -> dict:
    """Measure actual rg output plus complete reads of its candidate Python files."""
    rows = []
    with tempfile.TemporaryDirectory(prefix="nas-lsp-baseline-") as temporary:
        root = materialize(Path(temporary) / "workspace")
        for scenario in ORACLE["scenarios"]:
            set_scenario(root, scenario)
            for iteration in range(rounds):
                for query in ORACLE["queries"]:
                    started = time.perf_counter_ns()
                    # Same spelling deliberately yields both symbols and prose hits.
                    term = "no_symbol_here" if query["symbol"] is None else "render"
                    search = subprocess.run(
                        [
                            "rg",
                            "--line-number",
                            "--column",
                            "--with-filename",
                            "--no-heading",
                            "--color",
                            "never",
                            "--word-regexp",
                            "--glob",
                            "*.py",
                            term,
                            ".",
                        ],
                        cwd=root,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        check=False,
                    )
                    if search.returncode not in (0, 1):
                        raise RuntimeError(search.stderr.decode())
                    candidates = sorted(
                        {
                            line.split(":", 1)[0].removeprefix("./")
                            for line in search.stdout.decode("utf-8").splitlines()
                        }
                    )
                    files = {
                        relative: (root / relative).read_text(encoding="utf-8")
                        for relative in candidates
                    }
                    actual = resolve(query, source_symbols(files))
                    compact = [
                        {
                            key: loc[key]
                            for key in ("path", "line", "column", "end_column")
                        }
                        for loc in actual
                    ]
                    answer_bytes = len(
                        json.dumps(
                            compact, separators=(",", ":"), ensure_ascii=False
                        ).encode()
                    )
                    elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
                    read_bytes = sum(
                        len(text.encode("utf-8")) for text in files.values()
                    )
                    total = len(search.stdout) + read_bytes + answer_bytes
                    correct = actual == expected(query, scenario)
                    rows.append(
                        {
                            "scenario": scenario,
                            "query": query["id"],
                            "iteration": iteration,
                            "pass": "first" if iteration == 0 else "repeated",
                            "correct": correct,
                            "latency_ms": round(elapsed_ms, 3),
                            "rg_bytes": len(search.stdout),
                            "file_read_bytes": read_bytes,
                            "answer_bytes": answer_bytes,
                            "total_bytes": total,
                            "token_estimate_bytes_div_4": math.ceil(total / 4),
                        }
                    )
    summaries = []
    for scenario in ORACLE["scenarios"]:
        for phase in ("first", "repeated"):
            group = [
                row
                for row in rows
                if row["scenario"] == scenario and row["pass"] == phase
            ]
            if not group:
                continue
            latencies = sorted(row["latency_ms"] for row in group)
            summaries.append(
                {
                    "scenario": scenario,
                    "pass": phase,
                    "tasks": len(group),
                    "correct": sum(row["correct"] for row in group),
                    "latency_p50_ms": latencies[math.ceil(len(group) * 0.5) - 1],
                    "latency_p95_ms": latencies[math.ceil(len(group) * 0.95) - 1],
                    "total_bytes": sum(row["total_bytes"] for row in group),
                    "token_estimate_bytes_div_4": sum(
                        row["token_estimate_bytes_div_4"] for row in group
                    ),
                }
            )
    return {
        "method": "rg candidate search + complete candidate-file reads + fixture-only AST binding resolution",
        "cache_note": "first/repeated passes; OS caches are not flushed; these are not cold-server measurements",
        "token_note": "ceil(UTF-8 output bytes / 4) is an estimate, not measured model tokens",
        "python": platform.python_version(),
        "rg": subprocess.run(
            ["rg", "--version"], capture_output=True, text=True, check=True
        ).stdout.splitlines()[0],
        "corpus_sha256": corpus_digest(),
        "correct": sum(row["correct"] for row in rows),
        "tasks": len(rows),
        "summaries": summaries,
        "rows": rows,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("validate")
    prepare = commands.add_parser(
        "prepare", help="create two fresh roots under a new temporary parent"
    )
    prepare.add_argument(
        "--parent", type=Path, help="existing parent directory (default: system temp)"
    )
    stage = commands.add_parser("stage", help="mutate a previously prepared warm root")
    stage.add_argument("root", type=Path)
    stage.add_argument("scenario", choices=ORACLE["scenarios"])
    bench = commands.add_parser("baseline")
    bench.add_argument("--rounds", type=int, default=2)
    bench.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.command == "validate":
        result = validate()
    elif args.command == "prepare":
        parent = Path(tempfile.mkdtemp(prefix="nas-lsp-corpus-", dir=args.parent))
        result = {
            "roots": [
                str(materialize(parent / name)) for name in ("worktree-a", "worktree-b")
            ],
            "oracle": str(HERE / "oracle.json"),
            "corpus_sha256": corpus_digest(),
        }
    elif args.command == "stage":
        set_scenario(args.root.resolve(), args.scenario)
        result = {"root": str(args.root.resolve()), "scenario": args.scenario}
    else:
        if not 1 <= args.rounds <= 100:
            parser.error("--rounds must be between 1 and 100")
        validate()
        result = baseline(args.rounds)
        if args.output:
            args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    if args.command == "baseline" and result["correct"] != result["tasks"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
