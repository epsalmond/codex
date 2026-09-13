import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { buildFixture } from "./fixtures.ts";
import { ARMS } from "./arms.ts";
import { actualArmOrderForCell, armOrderForCell, buildExpectedCells, manifestFingerprint, MINIMAL_CONFIG, parseArgs, parseToolTelemetry, matchesEvaluationNotification } from "./run.ts";

const fixtures = [buildFixture(1), buildFixture(2)];
assert.equal(parseArgs(["--nested-telemetry", "unavailable"]).nestedTelemetry, "unavailable");
assert.equal(parseArgs([]).nestedTelemetry, "required");
assert.throws(() => parseArgs(["--nested-telemetry", "bad"]), /unknown --nested-telemetry policy/);
assert.equal(parseArgs(["--concurrency", "3"]).concurrency, 3);
assert.throws(() => parseArgs(["--concurrency", "0"]), /--concurrency must be a positive integer/);
const interleavedNotifications = [
  { method: "item/agentMessage/delta", params: { threadId: "other", turnId: "turn-1", delta: "wrong" } },
  { method: "thread/tokenUsage/updated", params: { threadId: "target", turnId: "old", tokenUsage: { last: { inputTokens: 1 } } } },
  { method: "item/agentMessage/delta", params: { threadId: "target", turnId: "turn-2", delta: "right" } },
  { method: "rawResponseItem/completed", params: { threadId: "target", turnId: "turn-2", item: { type: "custom_tool_call" } } },
  { method: "turn/completed", params: { threadId: "target", turn: { id: "turn-2" } } },
] as const;
assert.deepEqual(
  interleavedNotifications.filter((notification) => matchesEvaluationNotification(notification, "target", "turn-2")).map((notification) => notification.method),
  ["item/agentMessage/delta", "rawResponseItem/completed", "turn/completed"],
  "evaluation routing must ignore other threads, earlier turns, and terminal races",
);
const expected = buildExpectedCells(fixtures, 2, ARMS, "gpt-test", "plain");
assert.equal(expected.length, 2 * 2 * ARMS.length);
assert.deepEqual(expected[0], {
  fixtureId: "fixture-01",
  seed: 1,
  trial: 0,
  arm: "full",
  model: "gpt-test",
  variant: "plain",
});
assert.deepEqual(
  armOrderForCell(1, 0, 2),
  ["shake-elide-noread", "compact", "shake-then-compact", "full", "shake-elide"],
);
assert.deepEqual(actualArmOrderForCell(1, 0, 2), ["full", "shake-elide-noread", "compact", "shake-then-compact", "shake-elide"]);
assert.equal(new Set(armOrderForCell(0, 0, 2)).size, ARMS.length);
for (const setting of [
  "project_doc_max_bytes = 0",
  "apps = false",
  "plugins = false",
  "tool_suggest = false",
  "recommended_plugins = false",
  "memories = false",
  "multi_agent = false",
  "hooks = false",
  "skip_host_skill_discovery = true",
  "include_instructions = false",
  "enabled = false",
]) {
  assert.match(MINIMAL_CONFIG, new RegExp(setting.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
}

const telemetryLine = (payload: Record<string, unknown>): string =>
  JSON.stringify({ timestamp: "now", type: "response_item", payload });
const turnId = "turn-telemetry";
const baseMetadata = { turn_id: turnId, cell_id: "cell-1" };
const firstSnapshot = telemetryLine({
  type: "custom_tool_call_output",
  call_id: "outer-output-1",
  output: "yield",
  internal_chat_message_metadata_passthrough: {
    ...baseMetadata,
    executed_tool_calls: [{ name: "read_artifact", arguments: { artifact: "artifact://a" } }],
    tool_calls_complete: false,
  },
});
const finalSnapshot = telemetryLine({
  type: "custom_tool_call_output",
  call_id: "outer-output-2",
  output: "done",
  internal_chat_message_metadata_passthrough: {
    ...baseMetadata,
    executed_tool_calls: [{ name: "read_artifact", arguments: { artifact: "artifact://b", start_byte: 3072 } }],
    tool_calls_complete: true,
  },
});
const outerExec = telemetryLine({
  type: "custom_tool_call",
  call_id: "outer-exec",
  name: "exec",
  input: "await tools.read_artifact(...) ",
  internal_chat_message_metadata_passthrough: baseMetadata,
});
const telemetry = parseToolTelemetry([outerExec, firstSnapshot, firstSnapshot, finalSnapshot].join("\n"), turnId);
assert.equal(telemetry.outerToolCalls.length, 1);
assert.equal(telemetry.nestedReads.length, 2, "duplicate output snapshots must be deduplicated");
assert.equal(telemetry.telemetryComplete, true);
const duplicatedOuter = parseToolTelemetry([outerExec, outerExec, firstSnapshot, finalSnapshot].join("\n"), turnId);
assert.equal(duplicatedOuter.outerToolCalls.length, 1, "rollout and raw snapshots must deduplicate outer calls");
assert.equal(
  parseToolTelemetry([outerExec, firstSnapshot].join("\n"), turnId).telemetryComplete,
  false,
  "missing final complete marker must be rejected",
);
const truncated = telemetryLine({
  type: "custom_tool_call_output",
  call_id: "outer-output-truncated",
  internal_chat_message_metadata_passthrough: {
    ...baseMetadata,
    executed_tool_calls: [{ name: "read_artifact", arguments: { _codex_executed_tool_call_truncated: {} } }],
    tool_calls_complete: true,
  },
});
assert.equal(parseToolTelemetry([outerExec, truncated].join("\n"), turnId).telemetryComplete, false);
const forbiddenNested = telemetryLine({
  type: "custom_tool_call_output",
  call_id: "outer-output-forbidden",
  internal_chat_message_metadata_passthrough: {
    ...baseMetadata,
    executed_tool_calls: [{ name: "bash", arguments: { command: "echo forbidden" } }],
    tool_calls_complete: true,
  },
});
assert.deepEqual(parseToolTelemetry([outerExec, forbiddenNested].join("\n"), turnId).nestedToolCalls.map((call) => call.name), ["bash"]);

const manifestInput = {
  binary: "codex-next",
  binaryVersion: "test",
  model: "gpt-test",
  configFingerprint: "config",
  fixtureFingerprint: "fixtures",
  fixtureCount: 2,
  variant: "plain" as const,
  trials: 2,
  arms: [...ARMS],
  expectedCells: expected,
  codeFingerprint: "code",
  nestedTelemetryPolicy: "required" as const,
  concurrency: 1,
};
const fingerprint = manifestFingerprint(manifestInput);
assert.equal(fingerprint, manifestFingerprint({ ...manifestInput }));
assert.notEqual(fingerprint, manifestFingerprint({ ...manifestInput, trials: 3 }));

const output = execFileSync(
  process.execPath,
  ["--import", "tsx", "src/run.ts", "--dry-run", "--fixtures", "2", "--trials", "2", "--out", "/tmp/shake-bench-runner-test"],
  { encoding: "utf8" },
);
const cells = output.match(/^=== fixture-\d\d trial \d arm .+ ===$/gm) ?? [];
assert.equal(cells.length, expected.length, "dry-run must visit every matrix cell");
assert.match(output, /dry run: 2 fixture\(s\) x 2 trial\(s\) x 5 arm\(s\)/);
assert.match(output, /fixture 0 trial 0: full -> shake-elide/);

console.log("runner test ok");
