# Durable sleep wakes while reports are paused

Fixes [#89](https://github.com/epsalmond/codex/issues/89), a regression from
the failed-start retention change in #87.

## Behavior

Two states hold automatic wakeups. Both clear on the next user turn or an
explicit `followup_task`.

- **Paused** (Esc, an aborted approval, a failed turn): child reports and other
  trigger-turn mail wait in the mailbox. Queue-only mail, such as
  `send_message` progress, still wakes an outstanding durable sleep. That turn
  receives the queue-only mail only; held reports are delivered once, with the
  next explicit turn.
- **Start failed** (an automatic turn was rejected while binding to its
  assignment): accepted mail stays queued, and no automatic turn starts, so a
  deterministic startup failure is not repeated. This includes durable sleep
  and owned async completions.

A later pause does not downgrade a failed start. If the only unread mail is a
held report, no empty turn starts.

## Configuration

There is no new setting. `agent_polling = "enabled"` keeps polling throughout
the agent tree, as before.

## Validation

- `agent_wake::wake_mode_durable_sleep_while_paused_holds_reports`: queue-only
  mail wakes a paused durable sleep once; the held report is delivered once with
  the next user message.
- `session::failed_start_tests`: a rejected automatic start, including the
  durable-sleep case, keeps accepted mail and waits for explicit recovery.
- `session::input_queue` unit tests for the pause and failed-start states.
