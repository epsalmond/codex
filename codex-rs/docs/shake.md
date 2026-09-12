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
| Auto-shake decision layer (pure, unit-tested) | `codex-rs/core/src/shake/auto.rs` |
| Orchestration (persist, re-account, announce) | `codex-rs/core/src/session/handlers.rs` (`shake`, `apply_shake`) |
| Auto-shake trigger | `codex-rs/core/src/session/turn.rs` (`maybe_run_pre_sampling_auto_shake`) |

## Modes

| Mode | What it removes | Recoverable? |
| --- | --- | --- |
| `elide` | Whole tool-call outputs and large fenced/XML blocks in message text, replaced by short placeholders | **Yes** — each region is saved to a per-thread artifact first, and the placeholder carries `recover: artifact://<id>` for the `read_artifact` tool |
| `images` | Image blocks | No — content is discarded |
| `thinking` | Reasoning items | No — content is discarded |

A small recent tail (~4k tokens, `MANUAL_PROTECT_TOKENS`) is always protected so
shake cannot strip the tool outputs the agent is currently working from. Only
blocks above `FENCE_MIN_TOKENS` (400) are eligible.

`elide` requires a **persistent thread**: artifacts are files under
`$CODEX_HOME/artifacts/<thread-id>/`, and an ephemeral thread has nowhere durable
to put them. On an ephemeral thread, `elide` leaves history unchanged and reports
why.

## Manual `/shake`

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
5. If active context tokens `<` `threshold_percent`% of the model's **resolved**
   context window, stop.
6. Run the read-only preview. If it would free `<` `min_elidable_percent`% of the
   measured context, stop. This is the anti-thrash guard: a shake that frees
   little is not worth the guaranteed prompt-cache miss, and repeating it every
   turn would be pure loss.
7. Otherwise shake.

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
`ephemeral_thread`, `no_context_window`, `below_threshold`, or
`below_min_elidable_share`.

## Configuration

All keys live under `[auto_shake]` in `~/.codex/config.toml`. Percent values are
integers, matching `effective_context_window_percent`.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `auto_shake.enabled` | bool | per-family (below) | Global enable override |
| `auto_shake.threshold_percent` | int 1–100 | per-family (below) | Shake once active context reaches this percent of the resolved context window |
| `auto_shake.min_elidable_percent` | int 0–100 | `30` | Skip when the preview would free less than this percent of the measured context |
| `auto_shake.models.<family>.enabled` | bool | per-family (below) | Per-family enable |
| `auto_shake.models.<family>.threshold_percent` | int 1–100 | per-family (below) | Per-family threshold |
| `auto_shake.models.<family>.min_elidable_percent` | int 0–100 | `30` | Per-family minimum elidable share |

### Precedence

Highest wins:

1. the global `[auto_shake]` key,
2. the matching `[auto_shake.models.<family>]` key,
3. the built-in family default,
4. the built-in global default.

A global key therefore overrides every per-model entry — setting
`auto_shake.enabled = false` turns auto-shake off everywhere regardless of
`[auto_shake.models]`.

### Built-in family defaults

| Family | `enabled` | `threshold_percent` | `min_elidable_percent` |
| --- | --- | --- | --- |
| `gpt-5.6` (sol / terra / luna) | `true` | `60` | `30` |
| `gpt-6-astra` | `false` | — | — |
| anything else | `false` | `60` | `30` |

Unlisted families default to **off**, so a new or custom model never silently
rewrites history.

### Family matching

A family key matches a model slug that equals it, or extends it with a `-`
suffix — `gpt-5.6` matches `gpt-5.6-sol`, `gpt-5.6-terra` and `gpt-5.6-luna`,
but not `gpt-5.61-sol`. Provider and region qualifiers are stripped first, so
`us.openai.gpt-5.6-sol`, `global.openai.gpt-5.6-luna` and
`bedrock/openai.gpt-5.6-sol` all match `gpt-5.6`.

### Examples

Accept the defaults (gpt-5.6 on at 60%, astra off) — no config needed.

Shake gpt-5.6 earlier and require a bigger win:

```toml
[auto_shake]
threshold_percent = 50
min_elidable_percent = 40
```

Turn auto-shake on for astra too, leaving gpt-5.6 alone:

```toml
[auto_shake.models."gpt-6-astra"]
enabled = true
threshold_percent = 70
```

Disable auto-shake entirely:

```toml
[auto_shake]
enabled = false
```

## Maintenance

`[auto_shake]` lives in `ConfigToml` (`codex-rs/config/src/config_toml.rs`), so
changing it requires regenerating the committed config schema:

```
just write-config-schema
```

`core::config::schema_tests::config_schema_matches_fixture` fails otherwise.
