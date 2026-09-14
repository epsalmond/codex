#!/usr/bin/env python3
"""Estimate what auto-shake would have saved on unshaken Codex rollouts.

Read-only and offline. Never writes to a session file or sends transcript data
anywhere. Walks JSONL rollout transcripts under one or more session roots
(default: `$CODEX_HOME/sessions`, falling back to `~/.codex/sessions`), excludes
any thread that already shows a real `[shake]` compaction, and simulates the
`[auto_shake]` decision policy (`codex-rs/core/src/shake/auto.rs`) against each
remaining thread's per-request `token_count` events.

Token size estimate for tool-output text: len(utf8 bytes) / 4. This is a
rough proxy for the model's own tokenizer and is the single largest source of
error in every number this script prints -- see the docstring on
`estimate_tokens`.

Simulation, per unshaken session, in transcript order:

  1. Every distinct `event_msg` of type `token_count` is one model request.
     Adjacent or repeated notifications with the same `last_token_usage` and
     `total_token_usage` are counted once. Its
     `info.last_token_usage.input_tokens` is used as the context size at that
     point; the script does not re-derive request size from summed item bytes.
  2. Every `function_call_output` / `custom_tool_call_output` response_item
     is a candidate elision block, sized by estimate_tokens() on its output
     text, and tagged with the identity of the tool that produced it (via its
     call_id's matching function_call/custom_tool_call). Blocks produced by a
     protected tool (skills.read, skills.list) are never eligible.
  3. At each request whose modeled input_tokens crosses the model's resolved
     threshold (160,000 tokens for gpt-5.6, 40% of the window for gpt-6-astra,
     and 60% of the window for other model families), the policy simulation
     looks at every
     not-yet-elided eligible block that appears before this request AND
     outside the protected tail (the most recent AUTO_PROTECT_TOKENS=16000
     tokens' worth of blocks, walking backward from the request), with each
     block at least FENCE_MIN_TOKENS=400 tokens. If the eligible total E
     clears both min_elidable_percent=30% of input_tokens and
     min_savings_tokens=4000, a plain auto-shake fires. Otherwise the same
     computation is retried with the escalated protect window
     (MANUAL_PROTECT_TOKENS=4000) and a halved min_savings floor (2000); if
     that clears 30% and 2000, an escalated shake fires instead. Otherwise no
     shake fires at this request.
  4. A fired shake removes its blocks from the candidate pool for the rest of
     the session (an elided block cannot be elided again) and adds E to a
     running `cumulative_reduction`. Every request from here on (including
     the firing request itself, since a shake runs pre-sampling) is assumed
     to have sent `cumulative_reduction` fewer input tokens than it actually
     did; savings are split into cached/uncached shares using that request's
     own cached_input_tokens / input_tokens ratio, then priced with
     pricing_lib against the request's actual model.

  A second, independent "naive upper bound" simulation applies no threshold
  and no min_elidable/min_savings floor at all: at literally every request it
  elides every not-yet-elided eligible block outside the (plain,
  AUTO_PROTECT_TOKENS) protected tail. This is the ceiling the policy
  simulation is bounded by, not a realistic number.

A normal compaction replaces the live history. Blocks before that boundary
  are discarded from the simulation, and a later shake cannot claim their
  bytes again. A rollout that contains a real `[shake]` compaction is excluded
entirely because its recorded history is already counterfactual.

The estimator models threshold-triggered auto-shake only. It does not infer
prompt-cache expiry or the separate cold-resume trigger, so its savings can be
lower than a build with `auto_shake.cold_resume = true`.

  Counterfactual context after a fired shake: a rollout's recorded
  input_tokens is ground truth for what actually happened, but once a
  simulated shake has fired earlier in the thread, the *real* auto-shake
  would have been looking at a smaller live context from that point on. So
  step 3's threshold check (and the min_elidable_percent floor, which is
  also a percentage of that same modeled context) does not use the raw
  input_tokens for a request after any prior fire; it uses
  `max(0, input_tokens - cumulative_reduction)`, where cumulative_reduction
  is the running total of E from every simulated shake that already fired
  earlier in this thread (never negative). This is still an approximation
  (a real shake also changes what the *next* live request actually costs,
  which in turn changes cumulative_reduction's growth path slightly
  differently than this linear model), but it removes the previous
  systematic bias of checking the threshold against a context size the
  session would no longer actually have after its own earlier shakes.

Usage:
  shake-savings-estimate.py [--since HOURS] [--roots DIR [DIR ...]] [--json] [--context-window N]
  shake-savings-estimate.py --start YYYY-MM-DD [--end YYYY-MM-DD] [--roots DIR [DIR ...]] [--json]
  shake-savings-estimate.py --by-week [--roots DIR [DIR ...]] [--json]

--start/--end select rollouts by the thread's own local-time timestamp
(parsed from the rollout-<local-time>-<uuid>.jsonl filename, falling back to
the first line's top-level `timestamp` field -- which is UTC -- converted to
local time) instead of by file mtime; --end is exclusive. --start/--end and
--since are mutually exclusive; when either --start or --end is given,
--since is ignored.

`--since` selects sessions by rollout-file modification time; each selected
transcript is modeled in full, including requests older than the lookback
window. Date-bounded mode selects by the session's local start date instead.

Credit figures are projections from the bundled `codex-credits` rate card,
whose fetch date is printed and source is included in JSON reports. They are not plan
quota measurements. Token sizes for tool output use the rough UTF-8 bytes/4
proxy. A request's context window comes from `--context-window`, then raw
`config.toml`, then the recorded usable window reconstructed at 95%, and
finally the 1,050,000-token built-in fallback. No API calls, OMP session
support, or transcript modifications are performed.
"""

