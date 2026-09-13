#!/usr/bin/env node
// Fixture invariants are adapted from algal/pi-openai-server-compaction,
// benchmarks/native-vs-text/self-test.ts (MIT licence). The conversion and
// JSON-RPC framing checks are shake-bench specific.

import assert from "node:assert/strict";
import { buildFixture, buildFixtures } from "./fixtures.ts";
import { runFramingSelfTest } from "./appserver.ts";
import { ARMS, armOrder, isArm, promptVariant } from "./arms.ts";
import { applyVariant, eligibleToolOutputCount, isVariant, VARIANTS, ConversionError, answerSchema, evaluationPrompt, parseAnswers, prepareInjectItems, scoreAnswers, SHAKE_TOOL_OUTPUT_TARGET_BYTES, toInjectItems } from "./convert.ts";

// ---------------------------------------------------------------- fixtures

const fixture = buildFixture(1);
assert.equal(fixture.questions.length, 75);
assert.equal(new Set(fixture.questions.map((question) => question.id)).size, 75);
assert.deepEqual(
  [...new Set(fixture.questions.map((question) => question.category))].sort(),
  ["distractor_resolution", "exact_recall", "relational_state", "task_continuation", "tool_history"],
);
for (const category of new Set(fixture.questions.map((question) => question.category))) {
  assert.equal(fixture.questions.filter((question) => question.category === category).length, 15);
}
assert.ok(fixture.history.length > 300, "fixture should create a long conversation");
assert.ok(fixture.history.some((item) => item.type === "function_call"));
assert.ok(fixture.history.some((item) => item.type === "function_call_output"));
assert.equal(JSON.stringify(buildFixture(1)), JSON.stringify(fixture), "fixtures must be deterministic");
assert.notEqual(JSON.stringify(buildFixture(2)), JSON.stringify(fixture), "seeds must vary fixtures");
assert.equal(buildFixtures(3).length, 3);

// ---------------------------------------------------------------- convert

const injected = toInjectItems([...fixture.history, ...fixture.sharedTail]);
assert.equal(injected.length, fixture.history.length + fixture.sharedTail.length);
assert.deepEqual(
  [...new Set(injected.map((item) => item.type))].sort(),
  ["function_call", "function_call_output", "message"],
);
// Conversion is 1:1 on the fields shake and the Responses API care about.
for (const [index, item] of injected.entries()) {
  const source = [...fixture.history, ...fixture.sharedTail][index]!;
  assert.equal(item.type, source.type);
  if (item.type === "function_call") {
    assert.equal(item.name, source.name, "tool names must be preserved verbatim");
    assert.equal(item.call_id, source.call_id);
    assert.equal(item.arguments, source.arguments);
  }
  if (item.type === "function_call_output") assert.equal(item.output, source.output);
  if (item.type === "message") assert.deepEqual(item.content, source.content);
}
assert.ok(
  injected.some((item) => item.type === "function_call" && item.name === "database_query"),
  "fixture tool names such as database_query survive conversion",
);
const prepared = prepareInjectItems(fixture.history);
const preparedOutputs = prepared.filter((item) => item.type === "function_call_output");
assert.equal(preparedOutputs.length, fixture.history.filter((item) => item.type === "function_call_output").length);
assert.equal(eligibleToolOutputCount(prepared), preparedOutputs.length);
for (const item of preparedOutputs) {
  assert.ok(Buffer.byteLength(String(item.output), "utf8") >= SHAKE_TOOL_OUTPUT_TARGET_BYTES);
}
assert.equal(
  String(preparedOutputs[0]!.output).startsWith(String(injected.find((item) => item.type === "function_call_output")!.output)),
  true,
  "stress padding must preserve the original output prefix",
);
const preparedThread = prepareInjectItems([...fixture.history, ...fixture.sharedTail]);
assert.equal(
  eligibleToolOutputCount(preparedThread),
  [...fixture.history, ...fixture.sharedTail].filter((item) => item.type === "function_call_output").length,
);
assert.throws(() => toInjectItems([{ type: "reasoning" }]), ConversionError);
assert.throws(() => toInjectItems([{ type: "function_call", name: "read", arguments: "{}" }]), ConversionError);
assert.throws(
  () => toInjectItems([{ type: "function_call_output", call_id: "orphan", output: "x" }]),
  ConversionError,
);

// ---------------------------------------------------------------- prompt + scoring

const schema = answerSchema(fixture.questions) as { properties: { answers: { properties: Record<string, unknown> } } };
assert.equal(Object.keys(schema.properties.answers.properties).length, 75);
const prompt = evaluationPrompt(fixture.questions);
for (const question of fixture.questions) assert.ok(prompt.includes(question.id));
assert.ok(evaluationPrompt(fixture.questions, "noread").includes("Do not call any tools"));

const perfect = Object.fromEntries(fixture.questions.map((question) => [question.id, question.expected]));
assert.equal(scoreAnswers(fixture.questions, perfect).filter((row) => row.correct).length, 75);
assert.deepEqual(parseAnswers('{"answers":{"a":"b"}}'), { a: "b" });
assert.deepEqual(parseAnswers('noise before {"answers":{"a":" b "}} noise after'), { a: "b" });
assert.deepEqual(parseAnswers("not json at all"), {});

// ---------------------------------------------------------------- variants

assert.deepEqual([...VARIANTS], ["plain", "echo"]);
assert.ok(isVariant("echo") && !isVariant("loud"));
assert.equal(applyVariant(fixture.history, "plain"), fixture.history, "plain must be the identity");
const echoed = applyVariant(fixture.history, "echo");
const toolOutputs = fixture.history.filter((item) => item.type === "function_call_output").length;
assert.equal(echoed.length, fixture.history.length + toolOutputs, "echo adds one assistant message per tool output");
assert.ok(toolOutputs > 0);
// Every echo message names its tool and restates the output.
for (const [index, item] of echoed.entries()) {
  if (item.type !== "function_call_output") continue;
  const next = echoed[index + 1]!;
  assert.equal(next.type, "message");
  assert.equal(next.role, "assistant");
  const text = (next.content as Array<{ text: string }>)[0]!.text;
  const salient = String(item.output).split(/\r?\n/)[0]!.trim();
  assert.ok(text.includes(salient), `echo must restate: ${salient}`);
}
// The echo variant must still convert cleanly, and must not change questions.
assert.equal(toInjectItems(echoed).length, echoed.length);
assert.deepEqual(buildFixture(1).questions, fixture.questions);

// ---------------------------------------------------------------- arms

assert.equal(ARMS.length, 5);
assert.ok(isArm("shake-elide") && !isArm("nope"));
assert.equal(promptVariant("shake-elide-noread"), "noread");
assert.equal(promptVariant("shake-elide"), "default");
// Latin square: every arm occupies every position exactly once across 5 trials.
for (let position = 0; position < ARMS.length; position++) {
  const seen = Array.from({ length: ARMS.length }, (_, trial) => armOrder(trial)[position]);
  assert.equal(new Set(seen).size, ARMS.length, `position ${position} must see every arm`);
}
for (let trial = 0; trial < 7; trial++) assert.deepEqual([...armOrder(trial)].sort(), [...ARMS].sort());

// ---------------------------------------------------------------- JSON-RPC framing

await runFramingSelfTest();

console.log("shake-bench self-test ok");
