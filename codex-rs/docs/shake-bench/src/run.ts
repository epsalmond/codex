#!/usr/bin/env node
// shake-bench driver.
//
// Walks (fixture x trial x arm) cells against `codex-next app-server`, one
// fresh thread per cell, and writes one atomic JSON result per cell.
//
// The trial-record shape and the evaluation flow are informed by
// algal/pi-openai-server-compaction, benchmarks/native-vs-text/run.ts (MIT).

import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { AppServerClient, type Notification } from "./appserver.ts";
import { ARMS, type Arm, type ArmPreparation, type Driver, armOrder, isArm, prepareArm, promptVariant } from "./arms.ts";
import { answerSchema, applyVariant, eligibleToolOutputCount, evaluationPrompt, isVariant, parseAnswers, payloadBytes, prepareInjectItems, scoreAnswers, SYSTEM_INSTRUCTIONS, VARIANTS, type FixtureVariant, type ScoreRow } from "./convert.ts";
import { buildFixture, type BenchmarkFixture } from "./fixtures.ts";
import { runBounded } from "./concurrency.ts";

const CLIENT_INFO = { name: "shake_bench", title: "shake-bench", version: "0.1.0" };
const READ_ARTIFACT = "read_artifact";
export const MINIMAL_CONFIG = `# shake-bench: deterministic runtime configuration
project_doc_max_bytes = 0

[features]
apps = false
browser_use = false
browser_use_external = false
browser_use_full_cdp_access = false
code_mode_host = true
executed_tool_call_metadata = true
computer_use = false
goals = false
hooks = false
image_generation = false
in_app_browser = false
memories = false
multi_agent = false
plugins = false
recommended_plugins = false
remote_plugin = false
shell_tool = false
skill_mcp_dependency_install = false
skill_search = false
skip_host_skill_discovery = true
sleep_tool = false
tool_suggest = false
unified_exec = false
view_image = false
workspace_dependencies = false

[skills]
include_instructions = false

[skills.bundled]
enabled = false
`;
const REPO_ROOT = fileURLToPath(new URL("..", import.meta.url));
const SOURCE_FILES = [
  "package.json",
  "package-lock.json",
  "src/arms.ts",
  "src/appserver.ts",
  "src/analyze.ts",
  "src/cost.ts",
  "src/convert.ts",
  "src/concurrency.ts",
  "src/fixtures.ts",
  "src/run.ts",
] as const;

// ---------------------------------------------------------------- CLI

type Options = {
  fixtureCount: number;
  trials: number;
  arms: Arm[];
  outDir: string;
  model?: string;
  force: boolean;
  dryRun: boolean;
  bin: string;
  variant: FixtureVariant;
  nestedTelemetry: "required" | "unavailable";
  concurrency: number;
};

export function parseArgs(argv: string[]): Options {
  const read = (name: string): string | undefined => {
    const index = argv.indexOf(name);
    return index >= 0 ? argv[index + 1] : undefined;
  };
  const fixtureCount = Number(read("--fixtures") ?? "6");
  const trials = Number(read("--trials") ?? "2");
  const armsRaw = read("--arms");
  const arms = armsRaw ? armsRaw.split(",").map((value) => value.trim()).filter(Boolean) : [...ARMS];
  for (const arm of arms) if (!isArm(arm)) throw new Error(`unknown arm: ${arm} (known: ${ARMS.join(",")})`);
  if (new Set(arms).size !== arms.length) throw new Error("--arms must not contain duplicates");
  if (!arms.includes("full")) throw new Error("--arms must include the full control arm");
  if (!Number.isInteger(fixtureCount) || fixtureCount < 1) throw new Error("--fixtures must be a positive integer");
  if (!Number.isInteger(trials) || trials < 1) throw new Error("--trials must be a positive integer");
  const model = read("--model") ?? undefined;
  const variant = read("--variant") ?? "plain";
  if (!isVariant(variant)) throw new Error(`unknown variant: ${variant} (known: ${VARIANTS.join(",")})`);
  const nestedTelemetryArg = read("--nested-telemetry") ?? "strict";
  if (nestedTelemetryArg !== "strict" && nestedTelemetryArg !== "unavailable") {
    throw new Error(`unknown --nested-telemetry policy: ${nestedTelemetryArg} (known: strict, unavailable)`);
  }
  const nestedTelemetry = nestedTelemetryArg === "strict" ? "required" : nestedTelemetryArg;
  const concurrency = Number(read("--concurrency") ?? "1");
  if (!Number.isInteger(concurrency) || concurrency < 1) throw new Error("--concurrency must be a positive integer");
  const stamp = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
  const outDir = resolve(read("--out") ?? join("results", `${stamp}_${model ?? "default"}`));
  return {
    fixtureCount,
    trials,
    arms: arms as Arm[],
    outDir,
    model,
    force: argv.includes("--force"),
    dryRun: argv.includes("--dry-run"),
    bin: read("--bin") ?? "codex-next",
    variant,
    nestedTelemetry,
    concurrency,
  };
}

// ---------------------------------------------------------------- isolated CODEX_HOME

/** Copy only the credentials required to authenticate into a private run home. */
function prepareCodexHome(target: string, source: string): { configFingerprint: string } {
  mkdirSync(target, { recursive: true });
  const auth = join(source, "auth.json");
  if (!existsSync(auth)) throw new Error(`missing ${auth}; log in with codex first`);
  copyFileSync(auth, join(target, "auth.json"));
  writeFileSync(join(target, "config.toml"), MINIMAL_CONFIG);
  return { configFingerprint: hashText(MINIMAL_CONFIG) };
}

function hashText(text: string): string {
  return createHash("sha256").update(text).digest("hex");
}

function sourceFingerprint(): string {
  const digest = createHash("sha256");
  for (const file of SOURCE_FILES) {
    digest.update(file);
    digest.update("\0");
    digest.update(readFileSync(join(REPO_ROOT, file)));
    digest.update("\0");
  }
  return digest.digest("hex");
}

function createEvaluationCwd(): string {
  return mkdtempSync(join(tmpdir(), "shake-bench-cwd-"));
}

function fixtureFingerprint(fixtures: BenchmarkFixture[], variant: FixtureVariant): string {
  return hashText(
    JSON.stringify(
      fixtures.map((fixture) => ({
        id: fixture.id,
        seed: fixture.seed,
        history: prepareInjectItems(applyVariant(fixture.history, variant)),
        sharedTail: prepareInjectItems(applyVariant(fixture.sharedTail, variant)),
        questions: fixture.questions,
      })),
    ),
  );
}