import argparse
import datetime as dt
import json
import os
import pathlib
import re
import sys
import time

sys.dont_write_bytecode = True
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import pricing_lib

AUTO_PROTECT_TOKENS = 16_000
MANUAL_PROTECT_TOKENS = 4_000
FENCE_MIN_TOKENS = 400
MIN_ELIDABLE_PERCENT = 30
MIN_SAVINGS_TOKENS = 4_000
GLOBAL_THRESHOLD_PERCENT = 60
ASTRA_THRESHOLD_PERCENT = 40
GPT_5_6_THRESHOLD_TOKENS = 160_000
DEFAULT_CONTEXT_WINDOW = 1_050_000
# `token_count.info.model_context_window` is the usable 95% slice. The
# auto-shake threshold is defined against the model's resolved raw window.
USABLE_CONTEXT_WINDOW_PERCENT = 95
PROTECTED_TOOLS = {"skills.read", "skills.list"}

ROLLOUT_FILENAME_TS_RE = re.compile(r"rollout-(\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2})-")


def default_roots():
    """Resolve default session roots when the command is invoked."""
    codex_home = os.path.expanduser(os.environ.get("CODEX_HOME", "~/.codex"))
    candidates = [os.path.join(codex_home, "sessions")]
    second_home = os.environ.get("SECOND_CODEX_HOME")
    if second_home:
        candidates.append(os.path.join(os.path.expanduser(second_home), "sessions"))
    roots = []
    for root in candidates:
        root = os.path.normpath(root)
        if root not in roots and os.path.isdir(root):
            roots.append(root)
    return roots


