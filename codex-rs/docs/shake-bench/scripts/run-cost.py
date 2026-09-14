#!/usr/bin/env python3
"""Per-request cost for a replay run, in both pricing profiles.

Canonical home of what used to live at $BENCH_REPLAY/logs/run-cost.py
(that path is now a shim that execs this file, so there is one implementation).

Two figures per run, from scripts/pricing_lib.py:

  * API shadow $ --- the API rate card. Summed PER REQUEST, never in aggregate,
    because gpt-6-astra prices the whole request at 2x input / 2x cached / 1.5x
    output once it crosses 272,000 input tokens. The replay worker authenticates
    through ChatGPT login, so this is a shadow cost, not a bill.
  * Codex credits --- the plan rate card the run actually spent, at standard and
    at Fast (2.5x Astra's standard, the only published Fast multiplier). No
    long-context multiplier and no cache-write charge exist on this card.

Priming requests from a --cache warm run are listed and totalled separately as
priming overhead: they are paid, but they are not part of the arm.

Usage: run-cost.py <replay.json> [pricing.json]
"""

import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import pricing_lib

rec = json.load(open(sys.argv[1]))
pricing = pricing_lib.load(sys.argv[2]) if len(sys.argv) > 2 else pricing_lib.load()
model = rec.get("model", "gpt-6-astra")
tier = (
    "fast"
    if (
        rec.get("serviceTierApplied") in ("priority", "fast")
        or rec.get("serviceTier") == "fast"
    )
    else "standard"
)

api = pricing_lib.profile(pricing, "api")["models"][model]
credits = pricing_lib.profile(pricing, "codex-credits")["models"][model]
thr = api["longContextThresholdTokens"]

all_reqs = rec.get("compaction", {}).get("requests", [])
first_arm = pricing_lib.first_arm_index(rec)
prime, reqs = all_reqs[: first_arm - 1], all_reqs[first_arm - 1 :]

print(
    f"model {model}   tier {tier}   requests {len(reqs)}"
    + (f" (+{len(prime)} priming)" if prime else "")
)
print(
    f"long-context (>{thr:,} in, API card only) {sum(1 for r in reqs if r['inputTokens'] > thr)}"
)
print(
    f"peak input {max((r['inputTokens'] for r in reqs), default=0):,}   min input {min((r['inputTokens'] for r in reqs), default=0):,}"
)
print()
for line in pricing_lib.summary_lines(reqs, model, tier, pricing, indent=""):
    print(line)
if prime:
    print()
    print(
        "priming overhead (a --cache warm run's pre-arm request(s); paid, but not part of the arm):"
    )
    for line in pricing_lib.summary_lines(prime, model, tier, pricing, indent="  "):
        print(line)
print()

hdr = f"{'req':>4} {'input':>9} {'cached':>9} {'frac':>6} {'output':>7} {'long':>5} {'usd':>7} {'cr':>8}"
print(hdr)
for r in all_reqs:
    inp, cached, out = r["inputTokens"], r["cachedInputTokens"], r["outputTokens"]
    usd = pricing_lib.request_cost(api, inp, cached, out)
    cr = pricing_lib.request_cost(credits, inp, cached, out, tier=tier)
    mark = "P" if r["i"] < first_arm else ""
    print(
        f"{str(r['i']) + mark:>4} {inp:>9,} {cached:>9,} {cached / max(1, inp):>6.3f} {out:>7,} "
        f"{'yes' if inp > thr else 'no':>5} {'?' if usd is None else f'{usd:>7.3f}'} {'?' if cr is None else f'{cr:>8.2f}'}"
    )
if prime:
    print('("P" marks a priming request.)')
