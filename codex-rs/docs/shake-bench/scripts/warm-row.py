#!/usr/bin/env python3
"""One warm-matrix table row (and detail block) per replay.json.

Written so the report is generated from the record rather than transcribed from
it: every number below is read out of replay.json, the rollout-activity verdict
and the classified census that the run harness wrote beside it.

  warm-row.py header                      # the summary table's header rows
  warm-row.py row   <results-dir> <label> [note]
  warm-row.py detail <results-dir> <label>

`<results-dir>` is a run directory under `$BENCH_REPLAY/results`.
"""
import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import pricing_lib

PRICING = pricing_lib.load()

COLUMNS = [
    "run",
    "arm",
    "cache",
    "tier",
    "reqs",
    "history after arm",
    "input",
    "cached %",
    "output",
    "API shadow $",
    "credits (std)",
    "credits (fast equiv)",
    "priming overhead",
    "req1 cached frac",
    "steady cached frac",
    "quota Δ 7-day",
    "attributable",
    "acceptance",
    "wall",
    "compaction",
]


def load(results_dir):
    d = pathlib.Path(results_dir)
    rec = json.load(open(d / "replay.json"))
    verdict = {}
    census = {}
    if (d / "rollouts-verdict.json").exists():
        verdict = json.load(open(d / "rollouts-verdict.json"))
    if (d / "census-before.json").exists():
        census = json.load(open(d / "census-before.json"))
    accepted = None
    acc = d / "acceptance.txt"
    if acc.exists():
        text = acc.read_text(errors="replace")
        accepted = "ACCEPTED" if "\nACCEPTED" in text else ("REJECTED" if "REJECTED" in text else None)
        fails = text.count("\nFAIL ")
    else:
        fails = None
    return rec, verdict, census, accepted, fails


def tier_of(rec):
    return "fast" if (rec.get("serviceTierApplied") in ("priority", "fast") or rec.get("serviceTier") == "fast") else "standard"


def split_requests(rec):
    reqs = rec.get("compaction", {}).get("requests", [])
    first = pricing_lib.first_arm_index(rec)
    return reqs[: first - 1], reqs[first - 1 :]


def frac(rows):
    i = sum(r["inputTokens"] for r in rows)
    c = sum(r["cachedInputTokens"] for r in rows)
    return c / i if i else 0.0


def row(results_dir, label, note=""):
    rec, verdict, census, accepted, fails = load(results_dir)
    prime, arm = split_requests(rec)
    tier = tier_of(rec)
    costs = pricing_lib.all_costs(arm, rec.get("model", "gpt-6-astra"), tier, PRICING)
    fast = pricing_lib.run_cost(arm, rec.get("model", "gpt-6-astra"), "codex-credits", "fast", PRICING)
    pcost = pricing_lib.run_cost(prime, rec.get("model", "gpt-6-astra"), "codex-credits", tier, PRICING) if prime else None
    cc = rec.get("cacheCondition") or {}
    cache = cc.get("cache", rec.get("cache", "?"))
    if cc.get("warm"):
        cache = f"{cache}/{cc['warm']}"
    in_tok = sum(r["inputTokens"] for r in arm)
    out_tok = sum(r["outputTokens"] for r in arm)
    q = ((rec.get("quotaDelta") or {}).get("primary")) or {}
    qtxt = (
        f"{q['usedPercentBefore']}% → {q['usedPercentAfter']}% ({q['usedPercentDelta']:+d} pp)"
        + ("  INVALID: reset crossed" if q.get("resetCrossed") else "")
        if q
        else "not reported"
    )
    attributable = "no — another session wrote" if verdict and not verdict.get("quotaAttributable", True) else (
        "yes" if verdict else "?"
    )
    r1 = arm[0] if arm else None
    steady = arm[1:] if len(arm) > 1 else []
    cells = [
        f"**{label}**" + (f"<br>{note}" if note else ""),
        f"`{rec.get('arm')}`",
        cache,
        tier,
        str(len(arm)),
        f"{rec.get('historyTokensAfterArm', rec.get('historyTokens')):,}",
        f"{in_tok:,}",
        f"{frac(arm) * 100:.1f}%",
        f"{out_tok:,}",
        pricing_lib.fmt_usd(costs["apiUsd"]),
        f"**{pricing_lib.fmt_credits(costs['creditsActual'])}**",
        pricing_lib.fmt_credits(fast),
        pricing_lib.fmt_credits(pcost) + f" ({len(prime)} req)" if prime else "—",
        f"{r1['cachedInputTokens'] / max(1, r1['inputTokens']):.4f}" if r1 else "?",
        f"{frac(steady):.4f} ({len(steady)} reqs)" if steady else "—",
        qtxt,
        attributable,
        (f"{accepted} — {12 - (fails or 0)}/12" if accepted == "ACCEPTED" else f"{accepted} ({fails} failing)") if accepted else "?",
        f"{rec.get('workerTotals', {}).get('wallMs', 0) / 1000:.0f}s",
        "PASSED" if rec.get("compaction", {}).get("passed") else "FAILED",
    ]
    return "| " + " | ".join(cells) + " |"