export type ExpectedCell = {
  fixtureId: string;
  seed: number;
  trial: number;
  arm: Arm;
  model: string;
  variant: FixtureVariant;
};

export function armOrderForCell(fixtureIndex: number, trial: number, trials: number, arms: readonly Arm[] = ARMS): Arm[] {
  return armOrder(fixtureIndex * trials + trial, arms);
}

/** The full control always runs first; the remaining arms retain rotated order. */
export function actualArmOrderForCell(
  fixtureIndex: number,
  trial: number,
  trials: number,
  arms: readonly Arm[] = ARMS,
): Arm[] {
  return ["full", ...armOrderForCell(fixtureIndex, trial, trials, arms).filter((arm) => arm !== "full")];
}

export function buildExpectedCells(
  fixtures: BenchmarkFixture[],
  trials: number,
  arms: readonly Arm[],
  model: string,
  variant: FixtureVariant,
): ExpectedCell[] {
  return fixtures.flatMap((fixture) =>
    Array.from({ length: trials }, (_, trial) =>
      arms.map((arm) => ({ fixtureId: fixture.id, seed: fixture.seed, trial, arm, model, variant })),
    ).flat(),
  );
}

export type ManifestFingerprintInput = {
  binary: string;
  binaryVersion: string;
  model: string;
  configFingerprint: string;
  fixtureFingerprint: string;
  fixtureCount: number;
  variant: FixtureVariant;
  trials: number;
  arms: Arm[];
  expectedCells: ExpectedCell[];
  codeFingerprint: string;
  nestedTelemetryPolicy: "required" | "unavailable";
  concurrency: number;
};

type RunManifest = ManifestFingerprintInput & {
  fingerprint: string;
  startedAt: string;
  actualArmOrderByFixtureTrial: Record<string, Arm[]>;
  fixtureAdaptation: {
    toolOutputPaddingTargetBytes: number;
    eligibleToolOutputs: number;
    rationale: string;
  };
  upstreamFixtureSource: string;
};

export function manifestFingerprint(input: ManifestFingerprintInput): string {
  return hashText(JSON.stringify(input));
}

function manifestInput(
  options: Options,
  fixtures: BenchmarkFixture[],
  model: string,
  binaryVersion: string,
  configFingerprint: string,
  codeFingerprint: string,
): ManifestFingerprintInput {
  const expectedCells = buildExpectedCells(fixtures, options.trials, options.arms, model, options.variant);
  return {
    binary: options.bin,
    binaryVersion,
    model,
    configFingerprint,
    fixtureFingerprint: fixtureFingerprint(fixtures, options.variant),
    fixtureCount: options.fixtureCount,
    variant: options.variant,
    trials: options.trials,
    arms: [...options.arms],
    expectedCells,
    codeFingerprint,
    nestedTelemetryPolicy: options.nestedTelemetry,
    concurrency: options.concurrency,
  };
}

function readExistingManifest(outDir: string): Record<string, unknown> | undefined {
  if (!existsSync(outDir)) return undefined;
  const manifestPath = join(outDir, "manifest.json");
  if (!existsSync(manifestPath)) {
    if (readdirSync(outDir).length > 0) {
      throw new Error(`refusing to reuse ${outDir}: manifest.json is missing`);
    }
    return undefined;
  }
  let value: unknown;
  try {
    value = JSON.parse(readFileSync(manifestPath, "utf8"));
  } catch (error) {
    throw new Error(`refusing to reuse ${outDir}: manifest.json is invalid (${String(error)})`);
  }
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`refusing to reuse ${outDir}: manifest.json must be an object`);
  }
  return value as Record<string, unknown>;
}

function assertCompatibleResume(existing: Record<string, unknown>, expected: ManifestFingerprintInput): void {
  if (typeof existing.fingerprint !== "string" || existing.fingerprint.length === 0) {
    throw new Error("refusing to resume: existing manifest has no immutable fingerprint");
  }
  const expectedFingerprint = manifestFingerprint(expected);
  if (existing.fingerprint !== expectedFingerprint) {
    throw new Error(
      `refusing incompatible resume: manifest fingerprint differs (expected ${expectedFingerprint}, found ${existing.fingerprint})`,
    );
  }
}

// ---------------------------------------------------------------- drivers

class LiveDriver implements Driver {
  constructor(private readonly client: AppServerClient) {}
  request<T>(method: string, params: Record<string, unknown> = {}, timeoutMs?: number): Promise<T> {
    return this.client.request<T>(method, params, timeoutMs);
  }
  waitFor(predicate: (n: Notification) => boolean, timeoutMs?: number, label?: string): Promise<Notification> {
    return this.client.waitFor(predicate, timeoutMs, label);
  }
  now(): number {
    return Date.now();
  }
}

/** Records the request sequence and returns plausible canned results. */
class DryDriver implements Driver {
  readonly log: string[] = [];
  private clock = 0;
  async request<T>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    this.log.push(`--> ${method} ${summarize(params)}`);
    if (method === "thread/start") return { thread: { id: "dry-thread" } } as T;
    if (method === "thread/shake/preview") {
      return {
        preview: {
          tokensBefore: 35000,
          tokensAfter: 9000,
          toolOutputs: 60,
          textBlocks: 0,
          thinkingBlocks: 0,
          images: 0,
          fingerprint: "dry-fingerprint",
        },
      } as T;
    }
    if (method === "turn/start") return { turn: { id: "dry-turn", status: "inProgress" } } as T;
    return {} as T;
  }
  async waitFor(_predicate: (n: Notification) => boolean, _timeoutMs?: number, label = "notification"): Promise<Notification> {
    this.log.push(`<-- (await ${label})`);
    if (label.includes("shake")) {
      return {
        method: "warning",
        params: { threadId: "dry-thread", message: "⛭ shake: Shook ~26000 tokens" },
      };
    }
    return { method: "turn/completed", params: {} };
  }
  now(): number {
    this.clock += 1000;
    return this.clock;
  }
}

function summarize(params: Record<string, unknown>): string {
  const clone: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(params)) {
    if (key === "items" && Array.isArray(value)) clone[key] = `<${value.length} items, ${payloadBytes(value as Array<Record<string, unknown>>)} bytes>`;
    else if (key === "outputSchema") clone[key] = "<answers schema, 75 properties>";
    else if (key === "input" && Array.isArray(value)) clone[key] = `<${value.length} inputs, ${JSON.stringify(value).length} chars>`;
    else clone[key] = value;
  }
  return JSON.stringify(clone);
}

