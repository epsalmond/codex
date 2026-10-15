# Automatic process and cell completion

Completes [#79](https://github.com/epsalmond/codex/issues/79): a running
background command or Code Mode script cost a model request every time the
model checked on it. Results now arrive on their own, and the polling tools are
gone where delivery is guaranteed.

## Behavior

- A non-TTY `exec_command` session and a Code Mode `exec` cell that are still
  running when their tool call returns each deliver one message when they
  finish. An idle root starts a turn; a wake-mode subagent reports through its
  assignment; a busy agent receives it with its next model request.
- The message carries a header (`kind=process` or `kind=cell`, the call and
  turn, `status=` and `truncated=<bytes omitted>`) and the output's tail. The
  whole message, including the tail and its metadata, is at most 3072 bytes.
  A cell's images and audio are left out and counted.
- Code Mode registers `exec_terminate({cell_id})` in place of `wait`. It stops
  the cell and returns its output inline, and no automatic message follows,
  including when readiness fired before the call. For a cell that already
  finished, it returns "cell N already finished; its result was delivered
  automatically".
- A hidden `wait` handler answers models trained on `wait` with "results are
  delivered automatically; use `exec_terminate` to stop a cell".
- An empty `write_stdin` from the model to a running session with automatic
  delivery returns "session N is running; its exit status and output tail will
  be delivered automatically." as a normal result. Non-empty input, TTY
  sessions, and polls from inside a Code Mode cell are unchanged.
- Esc pauses delivery at the root until your next message. Interrupting a turn
  reports its running cells as `cancelled`.

## Which agents change

The tool list is fixed per session so prompt caching holds. Polling tools are
omitted only when all of these hold:

- `async_process_completion` is on (the packaged default);
- `polling_tools` is `omit` (the default);
- the agent can be woken: the root, or a wake-mode `spawn_agent` child.

Code Mode additionally requires a host that reports cell readiness, which the
bundled stdio and gRPC hosts do. If a host turns out not to support readiness
at runtime, Codex watches the cell itself and still delivers its result.

## Configuration

Keep `wait` and empty polls while results are still delivered automatically:

```toml
[features.async_process_completion]
polling_tools = "keep"
```

Turn automatic delivery off and restore polling:

```toml
[features]
async_process_completion = false
```

`PreToolUse` hooks observe `exec_terminate` like `wait`, without gating it.

## Validation

- `tools::spec_plan::tests`: `automatic_completion_replaces_wait_with_exec_terminate`,
  `wait_kept_when_async_completion_off`, `wait_kept_when_polling_tools_keep`,
  `wait_kept_for_polling_child`, `wait_kept_when_provider_lacks_readiness`.
- `suite::code_mode`: `code_mode_terminate_tool_can_terminate_and_continue`,
  `code_mode_terminate_after_delivery_reports_already_delivered`, and
  `code_mode_hidden_wait_explains_automatic_delivery`.
- `tools::code_mode::cell_completion::tests`: a terminate that reads the result
  after readiness fired but before collection leaves nothing to deliver; the
  fallback watcher publishes once and yields to `exec_terminate`.
- `suite::async_cell_completion_tests::active_cell_completion::unsupported_runtime_provider_falls_back_to_core_observer`.
- `suite::unified_exec::async_process_completion_tests::polling_tools_tests`:
  the model's empty poll is refused; input, TTY polls, and in-cell polls work.
- Existing polling tests run with `polling_tools = "keep"`, or with
  `async_process_completion = false` where they read a finished cell manually.
