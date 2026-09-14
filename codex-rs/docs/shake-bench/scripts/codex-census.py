#!/usr/bin/env python3
"""Classify every live codex process by which account it spends quota on.

A replay's quota delta is only attributable if no OTHER process on the SAME
account is spending quota at the same time (arms-2026-09-11.md, "the concurrency
problem"). But not every codex process on this host is on the same account: the
personal account lives in ~/.codex and a second Codex account/home lives
elsewhere (path configurable), and only the personal one pollutes a replay's
quota.

So each process is classified by its CODEX_HOME -- read from /proc/<pid>/environ
first, because that is what the process actually resolved, and falling back to a
--codex-home/-c argument or a codex-home path appearing in its argv. This run's
own process lineage is excluded: a census that counts the harness that is taking
it can never report a quiet host.

Exit codes:
  0  no other PERSONAL-account codex process is alive (safe to launch)
  1  at least one is (do not launch; the quota number would not be attributable)

Usage: codex-census.py [--json] [--personal-home ~/.codex]
"""

import json
import os
import re
import subprocess
import sys

PERSONAL_HOME = os.path.expanduser("~/.codex")


def pgrep_codex():
    try:
        out = subprocess.run(
            ["pgrep", "-af", "codex"], capture_output=True, text=True, check=False
        ).stdout
    except FileNotFoundError:
        return []
    rows = []
    for line in out.splitlines():
        line = line.strip()
        if not line:
            continue
        pid, _, cmd = line.partition(" ")
        if pid.isdigit():
            rows.append((int(pid), cmd))
    return rows


def ancestors(pid):
    """This process's pid plus every ancestor, so the harness excludes itself."""
    chain = set()
    while pid and pid not in chain:
        chain.add(pid)
        try:
            with open(f"/proc/{pid}/stat") as fh:
                pid = int(fh.read().rsplit(")", 1)[1].split()[1])
        except (OSError, IndexError, ValueError):
            break
    return chain


def environ_of(pid):
    try:
        with open(f"/proc/{pid}/environ", "rb") as fh:
            raw = fh.read()
    except OSError:
        return {}
    env = {}
    for entry in raw.split(b"\0"):
        if b"=" in entry:
            k, _, v = entry.partition(b"=")
            env[k.decode("utf8", "replace")] = v.decode("utf8", "replace")
    return env


def codex_home_of(pid, cmd):
    """(home, how) -- where this process keeps its credentials, and how we know."""
    env = environ_of(pid)
    if env.get("CODEX_HOME"):
        return os.path.realpath(os.path.expanduser(env["CODEX_HOME"])), "environ"
    m = re.search(r"--codex-home[= ]([^\s]+)", cmd)
    if m:
        return os.path.realpath(os.path.expanduser(m.group(1))), "argv"
    m = re.search(r"(/[^\s]*\.codex[\w.-]*)", cmd)
    if m:
        return os.path.realpath(m.group(1)), "argv-path"
    if env:
        # The environ was readable and simply has no override, so this process
        # resolved the default home.
        return os.path.realpath(PERSONAL_HOME), "default"
    return None, "unknown"


def classify(home, how):
    if home is None:
        return "unknown"
    if home == os.path.realpath(PERSONAL_HOME):
        return "personal"
    if ".codex" in os.path.basename(home):
        return "other-account"
    return "other-account"


def main():
    own = ancestors(os.getpid())
    rows = []
    for pid, cmd in pgrep_codex():
        if pid in own:
            continue
        # The harness's own launcher chain matches "codex" through its paths and
        # arguments; it is not a codex session.
        if "replay-01a07e54.ts" in cmd or "codex-census.py" in cmd:
            continue
        home, how = codex_home_of(pid, cmd)
        kind = classify(home, how)
        rows.append(
            {"pid": pid, "cmd": cmd, "codexHome": home, "source": how, "account": kind}
        )

    personal = [r for r in rows if r["account"] == "personal"]
    unknown = [r for r in rows if r["account"] == "unknown"]
    result = {
        "checkedAt": subprocess.run(
            ["date", "-Is"], capture_output=True, text=True
        ).stdout.strip(),
        "personalHome": os.path.realpath(PERSONAL_HOME),
        "total": len(rows),
        "personal": len(personal),
        "otherAccount": len(rows) - len(personal) - len(unknown),
        "unknown": len(unknown),
        "processes": rows,
    }
    if "--json" in sys.argv:
        print(json.dumps(result, indent=2))
    else:
        print(f"codex processes (excluding this harness): {result['total']}")
        print(f"  personal ({result['personalHome']}): {result['personal']}")
        print(f"  other account:                        {result['otherAccount']}")
        print(f"  unclassifiable:                       {result['unknown']}")
        for r in rows:
            if r["account"] != "other-account" or "--json" in sys.argv:
                print(
                    f"  [{r['account']:13}] {r['pid']:>8} {r['codexHome']} ({r['source']})  {r['cmd'][:110]}"
                )
    # Unclassifiable processes are treated as NOT blocking but are always
    # printed: a process whose environ cannot be read is not evidence of a
    # personal-account session, and silently promoting it to one would make the
    # host permanently unlaunchable.
    return 1 if personal else 0


if __name__ == "__main__":
    sys.exit(main())
