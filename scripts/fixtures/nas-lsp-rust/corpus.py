#!/usr/bin/env python3
"""Independent source oracle audit, linked fixtures, and actual rg/read baseline."""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import math
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tempfile
import time

HERE = Path(__file__).resolve().parent
ORACLE = json.loads((HERE / "oracle.json").read_text(encoding="utf-8"))
MARKER = ".nas-lsp-corpus.json"
EXTENSION = ORACLE["extension"]


def corpus_digest() -> str:
    digest = hashlib.sha256()
    for path in sorted(
        [
            HERE / "oracle.json",
            *(HERE / "workspace").rglob("*"),
            *(HERE / "variants").rglob("*"),
        ]
    ):
        if path.is_file():
            digest.update(path.relative_to(HERE).as_posix().encode() + b"\0")
            digest.update(path.read_bytes())
    return digest.hexdigest()


def materialize(root: Path, variant: str = "base") -> Path:
    """Create a new fixture root, refusing any existing destination."""
    profile = ORACLE["variants"][variant]
    root = Path(root).resolve()
    if root.exists():
        raise ValueError(f"fixture destination already exists: {root}")
    shutil.copytree(HERE / "workspace", root)
    for path in root.rglob(f"*.{EXTENSION}"):
        path.write_text(
            profile["source_prefix"] + path.read_text(encoding="utf-8"),
            encoding="utf-8",
        )
    (root / MARKER).write_text(
        json.dumps({"digest": corpus_digest(), "scenario": "base", "variant": variant})
        + "\n",
        encoding="utf-8",
    )
    return root


def set_scenario(root: Path, scenario: str) -> None:
    """Advance base -> changed -> deleted on the same saved-file worker root."""
    root = Path(root).resolve()
    receipt = json.loads((root / MARKER).read_text(encoding="utf-8"))
    stages = list(ORACLE["scenarios"])
    if receipt["digest"] != corpus_digest():
        raise ValueError("fixture root was created from a different corpus")
    if stages.index(scenario) < stages.index(receipt["scenario"]):
        raise ValueError("use a fresh root instead of reverting a warm fixture")
    change = ORACLE["scenarios"][scenario]
    prefix = ORACLE["variants"][receipt["variant"]]["source_prefix"]
    for relative, source in change.get("replace", {}).items():
        (root / relative).write_text(
            prefix + (HERE / source).read_text(encoding="utf-8"), encoding="utf-8"
        )
    for relative in change.get("delete", []):
        (root / relative).unlink(missing_ok=True)
    receipt["scenario"] = scenario
    (root / MARKER).write_text(json.dumps(receipt) + "\n", encoding="utf-8")


def location_key(location: dict) -> tuple:
    return tuple(location[key] for key in ("path", "line", "column", "end_column"))


def location(location_id: str, variant: str = "base") -> dict:
    result = copy.deepcopy(ORACLE["locations"][location_id])
    offset = ORACLE["variants"][variant]["line_offset"]
    result["line"] += offset
    for bound in ("start", "end"):
        result["lsp"][bound]["line"] += offset
    return result


def query_location(query: dict, variant: str = "base") -> dict:
    return location(query["at"], variant)


def expected(query: dict, scenario: str = "base", variant: str = "base") -> list[dict]:
    symbol = query["symbol"]
    if symbol is None:
        return []
    ids = (
        [ORACLE["symbols"][symbol]["definition"]]
        if query["operation"] == "definition"
        else ORACLE["scenarios"][scenario]["references"][symbol]
    )
    return sorted((location(key, variant) for key in ids), key=location_key)


