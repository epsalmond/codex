#!/usr/bin/env python3
"""Allocate immutable fork build identities without choosing a feature version."""

import argparse
import datetime
import fcntl
import os
from pathlib import Path
import re
import subprocess
import tempfile


def positive(value, bits):
    if not re.fullmatch(r"[0-9]+", value) or not 0 < int(value) < 2**bits:
        raise ValueError(f"expected a positive {bits}-bit build number, got {value!r}")
    return int(value)


def allocate_local(path, preview=False):
    # A separate lock inode survives atomic counter replacement. fcntl releases
    # it even after interruption; Linux and macOS both provide it.
    if preview:
        number = positive(path.read_text().strip(), 64) + 1 if path.exists() else 1
        if number >= 2**64:
            raise ValueError("local build counter exhausted")
        return number
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.with_suffix(path.suffix + ".lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        number = positive(path.read_text().strip(), 64) + 1 if path.exists() else 1
        if number >= 2**64:
            raise ValueError("local build counter exhausted")
        with tempfile.NamedTemporaryFile(
            mode="w", dir=path.parent, delete=False
        ) as counter:
            counter.write(f"{number}\n")
            counter.flush()
            os.fsync(counter.fileno())
        os.replace(counter.name, path)
        return number


def describe(tag):
    match = re.search(r"-r([0-9]{1,14})\.([0-9a-fA-F]+)$", tag)
    if match is None:
        return "legacy (see release tag)"
    counter, suffix = match.groups()
    if len(suffix) == 52:
        run = int(suffix[28:44], 16)
        attempt = int(suffix[44:52], 16)
        return f"local {run}" if len(counter) == 12 else f"{run}.{attempt}"
    if len(suffix) == 28:
        return f"source {int(suffix[:16], 16)} (legacy)"
    return "legacy (see release tag)"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    describe_parser = commands.add_parser("describe")
    describe_parser.add_argument("tag")
    allocate = commands.add_parser("allocate")
    allocate.add_argument("--repo-root", required=True)
    allocate.add_argument("--sha", required=True)
    allocate.add_argument("--version", required=True)
    allocate.add_argument("--kind", choices=("ci", "local"), required=True)
    allocate.add_argument("--run-number")
    allocate.add_argument("--run-attempt")
    allocate.add_argument("--counter-file", type=Path)
    allocate.add_argument("--preview", action="store_true")
    args = parser.parse_args()
    if args.command == "describe":
        print(describe(args.tag))
        return
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", args.version):
        parser.error("version must be X.Y.Z")

    def git(*cmd):
        return subprocess.check_output(
            ["git", "-C", args.repo_root, *cmd], text=True
        ).strip()

    sha = git("rev-parse", "--verify", args.sha + "^{commit}")
    sequence = int(git("rev-list", "--first-parent", "--count", sha))
    epoch = int(git("show", "-s", "--format=%ct", sha))
    date = datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc)
    if args.kind == "ci":
        run = positive(args.run_number or "", 64)
        attempt = positive(args.run_attempt or "", 32)
        timestamp = date.strftime("%Y%m%d%H%M%S")
    else:
        if args.counter_file is None:
            parser.error("local builds require --counter-file")
        run = allocate_local(args.counter_file, args.preview)
        attempt = 0
        timestamp = date.strftime("%Y%m%d%H%M")
    suffix = f"{sequence:016x}{sha[:12]}{run:016x}{attempt:08x}"
    tag = f"local-features-v{args.version}-main-r{timestamp}.{suffix}"
    print(f"release_tag={tag}")
    print(f"build_revision={describe(tag)}")
    print(f"source_sequence={sequence}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        raise SystemExit(f"fork build identity: {error}") from error
