import assert from "node:assert/strict";
import test from "node:test";

import { ARMS, type Arm } from "./arms.ts";
import { AnalysisValidationError, validateRun, type CellRecord, type ExpectedCell } from "./analyze.ts";

const MODEL = "gpt-6-astra";
const VARIANT = "plain";
const CATEGORIES = [
  "exact_recall",
  "relational_state",
  "tool_history",
  "distractor_resolution",
  "task_continuation",
] as const;

function scores(): CellRecord["evaluation"]["scores"] {
  return CATEGORIES.flatMap((category, categoryIndex) =>
    Array.from({ length: 15 }, (_, index) => ({
      questionId: `q${categoryIndex * 15 + index + 1}`,
      category,
      expected: "yes",
      actual: "yes",
      correct: true,
    })),
  );
}

function manifest(arms: Arm[] = ["full", "shake-elide"]): Record<string, unknown> {
  const expectedCells: ExpectedCell[] = arms.map((arm) => ({
    fixtureId: "fixture-01",
    seed: 1,
    trial: 0,
    arm,
    model: MODEL,
    variant: VARIANT,
  }));
  return {
    fingerprint: "test-fingerprint",
    nestedTelemetryPolicy: "required",
    model: MODEL,
    variant: VARIANT,
    fixtureCount: 1,
    trials: 1,
    arms,
    expectedCells,
  };
}

function cell(arm: Arm, options: Partial<CellRecord> = {}): CellRecord {
  return {
    fixtureId: "fixture-01",
    seed: 1,
    trial: 0,
    arm,
    variant: VARIANT,
    model: MODEL,
    threadId: `${arm}-thread`,
    preparation: { totalLatencyMs: 1 },
    evaluation: {
      latencyMs: 1,
      turnStatus: "completed",
      parseFailed: false,
      scores: scores(),
      firstRequestInputTokens: 10,
      turnInputTokens: 10,
      turnOutputTokens: 1,
      extraRecoveryInputTokens: 0,
      toolCalls: [],
    },
    recovery: {
      outerCodeModeContinuations: 0,
      nestedToolCalls: [],
      nestedToolCallsComplete: true,
      readArtifactAttempts: 0,
      derivedReadArtifactBytes: null,
      violation: false,
      artifactFiles: 0,
      artifactBytes: 0,
    },
    ...options,
  };
}

function expectInvalid(runManifest: unknown, cells: CellRecord[], message: RegExp): void {
  assert.throws(() => validateRun(runManifest, cells), (error: unknown) => {
    return error instanceof AnalysisValidationError && message.test(error.message);
  });
}

test("accepts a complete matrix with a perfect full control", () => {
  const runManifest = manifest();
  const result = validateRun(runManifest, [cell("full"), cell("shake-elide")]);

  assert.equal(result.manifest.expectedCells.length, 2);
  assert.equal(result.cells.length, 2);
  assert.equal(result.validCells.length, 2);
  assert.equal(result.protocolViolationCells.length, 0);
});

test("requires an immutable expected cell matrix", () => {
  const runManifest = manifest();
  delete runManifest.expectedCells;

  expectInvalid(runManifest, [cell("full"), cell("shake-elide")], /manifest\.expectedCells is required/);
});

test("requires the runner manifest fingerprint", () => {
  const runManifest = manifest();
  delete runManifest.fingerprint;

  expectInvalid(runManifest, [cell("full"), cell("shake-elide")], /manifest\.fingerprint must be a non-empty string/);
});

test("rejects missing and duplicate trial identities", () => {
  const runManifest = manifest();

  expectInvalid(runManifest, [cell("full")], /missing trial record.*shake-elide/);
  expectInvalid(runManifest, [cell("full"), cell("full")], /duplicate trial record.*full/);
});

test("rejects unexpected fixture, trial, model, and variant identities", () => {
  const runManifest = manifest();

  expectInvalid(runManifest, [cell("full"), cell("shake-elide", { fixtureId: "fixture-02" })], /unexpected trial record/);
  expectInvalid(runManifest, [cell("full"), cell("shake-elide", { trial: 1 })], /unexpected trial record/);
  expectInvalid(runManifest, [cell("full"), cell("shake-elide", { model: "gpt-5.6-sol" })], /\.model is gpt-5\.6-sol/);
  expectInvalid(runManifest, [cell("full"), cell("shake-elide", { variant: "echo" })], /\.variant is echo/);
});