def source_location(relative: str, text: str, offset: int, width: int = 6) -> dict:
    """Python text offsets count scalars; derive UTF-8 and UTF-16 independently."""
    lineno = text.count("\n", 0, offset) + 1
    start = text.rfind("\n", 0, offset) + 1
    prefix, token = text[start:offset], text[offset : offset + width]
    return {
        "path": relative,
        "line": lineno,
        "column": len(prefix) + 1,
        "end_column": len(prefix) + width + 1,
        "utf8_column": len(prefix.encode("utf-8")) + 1,
        "utf8_end_column": len((prefix + token).encode("utf-8")) + 1,
        "lsp": {
            "start": {
                "line": lineno - 1,
                "character": len(prefix.encode("utf-16-le")) // 2,
            },
            "end": {
                "line": lineno - 1,
                "character": len((prefix + token).encode("utf-16-le")) // 2,
            },
        },
    }


def source_symbols(files: dict[str, str]) -> dict:
    """Resolve only fixture function declarations and explicit namespace imports.

    This is a small fixture audit, not a production language parser. It never
    reads the oracle's binding/reference lists or any language-server output.
    The saved corpus deliberately contains no shadowing, reexports, macros,
    strings/comments resembling qualified calls, or generated code.
    """
    symbols = {
        f"{module}.render": {"definition": None, "references": []}
        for module in ("primary", "secondary")
    }
    for relative, text in sorted(files.items()):
        imports = {}
        if EXTENSION == "ts":
            for match in re.finditer(r'import \* as (\w+) from "\./(\w+)";', text):
                imports[match[1]] = match[2]
            declaration = r"export function (render)\("
            call = r"\b(\w+)\.(render)\("
        else:
            for match in re.finditer(r"use crate::(?:\{([^}]+)\}|(\w+));", text):
                for name in (match[1] or match[2]).split(","):
                    imports[name.strip()] = name.strip()
            declaration = r"pub fn (render)\("
            call = r"\b(\w+)::(render)\("
        for match in re.finditer(declaration, text):
            symbol = f"{Path(relative).stem}.render"
            value = symbols[symbol]
            if value["definition"] is not None:
                raise ValueError(f"duplicate fixture declaration: {symbol}")
            loc = source_location(relative, text, match.start(1))
            value["definition"] = loc
            value["references"].append(loc)
        for match in re.finditer(call, text):
            symbol = f"{imports[match[1]]}.render"
            symbols[symbol]["references"].append(
                source_location(relative, text, match.start(2))
            )
    return symbols


def resolve(query: dict, symbols: dict, variant: str = "base") -> list[dict]:
    at = location_key(query_location(query, variant))
    matches = [
        value
        for value in symbols.values()
        if any(location_key(loc) == at for loc in value["references"])
    ]
    if not matches:
        return []
    if len(matches) != 1:
        raise ValueError(f"ambiguous fixture binding at {at}")
    value = matches[0]
    answer = (
        [value["definition"]]
        if query["operation"] == "definition"
        else value["references"]
    )
    return sorted(answer, key=location_key)


def source_files(root: Path) -> dict[str, str]:
    return {
        path.relative_to(root).as_posix(): path.read_text(encoding="utf-8")
        for path in sorted(root.rglob(f"*.{EXTENSION}"))
    }


def git(root: Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [
            "git",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-C",
            str(root),
            *args,
        ],
        check=True,
        capture_output=True,
        text=True,
    )


def prepare(parent: Path) -> dict:
    """Create two actual linked Git worktrees with deliberately different ranges."""
    parent = Path(parent).resolve()
    control = materialize(parent / "control")
    git(control, "init", "--quiet")
    git(control, "add", ".")
    git(control, "commit", "--quiet", "-m", "Seed navigation acceptance fixture")
    roots = []
    for name, variant in (("worktree-a", "base"), ("worktree-b", "shifted")):
        root = parent / name
        git(control, "worktree", "add", "--quiet", "--detach", str(root), "HEAD")
        if variant != "base":
            for path in root.rglob(f"*.{EXTENSION}"):
                path.write_text(
                    ORACLE["variants"][variant]["source_prefix"]
                    + path.read_text(encoding="utf-8"),
                    encoding="utf-8",
                )
            receipt = json.loads((root / MARKER).read_text(encoding="utf-8"))
            receipt["variant"] = variant
            (root / MARKER).write_text(json.dumps(receipt) + "\n", encoding="utf-8")
        roots.append({"root": str(root), "variant": variant})
    return {
        "roots": roots,
        "git_common_dir": str(control / ".git"),
        "oracle": str(HERE / "oracle.json"),
        "corpus_sha256": corpus_digest(),
    }


