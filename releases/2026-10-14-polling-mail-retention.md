# Polling agents keep reports across interrupts

Completes [#92](https://github.com/epsalmond/codex/issues/92): inter-agent
reports were lost outside wake mode. Evicted-parent recovery and close/archive
serialization landed in [#144](https://github.com/epsalmond/codex/pull/144) and
[#150](https://github.com/epsalmond/codex/pull/150). This change covers the last
case: mail a polling turn had accepted but not yet read.

## Behavior

Applies with `agent_polling = "enabled"` (wake mode already kept this mail).

When Esc, a replacing turn or another abort ends a turn before it reads agent
mail it accepted, that mail returns to the front of the mailbox in order.
Steers and other user input from the aborted turn are still dropped.

- A root's child reports are queue-only in polling mode. The next user turn
  takes them, and no turn starts on its own.
- A subagent's interrupted `followup_task` pauses that subagent's wakeups, so
  it does not restart by itself. The next explicit `followup_task` or user turn
  clears the pause and delivers both messages once, in order. A replacing turn
  takes the mail directly and does not pause.
- Queue-only mail that arrives after the stop still wakes an outstanding
  durable sleep, as in wake mode.

## Configuration

There is no new setting; this restores delivery that polling mode was meant to
provide. Pending mail is kept only while the Codex process runs.

## Validation

- `agent_wake::polling_mode_interrupt_keeps_report_accepted_at_turn_start`: a
  report that entered the turn's input survives Esc and no turn starts on its
  own; the next user turn takes it and a later request carries it once. It fails
  without this change.
- `session::input_queue` unit tests for polling-mode retention, the follow-up
  pause, and the replaced-turn case.
