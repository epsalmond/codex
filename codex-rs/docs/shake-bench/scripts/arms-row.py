#!/usr/bin/env python3
"""One arms-table row per replay.json.

Both cost profiles are printed for every run (scripts/pricing_lib.py):

  * API shadow $ --- the API rate card, summed PER REQUEST because gpt-6-astra
    prices the whole request at 2x input / 2x cached / 1.5x output once it
    crosses 272,000 input tokens. The replay worker authenticates through
    ChatGPT login, so this figure is a shadow cost and is never what was paid.
  * Codex credits --- the plan rate card a ChatGPT-login run actually spends,
    at both standard and Fast rates (Fast is 2.5x Astra's standard), with the
    run's own tier marked.

Usage: arms-row.py <label> <replay.json> [<label> <replay.json> ...]
"""

import json, pathlib, sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import pricing_lib

PRICING = pricing_lib.load()


def requests_of(rec):
    return rec.get("compaction", {}).get("requests", [])


def tier_of(rec):
    """The tier the run actually ran on, from thread/start's echoed value."""
    applied = rec.get("serviceTierApplied")
    requested = rec.get("serviceTier")
    return (
        "fast"
        if (applied in ("priority", "fast") or requested == "fast")
        else "standard"
    )


def retrieval_from_rollout(rec):
    """Actual read_artifact invocations, counted from the worker's own rollout.

    The item/completed channel cannot answer this here: the replayed task IS the
    artifact reader, so "read_artifact" appears in 69 exec bodies that are
    writing read_artifact.rs and in the scripted user's own turn 4. Two exact
    signatures are counted instead --- a top-level tool call named
    read_artifact, and a code-mode `tools.read_artifact(` call, which is the
    nested form this worker would actually use (every one of its tool calls is
    an `exec` into code mode).
    """
    home = rec.get("codexHome")
    if not home:
        return None
    paths = sorted(pathlib.Path(home).glob("sessions/*/*/*/rollout-*.jsonl"))
    if not paths:
        return None
    top = nested = 0
    for path in paths:
        text = path.read_text(errors="replace")
        top += text.count('"name":"read_artifact"') + text.count(
            '"name": "read_artifact"'
        )
        nested += text.count("tools.read_artifact")
    return {"topLevelToolCalls": top, "codeModeCalls": nested, "rollouts": len(paths)}


def window(d, name):
    w = (d or {}).get(name)
    if not w:
        return "not reported"
    mark = " (INVALID: reset crossed)" if w.get("resetCrossed") else ""
    return f"{w['usedPercentBefore']}% -> {w['usedPercentAfter']}% ({w['usedPercentDelta']:+d} pp){mark}"


for i in range(1, len(sys.argv), 2):
    label, path = sys.argv[i], sys.argv[i + 1]
    rec = json.load(open(path))
    all_reqs = requests_of(rec)
    # A warm-cache run pays for its priming request(s) too, but they are not
    # part of the arm: they are reported separately as priming overhead.
    first_arm = pricing_lib.first_arm_index(rec)
    prime, reqs = all_reqs[: first_arm - 1], all_reqs[first_arm - 1 :]
    wt = rec.get("workerTotals", {})
    q = rec.get("quotaDelta", {})
    ra = rec.get("readArtifactCalls", {})
    r1 = reqs[0] if reqs else {}
    steady = reqs[1:] if len(reqs) > 1 else []
    print(f"## {label}  ({path})")
    print(
        f"  arm                {rec.get('arm')}   tier requested={rec.get('serviceTier')} applied={rec.get('serviceTierApplied')}"
    )
    print(
        f"  history tokens     {rec.get('historyTokens')}  after arm {rec.get('historyTokensAfterArm')}"
    )
    print(
        f"  requests           {len(reqs)}"
        + (f"  (+{len(prime)} priming)" if prime else "")
    )
    cc = rec.get("cacheCondition") or {}
    if cc:
        print(
            f"  cache condition    {cc.get('cache')}"
            + (f" / warm {cc.get('warm')}" if cc.get("warm") else "")
            + f"   req1 cached frac {cc.get('firstArmRequestCachedFraction')}"
            + f"   assertion {'met' if cc.get('assertionMet') else 'NOT MET'}"
        )
    print(
        f"  input/cached/out   {wt.get('inputTokens')} / {wt.get('cachedInputTokens')} / {wt.get('outputTokens')}"
    )
    for line in pricing_lib.summary_lines(
        reqs, rec.get("model", "gpt-6-astra"), tier_of(rec), PRICING
    ):
        print(line)
    if prime:
        for line in pricing_lib.summary_lines(
            prime,
            rec.get("model", "gpt-6-astra"),
            tier_of(rec),
            PRICING,
            indent="    priming ",
        ):
            print(line)
    print(f"  quota primary      {window(q, 'primary')}")
    print(f"  quota secondary    {window(q, 'secondary')}")
    print(
        f"  codex procs        {len(q.get('codexProcessesBefore', []))} start / {len(q.get('codexProcessesAfter', []))} end"
    )
    print(
        f"  wall               {wt.get('wallMs', 0) / 1000:.0f}s   turns {len(rec.get('turns', []))}   stop {rec.get('stopReason')}"
    )
    print(
        f"  read_artifact (items, over-counts) {ra.get('total')}  {ra.get('byItemType')}"
    )
    print(f"  read_artifact (rollout, exact)     {retrieval_from_rollout(rec)}")
    print(
        f"  compaction         passed={rec.get('compaction', {}).get('passed')}   window ok={rec.get('contextWindow', {}).get('ok')}"
    )
    if r1:
        print(
            f"  req1               in {r1['inputTokens']} cached {r1['cachedInputTokens']} -> cached frac {r1['cachedInputTokens'] / max(1, r1['inputTokens']):.4f}"
        )
    if steady:
        si = sum(r["inputTokens"] for r in steady)
        sc = sum(r["cachedInputTokens"] for r in steady)
        print(
            f"  steady state       {len(steady)} reqs, cached frac {sc / max(1, si):.4f}"
        )
    print(f"  per-turn requests  {[t['requests'] for t in rec.get('turns', [])]}")
    print()