def validate() -> dict:
    audited = inventories = 0
    with tempfile.TemporaryDirectory(
        prefix=f"nas-lsp-{ORACLE['language']}-audit-"
    ) as temporary:
        receipt = prepare(Path(temporary))
        root_a, root_b = (Path(entry["root"]) for entry in receipt["roots"])
        untouched = source_files(root_b)
        for scenario in ORACLE["scenarios"]:
            set_scenario(root_a, scenario)
            for root, variant, stage in (
                (root_a, "base", scenario),
                (root_b, "shifted", "base"),
            ):
                files = source_files(root)
                symbols = source_symbols(files)
                for query in ORACLE["queries"]:
                    actual, wanted = (
                        resolve(query, symbols, variant),
                        expected(query, stage, variant),
                    )
                    if actual != wanted:
                        raise ValueError(
                            f"oracle mismatch: {variant}/{stage}/{query['id']}: {actual} != {wanted}"
                        )
                    audited += 1
                for symbol, ids in ORACLE["scenarios"][stage]["references"].items():
                    wanted = sorted(
                        (location(key, variant) for key in ids), key=location_key
                    )
                    if (
                        sorted(symbols[symbol]["references"], key=location_key)
                        != wanted
                    ):
                        raise ValueError(
                            f"reference inventory mismatch: {variant}/{stage}/{symbol}"
                        )
                    inventories += len(wanted)
                # Audit every encoding against the actual saved identifier bytes.
                for query in ORACLE["queries"]:
                    loc = query_location(query, variant)
                    text = files[loc["path"]]
                    lines = text.splitlines(keepends=True)
                    width = loc["end_column"] - loc["column"]
                    offset = (
                        sum(len(row) for row in lines[: loc["line"] - 1])
                        + loc["column"]
                        - 1
                    )
                    token = "no_symbol_here" if query["symbol"] is None else "render"
                    if (
                        text[offset : offset + width] != token
                        or source_location(loc["path"], text, offset, width) != loc
                    ):
                        raise ValueError(f"coordinate encoding mismatch: {loc}")
            if source_files(root_b) != untouched:
                raise ValueError("mutating root A changed root B")
        # Root B is also audited in changed/deleted scenarios, after isolation checks.
        for scenario in ("changed", "deleted"):
            set_scenario(root_b, scenario)
            symbols = source_symbols(source_files(root_b))
            for query in ORACLE["queries"]:
                if resolve(query, symbols, "shifted") != expected(
                    query, scenario, "shifted"
                ):
                    raise ValueError(
                        f"shifted oracle mismatch: {scenario}/{query['id']}"
                    )
                audited += 1
        try:
            set_scenario(root_a, "base")
        except ValueError:
            pass
        else:
            raise ValueError("backward scenario mutation was accepted")
    astral = location("astral_primary")
    if (
        not astral["column"]
        < astral["lsp"]["start"]["character"] + 1
        < astral["utf8_column"]
    ):
        raise ValueError(
            "astral fixture must distinguish scalar, UTF-16 and byte columns"
        )
    return {
        "status": "passed",
        "audited_queries": audited,
        "audited_reference_locations": inventories,
        "scenarios": list(ORACLE["scenarios"]),
        "linked_worktrees": 2,
        "variants": list(ORACLE["variants"]),
        "corpus_sha256": corpus_digest(),
    }


