# Codex agent polling toolkit

This local toolkit helps you find Codex sessions with substantial token use around agent polling, then compare the same reviewed coding task with root-agent polling enabled and disabled. It does not upload transcripts or results. Transcript scanning is read-only; a replay starts from a task and a clean checkpoint, never from recorded assistant messages.

## Scan your own sessions

Run with Node.js 24 or newer:

```sh
cd codex-rs/tools/polling-toolkit
npm test
npm run scan -- --home /path/to/codex-home
```

Pass every Codex home you want to inspect with a separate `--home`. The scanner does not guess paths or use the active account implicitly. It searches only `sessions/` and `archived_sessions/` below those directories, and prints a local JSON report containing session IDs and rollout paths so you can identify the source on your machine. Nothing is sent to a service.

Replay output deliberately strips generated session IDs and rollout paths; it stores aggregate measurements and hashes only.

Sessions rank by **polling candidate input**: pure `wait_agent` input plus pure `write_stdin` input across the root and all descendants. Read the two categories separately. A `write_stdin` call may send input instead of polling, so it is a candidate until you inspect the local task context. The strict `waitAttributedInputTokens` field counts only pure `wait_agent`. Calls mixing `wait_agent` with another action appear in `wait_containing_mixed_calls` and are excluded from the candidate rank because their response usage cannot be split accurately. Missing or malformed action attribution stays in `unknown_no_attribution`. The candidate rank is `null` when no usage trace exists or any response has mixed/unknown action attribution; do not read that as zero polling.

