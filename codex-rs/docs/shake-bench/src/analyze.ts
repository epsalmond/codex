#!/usr/bin/env node
// Aggregation and report generation for a shake-bench results directory.
//
// `wilson`, `binomialCoefficient`, `exactMcNemarP`, `aggregateScores` and the
// scores.csv / summary.json / GENERATED_RESULTS.md structure are adapted from
// algal/pi-openai-server-compaction, benchmarks/native-vs-text/analyze.ts (MIT).
// The paired-vs-control comparison, recovery-cost table and preview-vs-realized
// table are shake-bench specific.

import { existsSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { ARMS, type Arm } from "./arms.ts";
import { VARIANTS, type ScoreRow } from "./convert.ts";

const CONTROL: Arm = "full";
const SCORE_CATEGORIES = [
  "exact_recall",
  "relational_state",
  "tool_history",
  "distractor_resolution",
  "task_continuation",
] as const;
const SCORES_PER_CATEGORY = 15;
const EXPECTED_SCORE_COUNT = SCORE_CATEGORIES.length * SCORES_PER_CATEGORY;

export type ToolCall = { callId: string; name: string; itemType: string };

export type CellRecord = {
  fixtureId: string;
  seed: number;
  trial: number;
  arm: Arm;
  variant?: string;
  model: string;
  threadId: string;
  preparation: {
    shake?: {
      preview: { tokensBefore: number; tokensAfter: number; toolOutputs: number; fingerprint: string };
      applied: boolean;
      warning: string;
      latencyMs: number;
    };
    compact?: { latencyMs: number; completedVia: string };
    totalLatencyMs: number;
  };
  evaluation: {
    latencyMs: number;
    turnStatus: string;
    parseFailed: boolean;
    scores: ScoreRow[];
    firstRequestInputTokens: number;
    turnInputTokens: number;
    turnOutputTokens: number;
    extraRecoveryInputTokens: number;
    /** Complete set of tool calls observed during the evaluation turn. */
    toolCalls: ToolCall[];
  };
  recovery: {
    outerCodeModeContinuations: number;
    nestedToolCalls: ToolCall[] | null;
    nestedToolCallsComplete: boolean | null;
    readArtifactAttempts: number | null;
    derivedReadArtifactBytes: number | null;
    violation: boolean;
    artifactFiles: number;
    artifactBytes: number;
  };
};

export type ExpectedCell = {
  fixtureId: string;
  seed?: number;
  trial: number;
  arm: Arm;
  model: string;
  variant: string;
};

export type RunManifest = {
  [key: string]: unknown;
  fingerprint: string;
  nestedTelemetryPolicy: "required" | "unavailable";
  model: string;
  variant: string;
  fixtureCount: number;
  trials: number;
  arms: Arm[];
  expectedCells: ExpectedCell[];
};

export type ValidationResult = {
  manifest: RunManifest;
  cells: CellRecord[];
  validCells: CellRecord[];
  protocolViolationCells: CellRecord[];
  nestedTelemetryUnverifiedCells: CellRecord[];
};

export class AnalysisValidationError extends Error {
  readonly issues: string[];

  constructor(issues: string[]) {
    super(`invalid benchmark results: ${issues.join("; ")}`);
    this.name = "AnalysisValidationError";
    this.issues = issues;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function requiredString(value: unknown, field: string, issues: string[]): string | undefined {
  if (typeof value !== "string" || value.length === 0) {
    issues.push(`${field} must be a non-empty string`);
    return undefined;
  }
  return value;
}

function requiredPositiveInteger(value: unknown, field: string, issues: string[]): number | undefined {
  if (typeof value !== "number" || !Number.isInteger(value) || value < 1) {
    issues.push(`${field} must be a positive integer`);
    return undefined;
  }
  return value;
}

function requiredNonNegativeInteger(value: unknown, field: string, issues: string[]): number | undefined {
  if (typeof value !== "number" || !Number.isInteger(value) || value < 0) {
    issues.push(`${field} must be a non-negative integer`);
    return undefined;
  }
  return value;
}

function identityKey(identity: Pick<ExpectedCell, "fixtureId" | "trial" | "arm">): string {
  return `${identity.fixtureId}\u0000${identity.trial}\u0000${identity.arm}`;
}

function parseManifest(raw: unknown): RunManifest {
  const issues: string[] = [];
  if (!isRecord(raw)) throw new AnalysisValidationError(["manifest must be a JSON object"]);

  const model = requiredString(raw.model, "manifest.model", issues);
  const fingerprint = requiredString(raw.fingerprint, "manifest.fingerprint", issues);
  const nestedTelemetryPolicy = raw.nestedTelemetryPolicy ?? "required";
  if (nestedTelemetryPolicy !== "required" && nestedTelemetryPolicy !== "unavailable") {
    issues.push("manifest.nestedTelemetryPolicy must be required or unavailable");
  }
  const variant = requiredString(raw.variant, "manifest.variant", issues);
  if (variant && !VARIANTS.includes(variant as (typeof VARIANTS)[number])) {
    issues.push(`manifest.variant ${variant} is not a known fixture variant`);
  }
  const fixtureCount = requiredPositiveInteger(raw.fixtureCount, "manifest.fixtureCount", issues);
  const trials = requiredPositiveInteger(raw.trials, "manifest.trials", issues);

  const armsValue = raw.arms;
  const arms: Arm[] = [];
  if (!Array.isArray(armsValue) || armsValue.length === 0) {
    issues.push("manifest.arms must be a non-empty array");
  } else {
    const seenArms = new Set<string>();
    for (const [index, value] of armsValue.entries()) {
      if (typeof value !== "string" || !ARMS.includes(value as Arm)) {
        issues.push(`manifest.arms[${index}] is not a known arm`);
      } else if (seenArms.has(value)) {
        issues.push(`manifest.arms contains duplicate arm ${value}`);
      } else {
        seenArms.add(value);
        arms.push(value as Arm);
      }
    }
    if (!arms.includes(CONTROL)) issues.push("manifest.arms must include the full control arm");
  }

  const expectedCells: ExpectedCell[] = [];
  const expectedValue = raw.expectedCells;
  if (!Array.isArray(expectedValue)) {
    issues.push("manifest.expectedCells is required and must be an array");
  } else {
    const expectedKeys = new Set<string>();
    for (const [index, value] of expectedValue.entries()) {
      if (!isRecord(value)) {
        issues.push(`manifest.expectedCells[${index}] must be an object`);
        continue;
      }
      const fixtureId = requiredString(value.fixtureId, `manifest.expectedCells[${index}].fixtureId`, issues);
      const trial = requiredNonNegativeInteger(value.trial, `manifest.expectedCells[${index}].trial`, issues);
      const cellModel = requiredString(value.model, `manifest.expectedCells[${index}].model`, issues);
      const cellVariant = requiredString(value.variant, `manifest.expectedCells[${index}].variant`, issues);
      const armValue = value.arm;
      const seed = value.seed === undefined
        ? undefined
        : requiredNonNegativeInteger(value.seed, `manifest.expectedCells[${index}].seed`, issues);
      if (typeof armValue !== "string" || !ARMS.includes(armValue as Arm)) {
        issues.push(`manifest.expectedCells[${index}].arm is not a known arm`);
        continue;
      }
      if (!fixtureId || trial === undefined || !cellModel || !cellVariant) continue;
      const identity = { fixtureId, trial, arm: armValue as Arm };
      const key = identityKey(identity);
      if (expectedKeys.has(key)) issues.push(`manifest.expectedCells contains duplicate ${fixtureId} trial ${trial} arm ${armValue}`);
      expectedKeys.add(key);
      if (trials !== undefined && trial >= trials) issues.push(`expected cell ${key} has trial outside manifest.trials`);
      if (arms.length > 0 && !arms.includes(identity.arm)) issues.push(`expected cell ${key} uses an arm absent from manifest.arms`);
      if (model && cellModel !== model) issues.push(`expected cell ${key} has model ${cellModel}, manifest has ${model}`);
      if (variant && cellVariant !== variant) issues.push(`expected cell ${key} has variant ${cellVariant}, manifest has ${variant}`);
      expectedCells.push({ fixtureId, trial, arm: identity.arm, model: cellModel, variant: cellVariant, ...(seed === undefined ? {} : { seed }) });
    }
    if (fixtureCount !== undefined && trials !== undefined && arms.length > 0) {
      const expectedCount = fixtureCount * trials * arms.length;
      if (expectedValue.length !== expectedCount) {
        issues.push(`manifest.expectedCells has ${expectedValue.length} entries; expected ${expectedCount}`);
      }
    }
    const fixtureIds = new Set(expectedCells.map((cell) => cell.fixtureId));
    if (fixtureCount !== undefined && fixtureIds.size !== fixtureCount) {
      issues.push(`manifest.expectedCells names ${fixtureIds.size} fixtures; expected ${fixtureCount}`);
    }
  }

  if (
    issues.length > 0 ||
    !fingerprint ||
    !model ||
    !variant ||
    fixtureCount === undefined ||
    trials === undefined ||
    (nestedTelemetryPolicy !== "required" && nestedTelemetryPolicy !== "unavailable")
  ) {
    throw new AnalysisValidationError(issues.length > 0 ? issues : ["manifest is incomplete"]);
  }
  return { ...raw, fingerprint, nestedTelemetryPolicy, model, variant, fixtureCount, trials, arms, expectedCells };
}

function scoreMap(cell: CellRecord, path: string, issues: string[]): Map<string, ScoreRow> | undefined {
  const evaluation = isRecord(cell.evaluation) ? cell.evaluation : undefined;
  const scoresValue = evaluation?.scores;
  if (!Array.isArray(scoresValue) || scoresValue.length === 0) {
    issues.push(`${path}.evaluation.scores must be a non-empty array`);
    return undefined;
  }
  const scores = new Map<string, ScoreRow>();
  for (const [index, value] of scoresValue.entries()) {
    if (!isRecord(value)) {
      issues.push(`${path}.evaluation.scores[${index}] must be an object`);
      continue;
    }
    const questionId = requiredString(value.questionId, `${path}.evaluation.scores[${index}].questionId`, issues);
    const category = requiredString(value.category, `${path}.evaluation.scores[${index}].category`, issues);
    const expected = requiredString(value.expected, `${path}.evaluation.scores[${index}].expected`, issues);
    const actual = typeof value.actual === "string" ? value.actual : undefined;
    if (actual === undefined) issues.push(`${path}.evaluation.scores[${index}].actual must be a string`);
    if (typeof value.correct !== "boolean") issues.push(`${path}.evaluation.scores[${index}].correct must be a boolean`);
    if (!questionId || !category || expected === undefined || actual === undefined || typeof value.correct !== "boolean") continue;
    if (scores.has(questionId)) {
      issues.push(`${path}.evaluation.scores contains duplicate question ${questionId}`);
      continue;
    }
    if (value.correct !== (actual === expected)) {
      issues.push(`${path}.evaluation.scores[${index}].correct disagrees with actual and expected`);
    }
    scores.set(questionId, { questionId, category, expected, actual, correct: value.correct });
  }
  if (scores.size !== EXPECTED_SCORE_COUNT) {
    issues.push(`${path}.evaluation.scores must contain exactly ${EXPECTED_SCORE_COUNT} unique questions`);
  }
  const categoryCounts = new Map<string, number>();
  for (const score of scores.values()) categoryCounts.set(score.category, (categoryCounts.get(score.category) ?? 0) + 1);
  for (const category of SCORE_CATEGORIES) {
    if (categoryCounts.get(category) !== SCORES_PER_CATEGORY) {
      issues.push(`${path}.evaluation.scores must contain ${SCORES_PER_CATEGORY} ${category} questions`);
    }
  }
  for (const category of categoryCounts.keys()) {
    if (!SCORE_CATEGORIES.includes(category as (typeof SCORE_CATEGORIES)[number])) {
      issues.push(`${path}.evaluation.scores contains unknown category ${category}`);
    }
  }
  return scores;
}

function parseToolCalls(cell: CellRecord, path: string, issues: string[]): ToolCall[] | undefined {
  const evaluation = isRecord(cell.evaluation) ? cell.evaluation : undefined;
  const callsValue = evaluation?.toolCalls;
  if (!Array.isArray(callsValue)) {
    issues.push(`${path}.evaluation.toolCalls must be an array`);
    return undefined;
  }
  const calls: ToolCall[] = [];
  const callIds = new Set<string>();
  for (const [index, value] of callsValue.entries()) {
    if (!isRecord(value)) {
      issues.push(`${path}.evaluation.toolCalls[${index}] must be an object`);
      continue;
    }
    const callId = requiredString(value.callId, `${path}.evaluation.toolCalls[${index}].callId`, issues);
    const name = requiredString(value.name, `${path}.evaluation.toolCalls[${index}].name`, issues);
    const itemType = requiredString(value.itemType, `${path}.evaluation.toolCalls[${index}].itemType`, issues);
    if (callId && name && itemType) {
      if (callIds.has(callId)) issues.push(`${path}.evaluation.toolCalls contains duplicate callId ${callId}`);
      callIds.add(callId);
      calls.push({ callId, name, itemType });
    }
  }
  return calls;
}

function cellIdentity(cell: CellRecord, path: string, issues: string[]): ExpectedCell | undefined {
  const value = cell as unknown as Record<string, unknown>;
  const fixtureId = requiredString(value.fixtureId, `${path}.fixtureId`, issues);
  const trial = requiredNonNegativeInteger(value.trial, `${path}.trial`, issues);
  const model = requiredString(value.model, `${path}.model`, issues);
  const variant = requiredString(value.variant, `${path}.variant`, issues);
  const armValue = value.arm;
  if (typeof armValue !== "string" || !ARMS.includes(armValue as Arm)) {
    issues.push(`${path}.arm is not a known arm`);
  }
  const seed = requiredNonNegativeInteger(value.seed, `${path}.seed`, issues);
  if (!fixtureId || trial === undefined || !model || !variant || typeof armValue !== "string" || !ARMS.includes(armValue as Arm) || seed === undefined) {
    return undefined;
  }
  return { fixtureId, trial, arm: armValue as Arm, model, variant, seed };
}

type RecoveryObservation = {
  outerCodeModeContinuations: number;
  nestedToolCalls: ToolCall[] | null;
  nestedToolCallsComplete: boolean | null;
  readArtifactAttempts: number | null;
  derivedReadArtifactBytes: number | null;
  unverified: boolean;
};

function recoveryObservation(cell: CellRecord, path: string, policy: RunManifest["nestedTelemetryPolicy"], issues: string[]): RecoveryObservation | undefined {
  const recovery = isRecord(cell.recovery) ? cell.recovery : undefined;
  if (!recovery) {
    issues.push(`${path}.recovery must be an object`);
    return undefined;
  }
  const outerCodeModeContinuations = recovery.outerCodeModeContinuations;
  const nestedToolCallsValue = recovery.nestedToolCalls;
  const nestedToolCallsComplete = recovery.nestedToolCallsComplete;
  const readArtifactAttempts = recovery.readArtifactAttempts;
  const derivedReadArtifactBytes = recovery.derivedReadArtifactBytes;
  const evaluation = isRecord(cell.evaluation) ? cell.evaluation : undefined;
  const evaluationToolCalls = Array.isArray(evaluation?.toolCalls) ? evaluation.toolCalls : undefined;
  if (typeof outerCodeModeContinuations !== "number" || !Number.isInteger(outerCodeModeContinuations) || outerCodeModeContinuations < 0) {
    issues.push(`${path}.recovery.outerCodeModeContinuations must be a non-negative integer`);
  }
  if (policy === "unavailable") {
    if (
      outerCodeModeContinuations === 0 &&
      evaluationToolCalls?.length === 0 &&
      Array.isArray(nestedToolCallsValue) &&
      nestedToolCallsValue.length === 0 &&
      nestedToolCallsComplete === true &&
      readArtifactAttempts === 0 &&
      (derivedReadArtifactBytes === 0 || derivedReadArtifactBytes === null)
    ) {
      return { outerCodeModeContinuations, nestedToolCalls: [], nestedToolCallsComplete: true, readArtifactAttempts: 0, derivedReadArtifactBytes: 0, unverified: false };
    }
    if (nestedToolCallsValue === null && nestedToolCallsComplete === null && readArtifactAttempts === null && derivedReadArtifactBytes === null) {
    return {
        outerCodeModeContinuations,
        nestedToolCalls: null,
        nestedToolCallsComplete: null,
        readArtifactAttempts: null,
        derivedReadArtifactBytes: null,
        unverified: true,
      };
    }
    if (nestedToolCallsValue !== null || nestedToolCallsComplete !== null || readArtifactAttempts !== null || derivedReadArtifactBytes !== null) {
      issues.push(`${path}.recovery nested telemetry must use null fields under unavailable policy`);
    }
    return typeof outerCodeModeContinuations === "number" && Number.isInteger(outerCodeModeContinuations) && outerCodeModeContinuations >= 0
      ? { outerCodeModeContinuations, nestedToolCalls: null, nestedToolCallsComplete: null, readArtifactAttempts: null, derivedReadArtifactBytes: null, unverified: true }
      : undefined;
  }
  if (!Array.isArray(nestedToolCallsValue)) issues.push(`${path}.recovery.nestedToolCalls must be an array`);
  if (typeof nestedToolCallsComplete !== "boolean") issues.push(`${path}.recovery.nestedToolCallsComplete must be a boolean`);
  if (typeof readArtifactAttempts !== "number" || !Number.isInteger(readArtifactAttempts) || readArtifactAttempts < 0) {
    issues.push(`${path}.recovery.readArtifactAttempts must be a non-negative integer`);
  }
  if (derivedReadArtifactBytes !== null && (typeof derivedReadArtifactBytes !== "number" || !Number.isInteger(derivedReadArtifactBytes) || derivedReadArtifactBytes < 0)) {
    issues.push(`${path}.recovery.derivedReadArtifactBytes must be null or a non-negative integer`);
  }
  const nestedToolCalls: ToolCall[] = [];
  if (Array.isArray(nestedToolCallsValue)) {
    for (const [index, value] of nestedToolCallsValue.entries()) {
      if (!isRecord(value)) {
        issues.push(`${path}.recovery.nestedToolCalls[${index}] must be an object`);
        continue;
      }
      const callId = requiredString(value.callId, `${path}.recovery.nestedToolCalls[${index}].callId`, issues);
      const name = requiredString(value.name, `${path}.recovery.nestedToolCalls[${index}].name`, issues);
      const itemType = requiredString(value.itemType, `${path}.recovery.nestedToolCalls[${index}].itemType`, issues);
      if (callId && name && itemType) nestedToolCalls.push({ callId, name, itemType });
    }
  }
  if (outerCodeModeContinuations > 0 && nestedToolCallsComplete !== true) {
    issues.push(`${path}.recovery.nestedToolCallsComplete must be true when outer code-mode continuations exist`);
  }
  if (outerCodeModeContinuations === 0 && nestedToolCalls.length > 0) {
    issues.push(`${path}.recovery.nestedToolCalls requires an outer code-mode continuation`);
  }
  if (
    typeof outerCodeModeContinuations !== "number" ||
    !Array.isArray(nestedToolCallsValue) ||
    typeof nestedToolCallsComplete !== "boolean" ||
    typeof readArtifactAttempts !== "number" ||
    derivedReadArtifactBytes !== null && typeof derivedReadArtifactBytes !== "number"
  ) {
    return undefined;
  }
  return {
    outerCodeModeContinuations,
    nestedToolCalls,
    nestedToolCallsComplete,
    readArtifactAttempts,
    derivedReadArtifactBytes,
    unverified: false,
  };
}

function validateCellContents(
  cell: CellRecord,
  path: string,
  policy: RunManifest["nestedTelemetryPolicy"],
  issues: string[],
): { scores: Map<string, ScoreRow>; toolCalls: ToolCall[]; recovery: RecoveryObservation } | undefined {
  const evaluation = isRecord(cell.evaluation) ? cell.evaluation : undefined;
  if (!evaluation) {
    issues.push(`${path}.evaluation must be an object`);
  } else {
    if (evaluation.turnStatus !== "completed") issues.push(`${path}.evaluation.turnStatus is ${String(evaluation.turnStatus)}, expected completed`);
    if (evaluation.parseFailed !== false) issues.push(`${path}.evaluation.parseFailed must be false`);
  }
  const recovery = isRecord(cell.recovery) ? cell.recovery : undefined;
  if (!recovery) {
    issues.push(`${path}.recovery must be an object`);
  } else if (typeof recovery.violation !== "boolean") {
    issues.push(`${path}.recovery.violation must be a boolean`);
  }
  const scores = scoreMap(cell, path, issues);
  const toolCalls = parseToolCalls(cell, path, issues);
  const observation = recoveryObservation(cell, path, policy, issues);
  return scores && toolCalls && observation ? { scores, toolCalls, recovery: observation } : undefined;
}

export function validateRun(rawManifest: unknown, cells: CellRecord[]): ValidationResult {
  const manifest = parseManifest(rawManifest);
  const issues: string[] = [];
  const expectedByKey = new Map(manifest.expectedCells.map((cell) => [identityKey(cell), cell]));
  const actualByKey = new Map<string, CellRecord>();
  const scoresByKey = new Map<string, Map<string, ScoreRow>>();
  const protocolViolationCells: CellRecord[] = [];
  const nestedTelemetryUnverifiedCells: CellRecord[] = [];

  if (!Array.isArray(cells)) throw new AnalysisValidationError(["trial records must be an array"]);
  for (const [index, cell] of cells.entries()) {
    const path = `trials[${index}]`;
    if (!isRecord(cell)) {
      issues.push(`${path} must be an object`);
      continue;
    }
    const typedCell = cell as unknown as CellRecord;
    const identity = cellIdentity(typedCell, path, issues);
    const contents = validateCellContents(typedCell, path, manifest.nestedTelemetryPolicy, issues);
    if (!identity) continue;
    const key = identityKey(identity);
    if (actualByKey.has(key)) {
      issues.push(`duplicate trial record for ${identity.fixtureId} trial ${identity.trial} arm ${identity.arm}`);
      continue;
    }
    actualByKey.set(key, typedCell);
    if (!expectedByKey.has(key)) {
      issues.push(`unexpected trial record for ${identity.fixtureId} trial ${identity.trial} arm ${identity.arm}`);
    } else {
      const expected = expectedByKey.get(key)!;
      if (identity.model !== expected.model) issues.push(`${path}.model is ${identity.model}, expected ${expected.model}`);
      if (identity.variant !== expected.variant) issues.push(`${path}.variant is ${identity.variant}, expected ${expected.variant}`);
      if (expected.seed !== undefined && identity.seed !== expected.seed) issues.push(`${path}.seed is ${identity.seed}, expected ${expected.seed}`);
    }
    const recovery = isRecord(typedCell.recovery) ? typedCell.recovery : undefined;
    const toolCalls = contents?.toolCalls ?? [];
    const observation = contents?.recovery;
    const nestedToolCalls = observation?.nestedToolCalls ?? [];
    const violation = identity.arm === "shake-elide-noread" && (toolCalls.length > 0 || nestedToolCalls.length > 0);
    const recordedViolation = recovery?.violation === true;
    const nestedReadAttempts = nestedToolCalls.filter((call) => call.name === "read_artifact").length;
    if (observation?.readArtifactAttempts !== null && observation && observation.readArtifactAttempts !== nestedReadAttempts) {
      issues.push(`${path}.recovery.readArtifactAttempts does not match nestedToolCalls`);
    }
    const nestedAllowed = identity.arm === "shake-elide" || identity.arm === "shake-then-compact";
    if (identity.arm !== "shake-elide-noread" && observation?.nestedToolCalls !== null && nestedToolCalls.some((call) => !nestedAllowed || call.name !== "read_artifact")) {
      issues.push(`${path}.recovery.nestedToolCalls contains a forbidden nested tool call`);
    }
    if (recovery && typeof recovery.violation === "boolean" && recordedViolation !== violation) {
      issues.push(`${path}.recovery.violation does not match evaluation.toolCalls`);
    }
    if (recordedViolation && identity.arm !== "shake-elide-noread") {
      issues.push(`${path}.recovery.violation is only allowed for shake-elide-noread`);
    }
    if (violation && identity.arm === "shake-elide-noread") protocolViolationCells.push(typedCell);
    if (contents) {
      scoresByKey.set(key, contents.scores);
      if (contents.recovery.unverified) nestedTelemetryUnverifiedCells.push(typedCell);
    }
  }

  if (cells.length !== manifest.expectedCells.length) {
    issues.push(`trial record count is ${cells.length}, expected ${manifest.expectedCells.length}`);
  }
  for (const expected of manifest.expectedCells) {
    const key = identityKey(expected);
    if (!actualByKey.has(key)) issues.push(`missing trial record for ${expected.fixtureId} trial ${expected.trial} arm ${expected.arm}`);
  }

  const controls = manifest.expectedCells.filter((cell) => cell.arm === CONTROL);
  if (controls.length === 0) issues.push("manifest.expectedCells must include a full control for every fixture and trial");
  for (const controlIdentity of controls) {
    const controlKey = identityKey(controlIdentity);
    const control = scoresByKey.get(controlKey);
    if (!control) continue;
    const incorrect = [...control.values()].filter((score) => !score.correct);
    if (incorrect.length > 0) {
      issues.push(`${controlIdentity.fixtureId} trial ${controlIdentity.trial} full control scored ${incorrect.length}/${control.size} incorrect`);
    }
    for (const expected of manifest.expectedCells.filter(
      (cell) => cell.fixtureId === controlIdentity.fixtureId && cell.trial === controlIdentity.trial,
    )) {
      const key = identityKey(expected);
      const scores = scoresByKey.get(key);
      if (!scores) continue;
      const controlIds = [...control.keys()].sort().join("\u0000");
      const scoreIds = [...scores.keys()].sort().join("\u0000");
      if (controlIds !== scoreIds) issues.push(`${expected.fixtureId} trial ${expected.trial} arm ${expected.arm} has a different question set from full`);
      for (const [questionId, score] of scores) {
        const controlScore = control.get(questionId);
        if (controlScore && (score.category !== controlScore.category || score.expected !== controlScore.expected)) {
          issues.push(`${expected.fixtureId} trial ${expected.trial} arm ${expected.arm} changes category or expected answer for ${questionId}`);
        }
      }
    }
  }

  if (issues.length > 0) throw new AnalysisValidationError(issues);
  return {
    manifest,
    cells,
    validCells: cells.filter(
      (cell) => !(cell.recovery?.violation ?? false),
    ),
    protocolViolationCells,
    nestedTelemetryUnverifiedCells,
  };
}

// ---------------------------------------------------------------- statistics

function wilson(successes: number, total: number): [number, number] {
  if (total === 0) return [0, 0];
  const z = 1.96;
  const p = successes / total;
  const denominator = 1 + (z * z) / total;
  const center = (p + (z * z) / (2 * total)) / denominator;
  const margin = (z * Math.sqrt((p * (1 - p) + (z * z) / (4 * total)) / total)) / denominator;
  return [Math.max(0, center - margin), Math.min(1, center + margin)];
}

function binomialCoefficient(n: number, k: number): number {
  const reduced = Math.min(k, n - k);
  let value = 1;
  for (let index = 1; index <= reduced; index++) value = (value * (n - reduced + index)) / index;
  return value;
}

function exactMcNemarP(aOnly: number, bOnly: number): number {
  const discordant = aOnly + bOnly;
  if (discordant === 0) return 1;
  const tail = Math.min(aOnly, bOnly);
  let cumulative = 0;
  for (let index = 0; index <= tail; index++) {
    cumulative += binomialCoefficient(discordant, index) * 0.5 ** discordant;
  }
  return Math.min(1, 2 * cumulative);
}

type Aggregate = { correct: number; total: number; accuracy: number | null; wilson95: [number, number] };
type Paired = {
  bothCorrect: number;
  controlOnly: number;
  armOnly: number;
  bothWrong: number;
  exactMcNemarP: number;
  regressions: Array<Record<string, unknown>>;
};

function aggregateScores(rows: ScoreRow[]): Aggregate {
  const correct = rows.filter((row) => row.correct).length;
  const total = rows.length;
  return { correct, total, accuracy: total ? correct / total : null, wilson95: wilson(correct, total) };
}

function pairedOutcomes(byArmCells: Record<Arm, CellRecord[]>, presentArms: Arm[]): Record<string, Paired> {
  const trialKey = (cell: CellRecord): string => `${cell.fixtureId}#${cell.trial}`;
  const controlByCell = new Map(byArmCells[CONTROL].map((cell) => [trialKey(cell), cell]));
  return Object.fromEntries(
    presentArms
      .filter((arm) => arm !== CONTROL)
      .map((arm) => {
        let bothCorrect = 0;
        let controlOnly = 0;
        let armOnly = 0;
        let bothWrong = 0;
        const regressions: Array<Record<string, unknown>> = [];
        for (const cell of byArmCells[arm]) {
          const control = controlByCell.get(trialKey(cell));
          if (!control) continue;
          const controlScores = new Map(control.evaluation.scores.map((score) => [score.questionId, score]));
          for (const score of cell.evaluation.scores) {
            const controlScore = controlScores.get(score.questionId);
            if (!controlScore) continue;
            if (controlScore.correct && score.correct) bothCorrect++;
            else if (controlScore.correct) {
              controlOnly++;
              regressions.push({
                fixtureId: cell.fixtureId,
                trial: cell.trial,
                arm,
                questionId: score.questionId,
                category: score.category,
                expected: score.expected,
                actual: score.actual,
              });
            } else if (score.correct) armOnly++;
            else bothWrong++;
          }
        }
        return [
          arm,
          { bothCorrect, controlOnly, armOnly, bothWrong, exactMcNemarP: exactMcNemarP(controlOnly, armOnly), regressions },
        ];
      }),
  ) as Record<string, Paired>;
}

function average(values: number[]): number {
  return values.length ? values.reduce((sum, value) => sum + value, 0) / values.length : 0;
}

function averageNullable(values: Array<number | null>): number | null {
  if (values.length === 0 || values.some((value) => value === null)) return null;
  return average(values as number[]);
}

function derivedReadArtifactBytes(cell: CellRecord): number | null {
  if (cell.recovery.derivedReadArtifactBytes !== null) return cell.recovery.derivedReadArtifactBytes;
  const noOuterCalls = cell.recovery.outerCodeModeContinuations === 0 && cell.evaluation.toolCalls.length === 0;
  const knownEmptyInventory =
    noOuterCalls &&
    Array.isArray(cell.recovery.nestedToolCalls) &&
    cell.recovery.nestedToolCalls.length === 0 &&
    cell.recovery.nestedToolCallsComplete === true &&
    cell.recovery.readArtifactAttempts === 0;
  return knownEmptyInventory ? 0 : null;
}

const pct = (value: number | null): string => (value === null ? "n/a" : `${(value * 100).toFixed(1)}%`);
const num = (value: number): string => (Number.isFinite(value) ? value.toFixed(1) : "n/a");
const int = (value: number): string => (Number.isFinite(value) ? Math.round(value).toLocaleString("en-US") : "n/a");

// ---------------------------------------------------------------- load

function loadCells(runDir: string): CellRecord[] {
  const trialsDir = join(runDir, "trials");
  if (!existsSync(trialsDir)) throw new Error(`no trials directory in ${runDir}`);
  const cells = readdirSync(trialsDir)
    .filter((name) => name.endsWith(".json"))
    .map((name) => JSON.parse(readFileSync(join(trialsDir, name), "utf8")) as CellRecord);
  if (cells.length === 0) throw new Error(`no completed cells in ${trialsDir}`);
  cells.sort((a, b) => `${a.fixtureId}-${a.trial}-${a.arm}`.localeCompare(`${b.fixtureId}-${b.trial}-${b.arm}`));
  return cells;
}

// ---------------------------------------------------------------- main

function main(): void {
  const runDir = resolve(process.argv[2] ?? "");
  if (!process.argv[2]) throw new Error("Usage: analyze.ts <run-directory>");
  const manifestPath = join(runDir, "manifest.json");
  if (!existsSync(manifestPath)) throw new AnalysisValidationError(["manifest.json is required"]);
  const rawManifest: unknown = JSON.parse(readFileSync(manifestPath, "utf8"));
  const loadedCells = loadCells(runDir);
  const validation = validateRun(rawManifest, loadedCells);
  const { manifest, cells } = validation;
  const presentArms = manifest.arms;
  const byArmCells = Object.fromEntries(presentArms.map((arm) => [arm, cells.filter((cell) => cell.arm === arm)])) as
    Record<Arm, CellRecord[]>;
  const validByArmCells = Object.fromEntries(presentArms.map((arm) => [arm, validation.validCells.filter((cell) => cell.arm === arm)])) as Record<Arm, CellRecord[]>;

  const categories = [...new Set(cells.flatMap((cell) => cell.evaluation.scores.map((score) => score.category)))].sort();

  const byArm = Object.fromEntries(
    presentArms.map((arm) => [arm, aggregateScores(byArmCells[arm].flatMap((cell) => cell.evaluation.scores))]),
  ) as Record<Arm, Aggregate>;

  const byCategory = Object.fromEntries(
    categories.map((category) => [
      category,
      Object.fromEntries(
        presentArms.map((arm) => [
          arm,
          aggregateScores(byArmCells[arm].flatMap((cell) => cell.evaluation.scores.filter((s) => s.category === category))),
        ]),
      ),
    ]),
  ) as Record<string, Record<Arm, Aggregate>>;

  const byArmExcludingProtocolViolations = Object.fromEntries(
    presentArms.map((arm) => [arm, aggregateScores(validByArmCells[arm].flatMap((cell) => cell.evaluation.scores))]),
  ) as Record<Arm, Aggregate>;

  const byCategoryExcludingProtocolViolations = Object.fromEntries(
    categories.map((category) => [
      category,
      Object.fromEntries(
        presentArms.map((arm) => [
          arm,
          aggregateScores(
            validByArmCells[arm].flatMap((cell) => cell.evaluation.scores.filter((score) => score.category === category)),
          ),
        ]),
      ),
    ]),
  ) as Record<string, Record<Arm, Aggregate>>;

  const perFixture = Object.fromEntries(
    [...new Set(cells.map((cell) => cell.fixtureId))].sort().map((fixtureId) => [
      fixtureId,
      Object.fromEntries(
        presentArms.map((arm) => [
          arm,
          aggregateScores(
            cells.filter((cell) => cell.fixtureId === fixtureId && cell.arm === arm).flatMap((cell) => cell.evaluation.scores),
          ),
        ]),
      ),
    ]),
  );

  // ---- paired outcomes against the full-context control
  const paired = pairedOutcomes(byArmCells, presentArms);
  const pairedExcludingProtocolViolations = pairedOutcomes(validByArmCells, presentArms);

  // ---- per-arm footprint
  const footprint = Object.fromEntries(
    presentArms.map((arm) => {
      const armCells = byArmCells[arm];
      return [
        arm,
        {
          cells: armCells.length,
          meanDownstreamInputTokens: average(armCells.map((cell) => cell.evaluation.firstRequestInputTokens)),
          meanTurnInputTokens: average(armCells.map((cell) => cell.evaluation.turnInputTokens)),
          meanOutputTokens: average(armCells.map((cell) => cell.evaluation.turnOutputTokens)),
          meanEvaluationLatencyMs: average(armCells.map((cell) => cell.evaluation.latencyMs)),
          meanPreparationLatencyMs: average(armCells.map((cell) => cell.preparation.totalLatencyMs)),
          parseFailures: armCells.filter((cell) => cell.evaluation.parseFailed).length,
          nonCompletedTurns: armCells.filter((cell) => cell.evaluation.turnStatus !== "completed").length,
        },
      ];
    }),
  );

  // ---- recovery cost for the shake arms
  const shakeArms = presentArms.filter((arm) => arm.startsWith("shake"));
  const recovery = Object.fromEntries(
    shakeArms.map((arm) => {
      const armCells = byArmCells[arm];
      return [
        arm,
        {
          cells: armCells.length,
          cellsWithAttempts: armCells.some((cell) => cell.recovery.readArtifactAttempts === null)
            ? null
            : armCells.filter((cell) => cell.recovery.readArtifactAttempts !== null && cell.recovery.readArtifactAttempts > 0).length,
          meanOuterCodeModeContinuations: average(armCells.map((cell) => cell.recovery.outerCodeModeContinuations)),
          meanNestedReadAttempts: averageNullable(armCells.map((cell) => cell.recovery.readArtifactAttempts)),
          totalNestedReadAttempts: armCells.some((cell) => cell.recovery.readArtifactAttempts === null)
            ? null
            : armCells.reduce((sum, cell) => sum + (cell.recovery.readArtifactAttempts ?? 0), 0),
          meanDerivedReadArtifactBytes: averageNullable(armCells.map((cell) => derivedReadArtifactBytes(cell))),
          meanExtraInputTokens: average(armCells.map((cell) => cell.evaluation.extraRecoveryInputTokens)),
          meanArtifactFiles: average(armCells.map((cell) => cell.recovery.artifactFiles)),
          meanArtifactBytes: average(armCells.map((cell) => cell.recovery.artifactBytes)),
          noreadViolations: armCells.filter((cell) => cell.recovery.violation).length,
        },
      ];
    }),
  );

  // ---- preview estimate vs realized downstream input
  const previewVsRealized = Object.fromEntries(
    shakeArms.map((arm) => {
      const armCells = byArmCells[arm].filter((cell) => cell.preparation.shake);
      return [
        arm,
        {
          cells: armCells.length,
          applied: armCells.filter((cell) => cell.preparation.shake?.applied).length,
          meanTokensBefore: average(armCells.map((cell) => cell.preparation.shake!.preview.tokensBefore)),
          meanTokensAfter: average(armCells.map((cell) => cell.preparation.shake!.preview.tokensAfter)),
          meanToolOutputsElided: average(armCells.map((cell) => cell.preparation.shake!.preview.toolOutputs)),
          meanRealizedDownstreamInputTokens: average(armCells.map((cell) => cell.evaluation.firstRequestInputTokens)),
          meanRealizedMinusPreviewTokens: average(
            armCells.map((cell) => cell.evaluation.firstRequestInputTokens - cell.preparation.shake!.preview.tokensAfter),
          ),
          meanRealizedToPreviewRatio: average(
            armCells
              .map((cell) => cell.preparation.shake!.preview.tokensAfter > 0
                ? cell.evaluation.firstRequestInputTokens / cell.preparation.shake!.preview.tokensAfter
                : Number.NaN)
              .filter(Number.isFinite),
          ),
          meanShakeLatencyMs: average(armCells.map((cell) => cell.preparation.shake!.latencyMs)),
        },
      ];
    }),
  );
  const controlDownstream = average(
    (byArmCells[CONTROL] ?? []).map((cell) => cell.evaluation.firstRequestInputTokens),
  );

  const summary = {
    manifest,
    runDir,
    validation: {
      expectedCells: manifest.expectedCells.length,
      completedCells: cells.length,
      compliantCells: validation.validCells.length,
      protocolViolationCells: validation.protocolViolationCells.map((cell) => ({
        fixtureId: cell.fixtureId,
        trial: cell.trial,
        arm: cell.arm,
      })),
      nestedTelemetryUnverifiedCells: validation.nestedTelemetryUnverifiedCells.map((cell) => ({
        fixtureId: cell.fixtureId,
        trial: cell.trial,
        arm: cell.arm,
      })),
    },
    completedCells: cells.length,
    variants: [...new Set(cells.map((cell) => cell.variant ?? "plain"))],
    arms: presentArms,
    byArm,
    byCategory,
    byArmExcludingProtocolViolations,
    byCategoryExcludingProtocolViolations,
    perFixture,
    pairedVsControl: paired,
    pairedVsControlExcludingProtocolViolations: pairedExcludingProtocolViolations,
    footprint,
    recovery,
    previewVsRealized,
    controlMeanDownstreamInputTokens: controlDownstream,
  };
  writeFileSync(join(runDir, "summary.json"), `${JSON.stringify(summary, null, 2)}\n`);
  writeFileSync(join(runDir, "trials.jsonl"), `${cells.map((cell) => JSON.stringify(cell)).join("\n")}\n`);

  const csv = ["fixture,trial,arm,question_id,category,correct,expected,actual"];
  for (const cell of cells) {
    for (const score of cell.evaluation.scores) {
      csv.push(
        [cell.fixtureId, cell.trial, cell.arm, score.questionId, score.category, score.correct, score.expected, score.actual]
          .map((value) => `"${String(value).replaceAll('"', '""')}"`)
          .join(","),
      );
    }
  }
  writeFileSync(join(runDir, "scores.csv"), `${csv.join("\n")}\n`);

  // ---------------------------------------------------------------- report

  const lines: string[] = [];
  lines.push("# shake retention benchmark: generated results\n");
  lines.push(
    `Run directory: \`${runDir}\`  \nModel: \`${manifest.model ?? "unknown"}\`  \n` +
      `Binary: \`${manifest.binaryVersion ?? "unknown"}\`  \n` +
      `Fixture variant(s): ${[...new Set(cells.map((cell) => cell.variant ?? "plain"))].join(", ")}  \n` +
      `Completed cells: ${cells.length}/${manifest.expectedCells.length}; scored observations per arm: ${byArm[presentArms[0]!]!.total}.\n` +
      "Validation: complete manifest matrix, matching model and variant identities, completed turns, parse success, and 100% full controls.\n",
  );
  if (validation.protocolViolationCells.length > 0) {
    lines.push(
      `Protocol violations: ${validation.protocolViolationCells.length} ` +
        "shake-elide-noread cell(s) called read_artifact. Inclusive accuracy includes these cells; the compliant view below excludes them.\n",
    );
  }
  if (validation.nestedTelemetryUnverifiedCells.length > 0) {
    lines.push(
      `Nested recovery telemetry is unavailable for ${validation.nestedTelemetryUnverifiedCells.length} cell(s); ` +
        "nested attempt counts and derived page bytes are shown as n/a, and noread compliance is unverified for those cells.\n",
    );
  }

  lines.push("## Accuracy\n");
  lines.push("| Arm | Correct | Accuracy | Descriptive Wilson 95% interval |\n|---|---:|---:|---:|");
  for (const arm of presentArms) {
    const value = byArm[arm]!;
    lines.push(
      `| ${arm} | ${value.correct}/${value.total} | ${pct(value.accuracy)} | ${pct(value.wilson95[0])}-${pct(value.wilson95[1])} |`,
    );
  }

  if (validation.protocolViolationCells.length > 0) {
    lines.push("\n## Accuracy excluding noread protocol violations\n");
    lines.push("| Arm | Inclusive accuracy | Accuracy conditional on compliant cells |\n|---|---:|---:|");
    for (const arm of presentArms) {
      lines.push(
        `| ${arm} | ${pct(byArm[arm]!.accuracy)} | ${pct(byArmExcludingProtocolViolations[arm]!.accuracy)} |`,
      );
    }
  }

  lines.push("\n## Accuracy by category\n");
  lines.push(`| Category | ${presentArms.join(" | ")} |\n|---|${presentArms.map(() => "---:").join("|")}|`);
  for (const category of categories) {
    lines.push(`| ${category} | ${presentArms.map((arm) => pct(byCategory[category]![arm]!.accuracy)).join(" | ")} |`);
  }

  lines.push("\n## Paired outcomes against the full-context control\n");
  lines.push(
    "Per question, per (fixture, trial). `control only` counts retention losses caused by the arm. " +
      "The paired p-value is descriptive because questions from one generated turn are not independent observations.\n\n" +
      "| Arm | Both correct | Control only | Arm only | Both wrong | Descriptive paired p |\n|---|---:|---:|---:|---:|---:|",
  );
  for (const arm of presentArms.filter((a) => a !== CONTROL)) {
    const value = paired[arm];
    if (!value) continue;
    lines.push(
      `| ${arm} | ${value.bothCorrect} | ${value.controlOnly} | ${value.armOnly} | ${value.bothWrong} | ${value.exactMcNemarP.toPrecision(3)} |`,
    );
  }

  lines.push("\n## Downstream footprint and latency\n");
  lines.push(
    "`Downstream input tokens` is the input of the evaluation turn's **first** model request, i.e. the context the arm presented. " +
      "`Turn input tokens` sums every request in the turn, so it includes recovery round trips.\n\n" +
      "| Arm | Downstream input tokens | Turn input tokens | Output tokens | Eval latency (s) | Prep latency (s) | Parse failures |\n" +
      "|---|---:|---:|---:|---:|---:|---:|",
  );
  for (const arm of presentArms) {
    const value = footprint[arm]!;
    lines.push(
      `| ${arm} | ${int(value.meanDownstreamInputTokens)} | ${int(value.meanTurnInputTokens)} | ${int(value.meanOutputTokens)} | ` +
        `${num(value.meanEvaluationLatencyMs / 1000)} | ${num(value.meanPreparationLatencyMs / 1000)} | ${value.parseFailures} |`,
    );
  }

  if (shakeArms.length > 0) {
    lines.push("\n## Recovery cost (shake arms)\n");
    lines.push(
      "| Arm | Cells with attempts | Outer code-mode continuations | Nested read attempts | Mean derived page bytes | Mean extra input tokens | Mean artifacts written | Mean artifact bytes | noread violations |\n" +
        "|---|---:|---:|---:|---:|---:|---:|---:|",
    );
    for (const arm of shakeArms) {
      const value = recovery[arm]!;
      lines.push(
        `| ${arm} | ${value.cellsWithAttempts === null ? "n/a" : value.cellsWithAttempts}/${value.cells} | ${num(value.meanOuterCodeModeContinuations)} | ` +
          `${num(value.meanNestedReadAttempts)} | ${value.meanDerivedReadArtifactBytes === null ? "n/a" : int(value.meanDerivedReadArtifactBytes)} | ` +
          `${int(value.meanExtraInputTokens)} | ${num(value.meanArtifactFiles)} | ${int(value.meanArtifactBytes)} | ${value.noreadViolations} |`,
      );
    }
    lines.push(
      "\nNested read attempts come from executed-tool metadata and may be batched inside one outer " +
        "code-mode continuation. Derived page bytes are estimates from artifact content and offsets; " +
        "actual handler-returned bytes are unavailable. Any evaluation tool call in the `shake-elide-noread` " +
        "arm is a protocol violation and is excluded only from the conditional compliant view.\n",
    );

    lines.push("\n## Shake preview estimate vs. realized downstream input\n");
    lines.push(
      "Preview numbers are local estimates that exclude base instructions and tool schemas; the table reports " +
        "their raw difference and ratio against the realized first request.\n\n" +
        "| Arm | Applied | Preview tokensBefore | Preview tokensAfter | Estimated saving | Realized downstream input | Realized - preview | Realized / preview | Tool outputs elided | Shake latency (s) |\n" +
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    );
    for (const arm of shakeArms) {
      const value = previewVsRealized[arm]!;
      lines.push(
        `| ${arm} | ${value.applied}/${value.cells} | ${int(value.meanTokensBefore)} | ${int(value.meanTokensAfter)} | ` +
          `${int(value.meanTokensBefore - value.meanTokensAfter)} | ${int(value.meanRealizedDownstreamInputTokens)} | ` +
          `${int(value.meanRealizedMinusPreviewTokens)} | ${num(value.meanRealizedToPreviewRatio)} | ` +
          `${num(value.meanToolOutputsElided)} | ${num(value.meanShakeLatencyMs / 1000)} |`,
      );
    }
    lines.push(
      `\nFull-context control realized downstream input: ${int(controlDownstream)} tokens on average.\n`,
    );
  }

  lines.push("\n## Per-fixture accuracy\n");
  lines.push(`| Fixture | ${presentArms.join(" | ")} |\n|---|${presentArms.map(() => "---:").join("|")}|`);
  for (const [fixtureId, values] of Object.entries(perFixture)) {
    lines.push(
      `| ${fixtureId} | ${presentArms.map((arm) => pct((values as Record<Arm, Aggregate>)[arm]!.accuracy)).join(" | ")} |`,
    );
  }

  lines.push(
    "\nThis generated document reports measurements only. Interpretive conclusions and limitations belong in REPORT.md.\n",
  );

  const report = lines.join("\n");
  writeFileSync(join(runDir, "GENERATED_RESULTS.md"), report);
  console.log(report);
}

if (import.meta.url === `file://${process.argv[1]}`) main();