def baseline(rounds: int) -> dict:
    """Measure real rg bytes, complete candidate reads, and source-derived answers."""
    rows = []
    with tempfile.TemporaryDirectory(
        prefix=f"nas-lsp-{ORACLE['language']}-baseline-"
    ) as temporary:
        for variant in ORACLE["variants"]:
            root = materialize(Path(temporary) / variant, variant)
            for scenario in ORACLE["scenarios"]:
                set_scenario(root, scenario)
                for iteration in range(rounds):
                    for query in ORACLE["queries"]:
                        started = time.perf_counter_ns()
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
                                f"*.{EXTENSION}",
                                term,
                                ".",
                            ],
                            cwd=root,
                            capture_output=True,
                            check=False,
                        )
                        if search.returncode not in (0, 1):
                            raise RuntimeError(search.stderr.decode())
                        candidates = sorted(
                            {
                                row.split(":", 1)[0].removeprefix("./")
                                for row in search.stdout.decode("utf-8").splitlines()
                            }
                        )
                        files = {
                            relative: (root / relative).read_text(encoding="utf-8")
                            for relative in candidates
                        }
                        actual = resolve(query, source_symbols(files), variant)
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
                        rows.append(
                            {
                                "variant": variant,
                                "scenario": scenario,
                                "query": query["id"],
                                "iteration": iteration,
                                "pass": "first" if iteration == 0 else "repeated",
                                "correct": actual == expected(query, scenario, variant),
                                "latency_ms": round(elapsed_ms, 3),
                                "rg_bytes": len(search.stdout),
                                "file_read_bytes": read_bytes,
                                "answer_bytes": answer_bytes,
                                "total_bytes": total,
                                "token_estimate_bytes_div_4": math.ceil(total / 4),
                            }
                        )
    summaries = []
    for variant in ORACLE["variants"]:
        for scenario in ORACLE["scenarios"]:
            for phase in ("first", "repeated"):
                group = [
                    row
                    for row in rows
                    if row["variant"] == variant
                    and row["scenario"] == scenario
                    and row["pass"] == phase
                ]
                if group:
                    latencies = sorted(row["latency_ms"] for row in group)
                    summaries.append(
                        {
                            "variant": variant,
                            "scenario": scenario,
                            "pass": phase,
                            "tasks": len(group),
                            "correct": sum(row["correct"] for row in group),
                            "latency_p50_ms": latencies[
                                math.ceil(len(group) * 0.5) - 1
                            ],
                            "latency_p95_ms": latencies[
                                math.ceil(len(group) * 0.95) - 1
                            ],
                            "total_bytes": sum(row["total_bytes"] for row in group),
                            "token_estimate_bytes_div_4": sum(
                                row["token_estimate_bytes_div_4"] for row in group
                            ),
                        }
                    )
    return {
        "method": "actual rg search + complete candidate-file reads + fixture-only source binding resolution; no language server",
        "cache_note": "first/repeated passes share OS caches; not cold/warm server measurements",
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
    prep = commands.add_parser("prepare")
    prep.add_argument(
        "--parent", type=Path, help="existing parent for a new temporary directory"
    )
    stage = commands.add_parser("stage")
    stage.add_argument("root", type=Path)
    stage.add_argument("scenario", choices=ORACLE["scenarios"])
    bench = commands.add_parser("baseline")
    bench.add_argument("--rounds", type=int, default=2)
    bench.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.command == "validate":
        result = validate()
    elif args.command == "prepare":
        result = prepare(
            Path(
                tempfile.mkdtemp(
                    prefix=f"nas-lsp-{ORACLE['language']}-", dir=args.parent
                )
            )
        )
    elif args.command == "stage":
        set_scenario(args.root, args.scenario)
        result = {"root": str(args.root.resolve()), "scenario": args.scenario}
    else:
        if not 1 <= args.rounds <= 100:
            parser.error("--rounds must be between 1 and 100")
        validate()
        result = baseline(args.rounds)
        if args.output:
            args.output.write_text(
                json.dumps(result, indent=2) + "\n", encoding="utf-8"
            )
    print(json.dumps(result, indent=2))
    if args.command == "baseline" and result["correct"] != result["tasks"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
