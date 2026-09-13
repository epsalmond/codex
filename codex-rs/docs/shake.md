# Shake — surgical context reduction

`shake` reduces a thread's live context by removing heavy content *mechanically*,
without asking a model to summarize. It is a fork-local feature (not upstream),
so this file is its reference documentation.

Implementation:

| Area | Path |
| --- | --- |
| Pure transform (region detection, in-place mutation) | `codex-rs/core/src/shake.rs` |
| Read-only measurement (`/shake` preview, auto-shake decision input) | `codex-rs/core/src/shake/preview.rs` |
| Placeholder text / already-shaken marker | `codex-rs/core/src/shake/recovery.rs` |
| Protected-tool allowlist (outputs shake never elides) | `codex-rs/core/src/shake/protection.rs` |
| Auto-shake decision layer (pure, unit-tested) | `codex-rs/core/src/shake/auto.rs` |
| Artifact store (`save`, the savable-in size cap) | `codex-rs/core/src/artifacts.rs` |
| Orchestration (persist, re-account, announce) | `codex-rs/core/src/session/handlers.rs` (`shake`, `apply_shake`) |
| Auto-shake trigger | `codex-rs/core/src/session/turn.rs` (`maybe_run_pre_sampling_auto_shake`) |

## Modes

| Mode | What it removes | Recoverable? |
| --- | --- | --- |
| `elide` | Whole tool-call outputs and large fenced/XML blocks in message text, replaced by short placeholders | **Yes, via the filesystem** — each region is saved to a per-thread file first, and the placeholder names that file's absolute on-disk path, so the model can search it directly with a shell tool |
| `images` | Image blocks | No — content is discarded |
| `thinking` | Reasoning items | No — content is discarded |

The exact placeholder text (`shake/recovery.rs`, `recovery_placeholder`) is:

```
[shaken ~{tokens} tokens from {label}. original: {abs_path}]
```

`{abs_path}` is the absolute on-disk path from `ArtifactStore::save`
(`$CODEX_HOME/artifacts/<thread-id>/<uuid>.<label>.log`), resolved once when
the store is constructed (`ArtifactStore::for_thread` canonicalizes its root
after creating it), so the path is absolute and symlink-free even if
`CODEX_HOME` was set relative.

There is no tool that reads an artifact back, on purpose. A census of 346
local oh-my-pi sessions and 9 shake-bench runs found zero read-backs after a
shake; no shipped model, config key, flag, or app-server parameter can disable
the shell tool anyway (Guardian's `Disabled` model-capability variant exists
only for the `/models` wire schema, upstream 903b7774bc, and is otherwise
unreachable); and every sandbox mode already gives the model read access to
the artifact directory — the Linux landlock sandbox grants read-only access
from `/` (`linux-sandbox/src/landlock.rs:148`), and the seatbelt read-only
profile allows broad file reads. A dedicated recovery tool would only
duplicate what `grep`/`awk`/`sed` already do for free, with none of a shell
tool's ability to extract just the part the model actually needs.

