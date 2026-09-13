#!/usr/bin/env python3
"""Rebuild the warm-matrix report's table and detail sections from the runs.

Idempotent: the whole table and every detail block are regenerated from each
run's own replay.json on every invocation, so a row can never drift from the
record it describes, and re-running after a later run adds that run without
touching the earlier ones.

  warm-report.py <report.md> <label>=<results-dir>[=<note>] ...
"""
import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent


def gen(*args):
    return subprocess.run(["python3", str(HERE / "warm-row.py"), *args], capture_output=True, text=True, check=True).stdout.rstrip()


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    report = pathlib.Path(sys.argv[1])
    runs = []
    for spec in sys.argv[2:]:
        parts = spec.split("=")
        runs.append((parts[0], parts[1], parts[2] if len(parts) > 2 else ""))

    table = [gen("header")] + [gen("row", d, label, note) for label, d, note in runs]
    details = [gen("detail", d, f"{label}{' — ' + note if note else ''}") for label, d, note in runs]

    text = report.read_text()
    for marker, body in (("TABLE", "\n".join(table)), ("DETAIL", "\n\n".join(details))):
        start, end = f"<!-- {marker}:START -->", f"<!-- {marker}:END -->"
        head, _, rest = text.partition(start)
        _, _, tail = rest.partition(end)
        text = f"{head}{start}\n{body}\n{end}{tail}"
    report.write_text(text)
    print(f"{report}: {len(runs)} run(s) written")


if __name__ == "__main__":
    sys.exit(main())