// ---------------------------------------------------------------- cell record

type UsageBreakdown = {
  inputTokens: number;
  cachedInputTokens: number;
  outputTokens: number;
  reasoningOutputTokens: number;
  totalTokens: number;
  cacheWriteInputTokens?: number;
};

export type ArtifactRead = {
  callId: string;
  artifact: string;
  startByte: number | null;
  outputBytes: number;
  outputBytesObserved: number | null;
  outputBytesEstimated: number | null;
  source: "direct" | "nested";
};
export type ToolCall = { callId: string; name: string; itemType: string; nested?: boolean; transport?: boolean };
export type ToolTelemetry = {
  outerToolCalls: ToolCall[];
  nestedToolCalls: ToolCall[];
  nestedReads: Array<{ callId: string; artifact: string; startByte: number | null }>;
  telemetryComplete: boolean;
  telemetryIncomplete: boolean;
  inventoryAvailable: boolean;
  telemetryUnavailable: boolean;
  telemetryConflict: boolean;
};

type CellRecord = {
  fixtureId: string;
  seed: number;
  trial: number;
  arm: Arm;
  variant: FixtureVariant;
  model: string;
  threadId: string;
  injectedItems: number;
  injectedBytes: number;
  preparation: ArmPreparation;
  evaluation: {
    turnId: string;
    latencyMs: number;
    turnStatus: string;
    rawText: string;
    parsedAnswers: Record<string, string>;
    parseFailed: boolean;
    scores: ScoreRow[];
    /** Input tokens of the first model request of the evaluation turn. */
    firstRequestInputTokens: number;
    /** Input tokens summed across every model request in the evaluation turn. */
    turnInputTokens: number;
    turnOutputTokens: number;
    extraRecoveryInputTokens: number;
    usageUpdates: UsageBreakdown[];
    toolCalls: ToolCall[];
    forbiddenToolCalls: ToolCall[];
    nestedToolCalls: ToolCall[];
    telemetryComplete: boolean;
    estimatedReadArtifactBytes: number;
    outerCodeModeContinuations: number;
    nestedToolCallsComplete: boolean | null;
    readArtifactAttempts: number | null;
    derivedReadArtifactBytes: number | null;
    telemetryPolicy: "required" | "unavailable";
    telemetryUnavailable: boolean;
    telemetryConflict: boolean;
  };
  recovery: {
    readArtifactCalls: number;
    readArtifactBytes: number;
    reads: ArtifactRead[];
    /** A noread-arm tool call is a protocol violation; reported both ways. */
    violation: boolean;
    toolCalls: ToolCall[];
    forbiddenToolCalls: ToolCall[];
    nestedToolCalls: ToolCall[] | null;
    telemetryComplete: boolean;
    estimatedReadArtifactBytes: number;
    outerCodeModeContinuations: number;
    nestedToolCallsComplete: boolean | null;
    readArtifactAttempts: number | null;
    derivedReadArtifactBytes: number | null;
    telemetryPolicy: "required" | "unavailable";
    telemetryUnavailable: boolean;
    telemetryConflict: boolean;
    artifactFiles: number;
    artifactBytes: number;
  };
  warnings: string[];
  startedAt: string;
  finishedAt: string;
};

// ---------------------------------------------------------------- rollout inspection

function findRollout(codexHome: string, threadId: string): string | undefined {
  const root = join(codexHome, "sessions");
  if (!existsSync(root)) return undefined;
  const stack = [root];
  while (stack.length > 0) {
    const dir = stack.pop()!;
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const path = join(dir, entry.name);
      if (entry.isDirectory()) stack.push(path);
      else if (entry.isFile() && entry.name.endsWith(`${threadId}.jsonl`)) return path;
    }
  }
  return undefined;
}

function asObject(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : undefined;
}

function hasTruncationMarker(value: unknown): boolean {
  const object = asObject(value);
  if (!object) return false;
  if ("_codex_executed_tool_call_truncated" in object || "_codex_executed_tool_call_raw" in object) return true;
  return Object.values(object).some((entry) => hasTruncationMarker(entry));
}

function parseCallArguments(value: unknown): Record<string, unknown> {
  if (typeof value === "string") {
    try {
      return asObject(JSON.parse(value)) ?? {};
    } catch {
      return {};
    }
  }
  return asObject(value) ?? {};
}

