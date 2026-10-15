# Hooks observe control-tool waits

Codex previously sent no `PreToolUse` event for Code Mode `exec` cells, Code
Mode `wait`, or unified exec `write_stdin`. A detector running as a
`PreToolUse` hook, such as a burn governor, could not see an agent repeatedly
polling a running process or cell. In one session an agent made 33 consecutive
empty `write_stdin` calls over 37 minutes without being flagged.

These calls now reach `PreToolUse` hooks as observational events, enabled by
default.

## Contract

- Hook stdin has `"observational": true`. Regular events omit the field.
- Hooks run like any other `PreToolUse` hook. `additionalContext` is recorded
  for the model. A deny, exit code 2, or `updatedInput` is ignored and shown as
  a warning on the hook run. The call proceeds with its original input, and its
  result is unchanged.
- Synchronous hooks get a 1-second budget, because these calls are on the
  tool's critical path. A hook still running at the deadline is killed and its
  run is reported as failed. Codex builds the payload only when a `PreToolUse`
  matcher selects the tool.
- `tool_use_id` is the control call's own call id. Its output in the rollout
  (`function_call_output` or `custom_tool_call_output`) has the same id, so an
  observer can correlate the call with its result.
- No observational `PostToolUse` is added. A `write_stdin` call that observes
  its command finishing still runs that command's single `Bash` `PostToolUse`.
- Nested tool calls made inside a cell already ran through gated `PreToolUse`
  and are unchanged.

## Payloads

`write_stdin`:

```json
{"session_id": 1000, "chars": "", "stdin_empty": true, "stdin_bytes": 0,
 "yield_time_ms": 60000, "exec_call_id": "call_abc"}
```

`chars` is present only for empty polls. Input text is never sent;
`stdin_bytes` gives its UTF-8 length. `exec_call_id` is the `exec_command` call
that started the process, when the process is still known. `max_output_tokens`
is present when the model set it.

`wait`:

```json
{"cell_id": "3", "yield_time_ms": 30000, "terminate": false}
```

`max_tokens` is present when the model set it.

`exec`:

```json
{"source_bytes": 812, "source_sha1": "<40 hex>", "yield_time_ms": 10000}
```

Source text is never sent. The digest lets an observer recognize a repeated
cell. `yield_time_ms` and `max_output_tokens` come from the optional
`// @exec:` pragma.

## Configuration

```toml
[features]
control_tool_hook_observation = false
```

This disables the events. Hooks that match `*` or omit a matcher receive these
events. Hooks matching `Bash` or other specific names do not.
