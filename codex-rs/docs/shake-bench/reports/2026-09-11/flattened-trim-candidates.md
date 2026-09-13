# Trim candidates for the flattened 01a07e54-r264 history

Source: `fixtures/01a07e54-r264-redacted.flattened.json` (949 items, 3,382,594
bytes, product-measured **880,020 tokens**, per `replay-proof.md` §1-2).
Codex's local model catalog caps `gpt-6-astra` at 872,000 tokens, so this
history auto-compacts on turn 1.

## Method

No tokenizer is vendored in this repo. `scripts/history-sizes.ts` (new,
read-only) re-derives the same emit ordering `flatten-history.ts` produces
(same 5 rollouts, same cutoff, same plumbing/role filters) but keeps
per-emit `timestamp`/`agent`/`kind` metadata that the shipped script discards
after sorting. Per-item size is `bytes/3.844`, a constant calibrated so the
summed estimate over all 949 items reproduces the fixture's measured 880,020
tokens exactly for this payload — **all "tokens" below are that calibrated
estimate**, not a real tokenizer run. Calling `thread/shake/preview` per
candidate span was ruled out as too slow/costly for a sizing pass (hundreds of
driver round-trips).

## Findings

The token mass is extremely concentrated: **42 of 949 items (4.4%) carry
409,703 tokens (46.6% of the total)**. Every one is a `function_call_output`
whose text literally says `Warning: truncated output (original token count:
N)` — these are already the *product's own* output-size cap (~40,000 chars /
~11,144 tokens) hit by parallel `exec_command` orientation dumps (git log/branch
state, `rg` greps over `shake.rs`, `AGENTS.md` dumps) that each subagent runs
independently on spawn. Top 5 by tokens (all at or near the 11,144-token cap):
items **15, 17, 19, 21, 46** (`shake_investigation`, all 11,144 tok) — parallel
`git show`/`git log --grep`/`rg` orientation at subagent start, superseded by
every later, more targeted read of the same files.

No genuine "16 failed apply_patch retries" or an oversized JUnit blob turned
up as outliers: the 8,723-test JUnit dump (item 909) is only ~5.5k tokens —
the product already truncates it in-line — and the nextest background-poll
loop (items ~795–924, ~153k tokens) is mostly small `write_stdin` polls, not a
single giant item. The real detour is **repeated, superseded onboarding
re-discovery**: `shake_investigation` re-runs the same git/AGENTS.md/shake.rs
survey three times (items 14–46, 605–650, 845–883) — the last cycle (845–883,
~102k tok) overlaps the nextest wait and its conclusion is captured in the
following kept assistant message (item 884), so the raw re-exploration under
it is redundant with items 14–46 and the concluding message can stay.

## Tiers (truncate to head+tail with elision marker, never delete)

Truncating each candidate `function_call_output` to keep ~800 tokens
(head of the command's stdout + tail matches) plus a
`[shaken ~N tokens (recover: item <idx>)]` marker:

- **Tier A** (→ ~750k): truncate the **13** items at/near the 11,144-token
  cap — indices **15, 17, 19, 21, 46, 73, 79, 85, 91, 417, 438, 605, 624**.
  Saves **134,472 tok** → **745,582 tokens** total.
- **Tier B** (→ ~650k): Tier A's 13 plus **9** more — indices **27, 68, 436,
  628, 638, 846, 848, 850, 915** — down to the ~6,500-token floor. Saves
  **226,143 tok** → **653,911 tokens** total (22 items truncated).

All 13 Tier-A items and all Tier-B items are `function_call_output`s from
`exec_command`/`rg`/`git` orientation calls, none are user messages or
assistant decision statements — those are untouched in both tiers.

## flatten-history.ts trim-spec hook

`flatten()` (scripts/flatten-history.ts:157) already builds one `emits[]`
array with `{timestamp, agent, ordinal, items}` before flattening to the
output list — a `--trim <ranges>` flag could filter/truncate `items[i].text`
(for `function_call_output`) right before the final `emits.flatMap(...)` at
line ~245, keyed on the same 0-based output index this report cites, without
touching the call/result pairing invariant.

## Risk

Truncating these 13–22 items removes only raw command stdout the continuing
agent can re-derive by re-running the same `git`/`rg` command against the
checkpoint tree — every conclusion drawn from them survives in a later kept
assistant message. No user message or decision statement is touched in
either tier.