def detail(results_dir, label):
    rec, verdict, census, accepted, fails = load(results_dir)
    prime, arm = split_requests(rec)
    cc = rec.get("cacheCondition") or {}
    lines = [f"### {label}", ""]
    lines.append(
        f"`{rec.get('arm')}`, cache **{cc.get('cache')}{'/' + cc['warm'] if cc.get('warm') else ''}**, "
        f"tier **{tier_of(rec)}** (thread/start echoed `{rec.get('serviceTierApplied')}`), "
        f"{rec.get('startedAt')} → {rec.get('finishedAt')}."
    )
    lines.append("")
    lines.append(f"- per-turn requests: `{[t['requests'] for t in rec.get('turns', [])]}`, stop `{rec.get('stopReason')}`")
    # Fractions and priming overhead are RECOMPUTED from the request sequence
    # rather than read from the record: the live values were computed against
    # firstArmRequestIndex, which over-counted priming by one on run (d) because
    # shake emits a usage notification that carries no request. Where the two
    # disagree, both are shown and the stored one is labelled.
    f1 = arm[0]["cachedInputTokens"] / max(1, arm[0]["inputTokens"]) if arm else None
    f2 = arm[1]["cachedInputTokens"] / max(1, arm[1]["inputTokens"]) if len(arm) > 1 else None
    stored = cc.get("firstArmRequestCachedFraction")
    drifted = stored is not None and f1 is not None and abs(stored - f1) > 1e-6
    lines.append(
        f"- cache condition: {cc.get('assertionRule')}"
    )
    lines.append(
        f"  - recomputed from the arm's own requests: request 1 cached fraction **{f1:.4f}**"
        + (f", request 2 **{f2:.4f}**" if f2 is not None else "")
        + (
            f" (the record's live value was {stored}, computed against a priming boundary one request too late)"
            if drifted
            else f" — live verdict **{'MET' if cc.get('assertionMet') else 'NOT MET'}**"
        )
    )
    if prime:
        lines.append(
            f"- priming overhead, recomputed: {len(prime)} request(s), "
            f"{sum(r['inputTokens'] for r in prime):,} input / {sum(r['cachedInputTokens'] for r in prime):,} cached / "
            f"{sum(r['outputTokens'] for r in prime):,} output tokens; "
            f"{(cc.get('primingToFirstTurnMs') or 0) / 1000:.1f}s from priming to the first arm turn "
            f"(budget {(cc.get('warmStartWindowMs') or 0) / 1000:.0f}s)"
        )
    if census:
        lines.append(
            f"- census before: {census.get('total')} codex processes — {census.get('personal')} personal, "
            f"{census.get('otherAccount')} other-account, {census.get('unknown')} unclassifiable"
        )
    if verdict:
        lines.append(
            f"- other-session rollouts that advanced during the run: **{len(verdict.get('advanced', []))}** → quota "
            f"**{'attributable' if verdict.get('quotaAttributable') else 'UNATTRIBUTABLE'}**"
        )
    q = ((rec.get("quotaDelta") or {}).get("primary")) or {}
    if q:
        drift = (q.get("resetsAtAfter") or 0) - (q.get("resetsAtBefore") or 0)
        lines.append(
            f"- quota, 7-day window: {q['usedPercentBefore']}% → {q['usedPercentAfter']}% "
            f"({q['usedPercentDelta']:+d} pp); resetsAt {q.get('resetsAtBeforeIso')} → {q.get('resetsAtAfterIso')} "
            f"(**{drift:+d} s** of drift), harness flag `resetCrossed: {q.get('resetCrossed')}`"
            + (
                " — a sliding window, not a crossed reset; read the delta as valid"
                if q.get("resetCrossed") and 0 < drift < 600
                else ""
            )
        )
    comp = rec.get("compaction", {})
    ignored = [e for e in comp.get("compactedEvents", []) if e.get("ignored")]
    lines.append(
        f"- compaction: {'PASSED' if comp.get('passed') else 'FAILED'}"
        + (f", {len(ignored)} pre-run shake record(s) ignored" if ignored else "")
        + (f", violations {comp.get('violations')}" if not comp.get("passed") else "")
        + f"; context window observed {rec.get('contextWindow', {}).get('observed')}"
    )
    lines.append("")
    return "\n".join(lines)


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    mode = sys.argv[1]
    if mode == "header":
        print("| " + " | ".join(COLUMNS) + " |")
        print("|" + "|".join("---" for _ in COLUMNS) + "|")
        return 0
    if mode == "row":
        print(row(sys.argv[2], sys.argv[3], sys.argv[4] if len(sys.argv) > 4 else ""))
        return 0
    if mode == "detail":
        print(detail(sys.argv[2], sys.argv[3]))
        return 0
    sys.exit(__doc__)


if __name__ == "__main__":
    sys.exit(main())
