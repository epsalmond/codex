#!/usr/bin/env python3
"""Per-request cost in both pricing profiles, from pricing.json.

One source of truth for the two Python reporters (arms-row.py, run-cost.py) and
the mirror of src/cost.ts's arithmetic. Two profiles, and which one is the REAL
cost depends on how the worker authenticated:

  api            USD per 1M tokens. Has the 272,000-token long-context rule
                 (2x input / 2x cached / 1.5x output for the WHOLE request), so
                 a run must be summed request by request, never in aggregate.
                 Applies to API-key billing only. For a ChatGPT-login worker
                 this is a SHADOW cost and nothing more.
  codex-credits  Credits per 1M tokens, the Codex plan rate card. No
                 long-context multiplier, no cache-write charge, and Fast mode
                 applies a 2.5x multiplier to Astra's Standard rate. This is
                 what a ChatGPT-login (auth_mode chatgpt) replay actually
                 spends.

A null rate is never guessed: cost() returns None and the caller prints
"unknown".
"""

import json
import os


def default_pricing_path():
    """Resolve the bundled card for both the source tree and release archive."""
    override = os.environ.get("SHAKE_BENCH_PRICING")
    if override:
        return override
    sibling = os.path.join(os.path.dirname(os.path.abspath(__file__)), "pricing.json")
    if os.path.isfile(sibling):
        return sibling
    return os.path.join(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "pricing.json"
    )


PRICING_PATH = default_pricing_path()


def load(path=None):
    path = path or default_pricing_path()
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


def profile(pricing, name="api"):
    """Resolve a profile name to {unit, unitLabel, models, source, fetchedAt}."""
    profiles = pricing.get("profiles") or {}
    p = profiles.get(name)
    if p is None:
        if name == "api":
            return {
                "unit": "usd",
                "unitLabel": "USD",
                "models": pricing["models"],
                "source": None,
                "fetchedAt": None,
            }
        raise KeyError(f"unknown pricing profile {name}; have {sorted(profiles)}")
    models = pricing[p["modelsRef"]] if p.get("modelsRef") else p.get("models", {})
    return {
        "unit": p["unit"],
        "unitLabel": p["unitLabel"],
        "models": models,
        "source": p.get("source"),
        "fetchedAt": p.get("fetchedAt"),
    }


def request_cost(m, inp, cached, out, tier="standard", cache_write=0):
    """Cost of ONE request under one model's rate card. None if a rate is null.

    The long-context multiplier, when the profile publishes one, applies to the
    whole request once inputTokens crosses the threshold -- which is why this
    takes a single request and never an aggregate.
    """
    thr = m.get("longContextThresholdTokens")
    long_ctx = thr is not None and inp > thr

    def mult(key):
        if not long_ctx:
            return 1.0
        v = m.get(key)
        return None if not isinstance(v, (int, float)) else float(v)

    total = 0.0
    for tokens, rate_key, mult_key in (
        (
            max(0, inp - cached - cache_write),
            "inputPerMTok",
            "longContextInputMultiplier",
        ),
        (cached, "cachedInputPerMTok", "longContextCachedInputMultiplier"),
        (cache_write, "cacheWriteInputPerMTok", "longContextCacheWriteInputMultiplier"),
        (out, "outputPerMTok", "longContextOutputMultiplier"),
    ):
        if tokens == 0:
            continue
        rate = m.get(rate_key)
        factor = mult(mult_key)
        if not isinstance(rate, (int, float)) or factor is None:
            return None
        total += (tokens / 1e6) * float(rate) * factor

    if tier == "fast":
        fast = m.get("fastMultiplier")
        if not isinstance(fast, (int, float)):
            return None  # not published for this model: unknown, never assumed 1
        total *= float(fast)
    return total


def run_cost(
    requests, model="gpt-6-astra", profile_name="api", tier="standard", pricing=None
):
    """Sum a replay's requests under one profile/tier. None if any rate is null."""
    pricing = pricing or load()
    m = profile(pricing, profile_name)["models"][model]
    total = 0.0
    for r in requests:
        c = request_cost(
            m, r["inputTokens"], r["cachedInputTokens"], r["outputTokens"], tier=tier
        )
        if c is None:
            return None
        total += c
    return total


def all_costs(requests, model="gpt-6-astra", tier="standard", pricing=None):
    """The three figures every reporter prints for a run.

    `tier` is the tier the run ACTUALLY used; credits are reported at both
    standard and fast so the table shows the multiplier rather than hiding it.
    """
    pricing = pricing or load()
    return {
        "apiUsd": run_cost(requests, model, "api", "standard", pricing),
        "creditsStandard": run_cost(
            requests, model, "codex-credits", "standard", pricing
        ),
        "creditsFast": run_cost(requests, model, "codex-credits", "fast", pricing),
        "creditsActual": run_cost(requests, model, "codex-credits", tier, pricing),
        "tier": tier,
    }


def first_arm_index(rec):
    """1-based index of the arm's first model request in the rollout sequence.

    Prefers the priming turns' OWN request counts over the recorded
    firstArmRequestIndex: the index was computed live from the usage-notification
    stream, which in run (d) of the warm matrix contained one notification that
    carried no request at all (shake republishing the thread's size), so the
    index over-counted priming by one. The priming turns' request counts come
    from the same stream but are summed per turn, so a stray notification
    outside a turn cannot inflate them.
    """
    priming = rec.get("priming")
    if isinstance(priming, list) and priming:
        return sum(int(p.get("requests", 0)) for p in priming) + 1
    cc = rec.get("cacheCondition") or {}
    return int(cc.get("firstArmRequestIndex", rec.get("firstArmRequestIndex", 1)) or 1)


def fmt_usd(v):
    return "unknown" if v is None else f"${v:,.2f}"


def fmt_credits(v):
    return "unknown" if v is None else f"{v:,.0f} cr"


def summary_lines(
    requests, model="gpt-6-astra", tier="standard", pricing=None, indent="  "
):
    """The cost block: API shadow dollars, then Codex credits at both tiers."""
    c = all_costs(requests, model, tier, pricing)
    return [
        f"{indent}API shadow $       {fmt_usd(c['apiUsd'])}   (API rate card; NOT what a ChatGPT-login run pays)",
        f"{indent}Codex credits      standard {fmt_credits(c['creditsStandard'])}   fast {fmt_credits(c['creditsFast'])}"
        + f"   <- run was {tier}: {fmt_credits(c['creditsActual'])}",
    ]
