# Shake — surgical context reduction

`shake` reduces a thread's live context by removing heavy content *mechanically*,
without asking a model to summarize. It is a fork-local feature (not upstream),
so this file is its reference documentation.

Implementation:

| Area | Path |
| --- | --- |
| Pure transform (region detection, in-place mutation) | `codex-rs/core/src/shake.rs` |
| Read-only measurement (`/shake` preview, auto-shake decision input) | `codex-rs/core/src/shake/preview.rs` |
| Recoverable placeholders / artifact markers | `codex-rs/core/src/shake/recovery.rs` |
| Protected-tool allowlist (outputs shake never elides) | `codex-rs/core/src/shake/protection.rs` |
| Auto-shake decision layer (pure, unit-tested) | `codex-rs/core/src/shake/auto.rs` |
| Artifact store, and both size caps (savable-in / readable-out) | `codex-rs/core/src/artifacts.rs` |
| Artifact recovery tool (`read_artifact`) | `codex-rs/core/src/tools/handlers/read_artifact.rs` |
| Orchestration (persist, re-account, announce) | `codex-rs/core/src/session/handlers.rs` (`shake`, `apply_shake`) |
| Auto-shake trigger | `codex-rs/core/src/session/turn.rs` (`maybe_run_pre_sampling_auto_shake`) |

## Modes

| Mode | What it removes | Recoverable? |
| --- | --- | --- |
| `elide` | Whole tool-call outputs and large fenced/XML blocks in message text, replaced by short placeholders | **Yes** — each region is saved to a per-thread artifact first, and the placeholder carries `recover: artifact://<id>` for the `read_artifact` tool, plus `file: <abs_path>` naming the artifact's absolute on-disk path so the model can search it directly with a shell tool |
| `images` | Image blocks | No — content is discarded |
| `thinking` | Reasoning items | No — content is discarded |

The exact placeholder text (`shake/recovery.rs`, `recovery_placeholder`) is:

```
[shaken ~{tokens} tokens from {label} (recover: artifact://<id>; file: {abs_path})]
```

`{abs_path}` is the absolute on-disk path from `ArtifactStore::save`
(`$CODEX_HOME/artifacts/<thread-id>/<uuid>.<label>.log`). Recovering through
`read_artifact` is one bounded page at a time; a model that only needs part of
a large elided output (a specific error line, a matching field) can instead
reach for a shell tool and `grep`/`awk` the path directly. A census of 346 omp
sessions and 9 benchmark runs found zero `read_artifact` call-backs after a
shake, so `read_artifact` is kept as a safety valve and partial extraction via
the path is the expected default.

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
| `read_artifact` | It is the artifact-recovery read itself. Eliding a recovery read only mints another artifact, and can repeat indefinitely. |
| `skills.read` | Skill content is loaded deliberately and is what the agent is working from. |
| `skills.list` | Same: the skill catalog the agent is choosing from. |

This mirrors oh-my-pi's `protectedTools` matcher list
(`packages/agent/src/compaction/tool-protection.ts`), which is checked *before*
elision in both its default (automatic) and aggressive (manual) presets —
`"skill"`, `isSkillReadToolResult` (a `read` of a `skill://` path), and
`isArtifactRecoveryToolResult` (a `read` of an `artifact://` path). The fork's
`skills` namespace is the equivalent of omp's `skill` tool plus its `skill://`
read matcher, and `read_artifact` the equivalent of its `artifact://` read
matcher.

A tool output carries no tool name on the wire —
`ResponseInputItem::FunctionCallOutput` has no `name` field, and the conversion
into `ResponseItem::FunctionCallOutput` sets `name: None` — so protection pairs
each output with its `call_id`'s `FunctionCall` / `CustomToolCall` to recover
the name. An output that *does* carry its own name (a replayed rollout, say) is
honored directly.

Protection is separate from, and additional to, the marker-based guard in
`shake/recovery.rs`, which stops *re*-eliding text that already carries an
`artifact://<id>` or `[shaken …]` marker. The allowlist stops the *first*
elision of a protected tool's output, which the marker guard cannot see.

Because `/shake`'s preview runs the real transformation on a copy
(`estimate_shake`), protected outputs are excluded from the preview's counts and
token estimate automatically — the confirmation prompt never promises savings a
shake cannot deliver.

`elide` requires a **persistent thread**: artifacts are files under
`$CODEX_HOME/artifacts/<thread-id>/`, and an ephemeral thread has nowhere durable
to put them. On an ephemeral thread, `elide` leaves history unchanged and reports
why.

## Artifact size caps

There are two independent caps, and they are easy to confuse. One bounds what
goes *into* an artifact at shake time; the other bounds what comes back *out* on
a recovery read.

| Constant | Direction | Value | Meaning |
| --- | --- | --- | --- |
| `MAX_ARTIFACT_BYTES` | savable **in** | 8 MiB | A region larger than this is not savable, so shake leaves it in place rather than promising a recovery it cannot deliver. Checked by `ArtifactStore::save`, and mirrored by `preview.rs` so the preview's counts match what a confirmed shake would do. |
| `MAX_READ_BYTES` | readable **out** | 3 KiB | Ceiling on one `read_artifact` page, regardless of context headroom. Also bounds the memory one read needs for a hostile artifact. |
| `MIN_READ_BYTES` | readable **out** | 512 B | Floor on one page. Less headroom than this means the read is refused with an actionable error rather than returning a useless sliver. |

### Bounded recovery reads

A recovery read must not be able to push the next request over the context
window. oh-my-pi #11365 documents the failure: a large spilled result recovered
through an unbounded `artifact://` read makes input plus requested output exceed
the window, and the provider rejects the request *before* generation, mid-turn.

So `read_artifact` derives its page budget from the context headroom **at the
time of the read**, not from a fixed number alone
(`artifacts::recovery_read_budget`):

