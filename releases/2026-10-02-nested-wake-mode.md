# Nested wakeups for MultiAgentV2 agents

MultiAgentV2 agents can now wait for delegated work without polling. When a
child has unfinished descendants, it yields and resumes when those descendants
report. The child reports completion to its own parent only after processing
the reports and finishing its assignment.

## Scope and configuration

Wake mode is enabled when `agent_polling = "disabled"` (the default). It
covers eligible V2 agents created with `ThreadSpawn` at every depth, including
children spawned under an Exec root. The Exec root itself continues to use
`collaboration.wait_agent` for its direct children. V1, disabled MultiAgentV2,
and explicit polling keep their existing behavior.

Set this option to retain polling throughout the agent tree:

```toml
[features.multi_agent_v2]
agent_polling = "enabled"
```

Wake-mode agents retain `clock.curr_time` and Code Mode's `wait` tool, and omit
`clock.sleep` and `collaboration.wait_agent`. Esc pauses root wakeups and holds
reports for the next user message. `send_message` queues a message without
resuming a waiting agent; `followup_task` resumes it. An interrupted child
attempt reports its interruption to the parent and remains paused until an
explicit follow-up.

Pending reports are retried when capacity frees, and evicted children can
reload while the Codex process is running. Delivery recovery across process
restarts is not guaranteed.

For the original root wake rollout and its measurements, see the
[September 26 release notes](2026-09-26-wake-mode.md).
