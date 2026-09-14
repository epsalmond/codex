#!/usr/bin/env python3
"""Did any OTHER personal-account codex session spend quota during a run?

The quiet-host census answers "is another session alive"; this answers the
question that actually matters for a quota delta -- "did another session on this
account make model requests while the run was in flight". A codex session writes
its rollout on every request, so a rollout under ~/.codex/sessions whose mtime
advanced between the start and the end of a run is direct evidence of concurrent
spend. Idle foreground TUIs, which is what this host is full of, touch nothing.

The worker's own rollout lives under the run's CODEX_HOME (bench-replay/…
codex-homes/), never under ~/.codex, so it cannot be confused for one; --exclude
is available anyway for a worker home placed somewhere unusual.

  rollout-watch.py snapshot > before.json
  rollout-watch.py compare before.json            # prints a verdict, exit 1 if any advanced

Exit codes for `compare`: 0 = no other session wrote a rollout (quota delta is
attributable), 1 = at least one did (mark the run's quota "unattributable").
"""

import glob
import json
import os
import sys

SESSIONS = os.path.expanduser(os.environ.get("CODEX_HOME", "~/.codex") + "/sessions")


def snapshot(exclude=()):
    rows = {}
    for path in glob.glob(os.path.join(SESSIONS, "*", "*", "*", "rollout-*.jsonl")):
        if any(ex and ex in path for ex in exclude):
            continue
        try:
            rows[path] = os.path.getmtime(path)
        except OSError:
            continue
    return {"sessions": SESSIONS, "count": len(rows), "mtimes": rows}


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in ("snapshot", "compare"):
        sys.exit(__doc__)
    exclude = tuple(
        a.split("=", 1)[1] for a in sys.argv[2:] if a.startswith("--exclude=")
    )
    if sys.argv[1] == "snapshot":
        print(json.dumps(snapshot(exclude), indent=2))
        return 0

    before = json.load(open([a for a in sys.argv[2:] if not a.startswith("--")][0]))
    after = snapshot(exclude)
    advanced = []
    for path, mtime in after["mtimes"].items():
        was = before["mtimes"].get(path)
        if was is None or mtime > was + 0.001:
            advanced.append(
                {"rollout": os.path.basename(path), "new": was is None, "mtime": mtime}
            )
    verdict = {
        "otherSessionsWroteDuringRun": bool(advanced),
        "rolloutsBefore": before["count"],
        "rolloutsAfter": after["count"],
        "advanced": advanced,
        "quotaAttributable": not advanced,
    }
    print(json.dumps(verdict, indent=2))
    return 1 if advanced else 0


if __name__ == "__main__":
    sys.exit(main())