test("rejects a non-perfect control before producing aggregates", () => {
  const badControl = cell("full", {
    evaluation: {
      ...cell("full").evaluation,
      scores: scores().map((score) => (score.questionId === "q1" ? { ...score, actual: "no", correct: false } : score)),
    },
  });

  expectInvalid(manifest(), [badControl, cell("shake-elide")], /full control scored 1\/75 incorrect/);
});

test("requires all 75 questions and 15 questions in every category", () => {
  const truncated = cell("shake-elide", {
    evaluation: { ...cell("shake-elide").evaluation, scores: scores().slice(0, 74) },
  });

  expectInvalid(manifest(), [cell("full"), truncated], /exactly 75 unique questions/);
});

test("rejects unknown categories and altered expected answers", () => {
  const unknownCategory = cell("shake-elide", {
    evaluation: {
      ...cell("shake-elide").evaluation,
      scores: scores().map((score) => (score.questionId === "q1" ? { ...score, category: "unknown" } : score)),
    },
  });
  expectInvalid(manifest(), [cell("full"), unknownCategory], /unknown category/);

  const changedExpected = cell("shake-elide", {
    evaluation: {
      ...cell("shake-elide").evaluation,
      scores: scores().map((score) => (score.questionId === "q1" ? { ...score, expected: "no", actual: "no" } : score)),
    },
  });
  expectInvalid(manifest(), [cell("full"), changedExpected], /changes category or expected answer/);
});

test("rejects failed turns and parse failures", () => {
  const failed = cell("full", {
    evaluation: { ...cell("full").evaluation, turnStatus: "failed", parseFailed: true },
  });

  expectInvalid(manifest(), [failed, cell("shake-elide")], /turnStatus is failed.*parseFailed must be false/);
});

test("requires a complete evaluation tool-call ledger", () => {
  const incomplete = cell("shake-elide", { evaluation: { ...cell("shake-elide").evaluation, toolCalls: undefined as never } });

  expectInvalid(manifest(), [cell("full"), incomplete], /evaluation\.toolCalls must be an array/);
});

test("rejects duplicate evaluation tool-call IDs", () => {
  const duplicate = cell("shake-elide", {
    evaluation: {
      ...cell("shake-elide").evaluation,
      toolCalls: [
        { callId: "outer-1", name: "exec", itemType: "custom_tool_call" },
        { callId: "outer-1", name: "exec", itemType: "custom_tool_call" },
      ],
    },
  });

  expectInvalid(manifest(), [cell("full"), duplicate], /duplicate callId outer-1/);
});

test("accepts deduplicated nested read attempts and derived page bytes", () => {
  const observed = cell("shake-elide", {
    recovery: {
      ...cell("shake-elide").recovery,
      outerCodeModeContinuations: 1,
      nestedToolCalls: [
        { callId: "nested-1", name: "read_artifact", itemType: "function_call" },
        { callId: "nested-2", name: "read_artifact", itemType: "function_call" },
      ],
      readArtifactAttempts: 2,
      derivedReadArtifactBytes: 6_144,
    },
  });

  const result = validateRun(manifest(), [cell("full"), observed]);
  assert.equal(result.validCells.length, 2);
});

test("rejects incomplete nested metadata inventories", () => {
  const incomplete = cell("shake-elide", {
    recovery: { ...cell("shake-elide").recovery, outerCodeModeContinuations: 1, nestedToolCallsComplete: false },
  });

  expectInvalid(manifest(), [cell("full"), incomplete], /nestedToolCallsComplete must be true when outer/);
});

test("allows neutral nested completeness when no outer code-mode wrapper exists", () => {
  const direct = cell("shake-elide", {
    recovery: { ...cell("shake-elide").recovery, nestedToolCallsComplete: false },
  });

  const result = validateRun(manifest(), [cell("full"), direct]);
  assert.equal(result.validCells.length, 2);
});