/** Parse persisted outer and Code Mode nested tool-call telemetry without inspecting source text. */
export function parseToolTelemetry(contents: string, evaluationTurnId: string, additionalItems: unknown[] = []): ToolTelemetry {
  const outerToolCalls: ToolCall[] = [];
  const nestedToolCalls: ToolCall[] = [];
  const nestedReads: ToolTelemetry["nestedReads"] = [];
  const snapshots = new Set<string>();
  const transportState = new Map<string, { sawMetadata: boolean; complete: boolean; truncated: boolean }>();
  const transportCells = new Map<string, string>();
  const outerByKey = new Map<string, ToolCall>();
  let telemetryConflict = false;
  let inventoryAvailable = false;
  const lines = [
    ...contents.split("\n"),
    ...additionalItems.map((value) => {
      const notification = asObject(value) ?? {};
      return JSON.stringify({ payload: notification.item ?? notification, raw_turn_id: notification.turnId });
    }),
  ];
  for (const line of lines) {
    if (!line.trim()) continue;
    let parsed: Record<string, unknown>;
    try {
      const value: unknown = JSON.parse(line);
      const object = asObject(value);
      if (!object) continue;
      parsed = object;
    } catch {
      continue;
    }
    const nestedPayload = asObject(parsed.payload);
    const payload = nestedPayload ?? parsed;
    if (payload === parsed && parsed.type === "response_item") continue;
    const metadata = asObject(payload.internal_chat_message_metadata_passthrough) ??
      asObject(parsed.internal_chat_message_metadata_passthrough);
    const payloadTurnId = String(metadata?.turn_id ?? parsed.raw_turn_id ?? "");
    if (payloadTurnId !== evaluationTurnId) continue;
    const itemType = String(payload.type ?? "");
    const isOuterCall = itemType === "function_call" || itemType === "custom_tool_call";
    const callId = String(payload.call_id ?? "");
    const name = String(payload.name ?? "");
    const transport = name === "exec" || name === "wait";
    if (isOuterCall) {
      const outer = { callId, name, itemType, ...(transport ? { transport: true } : {}) };
      const key = `${itemType}\0${callId}`;
      const previous = outerByKey.get(key);
      if (previous && previous.name !== name) telemetryConflict = true;
      if (!previous) {
        outerByKey.set(key, outer);
        outerToolCalls.push(outer);
      }
    }
    if (!transport && !String(itemType).endsWith("_output")) continue;
    const inventoryId = callId || String(payload.id ?? "");
    const cellId = String(metadata?.cell_id ?? inventoryId);
    const state = transportState.get(cellId) ?? { sawMetadata: false, complete: false, truncated: false };
    if (inventoryId) transportCells.set(inventoryId, cellId);
    const rawCalls = metadata?.executed_tool_calls;
    if (metadata && ("executed_tool_calls" in metadata || "tool_calls_complete" in metadata)) {
      inventoryAvailable = true;
    }
    if (!Array.isArray(rawCalls)) {
      if (metadata && ("tool_calls_complete" in metadata || "executed_tool_calls" in metadata)) state.sawMetadata = true;
      if (metadata?.tool_calls_complete === true) state.complete = true;
      transportState.set(cellId, state);
      continue;
    }
    state.sawMetadata = true;
    const nested = rawCalls.map((raw) => {
      const call = asObject(raw) ?? {};
      const callName = String(call.name ?? "");
      const args = parseCallArguments(call.arguments);
      return { callName, args, truncated: hasTruncationMarker(call.arguments) };
    });
    const complete = metadata?.tool_calls_complete === true;
    // The same outer output can be replayed in the rollout; count its inventory once.
    const snapshotKey = `${inventoryId}\0${JSON.stringify(nested)}\0${complete}`;
    if (!snapshots.has(snapshotKey)) {
      snapshots.add(snapshotKey);
      nested.forEach(({ callName, args, truncated }, index) => {
        state.truncated ||= truncated;
        const nestedCallId = `${inventoryId}/nested/${index}`;
        nestedToolCalls.push({ callId: nestedCallId, name: callName, itemType: "executed_tool_call", nested: true });
        if (callName === READ_ARTIFACT) {
          nestedReads.push({
            callId: nestedCallId,
            artifact: String(args.artifact ?? ""),
            startByte: typeof args.start_byte === "number" ? args.start_byte : null,
          });
        }
      });
    }
    state.complete ||= complete;
    transportState.set(cellId, state);
  }
  const transportCalls = outerToolCalls.filter((call) => call.transport);
  const telemetryComplete = transportCalls.length === 0
    ? true
    : [...new Set(transportCalls.map((call) => transportCells.get(call.callId) ?? call.callId))].every((cellId) => {
        const state = transportState.get(cellId);
        return Boolean(state?.sawMetadata && state.complete && !state.truncated);
      });
  return {
    outerToolCalls,
    nestedToolCalls,
    nestedReads,
    telemetryComplete,
    telemetryIncomplete: telemetryConflict || transportCalls.length > 0 && !telemetryComplete,
    inventoryAvailable,
    telemetryUnavailable: transportCalls.length > 0 && !inventoryAvailable && !telemetryConflict,
    telemetryConflict,
  };
}

/** Count `read_artifact` calls and returned bytes from the thread's rollout. */
function inspectRecovery(
  codexHome: string,
  threadId: string,
  evaluationTurnId: string,
  rawResponseItems: unknown[] = [],
): Omit<CellRecord["recovery"], "violation"> {
  const reads: ArtifactRead[] = [];
  const outputs = new Map<string, number>();
  const rollout = findRollout(codexHome, threadId);
  const rolloutContents = rollout ? readFileSync(rollout, "utf8") : "";
  const telemetry = parseToolTelemetry(rolloutContents, evaluationTurnId, rawResponseItems);
  const directReadToolCalls = telemetry.outerToolCalls
    .filter((call) => call.name === READ_ARTIFACT)
    .map((call) => ({ ...call, nested: true }));
  const nestedToolCalls = [...telemetry.nestedToolCalls, ...directReadToolCalls];
  const toolCalls = [...telemetry.outerToolCalls, ...telemetry.nestedToolCalls];
  if (rollout) {
    for (const line of rolloutContents.split("\n")) {
      if (!line.trim()) continue;
      let parsed: Record<string, unknown>;
      try {
        const value: unknown = JSON.parse(line);
        if (!value || typeof value !== "object" || Array.isArray(value)) continue;
        parsed = value as Record<string, unknown>;
      } catch {
        continue;
      }
      const nested = parsed.payload;
      const payload =
        nested && typeof nested === "object" && !Array.isArray(nested)
          ? (nested as Record<string, unknown>)
          : parsed;
      if (payload === parsed && parsed.type === "response_item") continue;
      const metadata = (payload.internal_chat_message_metadata_passthrough ??
        parsed.internal_chat_message_metadata_passthrough) as { turn_id?: string; turnId?: string } | undefined;
      const payloadTurnId = metadata?.turn_id ?? metadata?.turnId;
      if (payloadTurnId !== evaluationTurnId) continue;
      const itemType = String(payload.type ?? "");
      const name = String(payload.name ?? "");
      const callId = String(payload.call_id ?? "");
      if (itemType === "function_call" || itemType === "custom_tool_call") {
        if (name !== READ_ARTIFACT) continue;
        const args = parseCallArguments(payload.arguments ?? payload.input);
        reads.push({
          callId,
          artifact: String(args.artifact ?? ""),
          startByte: typeof args.start_byte === "number" ? args.start_byte : null,
          outputBytes: 0,
          outputBytesObserved: null,
          outputBytesEstimated: null,
          source: "direct",
        });
      } else if (itemType === "function_call_output" || itemType === "custom_tool_call_output") {
        outputs.set(callId, Buffer.byteLength(String(payload.output ?? ""), "utf8"));
      }
    }
  }
  for (const read of reads) {
    if (read.source !== "direct") continue;
    const bytes = outputs.get(read.callId) ?? 0;
    read.outputBytes = bytes;
    read.outputBytesObserved = bytes;
  }

  let artifactFiles = 0;
  let artifactBytes = 0;
  const artifactDir = join(codexHome, "artifacts", threadId);
  if (existsSync(artifactDir)) {
    for (const entry of readdirSync(artifactDir)) {
      const path = join(artifactDir, entry);
      if (statSync(path).isFile()) {
        artifactFiles += 1;
        artifactBytes += statSync(path).size;
      }
    }
  }
  const artifactSize = (artifact: string): number | null => {
    const id = artifact.startsWith("artifact://") ? artifact.slice("artifact://".length) : artifact;
    if (!id) return null;
    const path = join(artifactDir, `${id}.tool_output.log`);
    return existsSync(path) && statSync(path).isFile() ? statSync(path).size : null;
  };
  for (const nested of telemetry.nestedReads) {
    const size = artifactSize(nested.artifact);
    const start = Math.max(0, nested.startByte ?? 0);
    const estimated = size === null ? null : Math.min(3_072, Math.max(0, size - start));
    reads.push({
      callId: nested.callId,
      artifact: nested.artifact,
      startByte: nested.startByte,
      outputBytes: 0,
      outputBytesObserved: null,
      outputBytesEstimated: estimated,
      source: "nested",
    });
  }
  const observedBytes = reads.reduce((sum, read) => sum + (read.outputBytesObserved ?? read.outputBytes), 0);
  const estimatedBytes = reads.reduce((sum, read) => sum + (read.outputBytesEstimated ?? 0), 0);
  return {
    readArtifactCalls: reads.length,
    readArtifactBytes: observedBytes,
    reads,
    toolCalls,
    forbiddenToolCalls: [],
    nestedToolCalls,
    telemetryComplete: telemetry.telemetryComplete,
    estimatedReadArtifactBytes: estimatedBytes,
    outerCodeModeContinuations: telemetry.outerToolCalls.filter((call) => call.transport).length,
    nestedToolCallsComplete: telemetry.telemetryComplete,
    readArtifactAttempts: nestedToolCalls.filter((call) => call.name === READ_ARTIFACT).length,
    derivedReadArtifactBytes: estimatedBytes > 0 ? estimatedBytes : nestedToolCalls.some((call) => call.name === READ_ARTIFACT) ? 0 : null,
    telemetryPolicy: "required",
    telemetryUnavailable: telemetry.telemetryUnavailable,
    telemetryConflict: telemetry.telemetryConflict,
    artifactFiles,
    artifactBytes,
  };
}