def iso_utc_to_local(ts):
    """Parse an ISO-8601 UTC timestamp (`...Z`) into a naive local datetime.

    Returns None if `ts` is missing or unparseable.
    """
    if not ts:
        return None
    try:
        parsed = dt.datetime.fromisoformat(ts.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is not None:
        parsed = parsed.astimezone().replace(tzinfo=None)
    return parsed


def rollout_local_timestamp(path):
    """Best-effort local-time timestamp for a rollout file, for --start/--end.

    Prefers the filename's embedded timestamp (rollout filenames are named
    with local wall-clock time, e.g. rollout-2026-07-24T09-23-45-<uuid>.jsonl
    for a session that started at 09:23 local while the first line inside it
    is stamped 16:23Z -- confirmed against real rollouts, a 7h offset
    matching Pacific Daylight Time). Falls back to the first line's
    top-level `timestamp` field (UTC, `Z` suffix), converted to local time.
    """
    m = ROLLOUT_FILENAME_TS_RE.search(os.path.basename(path))
    if m:
        try:
            return dt.datetime.strptime(m.group(1), "%Y-%m-%dT%H-%M-%S")
        except ValueError:
            pass
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            first = fh.readline()
        d = json.loads(first)
    except (OSError, json.JSONDecodeError):
        return None
    return iso_utc_to_local(d.get("timestamp"))


def estimate_tokens(text):
    """bytes/4 -- a rough proxy for the real tokenizer, not a substitute."""
    if not text:
        return 0
    return len(text.encode("utf-8", "replace")) // 4


def output_text(payload):
    """Flatten a function_call_output / custom_tool_call_output's `output`."""
    out = payload.get("output")
    if isinstance(out, str):
        return out
    if isinstance(out, list):
        parts = []
        for item in out:
            if isinstance(item, dict) and isinstance(item.get("text"), str):
                parts.append(item["text"])
        return "".join(parts)
    if isinstance(out, dict):
        return json.dumps(out)
    return ""


def tool_identity(name, namespace):
    if namespace:
        return f"{namespace}.{name}"
    return name or ""


def threshold_for_model(model, context_window):
    """Return the built-in auto-shake threshold for one model."""
    model = (model or "").split("/")[-1]
    if "openai." in model:
        model = model.rsplit("openai.", 1)[-1]
    if model == "gpt-5.6" or model.startswith("gpt-5.6-"):
        return min(GPT_5_6_THRESHOLD_TOKENS, context_window)
    percent = (
        ASTRA_THRESHOLD_PERCENT
        if model == "gpt-6-astra" or model.startswith("gpt-6-astra-")
        else GLOBAL_THRESHOLD_PERCENT
    )
    return context_window * percent // 100


def context_window_from_config(codex_home):
    """Best-effort model_context_window read from <codex_home>/config.toml."""
    path = os.path.join(codex_home, "config.toml")
    try:
        with open(path, encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if line.startswith("model_context_window"):
                    _, _, rhs = line.partition("=")
                    try:
                        return int(rhs.strip().split("#")[0].strip())
                    except ValueError:
                        pass
    except OSError:
        pass
    return None


def codex_home_for_root(root):
    # <codex_home>/sessions -> <codex_home>
    return os.path.dirname(os.path.normpath(root))


def load_rollout(path):
    """Parse one rollout JSONL file into the structures the simulation needs.

    Returns None if the file is unreadable/unparseable-into-anything-useful.
    """
    lines = []
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            for i, raw in enumerate(fh):
                raw = raw.strip()
                if not raw:
                    continue
                try:
                    d = json.loads(raw)
                except json.JSONDecodeError:
                    continue
                d.setdefault("ordinal", i)
                lines.append(d)
    except OSError:
        return None
    if not lines:
        return None

    shaken = False
    call_names = {}  # call_id -> tool identity
    blocks = []  # dicts: ordinal, call_id, tokens, protected
    requests = []  # dicts: ordinal, input_tokens, cached_input_tokens, output_tokens, model
    model_changes = []  # (ordinal, model)
    message_count = 0
    base_model = None
    history_epoch = 0
    last_usage_key = None

    for d in lines:
        typ = d.get("type")
        payload = d.get("payload") or {}
        ordinal = d.get("ordinal", 0)

        if typ == "compacted":
            msg = payload.get("message")
            if isinstance(msg, str) and msg.startswith("[shake]"):
                shaken = True
            # Compaction replaces the live history. Keep old blocks tagged with
            # their epoch so earlier requests can still be simulated, while
            # preventing them from leaking into the replacement history.
            history_epoch += 1
            call_names.clear()
            last_usage_key = None

        elif typ == "session_meta":
            base_model = (
                (payload.get("base_instructions") or {})
                .get("provenance", {})
                .get("model")
            )

        elif typ == "turn_context":
            m = payload.get("model")
            if m:
                model_changes.append((ordinal, m))

        elif typ == "response_item":
            ptype = payload.get("type")
            if ptype == "message":
                message_count += 1
            elif ptype == "function_call":
                call_names[payload.get("call_id")] = tool_identity(
                    payload.get("name"), payload.get("namespace")
                )
            elif ptype == "custom_tool_call":
                call_names[payload.get("call_id")] = tool_identity(
                    payload.get("name"), payload.get("namespace")
                )
            elif ptype in ("function_call_output", "custom_tool_call_output"):
                call_id = payload.get("call_id")
                text = output_text(payload)
                tokens = estimate_tokens(text)
                identity = call_names.get(call_id, "")
                blocks.append(
                    {
                        "ordinal": ordinal,
                        "epoch": history_epoch,
                        "tokens": tokens,
                        "protected": identity in PROTECTED_TOOLS,
                    }
                )

        elif typ == "event_msg" and payload.get("type") == "token_count":
            info = payload.get("info") or {}
            last = info.get("last_token_usage") or {}
            if "input_tokens" not in last:
                continue
            input_tokens = last.get("input_tokens")
            if not isinstance(input_tokens, (int, float)) or input_tokens <= 0:
                # Compaction emits a zero usage notification; it is not a
                # model request and must not add an empty request to totals.
                continue
            total = info.get("total_token_usage") or {}
            usage_key = (
                tuple(
                    last.get(k)
                    for k in (
                        "input_tokens",
                        "cached_input_tokens",
                        "cache_write_input_tokens",
                        "output_tokens",
                        "reasoning_output_tokens",
                    )
                ),
                tuple(
                    total.get(k)
                    for k in (
                        "input_tokens",
                        "cached_input_tokens",
                        "cache_write_input_tokens",
                        "output_tokens",
                        "reasoning_output_tokens",
                    )
                ),
            )
            if usage_key == last_usage_key:
                continue
            last_usage_key = usage_key
            recorded_context_window = info.get("model_context_window")
            if (
                not isinstance(recorded_context_window, (int, float))
                or recorded_context_window <= 0
            ):
                recorded_context_window = None
            else:
                usable = int(recorded_context_window)
                recorded_context_window = (
                    usable * 100 + USABLE_CONTEXT_WINDOW_PERCENT - 1
                ) // USABLE_CONTEXT_WINDOW_PERCENT
            requests.append(
                {
                    "ordinal": ordinal,
                    "input_tokens": input_tokens,
                    "cached_input_tokens": last.get("cached_input_tokens", 0) or 0,
                    "output_tokens": last.get("output_tokens", 0) or 0,
                    "context_window": recorded_context_window,
                    "epoch": history_epoch,
                    "timestamp": d.get("timestamp"),
                }
            )

    model_changes.sort(key=lambda t: t[0])

    def model_at(ordinal):
        m = base_model
        for o, mm in model_changes:
            if o <= ordinal:
                m = mm
            else:
                break
        return m or "unknown"

    for r in requests:
        r["model"] = model_at(r["ordinal"])

    return {
        "path": path,
        "shaken": shaken,
        "blocks": blocks,
        "requests": requests,
        "message_count": message_count,
    }


def eligible_sum(blocks, before_ordinal, elided_ids, protect_tokens, epoch=None):
    """Sum of not-yet-elided, non-protected-tool blocks before `before_ordinal`
    that fall outside the trailing `protect_tokens` window, each >= FENCE_MIN_TOKENS.

    Returns (sum_tokens, set_of_block_indices_that_would_be_elided).
    `blocks` is the full ordered list; `elided_ids` holds indices already
    removed by an earlier shake in this simulation.
    """
    candidates = [
        (i, b)
        for i, b in enumerate(blocks)
        if b["ordinal"] < before_ordinal
        and i not in elided_ids
        and (epoch is None or b.get("epoch") == epoch)
    ]
    # Walk newest-first to find the protected tail.
    candidates.sort(key=lambda ib: ib[1]["ordinal"], reverse=True)
    tail_used = 0
    eligible = []
    for i, b in candidates:
        if tail_used < protect_tokens:
            tail_used += b["tokens"]
            continue
        if b["protected"]:
            continue
        if b["tokens"] < FENCE_MIN_TOKENS:
            continue
        eligible.append((i, b["tokens"]))
    total = sum(t for _, t in eligible)
    return total, {i for i, _ in eligible}


def simulate_policy(rec, context_window, requests=None):
    """Threshold-driven auto-shake simulation. Returns per-request savings rows
    and a list of fired shake events.

    Counterfactual context: once a simulated shake has fired earlier in this
    thread, the input_tokens a later request actually reported is no longer
    what the real (already-shrunk) context would have looked like at that
    request. Both the threshold check and the min_elidable_percent floor
    (a percentage of that same modeled context) use
    `counterfactual = max(0, input_tokens - cumulative_reduction)`, where
    cumulative_reduction is the running total of every earlier fire in this
    thread -- never the raw reported input_tokens once a prior shake fired.
    """
    blocks = rec["blocks"]
    requests = (
        requests
        if requests is not None
        else sorted(rec["requests"], key=lambda r: r["ordinal"])
    )
    elided = set()
    cumulative_reduction = 0
    history_epoch = None
    events = []
    rows = []

    for r in requests:
        if r.get("epoch") != history_epoch:
            elided = set()
            cumulative_reduction = 0
            history_epoch = r.get("epoch")
        inp = r["input_tokens"]
        counterfactual_inp = max(0, inp - cumulative_reduction)
        model = r["model"]
        resolved_window = context_window
        if resolved_window is None:
            resolved_window = (
                rec.get("fallback_context_window")
                or r.get("context_window")
                or DEFAULT_CONTEXT_WINDOW
            )
        threshold = threshold_for_model(model, resolved_window)
        fired_e = 0

        if counterfactual_inp >= threshold and counterfactual_inp > 0:
            e_plain, ids_plain = eligible_sum(
                blocks, r["ordinal"], elided, AUTO_PROTECT_TOKENS, r.get("epoch")
            )
            if (
                e_plain >= MIN_SAVINGS_TOKENS
                and (e_plain * 100 // counterfactual_inp) >= MIN_ELIDABLE_PERCENT
            ):
                fired_e, fired_ids, tier = e_plain, ids_plain, "automatic"
            else:
                e_esc, ids_esc = eligible_sum(
                    blocks, r["ordinal"], elided, MANUAL_PROTECT_TOKENS, r.get("epoch")
                )
                if (
                    e_esc >= (MIN_SAVINGS_TOKENS // 2)
                    and (e_esc * 100 // counterfactual_inp) >= MIN_ELIDABLE_PERCENT
                ):
                    fired_e, fired_ids, tier = e_esc, ids_esc, "automatic_escalated"

            if fired_e:
                elided |= fired_ids
                cumulative_reduction += fired_e
                events.append(
                    {
                        "ordinal": r["ordinal"],
                        "model": model,
                        "tokens_freed": fired_e,
                        "tier": tier,
                    }
                )

        savings = min(inp, cumulative_reduction)
        cached_ratio = (r["cached_input_tokens"] / inp) if inp else 0.0
        saved_cached = savings * cached_ratio
        saved_uncached = savings - saved_cached
        rows.append(
            {
                "model": model,
                "input_tokens": inp,
                "cached_input_tokens": r["cached_input_tokens"],
                "output_tokens": r["output_tokens"],
                "saved_tokens": savings,
                "saved_cached": saved_cached,
                "saved_uncached": saved_uncached,
                "timestamp": r.get("timestamp"),
            }
        )
    return rows, events


def simulate_naive(rec, requests=None):
    """Naive upper bound: elide every eligible block outside the plain
    protect window at every single request, no threshold, no floors.

    Returns a list of per-request saved-token amounts, in the same order as
    `requests` (or `rec["requests"]` sorted by ordinal, if not given), so
    callers can zip it against simulate_policy's rows for the same thread.
    """
    blocks = rec["blocks"]
    requests = (
        requests
        if requests is not None
        else sorted(rec["requests"], key=lambda r: r["ordinal"])
    )
    elided = set()
    cumulative_reduction = 0
    history_epoch = None
    saved = []
    for r in requests:
        if r.get("epoch") != history_epoch:
            elided = set()
            cumulative_reduction = 0
            history_epoch = r.get("epoch")
        e, ids = eligible_sum(
            blocks, r["ordinal"], elided, AUTO_PROTECT_TOKENS, r.get("epoch")
        )
        if e:
            elided |= ids
            cumulative_reduction += e
        saved.append(min(r["input_tokens"], cumulative_reduction))
    return saved


def price_rows(rows, pricing):
    """Projected baseline credits, policy credits, and savings, summed."""
    baseline_projected = 0.0
    policy_projected = 0.0
    unknown_model = set()
    for r in rows:
        model = r["model"]
        try:
            m = pricing_lib.profile(pricing, "codex-credits")["models"][model]
        except KeyError:
            unknown_model.add(model)
            continue
        actual = pricing_lib.request_cost(
            m, r["input_tokens"], r["cached_input_tokens"], r["output_tokens"]
        )
        policy = pricing_lib.request_cost(
            m,
            r["input_tokens"] - r["saved_tokens"],
            max(0.0, r["cached_input_tokens"] - r["saved_cached"]),
            r["output_tokens"],
        )
        if actual is not None:
            baseline_projected += actual
        if policy is not None:
            policy_projected += policy
    return baseline_projected, policy_projected, unknown_model


def select_files(root, cutoff, start_dt, end_dt):
    """Return the .jsonl paths under `root` matching the active time filter.

    `cutoff` (epoch seconds, mtime-based) is used when neither `start_dt` nor
    `end_dt` is set. Otherwise files are filtered by rollout_local_timestamp
    against `start_dt` (inclusive) / `end_dt` (exclusive), either of which
    may be None for an open-ended bound.
    """
    files = []
    date_mode = start_dt is not None or end_dt is not None
    for dirpath, _dirnames, filenames in os.walk(root):
        for fn in filenames:
            if not fn.endswith(".jsonl"):
                continue
            fp = os.path.join(dirpath, fn)
            if date_mode:
                ts = rollout_local_timestamp(fp)
                if ts is None:
                    continue
                if start_dt is not None and ts < start_dt:
                    continue
                if end_dt is not None and ts >= end_dt:
                    continue
                files.append(fp)
            else:
                try:
                    if os.path.getmtime(fp) >= cutoff:
                        files.append(fp)
                except OSError:
                    continue
    return files


def week_key(ts_raw):
    """(iso_year, iso_week) for a request's raw UTC timestamp, local time.

    Returns None if `ts_raw` is missing/unparseable, so callers can bucket
    those separately rather than silently mis-attribute them.
    """
    local = iso_utc_to_local(ts_raw)
    if local is None:
        return None
    y, w, _ = local.isocalendar()
    return (y, w)


def row_costs(r, pricing):
    """Projected baseline and policy credits for one row, or (None, None) if
    its model has no published codex-credits rate."""
    try:
        m = pricing_lib.profile(pricing, "codex-credits")["models"][r["model"]]
    except KeyError:
        return None, None
    actual = pricing_lib.request_cost(
        m, r["input_tokens"], r["cached_input_tokens"], r["output_tokens"]
    )
    policy = pricing_lib.request_cost(
        m,
        r["input_tokens"] - r["saved_tokens"],
        max(0.0, r["cached_input_tokens"] - r["saved_cached"]),
        r["output_tokens"],
    )
    return actual, policy


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument(
        "--since",
        type=float,
        default=24.0,
        help="lookback window in hours (default 24)",
    )
    ap.add_argument(
        "--start",
        type=str,
        default=None,
        help="YYYY-MM-DD, local time, inclusive; alternative to --since",
    )
    ap.add_argument(
        "--end",
        type=str,
        default=None,
        help="YYYY-MM-DD, local time, exclusive; pairs with --start",
    )
    ap.add_argument(
        "--by-week",
        action="store_true",
        help="print per-ISO-week, per-root totals instead",
    )
    ap.add_argument(
        "--context-window",
        type=int,
        default=None,
        help="override the resolved model context window for every root (bypasses config.toml); "
        "use this to model an earlier config.toml value, e.g. --context-window 272000",
    )
    ap.add_argument(
        "--roots", nargs="+", default=None, help="session root directories to scan"
    )
    ap.add_argument(
        "--json",
        action="store_true",
        help="emit machine-readable JSON instead of a report",
    )
    args = ap.parse_args()

    roots = args.roots or default_roots()
    start_dt = dt.datetime.strptime(args.start, "%Y-%m-%d") if args.start else None
    end_dt = dt.datetime.strptime(args.end, "%Y-%m-%d") if args.end else None
    cutoff = time.time() - args.since * 3600
    pricing = pricing_lib.load()
    pricing_profile = pricing_lib.profile(pricing, "codex-credits")
    pricing_info = {
        "profile": "codex-credits",
        "unit": pricing_profile["unit"],
        "source": pricing_profile.get("source"),
        "fetched_at": pricing_profile.get("fetchedAt"),
    }

    per_root = {}
    all_sessions = []  # for top-5 across roots
    by_week = {}  # (iso_year, iso_week, root) -> agg dict

    for root in roots:
        root = os.path.expanduser(root)
        configured_context_window = context_window_from_config(
            codex_home_for_root(root)
        )
        fallback_context_window = (
            args.context_window
            if args.context_window is not None
            else configured_context_window or DEFAULT_CONTEXT_WINDOW
        )
        context_window_source = (
            "command-line override"
            if args.context_window is not None
            else "recorded per request, config.toml fallback"
            if configured_context_window
            else "recorded per request, built-in fallback"
        )
        files = select_files(root, cutoff, start_dt, end_dt)

        found = len(files)
        excluded_shaken = 0
        agg = {
            "requests": 0,
            "input_tokens_actual": 0,
            "tokens_saved_policy": 0,
            "tokens_saved_naive": 0,
            "projected_credits_baseline": 0.0,
            "projected_credits_saved_policy": 0.0,
        }
        unknown_models = set()

        for fp in files:
            rec = load_rollout(fp)
            if rec is None:
                continue
            if rec["shaken"]:
                excluded_shaken += 1
                continue
            if not rec["requests"]:
                continue

            # Keep the raw config separate from the display fallback so a
            # recorded usable window can still be reconstructed when config
            # has no model_context_window entry.
            rec["fallback_context_window"] = configured_context_window

            sorted_requests = sorted(rec["requests"], key=lambda r: r["ordinal"])
            rows, events = simulate_policy(
                rec, args.context_window, requests=sorted_requests
            )
            naive_saved_rows = simulate_naive(rec, requests=sorted_requests)
            baseline_projected, policy_projected, unk = price_rows(rows, pricing)
            unknown_models |= unk
            tokens_saved_policy = sum(r["saved_tokens"] for r in rows)
            naive_saved = sum(naive_saved_rows)

            agg["requests"] += len(rows)
            agg["input_tokens_actual"] += sum(r["input_tokens"] for r in rows)
            agg["tokens_saved_policy"] += tokens_saved_policy
            agg["tokens_saved_naive"] += naive_saved
            agg["projected_credits_baseline"] += baseline_projected
            agg["projected_credits_saved_policy"] += (
                baseline_projected - policy_projected
            )

            models_used = sorted({r["model"] for r in rows}) if rows else []
            all_sessions.append(
                {
                    "root": root,
                    "path": fp,
                    "models": models_used,
                    "message_count": rec["message_count"],
                    "requests": len(rows),
                    "tokens_saved_policy": tokens_saved_policy,
                    "tokens_saved_naive": naive_saved,
                    "projected_credits_saved_policy": baseline_projected
                    - policy_projected,
                    "shake_events": len(events),
                    "peak_input_tokens": max(
                        (r["input_tokens"] for r in rows), default=0
                    ),
                }
            )

            if args.by_week:
                thread_id = fp
                for row, naive_row in zip(rows, naive_saved_rows):
                    wk = week_key(row.get("timestamp"))
                    if wk is None:
                        continue
                    key = (wk[0], wk[1], root)
                    bucket = by_week.setdefault(
                        key,
                        {
                            "threads": set(),
                            "requests": 0,
                            "input_tokens_actual": 0,
                            "tokens_saved_policy": 0.0,
                            "tokens_saved_naive": 0.0,
                            "projected_credits_baseline": 0.0,
                            "projected_credits_saved_policy": 0.0,
                        },
                    )
                    bucket["threads"].add(thread_id)
                    bucket["requests"] += 1
                    bucket["input_tokens_actual"] += row["input_tokens"]
                    bucket["tokens_saved_policy"] += row["saved_tokens"]
                    bucket["tokens_saved_naive"] += naive_row
                    actual_c, policy_c = row_costs(row, pricing)
                    if actual_c is not None:
                        bucket["projected_credits_baseline"] += actual_c
                    if actual_c is not None and policy_c is not None:
                        bucket["projected_credits_saved_policy"] += actual_c - policy_c

        per_root[root] = {
            "found": found,
            "excluded_shaken": excluded_shaken,
            "context_window_fallback": fallback_context_window,
            "context_window_source": context_window_source,
            **agg,
            "unknown_models": sorted(unknown_models),
        }

    all_sessions.sort(key=lambda s: s["tokens_saved_policy"], reverse=True)
    top5 = all_sessions[:5]

    if args.by_week:
        if args.json:
            weeks = {
                f"{y}-W{w:02d}|{root}": {
                    "threads": len(v["threads"]),
                    "requests": v["requests"],
                    "input_tokens_actual": v["input_tokens_actual"],
                    "tokens_saved_policy": v["tokens_saved_policy"],
                    "tokens_saved_naive": v["tokens_saved_naive"],
                    "projected_credits_baseline": v["projected_credits_baseline"],
                    "projected_credits_saved_policy": v[
                        "projected_credits_saved_policy"
                    ],
                }
                for (y, w, root), v in sorted(by_week.items())
            }
            print(
                json.dumps(
                    {
                        "since_hours": None if start_dt or end_dt else args.since,
                        "start": args.start,
                        "end": args.end,
                        "pricing": pricing_info,
                        "weeks": weeks,
                    },
                    indent=2,
                )
            )
            return

        fetched_at = pricing_info["fetched_at"] or "unknown date"
        print(
            "shake-savings-estimate --by-week  "
            f"(token estimate: UTF-8 bytes/4; projected Codex credits, rate card fetched {fetched_at})\n"
        )
        header = (
            f"{'week':<9} {'root':<24} {'threads':>7} {'requests':>8} {'input_actual':>13} "
            f"{'saved_policy':>13} {'naive_ceiling':>13} {'projected_baseline':>18} {'projected_saved':>16}"
        )
        print(header)
        for (y, w, root), v in sorted(by_week.items()):
            root_label = os.path.basename(codex_home_for_root(root)) or root
            print(
                f"{y}-W{w:02d}  {root_label:<24} {len(v['threads']):>7} "
                f"{v['requests']:>8} {v['input_tokens_actual']:>13,} {v['tokens_saved_policy']:>13,.0f} "
                f"{v['tokens_saved_naive']:>13,.0f} {v['projected_credits_baseline']:>17,.0f} "
                f"{v['projected_credits_saved_policy']:>16,.0f}"
            )
        return

    if args.json:
        print(
            json.dumps(
                {
                    "since_hours": None if start_dt or end_dt else args.since,
                    "start": args.start,
                    "end": args.end,
                    "roots": per_root,
                    "top_sessions": top5,
                    "pricing": pricing_info,
                },
                indent=2,
            )
        )
        return

    if start_dt or end_dt:
        print(
            f"shake-savings-estimate --start {args.start or '-inf'} --end {args.end or '+inf'}  "
            f"(token estimate: UTF-8 bytes/4; projected Codex credits, rate card fetched "
            f"{pricing_info['fetched_at'] or 'unknown date'})\n"
        )
    else:
        print(
            f"shake-savings-estimate --since {args.since}h  "
            f"(token estimate: UTF-8 bytes/4; projected Codex credits, rate card fetched "
            f"{pricing_info['fetched_at'] or 'unknown date'})\n"
        )
    for root, a in per_root.items():
        print(f"== {root} ==")
        print(f"  rollouts found (selected window):       {a['found']}")
        print(f"  excluded (already shaken):            {a['excluded_shaken']}")
        print(
            f"  context window fallback:              {a['context_window_fallback']:,} "
            f"({a['context_window_source']})"
        )
        if a["unknown_models"]:
            print(
                f"  unpriced models (skipped in credits):  {', '.join(a['unknown_models'])}"
            )
        print(
            f"  requests={a['requests']:,}  input_tokens_actual={a['input_tokens_actual']:,}  "
            f"saved_policy={a['tokens_saved_policy']:,.0f}  saved_naive={a['tokens_saved_naive']:,.0f}"
        )
        print(
            f"  projected_credits_baseline={a['projected_credits_baseline']:,.0f}  "
            f"projected_credits_saved_policy={a['projected_credits_saved_policy']:,.0f}"
        )
        print()

    print("Top 5 sessions by policy tokens saved:")
    print(
        f"{'tokens_saved':>13} {'naive_bound':>13} {'projected_saved':>14} {'reqs':>5} {'peak_input':>11} "
        f"{'msgs':>6} {'model(s)':<28} path"
    )
    for s in top5:
        print(
            f"{s['tokens_saved_policy']:>13,.0f} {s['tokens_saved_naive']:>13,.0f} "
            f"{s['projected_credits_saved_policy']:>14,.0f} {s['requests']:>5} {s['peak_input_tokens']:>11,} "
            f"{s['message_count']:>6} {','.join(s['models']):<28} {s['path']}"
        )


if __name__ == "__main__":
    main()