test("requires explicit unavailable policy for null nested telemetry", () => {
  const nullRecovery = {
    ...cell("shake-elide").recovery,
    nestedToolCalls: null,
    nestedToolCallsComplete: null,
    readArtifactAttempts: null,
    derivedReadArtifactBytes: null,
  };
  const strict = cell("shake-elide", { recovery: nullRecovery });
  expectInvalid(manifest(), [cell("full"), strict], /nestedToolCalls must be an array/);

  const unavailableManifest = { ...manifest(), nestedTelemetryPolicy: "unavailable" };
  const fullUnavailable = cell("full", {
    recovery: { ...cell("full").recovery, nestedToolCalls: null, nestedToolCallsComplete: null, readArtifactAttempts: null, derivedReadArtifactBytes: null },
  });
  const result = validateRun(unavailableManifest, [fullUnavailable, strict]);
  assert.deepEqual(result.nestedTelemetryUnverifiedCells.map((cell) => cell.arm), ["full", "shake-elide"]);
  assert.equal(result.validCells.length, 2);
});

test("accepts the live v8 no-wrapper known-zero telemetry shape", () => {
  const unavailableManifest = { ...manifest(), nestedTelemetryPolicy: "unavailable" };
  const full = cell("full", {
    recovery: {
      ...cell("full").recovery,
      nestedToolCalls: [],
      nestedToolCallsComplete: true,
      readArtifactAttempts: 0,
      derivedReadArtifactBytes: 0,
    },
  });
  const other = cell("shake-elide", {
    recovery: { ...cell("shake-elide").recovery, nestedToolCalls: null, nestedToolCallsComplete: null, readArtifactAttempts: null, derivedReadArtifactBytes: null },
  });
  const result = validateRun(unavailableManifest, [full, other]);

  assert.equal(result.nestedTelemetryUnverifiedCells.length, 1);
  assert.equal(result.validCells.length, 2);
});

test("accepts the smoke producer's zero-attempt null-byte shape", () => {
  const unavailableManifest = { ...manifest(), nestedTelemetryPolicy: "unavailable" };
  const full = cell("full", {
    recovery: {
      ...cell("full").recovery,
      nestedToolCalls: [],
      nestedToolCallsComplete: true,
      readArtifactAttempts: 0,
      derivedReadArtifactBytes: null,
    },
  });
  const other = cell("shake-elide", {
    recovery: { ...cell("shake-elide").recovery, nestedToolCalls: null, nestedToolCallsComplete: null, readArtifactAttempts: null, derivedReadArtifactBytes: null },
  });

  const result = validateRun(unavailableManifest, [full, other]);
  assert.deepEqual(result.nestedTelemetryUnverifiedCells.map((cell) => cell.arm), ["shake-elide"]);
});

test("unavailable noread telemetry remains an outer-call violation", () => {
  const unavailableManifest = { ...manifest(["full", "shake-elide-noread"]), nestedTelemetryPolicy: "unavailable" };
  const fullUnavailable = cell("full", {
    recovery: { ...cell("full").recovery, nestedToolCalls: null, nestedToolCallsComplete: null, readArtifactAttempts: null, derivedReadArtifactBytes: null },
  });
  const violating = cell("shake-elide-noread", {
    evaluation: {
      ...cell("shake-elide-noread").evaluation,
      toolCalls: [{ callId: "outer-1", name: "exec", itemType: "custom_tool_call" }],
    },
    recovery: {
      ...cell("shake-elide-noread").recovery,
      nestedToolCalls: null,
      nestedToolCallsComplete: null,
      readArtifactAttempts: null,
      derivedReadArtifactBytes: null,
      violation: true,
    },
  });

  const result = validateRun(unavailableManifest, [fullUnavailable, violating]);
  assert.equal(result.protocolViolationCells.length, 1);
  assert.equal(result.validCells.length, 1);
  assert.deepEqual(result.nestedTelemetryUnverifiedCells.map((cell) => cell.arm), ["full", "shake-elide-noread"]);
});

test("unavailable noread telemetry with no outer call remains conditionally compliant", () => {
  const unavailableManifest = { ...manifest(["full", "shake-elide-noread"]), nestedTelemetryPolicy: "unavailable" };
  const unavailable = (arm: Arm): CellRecord =>
    cell(arm, {
      recovery: { ...cell(arm).recovery, nestedToolCalls: null, nestedToolCallsComplete: null, readArtifactAttempts: null, derivedReadArtifactBytes: null },
    });

  const result = validateRun(unavailableManifest, [unavailable("full"), unavailable("shake-elide-noread")]);
  assert.equal(result.protocolViolationCells.length, 0);
  assert.equal(result.validCells.length, 2);
});