A recent tail is always protected so shake cannot strip the tool outputs the
agent is currently working from. The protected size depends on how the shake
was triggered: manual `/shake` protects `MANUAL_PROTECT_TOKENS` (4,000 tokens,
matching oh-my-pi's aggressive/manual preset), while automatic shake protects
the much larger `AUTO_PROTECT_TOKENS` (16,000 tokens, matching oh-my-pi's
default automatic preset) since it runs unattended and must not risk
stripping content the agent is actively relying on. Only blocks above
`FENCE_MIN_TOKENS` (400) are eligible.

## Protected tools

Some tool outputs are never elided, however large or old they are, and whether
the shake was manual or automatic. `shake/protection.rs` holds the allowlist
(`PROTECTED_TOOLS`), matched on the tool *identity* of the call that produced
the output:

| Tool | Why it is protected |
| --- | --- |
| `skills.read` | Skill content is loaded deliberately and is what the agent is working from. |
| `skills.list` | Same: the skill catalog the agent is choosing from. |

This mirrors oh-my-pi's `protectedTools` matcher list
(`packages/agent/src/compaction/tool-protection.ts`), which is checked *before*
elision in both its default (automatic) and aggressive (manual) presets —
`"skill"` and `isSkillReadToolResult` (a `read` of a `skill://` path). The
fork's `skills` namespace is the equivalent of omp's `skill` tool plus its
`skill://` read matcher. (management-plane#990 previously also protected the
fork's `read_artifact` tool here; that tool is gone — see "Modes" above.)

A tool output carries no tool name on the wire —
`ResponseInputItem::FunctionCallOutput` has no `name` field, and the conversion
into `ResponseItem::FunctionCallOutput` sets `name: None` — so protection pairs
each output with its `call_id`'s `FunctionCall` / `CustomToolCall` to recover
the name. An output that *does* carry its own name (a replayed rollout, say) is
honored directly.

Protection is separate from, and additional to, the marker-based guard in
`shake/recovery.rs`, which stops *re*-eliding text that already carries a
`[shaken …]` marker. The allowlist stops the *first* elision of a protected
tool's output, which the marker guard cannot see.

Because `/shake`'s preview runs the real transformation on a copy
(`estimate_shake`), protected outputs are excluded from the preview's counts and
token estimate automatically — the confirmation prompt never promises savings a
shake cannot deliver.

`elide` requires a **persistent thread**: artifacts are files under
`$CODEX_HOME/artifacts/<thread-id>/`, and an ephemeral thread has nowhere durable
to put them. On an ephemeral thread, `elide` leaves history unchanged and reports
why.

## Artifact size cap

`MAX_ARTIFACT_BYTES` (8 MiB) bounds what a shake may put *into* an artifact. A
region larger than this is not savable, so shake leaves it in place rather than
promising a recovery it cannot deliver. Checked by `ArtifactStore::save`, and
mirrored by `preview.rs` so the preview's counts match what a confirmed shake
would do.

## Manual `/shake`

Usage: `/shake [elide|images|thinking|help]`. Bare `/shake` defaults to
`elide`; `/shake help` (also `-h`, `--help`, `?`) prints the mode list without
touching history.

`/shake` previews first: it runs the real transformation on a copy, reports
estimated tokens before/after and what would be affected, writes nothing, and
makes no model request. Confirmation is checked against a fingerprint of the
exact history that was measured, so a stale confirmation is rejected.

`/shake` refuses while a turn is in progress, because a running task may hold a
`StepContext` snapshot of the history that the rewrite would invalidate.

## Persistence

Codex's thread store is append-only, so the pre-shake rollout bytes cannot be
rewritten. Shake instead writes a `CompactedItem` carrying the post-shake
replacement history — the only fixpoint the resume reader already knows how to
replay — and starts a fresh auto-compaction window, exactly like a manual
`/compact`. See `Session::replace_history_and_persist_after_shake`.

## Auto-shake

Auto-shake runs `elide` automatically at the pre-sampling point in `run_turn`,
the same place auto-compaction decides. The intent is that a cheap surgical
reduction removes the *need* to compact; auto-compaction stays the fallback and
still fires when auto-shake is disabled, impossible, or would not free enough.

### Decision sequence

1. Resolve the effective settings for the turn's model slug (precedence below).
2. If disabled for that model, stop.
3. If the thread is ephemeral, stop — `elide` needs artifact recovery.
4. If the resolved threshold is a **percent** and the model has no resolved
   context window, stop (no window to compare a percent against). An
   **absolute** (token-count) threshold has no such requirement and skips this
   step.
5. Compute the effective threshold:
   - percent threshold: that percent of the model's **resolved** context
     window;
   - absolute threshold: the configured token count, or `min(configured,
     resolved context window)` when a window is known — so a misconfigured
     absolute value above the window still fires rather than silently never
     triggering.
   If active context tokens `<` the effective threshold, stop.
6. Run the read-only preview. If it would free `<` `min_elidable_percent`% of the
   measured context, stop. This is the anti-thrash guard: a shake that frees
   little is not worth the guaranteed prompt-cache miss, and repeating it every
   turn would be pure loss.
7. If the preview's freed-token estimate is below the absolute
   `min_savings_tokens` floor, stop — a secondary check alongside step 6 that
   catches the case where a shake clears the threshold but still frees a
   trivial number of tokens (small context windows).
8. Otherwise shake.

A percent threshold is measured against `ModelInfo::resolved_context_window()`
— which already reflects a `model_context_window` config override — not the
95% "effective" slice and not the 90% auto-compaction limit. With the default
60%, auto-shake fires well before either compaction trigger.

### Why gpt-5.6 defaults to an absolute token count

gpt-5.6 models pay a long-context penalty above **272,000 input tokens**. A
percent-of-window threshold is fine when the window is close to that number,
but it breaks down on a large configured window: at an 872k window (as some
users run), 60% of the window is over 500k tokens — well past the 272k penalty
line, and likely past the point where auto-compaction (90% of the window) has
already fired. In other words, a percent threshold scales *up* with the
window, but the penalty line does not move.

gpt-5.6's built-in default is therefore an **absolute** threshold of
**160,000 tokens**, regardless of the configured window:

- it is about 60% of the *old* 272k default window, so behavior is unchanged
  for anyone still on that default window size;
- it is comfortably below the 272k penalty line, so the first shake lands
  before the penalty band rather than inside or after it;
- it is well below the 80% compaction point of a 272k window, so auto-shake
  still gets a chance to run before auto-compaction would.

Because it is absolute, this threshold fires at the same point in a session
regardless of how large the configured context window is.

### Escalation

A single elide pass is not always enough: `AUTO_PROTECT_TOKENS`'s large
protected tail (16,000 tokens) can leave the thread above the auto-shake
threshold even after removing everything eligible. When that happens —
whether the first pass applied, or was skipped at step 6 or 7 above
(`below_min_elidable_share` or `below_min_savings`) — `maybe_run_pre_sampling_auto_shake`
runs one more pass with oh-my-pi's aggressive settings before falling through
to compaction:

- protect window: `MANUAL_PROTECT_TOKENS` (4,000 tokens) instead of
  `AUTO_PROTECT_TOKENS`, so more of the recent tail becomes eligible;
- `min_savings_tokens`: halved from the resolved setting (still subject to the
  same `min_elidable_percent`).

"Still above the auto-shake threshold" is re-checked with the exact same
`AutoShakeConfig::decide` call as the first pass, against the token count after
the first pass (the real recomputed total if it shook; the original reading if
it didn't) — so escalation uses the identical enabled / persistent-thread /
context-window / threshold logic, just evaluated a second time. At most one
escalated pass runs per pre-sampling point; only after it (or its skip) does
`run_pre_sampling_compact` re-check `token_limit_reached` and fall through to
`run_auto_compact`. There is no separate config for this: escalation is on
whenever `auto_shake` is on for the model. See management-plane#988; the
one-tier-then-fallback shape mirrors oh-my-pi PR #9705's proposed escalation
(open/unmerged upstream at the time this landed).

### Turn ordering

`/shake` refuses when `active_turn` is `Some`, and by the time `run_turn` runs,
`active_turn` *is* `Some` — so auto-shake cannot go through the manual entry
point. It is nevertheless safe at this exact point, and calls
`handlers::apply_shake` directly:

- `run_pre_sampling_compact` is the first thing `run_turn` does, before
  `capture_step_context`, so no step context has snapshotted this history yet —
  which is the guard's actual precondition.
- The turn's user input has not been recorded yet
  (`run_hooks_and_record_inputs(.., TurnStart)` comes later), so the rewritten
  history is exactly the settled history of previous turns.
- Auto-compaction already replaces history wholesale at this same point via
  `run_auto_compact` → `replace_compacted_history`.

No expected fingerprint is passed: the preview and the shake run back-to-back in
the same turn with no wait on client input, so there is no stale-confirmation
window.

### Subagents

Spawned subagent threads inherit `auto_shake` automatically:
`build_agent_shared_config` (`codex-rs/core/src/tools/handlers/multi_agents_common.rs`)
clones the parent `Config` wholesale and then refreshes only runtime-owned
fields, and subagent turns run through the same `run_turn` →
`run_pre_sampling_compact` path. Note that the effective *model* may differ from
the parent's, so a subagent on a family with auto-shake off will not shake even
when its parent does — which is the intended per-model behavior.

### Observability

A shake that changed anything emits:

- the persisted `CompactedItem` message, `[shake] context reduced surgically`
  for a manual shake, `[shake] context reduced surgically (automatic)` for the
  plain automatic pass, and `[shake] context reduced surgically (automatic,
  escalated)` for the escalated pass — the shared prefix keeps existing
  readers matching, the suffix lets benchmarks separate the three;
- a transcript `Warning` event, prefixed `⛭ shake:` (manual), `⛭ shake (auto):`
  (automatic), or `⛭ shake (auto, escalated):` (escalated);
- a structured `INFO` log on target `codex_core::shake` with
  `trigger=manual|automatic|automatic_escalated`, `mode`, the per-category
  counts, and `tokens_freed`.

Auto-shake decisions themselves log on target `codex_core::auto_shake`: `INFO`
`"auto-shake triggered"` with the measured before/after tokens, shares, and an
`escalated` boolean, and `TRACE` `"auto-shake skipped"` with a `reason` of
`disabled`, `ephemeral_thread`, `no_context_window`, `below_threshold`,
`below_min_elidable_share`, or `below_min_savings` (also carrying `escalated`
when the skip belongs to the second pass).

## Configuration

All keys live under `[auto_shake]` in `~/.codex/config.toml`.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `auto_shake.threshold` | `"off"` \| percent \| tokens \| `"inherit"` | `60%` | Global threshold: shake once active context reaches this percent of the resolved context window (or this many absolute tokens), or `"off"` to disable globally. `"inherit"` is invalid here — there is nothing for the global scope to inherit from. |
| `auto_shake.min_elidable_percent` | int 0–100 | `30` | Skip when the preview would free less than this percent of the measured context |
| `auto_shake.min_savings_tokens` | int ≥ 0 | `4000` | Skip when the preview would free fewer than this many tokens, even if `min_elidable_percent` is cleared. Global only (no per-family override), matching oh-my-pi's `minSavings`. |
| `auto_shake.models.<family>.threshold` | `"off"` \| percent \| tokens \| `"inherit"` | per-family (below) | Per-family threshold. `"inherit"` defers to the resolved global `auto_shake.threshold`; an explicit `"off"`, percent, or absolute token count wins over the global value. |
| `auto_shake.models.<family>.min_elidable_percent` | int 0–100 | `30` | Per-family minimum elidable share |

A percent value accepts either an integer 1–100 (`40`) or a percent string
(`"40%"`). An absolute token count accepts a string with a `k` suffix
(`"160k"`) or a bare integer/string above 100 (`160000`, `"200000"`).

**Disambiguating a bare integer** (no `%` or `k` suffix): `1..=100` is a
percent, anything above `100` is an absolute token count. `0` and negative
values are rejected for every numeric form with a config error.

### Precedence

Highest wins:

1. a family's explicit `"off"`, percent, or absolute token count (user-set,
   or the built-in family default),
2. a family's `"inherit"` (user-set, or the built-in family default),
   resolving to the global value,
3. the global `auto_shake.threshold` (user-set, or the built-in `60%`
   default).

In other words: a family wins whenever it commits to a value, and only defers
via `"inherit"`. This is the opposite of the old scheme, where a global key
always overruled every per-model entry — that meant there was no way to opt a
single family out (or in) without also touching every other family's
behavior. Now `[auto_shake.models."gpt-6-astra"] threshold = "off"` disables
just Astra while `gpt-5.6` keeps its own absolute default, and raising the
global default only raises the families that still say `"inherit"` (that is,
any family with no built-in default of its own, unless the user overrides it).

### Built-in family defaults

| Family | `threshold` | `min_elidable_percent` |
| --- | --- | --- |
| `gpt-5.6` (sol / terra / luna) | `160000` (absolute tokens) | `30` |
| `gpt-6-astra` | `40%` | `30` |
| anything else | `inherit` (→ `60%` by default) | `30` |

Astra is on at a percent because the benchmark shows a shake costs one
uncached request and pays back within ~6 requests at typical 300k contexts.
gpt-5.6 is on at an absolute token count instead of a percent — see "Why
gpt-5.6 defaults to an absolute token count" above. Since the built-in default
is now an explicit value rather than `"inherit"`, a global `auto_shake.threshold
= "off"` no longer disables gpt-5.6; set `[auto_shake.models."gpt-5.6"]
threshold = "off"` (or `"inherit"`, to defer to the global value again).

Unlisted families inherit the global default rather than defaulting to off:
auto-shake is on by default for every model, deferring to whatever the global
threshold resolves to.

### Family matching

A family key matches a model slug that equals it, or extends it with a `-`
suffix — `gpt-5.6` matches `gpt-5.6-sol`, `gpt-5.6-terra` and `gpt-5.6-luna`,
but not `gpt-5.61-sol`. Provider and region qualifiers are stripped first, so
`us.openai.gpt-5.6-sol`, `global.openai.gpt-5.6-luna` and
`bedrock/openai.gpt-5.6-sol` all match `gpt-5.6`.

### Examples

Accept the defaults (unlisted families inherit 60%, astra at 40%, gpt-5.6 at
an absolute 160k tokens) — no config needed.

Shake earlier and require a bigger win, for every family still on `inherit`:

```toml
[auto_shake]
threshold = "50%"
min_elidable_percent = 40
```

Give astra a different threshold than the global default:

```toml
[auto_shake.models."gpt-6-astra"]
threshold = 70
```

Run a very large context window and want gpt-5.6 to shake even earlier than
the 160k default:

```toml
[auto_shake.models."gpt-5.6"]
threshold = "120k"
```

Turn auto-shake off for one family, leaving everything else inheriting the
global value:

```toml
[auto_shake.models."gpt-6-astra"]
threshold = "off"
```

Disable auto-shake entirely:

```toml
[auto_shake]
threshold = "off"
```

## Maintenance

`[auto_shake]` lives in `ConfigToml` (`codex-rs/config/src/config_toml.rs`), so
changing it requires regenerating the committed config schema:

```
just write-config-schema
```

`core::config::schema_tests::config_schema_matches_fixture` fails otherwise.
