# Subagents on a different model provider

An agent role's `config_file` can set `model_provider`, so `spawn_agent(agent_type=...)` runs that role's children against a different provider (self-hosted or otherwise) while the parent keeps its own ChatGPT login and provider.

## Summary

A root Codex session running on OpenAI (ChatGPT login, typically) can spawn a subagent that runs on a different model provider, such as a self-hosted vLLM deployment serving the Responses API. Only the *provider* for that role's children changes; the parent's session, auth, and everything else about multi-agent orchestration and wake mode continue to work unchanged.

## Configuration

Root config declares the provider once, and a named agent role points at it:

```toml
# ~/.codex/config.toml (root)

[model_providers.self_hosted]
name = "Self-hosted vLLM"
base_url = "https://vllm.internal.example.com/v1"
wire_api = "responses"
env_key = "SELF_HOSTED_API_KEY"
requires_openai_auth = false

[agents.local]
config_file = "agents/local.toml"
```

```toml
# ~/.codex/agents/local.toml

model_provider = "self_hosted"
model = "my-org/local-coder-7b"
model_context_window = 32000
```

`spawn_agent(agent_type="local")` then runs that child against `self_hosted`, while the parent keeps its own provider and auth.

## Resolution rules

- A role's `model_provider` is looked up in the parent's resolved `config.model_providers` map, the same map the root config populates. It is not a fresh lookup against any global registry.
- A role file may only *reference* a provider id already defined in the root config's `[model_providers]`. It may not define a new `[model_providers.*]` table inline; doing so is rejected with a clear error pointing back at the root config.
- If a role names a provider id that isn't declared at the root, `spawn_agent` fails with a clear error naming the role and the missing provider id. This error is surfaced as-is, not downgraded to the generic "agent type unavailable" error used for other role failures.
- The role's provider is resolved before the requested-model override is validated, so a role that changes providers is not checked against the root provider's model catalog. The role's `model` still wins over any caller-supplied `model` argument, matching existing role precedence.
- A provider-switching role must be spawned with `fork_turns = "none"` (the default). Forked history replays the parent's OpenAI reasoning items, which another provider cannot decrypt, so `spawn_agent` rejects the combination with an error.
- When subagent context reduction is on, a role's `model_auto_compact_token_limit` can only lower the subagent limit, never raise it.
- If managed requirements pin `model_provider`, a role naming a different provider is rejected, so roles cannot bypass an admin policy.
- `wire_api = "responses"` is required for any provider a role targets. Chat Completions was removed from this fork, so a self-hosted target has to speak the Responses API shape.

## What is unchanged

- **Auth stays per-session.** `SessionConfiguration.provider` is built fresh per session from that session's own `config.model_provider`, and provider-scoped auth (an `env_key` or `auth` command) is picked the same way it already is for the root. A role does not need any new auth plumbing beyond declaring its provider.
- **Local compaction fallback for non-OpenAI providers is unchanged.** Remote compaction is only offered when the provider is OpenAI or an Azure Responses provider; a role's alternate provider falls back to local compaction automatically, the same as it would outside a role.
- **Websocket transport stays off by default.** It is gated per-provider by `supports_websockets`, and a role's provider simply leaves this `false` unless the root config sets it.

## Limitations

- There is no live model catalog for the alternate provider. Model validation and metadata for a role that changes providers rely on fallback metadata plus whatever the role declares, not a live `/models`-style lookup against that provider.
- Because of the above, a role that switches providers should declare `model_context_window` (and optionally `model_auto_compact_token_limit`) so context and compaction tracking still have real numbers to work with instead of falling back to defaults.
- `model_catalog_json` in a role file is not supported at this stage.
- `prompt_cache_key` is still sent to the alternate provider regardless of provider; it is not suppressed or made conditional.
- Resume caveat: a MultiAgentV2 child reloaded after eviction re-applies its role and restores the provider saved with its session, and a missing provider fails the reload. Legacy v1 `resume_agent` and `send_input` rebuild the child from the parent's turn without re-applying the role, so a closed v1 child comes back on the parent's provider. Every role setting shares that v1 gap today.

## Test coverage

See the integration test `spawn_agent_alternate_provider`, which spawns a child against a second mock provider server and asserts the child's request lands on that server (not the root's) and that an unknown provider id in a role produces a clear `spawn_agent` error.

## Links

- epsalmond/codex#24
- epsalmond/codex#19 (wake mode plan)