// ---------------------------------------------------------------- one cell

export function notificationTurnId(notification: Notification): string | undefined {
  if (typeof notification.params.turnId === "string") return notification.params.turnId;
  const turn = notification.params.turn as { id?: unknown } | undefined;
  return typeof turn?.id === "string" ? turn.id : undefined;
}

export function matchesEvaluationNotification(
  notification: Notification,
  threadId: string,
  turnId: string,
): boolean {
  return notification.params.threadId === threadId && notificationTurnId(notification) === turnId;
}

type RunCellParams = {
  client: AppServerClient | undefined;
  driver: Driver;
  codexHome: string;
  fixture: BenchmarkFixture;
  trial: number;
  arm: Arm;
  variant: FixtureVariant;
  model: string;
  cwd: string;
  dryRun: boolean;
  nestedTelemetry: "required" | "unavailable";
  threadIdRef?: { value?: string };
};

async function runCell(params: RunCellParams): Promise<CellRecord> {
  const { driver, fixture, arm } = params;
  const startedAt = new Date().toISOString();
  const warnings: string[] = [];
  const usageUpdates: UsageBreakdown[] = [];
  let turnUsageStarted = false;
  let deltas = "";
  let finalMessage = "";
  let turnStatus = "unknown";
  const observedTurnTerminals: Notification[] = [];
  const rawResponseItems: unknown[] = [];
  let threadId: string | undefined;
  let evaluationTurnId: string | undefined;
  const pendingNotifications: Notification[] = [];
  const ownedNotifications: Notification[] = [];

  const handleEvaluationNotification = (notification: Notification): void => {
    if (!threadId || !evaluationTurnId || !matchesEvaluationNotification(notification, threadId, evaluationTurnId)) return;
    if (notification.method === "rawResponseItem/completed") rawResponseItems.push(notification.params);
    if (notification.method === "thread/tokenUsage/updated") {
      const usage = (notification.params.tokenUsage as { last?: UsageBreakdown } | undefined)?.last;
      if (usage) usageUpdates.push(usage);
    }
    if (notification.method === "item/agentMessage/delta") deltas += String(notification.params.delta ?? "");
    if (notification.method === "item/completed") {
      const item = notification.params.item as { type?: string; text?: string } | undefined;
      if (item?.type === "agentMessage" && typeof item.text === "string") finalMessage = item.text;
    }
  };
  const handleOwnedNotification = (notification: Notification): void => {
    ownedNotifications.push(notification);
    if (notification.method === "warning") warnings.push(String(notification.params.message ?? ""));
    if (notification.method === "turn/completed" || notification.method === "turn/failed") {
      observedTurnTerminals.push(notification);
    }
    handleEvaluationNotification(notification);
  };

  const off = params.client?.onNotification((notification) => {
    if (!threadId) {
      pendingNotifications.push(notification);
    } else if (notification.params.threadId === threadId) {
      handleOwnedNotification(notification);
    }
  });

  try {
    const startParams: Record<string, unknown> = {
      cwd: params.cwd,
      approvalPolicy: "never",
      sandbox: "read-only",
      threadSource: "user",
      baseInstructions: "",
      developerInstructions: SYSTEM_INSTRUCTIONS,
      environments: [],
      dynamicTools: [],
      selectedCapabilityRoots: [],
      runtimeWorkspaceRoots: [],
      experimentalRawEvents: true,
    };
    if (params.model) startParams.model = params.model;
    const started = await driver.request<{ thread: { id: string } }>("thread/start", startParams);
    const activeThreadId = started.thread.id;
    threadId = activeThreadId;
    if (params.threadIdRef) params.threadIdRef.value = activeThreadId;
    for (const notification of pendingNotifications) {
      if (notification.params.threadId === activeThreadId) handleOwnedNotification(notification);
    }

    const history = prepareInjectItems(applyVariant(fixture.history, params.variant));
    const tail = prepareInjectItems(applyVariant(fixture.sharedTail, params.variant));
    const expectedToolOutputs = eligibleToolOutputCount([...history, ...tail]);
    if (expectedToolOutputs === 0) throw new Error(`${fixture.id}: fixture has no eligible tool outputs for shake`);
    await driver.request("thread/inject_items", { threadId: activeThreadId, items: history });
    await driver.request("thread/inject_items", { threadId: activeThreadId, items: tail });

    const preparation = await prepareArm(driver, activeThreadId, arm, params.dryRun ? undefined : expectedToolOutputs);
    if (!params.dryRun && preparation.shake) {
      const { preview } = preparation.shake;
      if (preview.toolOutputs !== expectedToolOutputs) {
        throw new Error(
          `${fixture.id} trial ${params.trial} ${arm}: shake preview saw ${preview.toolOutputs} ` +
            `tool outputs, expected ${expectedToolOutputs}`,
        );
      }
      if (preview.tokensAfter >= preview.tokensBefore) {
        throw new Error(
          `${fixture.id} trial ${params.trial} ${arm}: shake preview did not reduce tokens ` +
            `(${preview.tokensBefore} -> ${preview.tokensAfter})`,
        );
      }
      if (!preparation.shake.applied) {
        throw new Error(`${fixture.id} trial ${params.trial} ${arm}: shake warning did not confirm application`);
      }
    }

    const prompt = evaluationPrompt(fixture.questions, promptVariant(arm));
    turnUsageStarted = true;
    const evaluationStarted = driver.now();
    const startedTurn = await driver.request<{ turn: { id: string } }>("turn/start", {
      threadId: activeThreadId,
      input: [{ type: "text", text: prompt }],
      effort: "low",
      outputSchema: answerSchema(fixture.questions),
      cwd: params.cwd,
      environments: [],
      runtimeWorkspaceRoots: [],
    });
    evaluationTurnId = startedTurn.turn.id;
    if (!evaluationTurnId) throw new Error(`turn/start for ${activeThreadId} did not return a turn id`);
    for (const notification of ownedNotifications) handleEvaluationNotification(notification);
    const isEvaluationTerminal = (notification: Notification): boolean =>
      (notification.method === "turn/completed" || notification.method === "turn/failed") &&
      matchesEvaluationNotification(notification, activeThreadId, evaluationTurnId);
    const alreadyCompleted = observedTurnTerminals.find(isEvaluationTerminal);
    const turnDone = alreadyCompleted
      ? Promise.resolve(alreadyCompleted)
      : driver.waitFor(isEvaluationTerminal, 1_800_000, `evaluation turn ${evaluationTurnId} completion`);
    const completed = await turnDone;
    const latencyMs = driver.now() - evaluationStarted;
    turnUsageStarted = false;
    const turn = completed.params.turn as { status?: string; items?: Array<Record<string, unknown>> } | undefined;
    turnStatus = String(turn?.status ?? completed.method);
    if (!finalMessage) {
      for (const item of turn?.items ?? []) {
        if (item.type === "agentMessage" && typeof item.text === "string") finalMessage = item.text;
      }
    }
    const rawText = (finalMessage || deltas).trim();
    const parsedAnswers = parseAnswers(rawText);
    const scores = scoreAnswers(fixture.questions, parsedAnswers);
    const parseFailed =
      Object.keys(parsedAnswers).length !== fixture.questions.length ||
      fixture.questions.some((question) => !(question.id in parsedAnswers));

    const firstRequestInputTokens = usageUpdates[0]?.inputTokens ?? 0;
    const turnInputTokens = usageUpdates.reduce((sum, usage) => sum + usage.inputTokens, 0);
    const turnOutputTokens = usageUpdates.reduce((sum, usage) => sum + usage.outputTokens, 0);

    const recovery = params.dryRun
      ? {
          readArtifactCalls: 0,
          readArtifactBytes: 0,
          reads: [],
          toolCalls: [],
          forbiddenToolCalls: [],
          nestedToolCalls: [],
          telemetryComplete: true,
          estimatedReadArtifactBytes: 0,
          outerCodeModeContinuations: 0,
          nestedToolCallsComplete: true,
          readArtifactAttempts: 0,
          derivedReadArtifactBytes: null,
          telemetryPolicy: params.nestedTelemetry,
          telemetryUnavailable: false,
          telemetryConflict: false,
          artifactFiles: 0,
          artifactBytes: 0,
        }
      : inspectRecovery(params.codexHome, activeThreadId, evaluationTurnId, rawResponseItems);
    const recoveryAllowed = arm === "shake-elide" || arm === "shake-then-compact";
    const telemetryUnavailable = params.nestedTelemetry === "unavailable" && recovery.telemetryUnavailable;
    if (telemetryUnavailable) {
      recovery.telemetryPolicy = "unavailable";
      recovery.nestedToolCalls = null;
      recovery.nestedToolCallsComplete = null;
      recovery.readArtifactAttempts = null;
      recovery.derivedReadArtifactBytes = null;
    } else if (params.nestedTelemetry === "unavailable" && recovery.outerCodeModeContinuations === 0 && recovery.toolCalls.length === 0) {
      recovery.nestedToolCalls = [];
      recovery.nestedToolCallsComplete = true;
      recovery.readArtifactAttempts = 0;
      recovery.derivedReadArtifactBytes = 0;
    }
    const forbiddenToolCalls = recoveryAllowed
      ? [
          ...recovery.toolCalls.filter(
            (call) => !call.transport && call.name !== READ_ARTIFACT,
          ),
          ...(telemetryUnavailable || recovery.telemetryComplete || !recovery.toolCalls.some((call) => call.transport)
            ? []
            : [{ callId: "telemetry", name: "executed_tool_call_metadata", itemType: "telemetry_incomplete" }]),
        ]
      : recovery.toolCalls;
    recovery.forbiddenToolCalls = forbiddenToolCalls;
    if (forbiddenToolCalls.length > 0 && arm !== "shake-elide-noread") {
      throw new Error(
        `${fixture.id} trial ${params.trial} ${arm}: forbidden evaluation tool call(s): ` +
          forbiddenToolCalls.map((call) => `${call.name || "<unnamed>"}(${call.callId})`).join(", "),
      );
    }

    return {
      fixtureId: fixture.id,
      seed: fixture.seed,
      trial: params.trial,
      arm,
      variant: params.variant,
      model: params.model,
      threadId: activeThreadId,
      injectedItems: history.length + tail.length,
      injectedBytes: payloadBytes(history) + payloadBytes(tail),
      preparation,
      evaluation: {
        turnId: evaluationTurnId,
        latencyMs,
        turnStatus,
        rawText,
        parsedAnswers,
        parseFailed,
        scores,
        firstRequestInputTokens,
        turnInputTokens,
        turnOutputTokens,
        extraRecoveryInputTokens: Math.max(0, turnInputTokens - firstRequestInputTokens),
        usageUpdates,
        toolCalls: recovery.toolCalls,
        forbiddenToolCalls,
        nestedToolCalls: recovery.nestedToolCalls ?? [],
        telemetryComplete: recovery.telemetryComplete,
        estimatedReadArtifactBytes: recovery.estimatedReadArtifactBytes,
      },
      recovery: { ...recovery, violation: forbiddenToolCalls.length > 0 },
      warnings,
      startedAt,
      finishedAt: new Date().toISOString(),
    };
  } finally {
    off?.();
  }
}