The scanner reads normal Codex rollout `.jsonl` and `.jsonl.zst` files. Its primary usage source is the upstream `token_usage_record` row, with `compacted.latest_token_usage_record` as a checkpoint fallback; it can also read `event_msg` rows of type `raw_response_completed` when those are present. Upstream Codex added persisted per-response usage in [commit 5f79a92 (#41912)](https://github.com/openai/codex/commit/5f79a92e3936274318d2122ae3244e5edd80dd1f), so this is not fork-only instrumentation. The rollout recorder treats `raw_response_completed` as transient, so ordinary current logs generally rely on the persisted usage record. Action attribution additionally needs the matching `response_item` tool-call rows. A session with no recognized usage rows has `usageTraceAvailable: false` and null rank fields; older stock logs from before persisted usage was available therefore report unavailable, not zero. If token usage exists but its tool action cannot be recovered, the tokens appear in `unknown_no_attribution` and the rank is null.

Each row separates root from descendants and reports total input, cached input, uncached input, reported output, and response count by supported action category. Cached input is a subset of input, not an extra amount; uncached input is the remainder. Output uses the provider's `output_tokens` field. Duplicate usage records are counted once per thread and response. Malformed or unreadable rollout files are skipped and counted; the scanner supports `.jsonl` and `.jsonl.zst` rollouts.

## Prepare a reviewed replay

Choose a task and checkpoint you have reviewed for answer leakage. Write a fresh task prompt yourself; do not paste a transcript or a previous assistant answer. Make the prompt naturally require agent delegation and ask for `BENCHMARK_COMPLETE` in the final response. Do not add artificial waiting to inflate the result.

Create a private fixture directory and a one-commit repository containing exactly the checkpoint tree. For example:

```sh
mkdir -m 700 /path/to/polling-fixture
mkdir -m 700 /path/to/polling-fixture/snapshot
git -C /path/to/source-repo archive <reviewed-checkpoint> | tar -x -C /path/to/polling-fixture/snapshot
git -C /path/to/polling-fixture/snapshot init --initial-branch=main
git -C /path/to/polling-fixture/snapshot add -A
git -C /path/to/polling-fixture/snapshot \
  -c user.name='Local Benchmark' -c user.email='benchmark@example.invalid' \
  commit -m 'reviewed polling benchmark checkpoint'
```

Place your task prompt at `task.md` in the fixture directory. The snapshot must be clean and expose one commit, with no remotes, tags, or unreachable objects. The fixture builder rejects dirty/untracked files. At replay time the toolkit makes a fresh local clone of that single commit, so ignored build outputs and untracked files from the source checkout are not copied. Include any dependencies the acceptance command needs inside the reviewed snapshot; replay task commands run without network access.

Create the manifest. The acceptance command is an argv array, not a shell string. Set its working directory and timeout to suit your task; timeouts are capped at 30 minutes.

```sh
npm run fixture -- \
  --directory /path/to/polling-fixture \
  --id reviewed-task \
  --snapshot snapshot \
  --prompt task.md \
  --model gpt-your-exact-model-slug \
  --effort high \
  --acceptance '{"argv":["your-test-command","--flag"],"cwd":".","timeoutMs":1800000}'
```

The manifest stores hashes and relative paths, not the prompt text. Use a generic fixture ID; do not put a session ID, title, or other private label in it. Before proceeding, review the task prompt, checkpoint, acceptance command, and manifest yourself.

## Preflight and paired replay

Build a Codex binary from a revision that contains `features.multi_agent_v2` and the `agent_polling` setting. The binary's exact version and SHA-256 are recorded with each result. Both arms enable V2 and differ only in `agent_polling`:

- `enabled`: the root agent uses polling.
- `disabled`: the interactive root agent uses wake mode.

Wake mode is root-only today; descendants continue polling. `codex exec` is not used. The replay runs through app-server because exec lifecycle support for wake mode is not part of this comparison.

First validate the fixture and paired config without a binary or provider call:

```sh
npm run replay -- --manifest /path/to/polling-fixture/manifest.json --dry-run
```

Replay and isolation probes require Linux with `/usr/bin/bwrap`. The scanner works without bubblewrap. Give `--toolchain-root` only the exact toolchain directory needed by your task, such as one Rust toolchain directory; do not point it at your home directory or a broad cache. Run both model-free checks before a live run:

```sh
npm run replay -- --isolation-check --bin /path/to/codex
npm run replay -- --manifest /path/to/polling-fixture/manifest.json \
  --probe-config --bin /path/to/codex [--toolchain-root /path/to/rust-toolchain]
```

The isolation check verifies a readable workspace control, a permission-denied read of the exact mounted auth file, and that the host home is hidden from task commands. The config probe starts app-server threads for both settings but sends no model request. If either probe fails, do not run a live comparison.

The live comparison is the only provider-backed command. It requires an explicit Codex home whose `auth.json` you intend to use:

```sh
npm run replay -- --manifest /path/to/polling-fixture/manifest.json \
  --live --bin /path/to/codex --auth-home /path/to/selected-codex-home \
  [--toolchain-root /path/to/rust-toolchain] --order polling,wake
```

The selected auth file is copied into a temporary private Codex home for the app-server. Bubblewrap hides the host home; Codex's managed `deny_read` policy blocks task commands from reading the mounted auth file. Task commands and acceptance run without network access or host credentials. Live output is written locally under `${XDG_STATE_HOME:-~/.local/state}/codex-polling-toolkit/runs`, with private directory/file permissions. It contains task artifacts and logs; keep it local unless you have reviewed it for disclosure. `--out` can select another directory under that same private state root. Use `--order wake,polling` for a reverse-order repeat if you want to reduce order effects.

Each arm has finite caps: 5,000,000 input tokens, 250,000 output tokens, 400 model responses, 60 root turns, 8 descendant threads, 45 minutes of task wall time, and 4 minutes without progress. Input, output, and response caps include the root and every descendant; root-turn and descendant-count limits are reported separately. You may lower caps with `--limits` when creating a fixture, but cannot raise these hard ceilings. Acceptance is separately capped at 30 minutes.

A comparison is valid only when the task uses at least one descendant and returns the marker after all observed descendants finish, every observed root/descendant response and token total matches app-server usage notifications, no response has mixed or unknown action attribution, the acceptance command passes, and the checkpoint workspace changes. A missing or malformed rollout, usage mismatch, zero-usage child, mixed/unknown attribution, limit, failed acceptance, missing marker, no descendant, or no workspace change produces an incomplete result that must not be treated as a successful benchmark; `validPair` is true only when both arms pass. The paired report gives polling-minus-wake token differences for the root and descendants separately. Disabling root polling does not disable descendant polling. Cached input is reported separately and remains part of the input total. Persisted coverage diagnostics are aggregate counts and contain no local thread IDs or rollout paths.

This measures one chosen task, not a literal replay of an old conversation. Model runs are variable; use the same model, effort, task, checkpoint, acceptance command, and limits in both arms, then repeat in the opposite order when needed.
