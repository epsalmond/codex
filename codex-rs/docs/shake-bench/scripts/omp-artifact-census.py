#!/usr/bin/env python3
"""omp-artifact-census.py

Census tool for oh-my-pi (omp) session transcripts: counts shake ("elide")
events, artifact recoveries (reads of a shaken artifact after the fact), and
tool-call re-runs that happened after their original output was elided.

Read-only. Never writes, moves, or deletes session files.

Background
----------
omp's "shake" compaction (packages/agent/src/compaction/shake.ts, applied by
packages/coding-agent/src/session/session-maintenance.ts) replaces a tool
result's text (or a large fenced/XML block) in place with a short placeholder
that embeds a recovery link:

    [shaken ~<N> tokens — recover: artifact://<id> (region <M>)]

or, when the session isn't persisted and no artifact could be saved:

    [shaken ~<N> tokens]

The elided content is concatenated into one artifact file per shake op, named
`<id>.shake.log` in the session's artifacts directory (the session's .jsonl
path with the extension stripped -- see
packages/coding-agent/src/session/artifacts.ts and
packages/coding-agent/src/internal-urls/artifact-protocol.ts). Any tool can
also spill oversized output straight to an artifact without ever having lived
in context (e.g. `<id>.bash.log`); those are NOT shake recoveries, so this
script only tracks artifact ids that a shake placeholder actually named for
that session.

Usage
-----
    omp-artifact-census.py SESSION_ROOT [SESSION_ROOT ...] [--json]

A SESSION_ROOT is walked recursively for *.jsonl files.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from dataclasses import dataclass, field

SHAKE_PLACEHOLDER_RE = re.compile(
    r"^\[shaken ~\d+ tokens(?: — recover: artifact://(?P<id>\d+) \(region \d+\))?\]$"
)

# artifact://<id> possibly followed by a selector (:raw, :1-300, :raw:1-300, etc).
# Only numeric ids are real omp artifact ids (artifact-protocol.ts parseArtifactId).
ARTIFACT_URI_RE = re.compile(r"artifact://(\d+)(?::[A-Za-z0-9:_-]*)?")

RECOVERY_TOOL_NAMES = {"read", "grep", "bash", "eval", "shell"}


def is_enoent_line(line: str) -> bool:
    return not line.strip()


@dataclass
class ShakeEvent:
    index: int
    timestamp: str | None
    artifact_id: str | None
    tool_call_id: str | None
    tool_name: str | None
    preview: str


@dataclass
class RecoveryEvent:
    index: int
    timestamp: str | None
    tool_name: str
    artifact_id: str | None
    kind: str  # "artifact_uri" or "artifacts_dir_path"
    preview: str


@dataclass
class RerunEvent:
    index: int
    timestamp: str | None
    tool_name: str
    original_index: int
    shake_index: int
    preview: str


@dataclass
class SessionCensus:
    path: str
    session_id: str | None = None
    model: str | None = None
    total_messages: int = 0
    malformed_lines: int = 0
    shake_events: list[ShakeEvent] = field(default_factory=list)
    recoveries: list[RecoveryEvent] = field(default_factory=list)
    reruns: list[RerunEvent] = field(default_factory=list)
    parse_error: str | None = None

    @property
    def shaken_artifact_count(self) -> int:
        return len({e.artifact_id for e in self.shake_events if e.artifact_id})


def find_session_files(roots: list[str]) -> list[str]:
    files: list[str] = []
    for root in roots:
        root = os.path.abspath(os.path.expanduser(root))
        if os.path.isfile(root) and root.endswith(".jsonl"):
            files.append(root)
            continue
        for dirpath, _dirnames, filenames in os.walk(root):
            for name in filenames:
                if name.endswith(".jsonl"):
                    files.append(os.path.join(dirpath, name))
    return sorted(files)


def toolresult_text_blocks(message: dict) -> list[str]:
    content = message.get("content")
    out = []
    if isinstance(content, list):
        for block in content:
            if isinstance(block, dict) and block.get("type") == "text" and isinstance(block.get("text"), str):
                out.append(block["text"])
    elif isinstance(content, str):
        out.append(content)
    return out


def args_hash(name: str, arguments: dict) -> str:
    try:
        canon = json.dumps(arguments, sort_keys=True, default=str)
    except Exception:
        canon = repr(arguments)
    return f"{name}\x00{canon}"


def preview(text: str, n: int = 100) -> str:
    text = text.replace("\n", "\\n")
    return text[:n]


def censor_session(path: str) -> SessionCensus:
    census = SessionCensus(path=path)

    tool_calls: dict[str, dict] = {}  # toolCallId -> {name, arguments, index, timestamp}
    elided_hashes: dict[str, int] = {}  # args_hash -> earliest shake index that elided it
    shaken_ids: dict[str, int] = {}  # artifact id (numeric str) -> earliest shake index

    with open(path, "r", encoding="utf-8", errors="replace") as f:
        for idx, raw_line in enumerate(f):
            if is_enoent_line(raw_line):
                continue
            try:
                obj = json.loads(raw_line)
            except Exception:
                census.malformed_lines += 1
                continue

            if not isinstance(obj, dict):
                census.malformed_lines += 1
                continue

            etype = obj.get("type")
            timestamp = obj.get("timestamp")

            if etype == "session" and census.session_id is None:
                census.session_id = obj.get("id")

            if etype != "message":
                continue

            census.total_messages += 1
            message = obj.get("message")
            if not isinstance(message, dict):
                continue
            role = message.get("role")

            if isinstance(message.get("model"), str):
                census.model = message["model"]

            if role == "assistant":
                content = message.get("content")
                if isinstance(content, list):
                    for block in content:
                        if not isinstance(block, dict) or block.get("type") != "toolCall":
                            continue
                        tc_id = block.get("id")
                        name = block.get("name")
                        arguments = block.get("arguments") if isinstance(block.get("arguments"), dict) else {}
                        if not tc_id or not name:
                            continue
                        tool_calls[tc_id] = {
                            "name": name,
                            "arguments": arguments,
                            "index": idx,
                            "timestamp": timestamp,
                        }
                        # Re-run detection: same (name, args) hash seen again after
                        # an earlier occurrence was elided by shake.
                        h = args_hash(name, arguments)
                        shake_idx = elided_hashes.get(h)
                        if shake_idx is not None and idx > shake_idx:
                            census.reruns.append(
                                RerunEvent(
                                    index=idx,
                                    timestamp=timestamp,
                                    tool_name=name,
                                    original_index=tool_calls[tc_id]["index"],
                                    shake_index=shake_idx,
                                    preview=preview(json.dumps(arguments, default=str)),
                                )
                            )

                        # Recovery detection (a): read/grep/bash/eval calls whose
                        # args reference a numeric artifact:// id that a shake in
                        # THIS session actually produced (not e.g. an unrelated
                        # bash-spill artifact, which was never in-context to elide).
                        args_str = json.dumps(arguments, default=str)
                        for m in ARTIFACT_URI_RE.finditer(args_str):
                            aid = m.group(1)
                            shake_idx = shaken_ids.get(aid)
                            if shake_idx is None or idx <= shake_idx:
                                continue
                            census.recoveries.append(
                                RecoveryEvent(
                                    index=idx,
                                    timestamp=timestamp,
                                    tool_name=name,
                                    artifact_id=aid,
                                    kind="artifact_uri",
                                    preview=preview(args_str),
                                )
                            )

                        # Recovery detection (b): bash/shell command referencing
                        # the session's own artifacts directory path directly, or
                        # naming a shaken artifact's file (<id>.shake.log) by
                        # relative name within that directory.
                        if name in ("bash", "shell"):
                            command = arguments.get("command") if isinstance(arguments, dict) else None
                            if isinstance(command, str):
                                # The artifacts dir also holds unrelated files (local:// scratch
                                # writes under <dir>/local/, other tools' <id>.bash.log spills,
                                # etc), so a bare "session_dir appears in the command" match is too
                                # noisy -- e.g. `term-check <dir>/local/plan.md` is a normal
                                # local:// write, not a shake recovery. Only count it as a
                                # directory-path recovery when the command names a SPECIFIC
                                # shaken artifact's file (`<id>.shake.log`) after that shake
                                # happened.
                                name_hit = any(
                                    idx > sidx and f"{aid}.shake.log" in command
                                    for aid, sidx in shaken_ids.items()
                                )
                                if name_hit:
                                    census.recoveries.append(
                                        RecoveryEvent(
                                            index=idx,
                                            timestamp=timestamp,
                                            tool_name=name,
                                            artifact_id=None,
                                            kind="artifacts_dir_path",
                                            preview=preview(command),
                                        )
                                    )
                continue

            if role == "toolResult":
                texts = toolresult_text_blocks(message)
                tool_call_id = message.get("toolCallId")
                tool_name = message.get("toolName")
                for text in texts:
                    stripped = text.strip()
                    m = SHAKE_PLACEHOLDER_RE.match(stripped)
                    if not m:
                        continue
                    artifact_id = m.group("id")
                    if artifact_id is not None and artifact_id not in shaken_ids:
                        shaken_ids[artifact_id] = idx
                    census.shake_events.append(
                        ShakeEvent(
                            index=idx,
                            timestamp=timestamp,
                            artifact_id=artifact_id,
                            tool_call_id=tool_call_id,
                            tool_name=tool_name,
                            preview=preview(stripped),
                        )
                    )
                    # Mark the original tool call (if we've seen it) as elided,
                    # for re-run detection.
                    orig = tool_calls.get(tool_call_id) if tool_call_id else None
                    if orig:
                        h = args_hash(orig["name"], orig["arguments"])
                        if h not in elided_hashes or idx < elided_hashes[h]:
                            elided_hashes[h] = idx

    return census


def print_table(rows: list[tuple[str, ...]], headers: tuple[str, ...]) -> None:
    widths = [len(h) for h in headers]
    for row in rows:
        for i, cell in enumerate(row):
            widths[i] = max(widths[i], len(str(cell)))
    fmt = "  ".join(f"{{:<{w}}}" for w in widths)
    print(fmt.format(*headers))
    print(fmt.format(*("-" * w for w in widths)))
    for row in rows:
        print(fmt.format(*row))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("roots", nargs="+", help="Session root directories (or individual .jsonl files) to scan")
    parser.add_argument("--json", action="store_true", help="Emit machine-readable JSON instead of the summary table")
    args = parser.parse_args()

    files = find_session_files(args.roots)
    censuses: list[SessionCensus] = []
    total_malformed = 0
    for path in files:
        try:
            c = censor_session(path)
        except Exception as e:  # never let one bad file kill the census
            c = SessionCensus(path=path, parse_error=str(e))
        censuses.append(c)
        total_malformed += c.malformed_lines

    shaken_sessions = [c for c in censuses if c.shake_events]
    total_artifacts = sum(c.shaken_artifact_count for c in censuses)
    total_recoveries = sum(len(c.recoveries) for c in censuses)
    total_reruns = sum(len(c.reruns) for c in censuses)

    if args.json:
        out = {
            "sessions_scanned": len(censuses),
            "shaken_sessions": len(shaken_sessions),
            "total_shake_events": sum(len(c.shake_events) for c in censuses),
            "total_shaken_artifacts": total_artifacts,
            "total_recoveries": total_recoveries,
            "total_reruns": total_reruns,
            "malformed_lines": total_malformed,
            "sessions": [
                {
                    "path": c.path,
                    "session_id": c.session_id,
                    "model": c.model,
                    "total_messages": c.total_messages,
                    "malformed_lines": c.malformed_lines,
                    "shake_events": len(c.shake_events),
                    "shaken_artifacts": c.shaken_artifact_count,
                    "recoveries": [r.__dict__ for r in c.recoveries],
                    "reruns": [r.__dict__ for r in c.reruns],
                }
                for c in censuses
            ],
        }
        print(json.dumps(out, indent=2))
        return 0

    print(f"Session roots: {', '.join(os.path.abspath(os.path.expanduser(r)) for r in args.roots)}")
    print()
    print_table(
        [
            (
                "sessions scanned",
                str(len(censuses)),
            ),
            ("shaken sessions", str(len(shaken_sessions))),
            ("total shake events (placeholders)", str(sum(len(c.shake_events) for c in censuses))),
            ("total artifacts created by shake", str(total_artifacts)),
            ("total recoveries", str(total_recoveries)),
            ("total re-runs", str(total_reruns)),
            ("malformed lines skipped", str(total_malformed)),
        ],
        ("metric", "count"),
    )

    interesting = [c for c in censuses if c.recoveries or c.reruns]
    if interesting:
        print()
        print("Sessions with a recovery or re-run:")
        rows = []
        for c in interesting:
            rows.append(
                (
                    os.path.basename(c.path),
                    c.session_id or "?",
                    c.model or "?",
                    str(c.total_messages),
                    str(len(c.shake_events)),
                    str(len(c.recoveries)),
                    str(len(c.reruns)),
                )
            )
        print_table(rows, ("session file", "session id", "model", "messages", "shakes", "recoveries", "reruns"))

        print()
        print("Example recovery/re-run lines (up to 10):")
        shown = 0
        for c in interesting:
            for r in c.recoveries:
                if shown >= 10:
                    break
                print(f"  [{c.session_id or os.path.basename(c.path)}] t={r.timestamp} RECOVERY({r.kind}) {r.tool_name}: {r.preview}")
                shown += 1
            if shown >= 10:
                break
        for c in interesting:
            for r in c.reruns:
                if shown >= 10:
                    break
                print(f"  [{c.session_id or os.path.basename(c.path)}] t={r.timestamp} RERUN {r.tool_name}: {r.preview}")
                shown += 1
            if shown >= 10:
                break
    else:
        print()
        print("No recoveries or re-runs found in any scanned session.")

    if any(c.parse_error for c in censuses):
        print()
        print("Sessions that failed to parse entirely:")
        for c in censuses:
            if c.parse_error:
                print(f"  {c.path}: {c.parse_error}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