// ---------------------------------------------------------------- main

function writeAtomic(path: string, contents: string): void {
  const partial = `${path}.partial`;
  writeFileSync(partial, contents);
  renameSync(partial, path);
}

function sanitizeRuntimeValue(value: unknown, key?: string): unknown {
  if (key && /^(auth|authorization|access_token|refresh_token|client_secret|api_key|password|secret|token)$/i.test(key)) {
    return "[redacted]";
  }
  if (Array.isArray(value)) return value.map((entry) => sanitizeRuntimeValue(entry));
  if (!value || typeof value !== "object") return value;
  return Object.fromEntries(
    Object.entries(value).map(([entryKey, entryValue]) => [entryKey, sanitizeRuntimeValue(entryValue, entryKey)]),
  );
}

function sanitizeRollout(contents: string): string {
  return contents
    .split("\n")
    .filter((line) => line.trim().length > 0)
    .flatMap((line) => {
      try {
        return [JSON.stringify(sanitizeRuntimeValue(JSON.parse(line)))];
      } catch {
        return [];
      }
    })
    .join("\n") + "\n";
}

/** Persist only inspectable runtime evidence; the private home (and auth) is removed later. */
function persistRuntime(codexHome: string, outDir: string, threadId: string): void {
  const rollout = findRollout(codexHome, threadId);
  const artifactSource = join(codexHome, "artifacts", threadId);
  if (!rollout && !existsSync(artifactSource)) return;
  const runtimeDir = join(outDir, "runtime", threadId);
  mkdirSync(runtimeDir, { recursive: true });
  if (rollout) writeAtomic(join(runtimeDir, "rollout.jsonl"), sanitizeRollout(readFileSync(rollout, "utf8")));
  if (!existsSync(artifactSource)) return;
  const artifactTarget = join(runtimeDir, "artifacts");
  mkdirSync(artifactTarget, { recursive: true });
  for (const entry of readdirSync(artifactSource, { withFileTypes: true })) {
    if (!entry.isFile()) continue;
    const source = join(artifactSource, entry.name);
    const target = join(artifactTarget, entry.name);
    copyFileSync(source, `${target}.partial`);
    renameSync(`${target}.partial`, target);
  }
}