1. Take `ContextWindowTokenStatus::base_window_tokens_remaining` — remaining
   tokens against the model's resolved context window, the same number
   `get_context_remaining` reports. `None` (no resolved window) means there is
   no headroom to derive a bound from, and the ceiling applies.
2. Allow one read at most `RECOVERY_HEADROOM_PERCENT` (50%) of that headroom.
   Recovery is a step in a turn, not the whole turn: the model still needs room
   for its own reasoning and output, and for the next tool call.
3. Clamp into `[MIN_READ_BYTES, MAX_READ_BYTES]`. Below the floor the read is
   refused with an error naming the shortfall and telling the model to free
   context first (`/shake`, `/compact`, or a fresh thread).

`ArtifactStore::read` re-clamps its `max_bytes` argument into the same range, so
no caller can ask for an unbounded page.

A page that the headroom (rather than the ceiling) bounded carries an extra
notice after the usual `[artifact source: …]` marker, naming the byte budget,
the remaining window that produced it, and pointing at the `start_byte` in the
marker above for continuing. The marker itself is unchanged, so the
marker-recognition in `shake/recovery.rs` keeps working.

Recovery reads are also on the protected-tool allowlist above, so a recovered
page is never immediately re-elided — that would only mint another artifact and
could repeat indefinitely.

Note that the fork does **not** implement omp #11365's other suggested fix,
clamping the requested output-token budget to the remaining context before
every provider request. Bounding the read is the half that lives entirely in
`core`; the budget clamp belongs to request construction and is tracked
separately.

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
4. If the model has no resolved context window, stop (no threshold to compare).
5. If active context tokens `<` the resolved threshold percent of the model's
   **resolved** context window, stop.
6. Run the read-only preview. If it would free `<` `min_elidable_percent`% of the
   measured context, stop. This is the anti-thrash guard: a shake that frees
   little is not worth the guaranteed prompt-cache miss, and repeating it every
   turn would be pure loss.
7. If the preview's freed-token estimate is below the absolute
   `min_savings_tokens` floor, stop — a secondary gate alongside step 6 that
   catches the case where a shake clears the percent threshold but still frees
   a trivial number of tokens (small context windows).
8. Otherwise shake.

The threshold is measured against `ModelInfo::resolved_context_window()` — which
already reflects a `model_context_window` config override — not the 95%
"effective" slice and not the 90% auto-compaction limit. With the default 60%,
auto-shake fires well before either compaction trigger.

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
  for a manual shake and `[shake] context reduced surgically (automatic)` for an
  automatic one — the shared prefix keeps existing readers matching, the suffix
  lets benchmarks separate auto from manual;
- a transcript `Warning` event, prefixed `⛭ shake:` (manual) or `⛭ shake (auto):`
  (automatic);
- a structured `INFO` log on target `codex_core::shake` with
  `trigger=manual|automatic`, `mode`, the per-category counts, and
  `tokens_freed`.

Auto-shake decisions themselves log on target `codex_core::auto_shake`: `INFO`
`"auto-shake triggered"` with the measured before/after tokens and shares, and
`TRACE` `"auto-shake skipped"` with a `reason` of `disabled`,
`ephemeral_thread`, `no_context_window`, `below_threshold`,
`below_min_elidable_share`, or `below_min_savings`.

## Configuration

All keys live under `[auto_shake]` in `~/.codex/config.toml`.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `auto_shake.threshold` | `"off"` \| percent \| `"inherit"` | `60%` | Global threshold: shake once active context reaches this percent of the resolved context window, or `"off"` to disable globally. `"inherit"` is invalid here — there is nothing for the global scope to inherit from. |
| `auto_shake.min_elidable_percent` | int 0–100 | `30` | Skip when the preview would free less than this percent of the measured context |
| `auto_shake.min_savings_tokens` | int ≥ 0 | `4000` | Skip when the preview would free fewer than this many tokens, even if `min_elidable_percent` is cleared. Global only (no per-family override), matching oh-my-pi's `minSavings`. |
| `auto_shake.models.<family>.threshold` | `"off"` \| percent \| `"inherit"` | per-family (below) | Per-family threshold. `"inherit"` defers to the resolved global `auto_shake.threshold`; an explicit `"off"` or percent wins over the global value. |
| `auto_shake.models.<family>.min_elidable_percent` | int 0–100 | `30` | Per-family minimum elidable share |

A percent value accepts either an integer (`40`) or a percent string
(`"40%"`).

### Precedence

Highest wins:

1. a family's explicit `"off"` or percent (user-set, or the built-in family
   default),
2. a family's `"inherit"` (user-set, or the built-in family default),
   resolving to the global value,
3. the global `auto_shake.threshold` (user-set, or the built-in `60%`
   default).

In other words: a family wins whenever it commits to a value, and only defers
via `"inherit"`. This is the opposite of the old scheme, where a global key
always overruled every per-model entry — that meant there was no way to opt a
single family out (or in) without also touching every other family's
behavior. Now `[auto_shake.models."gpt-6-astra"] threshold = "off"` disables
just Astra while `gpt-5.6` keeps inheriting the global default, and raising
the global default now also raises every family that still says `"inherit"`.

### Built-in family defaults

| Family | `threshold` | `min_elidable_percent` |
| --- | --- | --- |
| `gpt-5.6` (sol / terra / luna) | `inherit` (→ `60%` by default) | `30` |
| `gpt-6-astra` | `40%` | `30` |
| anything else | `inherit` (→ `60%` by default) | `30` |

Astra is on because the benchmark shows a shake costs one uncached request and
pays back within ~6 requests at typical 300k contexts.

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

Accept the defaults (everything inherits 60%, astra at 40%) — no config
needed.

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