test("rejects inconsistent score rows across paired arms", () => {
  const changedQuestion = cell("shake-elide", {
    evaluation: {
      ...cell("shake-elide").evaluation,
      scores: scores().map((score) => (score.questionId === "q1" ? { ...score, questionId: "different-q" } : score)),
    },
  });

  expectInvalid(manifest(), [cell("full"), changedQuestion], /different question set from full/);
});

test("rejects score rows whose correct flag disagrees with their values", () => {
  const inconsistent = cell("shake-elide", {
    evaluation: {
      ...cell("shake-elide").evaluation,
      scores: scores().map((score) => (score.questionId === "q1" ? { ...score, actual: "no" } : score)),
    },
  });

  expectInvalid(manifest(), [cell("full"), inconsistent], /correct disagrees/);
});

test("reports noread violations in both inclusive and compliant cell sets", () => {
  const runManifest = manifest(["full", "shake-elide", "shake-elide-noread"]);
  const violating = cell("shake-elide-noread", {
    evaluation: {
      ...cell("shake-elide-noread").evaluation,
      toolCalls: [{ callId: "tool-1", name: "bash", itemType: "function_call" }],
    },
    recovery: { ...cell("shake-elide-noread").recovery, violation: true },
  });
  const result = validateRun(runManifest, [cell("full"), cell("shake-elide"), violating]);

  assert.equal(result.cells.length, 3);
  assert.equal(result.validCells.length, 2);
  assert.deepEqual(result.protocolViolationCells.map((value) => value.arm), ["shake-elide-noread"]);
});

test("retains nested noread attempts as an inclusive violation", () => {
  const runManifest = manifest(["full", "shake-elide-noread"]);
  const violating = cell("shake-elide-noread", {
    recovery: {
      ...cell("shake-elide-noread").recovery,
      outerCodeModeContinuations: 1,
      nestedToolCalls: [{ callId: "nested-1", name: "read_artifact", itemType: "executed_tool_call" }],
      readArtifactAttempts: 1,
      derivedReadArtifactBytes: null,
      violation: true,
    },
  });
  const result = validateRun(runManifest, [cell("full"), violating]);

  assert.equal(result.protocolViolationCells.length, 1);
  assert.equal(result.validCells.length, 1);
});

test("rejects protocol violations on arms other than noread", () => {
  const violating = cell("shake-elide", {
    recovery: {
      ...cell("shake-elide").recovery,
      nestedToolCalls: [{ callId: "nested-1", name: "bash", itemType: "function_call" }],
      readArtifactAttempts: 0,
      violation: true,
    },
  });

  expectInvalid(manifest(), [cell("full"), violating], /violation is only allowed/);
});

test("does not trust a false noread violation flag", () => {
  const unmarked = cell("shake-elide-noread", {
    evaluation: {
      ...cell("shake-elide-noread").evaluation,
      toolCalls: [{ callId: "tool-1", name: "bash", itemType: "function_call" }],
    },
    recovery: { ...cell("shake-elide-noread").recovery, violation: false },
  });
  const runManifest = manifest(["full", "shake-elide-noread"]);

  expectInvalid(runManifest, [cell("full"), unmarked], /violation does not match evaluation\.toolCalls/);
});

test("a compliant view can contain no cells when every noread cell violates", () => {
  const runManifest = manifest(["full", "shake-elide-noread"]);
  const violating = cell("shake-elide-noread", {
    evaluation: {
      ...cell("shake-elide-noread").evaluation,
      toolCalls: [{ callId: "tool-1", name: "bash", itemType: "function_call" }],
    },
    recovery: { ...cell("shake-elide-noread").recovery, violation: true },
  });
  const result = validateRun(runManifest, [cell("full"), violating]);

  assert.equal(result.validCells.length, 1);
  assert.equal(result.validCells[0]!.arm, "full");
  assert.equal(result.protocolViolationCells.length, 1);
});

test("requires the full control arm in the manifest", () => {
  const runManifest = manifest(["shake-elide"]);

  expectInvalid(runManifest, [cell("shake-elide")], /manifest\.arms must include the full control/);
});

test("keeps the known arm vocabulary closed", () => {
  assert.equal(ARMS.includes("full"), true);
  const runManifest = { ...manifest(), arms: ["full", "invented"] };

  expectInvalid(runManifest, [cell("full"), cell("shake-elide")], /manifest\.arms\[1\] is not a known arm/);
});