async function runCellWithEvidence(params: RunCellParams, outDir: string): Promise<CellRecord> {
  const threadIdRef: { value?: string } = {};
  try {
    const record = await runCell({ ...params, threadIdRef });
    persistRuntime(params.codexHome, outDir, record.threadId);
    return record;
  } catch (error) {
    if (threadIdRef.value) persistRuntime(params.codexHome, outDir, threadIdRef.value);
    throw error;
  }
}

function validateControlRecord(path: string, fixture: BenchmarkFixture, trial: number): void {
  let value: unknown;
  try {
    value = JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    throw new Error(`full control cell ${path} is invalid: ${String(error)}`);
  }
  const scores = (value as { evaluation?: { scores?: Array<{ correct?: unknown }> } } | null)?.evaluation?.scores;
  if (!Array.isArray(scores) || scores.length !== fixture.questions.length) {
    throw new Error(`full control cell ${path} is incomplete; refusing to launch trial ${trial} non-control arms`);
  }
  const correct = scores.filter((score) => score?.correct === true).length;
  if (correct !== fixture.questions.length) {
    throw new Error(
      `full control ${fixture.id} trial ${trial} scored ${correct}/${fixture.questions.length}; refusing non-control evaluation`,
    );
  }
}

export async function main(): Promise<void> {
  const options = parseArgs(process.argv.slice(2));
  const fixtures = Array.from({ length: options.fixtureCount }, (_, index) => buildFixture(index + 1));

  if (options.dryRun) {
    const driver = new DryDriver();
    for (const [fixtureIndex, fixture] of fixtures.entries()) {
      for (let trial = 0; trial < options.trials; trial++) {
        for (const arm of actualArmOrderForCell(fixtureIndex, trial, options.trials, options.arms)) {
          driver.log.push(`\n=== ${fixture.id} trial ${trial} arm ${arm} ===`);
          await runCell({
            client: undefined,
            driver,
            codexHome: "<dry>",
            fixture,
            trial,
            arm,
            variant: options.variant,
            model: options.model ?? "<codex default>",
            cwd: options.outDir,
            dryRun: true,
            nestedTelemetry: options.nestedTelemetry,
          });
        }
      }
    }
    console.log(
      `dry run: ${fixtures.length} fixture(s) x ${options.trials} trial(s) x ${options.arms.length} arm(s), variant ${options.variant}`,
    );
    console.log(
      `actual arm order by fixture/trial, e.g. fixture 0 trial 0: ${actualArmOrderForCell(0, 0, options.trials, options.arms).join(" -> ")}`,
    );
    console.log(driver.log.join("\n"));
    return;
  }

  const version = execFileSync(options.bin, ["--version"], { encoding: "utf8" }).trim();
  const existing = readExistingManifest(options.outDir);
  const existingModel = typeof existing?.model === "string" ? existing.model : undefined;
  if (existing && !existingModel) throw new Error(`refusing to resume ${options.outDir}: manifest.model is missing`);
  if (existing && options.model && options.model !== existingModel) {
    throw new Error(`refusing incompatible resume: requested model ${options.model} differs from ${existingModel}`);
  }

  // CODEX_HOME intentionally lives outside the publishable result directory.
  // Only sanitized rollout evidence and artifact bytes are copied into results.
  const codexHome = mkdtempSync(join(tmpdir(), "shake-bench-home-"));
  const { configFingerprint } = prepareCodexHome(codexHome, join(process.env.HOME ?? "", ".codex"));
  const configOverrides = options.model ? [`model=${options.model}`] : existingModel ? [`model=${existingModel}`] : [];
  const client = AppServerClient.spawn({
    bin: options.bin,
    codexHome,
    configOverrides,
    stderrLogPath: join(codexHome, "app-server.stderr.log"),
  });
  const onSigint = () => {
    void client.close().finally(() => {
      rmSync(codexHome, { recursive: true, force: true });
      process.exit(130);
    });
  };
  process.once("SIGINT", onSigint);

  try {
    await client.initialize(CLIENT_INFO);
    const driver = new LiveDriver(client);

    let model = existingModel ?? options.model;
    if (!model) {
      // Resolve the default once, then pin it in this run's immutable manifest.
      const probeCwd = createEvaluationCwd();
      try {
        const probe = await client.request<{ thread: Record<string, unknown> }>("thread/start", {
          cwd: probeCwd,
          approvalPolicy: "never",
          sandbox: "read-only",
          baseInstructions: "",
          developerInstructions: SYSTEM_INSTRUCTIONS,
          environments: [],
          dynamicTools: [],
          selectedCapabilityRoots: [],
          runtimeWorkspaceRoots: [],
        });
        model = String((probe.thread.model as string | undefined) ?? "unknown");
      } finally {
        rmSync(probeCwd, { recursive: true, force: true });
      }
    }
    if (!model) throw new Error("app-server did not resolve a model");

    const codeFingerprint = sourceFingerprint();
    const input = manifestInput(options, fixtures, model, version, configFingerprint, codeFingerprint);
    if (existing) {
      assertCompatibleResume(existing, input);
    } else {
      mkdirSync(join(options.outDir, "trials"), { recursive: true });
      const manifest: RunManifest = {
        ...input,
        fingerprint: manifestFingerprint(input),
        startedAt: new Date().toISOString(),
        actualArmOrderByFixtureTrial: Object.fromEntries(
          fixtures.flatMap((fixture, fixtureIndex) =>
            Array.from({ length: options.trials }, (_, trial) => [
              `${fixture.id}:${trial}`,
              actualArmOrderForCell(fixtureIndex, trial, options.trials, options.arms),
            ] as const),
          ),
        ),
        fixtureAdaptation: {
          toolOutputPaddingTargetBytes: 2048,
          eligibleToolOutputs: fixtures.reduce((total, fixture) => {
            const history = prepareInjectItems(applyVariant(fixture.history, options.variant));
            const tail = prepareInjectItems(applyVariant(fixture.sharedTail, options.variant));
            return total + eligibleToolOutputCount([...history, ...tail]);
          }, 0),
          rationale: "The upstream fixture outputs are shorter than shake's approximately 400-token elision threshold; deterministic answer-free padding makes the shipped elision path measurable.",
        },
        upstreamFixtureSource:
          "https://github.com/algal/pi-openai-server-compaction/tree/main/benchmarks/native-vs-text (MIT)",
      };
      writeAtomic(join(options.outDir, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
      writeAtomic(
        join(options.outDir, "fixtures.json"),
        `${JSON.stringify(
          fixtures.map((fixture) => {
            const history = prepareInjectItems(applyVariant(fixture.history, options.variant));
            const tail = prepareInjectItems(applyVariant(fixture.sharedTail, options.variant));
            return {
              id: fixture.id,
              seed: fixture.seed,
              variant: options.variant,
              historyItems: history.length,
              plainHistoryItems: fixture.history.length,
              tailItems: tail.length,
              questions: fixture.questions.length,
              eligibleToolOutputs: eligibleToolOutputCount([...history, ...tail]),
            };
          }),
          null,
          2,
        )}\n`,
      );
    }

    let done = 0;
    let skipped = 0;
    const total = fixtures.length * options.trials * options.arms.length;
    const jobs = fixtures.flatMap((fixture, fixtureIndex) =>
      Array.from({ length: options.trials }, (_, trial) => ({ fixture, fixtureIndex, trial })),
    );
    const runJob = async ({ fixture, fixtureIndex, trial }: (typeof jobs)[number]): Promise<void> => {
      const cellPathFor = (arm: Arm): string =>
        join(
          options.outDir,
          "trials",
          `${fixture.id}-${trial}-${arm}${options.variant === "plain" ? "" : `-${options.variant}`}.json`,
        );
      const runCellForArm = async (arm: Arm): Promise<CellRecord> => {
        const cwd = createEvaluationCwd();
        try {
          return await runCellWithEvidence({
            client,
            driver,
            codexHome,
            fixture,
            trial,
            arm,
            variant: options.variant,
            model,
            cwd,
            dryRun: false,
            nestedTelemetry: options.nestedTelemetry,
          }, options.outDir);
        } finally {
          rmSync(cwd, { recursive: true, force: true });
        }
      };
      const controlPath = cellPathFor("full");
      if (existsSync(controlPath) && !options.force) {
        validateControlRecord(controlPath, fixture, trial);
        skipped += 1;
        console.log(`skip ${fixture.id} trial ${trial} full (already done)`);
      } else {
        console.log(`run  ${fixture.id} trial ${trial} full ...`);
        const record = await runCellForArm("full");
        writeAtomic(controlPath, `${JSON.stringify(record, null, 2)}\n`);
        done += 1;
        const correct = record.evaluation.scores.filter((score) => score.correct).length;
        if (correct !== fixture.questions.length) {
          throw new Error(
            `full control ${fixture.id} trial ${trial} scored ${correct}/${fixture.questions.length}; refusing non-control evaluation`,
          );
        }
        console.log(
          `     ${correct}/${record.evaluation.scores.length} correct, ` +
            `${record.evaluation.firstRequestInputTokens} input tokens, ` +
            `${record.recovery.readArtifactCalls} read_artifact call(s), ` +
            `${Math.round(record.evaluation.latencyMs / 1000)}s`,
        );
      }
      for (const arm of actualArmOrderForCell(fixtureIndex, trial, options.trials, options.arms)) {
        if (arm === "full") continue;
        const cellPath = cellPathFor(arm);
        if (existsSync(cellPath) && !options.force) {
          skipped += 1;
          console.log(`skip ${fixture.id} trial ${trial} ${arm} (already done)`);
          continue;
        }
        console.log(`run  ${fixture.id} trial ${trial} ${arm} ...`);
        const record = await runCellForArm(arm);
        writeAtomic(cellPath, `${JSON.stringify(record, null, 2)}\n`);
        done += 1;
        const correct = record.evaluation.scores.filter((score) => score.correct).length;
        console.log(
          `     ${correct}/${record.evaluation.scores.length} correct, ` +
            `${record.evaluation.firstRequestInputTokens} input tokens, ` +
            `${record.recovery.readArtifactCalls} read_artifact call(s), ` +
            `${Math.round(record.evaluation.latencyMs / 1000)}s`,
        );
      }
    };
    await runBounded(
      jobs.map((job) => () => runJob(job)),
      options.concurrency,
    );
    console.log(`\n${done} cell(s) run, ${skipped} skipped, ${total} total. Results in ${options.outDir}`);
    console.log(`Next: npm run analyze -- ${options.outDir}`);
  } finally {
    process.off("SIGINT", onSigint);
    await client.close();
    rmSync(codexHome, { recursive: true, force: true });
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await main();
