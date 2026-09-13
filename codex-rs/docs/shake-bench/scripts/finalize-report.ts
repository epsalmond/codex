#!/usr/bin/env node

import { copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

type Cell = {
  fixtureId: string;
  trial: number;
  arm: string;
  evaluation: {
    turnStatus: string;
    parseFailed: boolean;
    scores: Array<{ correct?: unknown }>;
    firstRequestInputTokens: number;
    turnInputTokens: number;
    turnOutputTokens: number;
    latencyMs: number;
    usageUpdates?: Array<{
      inputTokens?: number;
      cachedInputTokens?: number;
      cacheWriteInputTokens?: number;
      outputTokens?: number;
    }>;
  };
  preparation: { totalLatencyMs: number };
  recovery: {
    violation: boolean;
    forbiddenToolCalls?: unknown[];
    outerCodeModeContinuations: number;
    readArtifactAttempts: number | null;
    derivedReadArtifactBytes: number | null;
  };
};

type Manifest = {
  expectedCells: Array<{ fixtureId: string; trial: number; arm: string }>;
  variant: string;
  model: string;
};

type Usage = {
  cells: number;
  input: number;
  cached: number;
  cacheWrite: number;
  cacheWriteKnown: boolean;
  output: number;
  firstInput: number;
  outerContinuations: number;
  attempts: number | null;
  evalLatencyMs: number;
  prepLatencyMs: number;
};

function readJson<T>(path: string): T {
  return JSON.parse(readFileSync(path, "utf8")) as T;
}

function finite(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function key(value: { fixtureId: string; trial: number; arm: string }): string {
  return `${value.fixtureId}#${value.trial}#${value.arm}`;
}

function loadRun(runDir: string): { manifest: Manifest; cells: Cell[] } {
  const manifest = readJson<Manifest>(join(runDir, "manifest.json"));
  const trialsDir = join(runDir, "trials");
  if (!Array.isArray(manifest.expectedCells) || !existsSync(trialsDir)) throw new Error(`${runDir}: incomplete run layout`);
  const cells = readdirSync(trialsDir)
    .filter((name) => name.endsWith(".json") && !name.endsWith(".partial"))
    .map((name) => readJson<Cell>(join(trialsDir, name)));
  const expected = new Set(manifest.expectedCells.map(key));
  const actual = new Set(cells.map(key));
  if (cells.length !== expected.size || actual.size !== cells.length || [...expected].some((entry) => !actual.has(entry))) {
    throw new Error(`${runDir}: expected ${expected.size} unique cells, found ${cells.length}`);
  }
  for (const cell of cells) {
    if (cell.evaluation.turnStatus !== "completed") throw new Error(`${runDir}: ${key(cell)} did not complete`);
    if (cell.evaluation.parseFailed) throw new Error(`${runDir}: ${key(cell)} has a parse failure`);
    if (cell.recovery.violation || (cell.recovery.forbiddenToolCalls?.length ?? 0) > 0) {
      throw new Error(`${runDir}: ${key(cell)} has a protocol violation`);
    }
  }
  const controls = cells.filter((cell) => cell.arm === "full");
  if (controls.length !== new Set(controls.map((cell) => `${cell.fixtureId}#${cell.trial}`)).size) {
    throw new Error(`${runDir}: duplicate full controls`);
  }
  for (const cell of controls) {
    if (cell.evaluation.scores.length !== 75 || !cell.evaluation.scores.every((score) => score.correct === true)) {
      throw new Error(`${runDir}: invalid full control ${key(cell)}`);
    }
  }
  return { manifest, cells };
}

function usageByArm(cells: Cell[]): Map<string, Usage> {
  const byArm = new Map<string, Usage>();
  for (const cell of cells) {
    const usage = byArm.get(cell.arm) ?? {
      cells: 0,
      input: 0,
      cached: 0,
      cacheWrite: 0,
      cacheWriteKnown: true,
      output: 0,
      firstInput: 0,
      outerContinuations: 0,
      attempts: 0,
      evalLatencyMs: 0,
      prepLatencyMs: 0,
    };
    usage.cells += 1;
    usage.firstInput += finite(cell.evaluation.firstRequestInputTokens);
    usage.evalLatencyMs += finite(cell.evaluation.latencyMs);
    usage.prepLatencyMs += finite(cell.preparation.totalLatencyMs);
    usage.outerContinuations += finite(cell.recovery.outerCodeModeContinuations);
    if (cell.recovery.readArtifactAttempts === null) usage.attempts = null;
    else if (usage.attempts !== null) usage.attempts += cell.recovery.readArtifactAttempts;
    for (const update of cell.evaluation.usageUpdates ?? []) {
      usage.input += finite(update.inputTokens);
      usage.cached += finite(update.cachedInputTokens);
      usage.output += finite(update.outputTokens);
      if (typeof update.cacheWriteInputTokens === "number" && Number.isFinite(update.cacheWriteInputTokens)) {
        usage.cacheWrite += update.cacheWriteInputTokens;
      } else {
        usage.cacheWriteKnown = false;
      }
    }
    byArm.set(cell.arm, usage);
  }
  return byArm;
}

function writeUsageReport(path: string, model: string, variant: string, usages: Map<string, Usage>): void {
  const lines = [
    `# Evaluation usage: ${variant}`,
    "",
    `Model: \`${model}\``,
    "",
    "Values below are the usage updates recorded by the connected runtime. They are not API billing data; reported cache writes of zero do not prove billing behavior.",
    "",
    "| Arm | Cells | Input tokens | Cached input | Noncached input | Reported cache writes | Output tokens | Mean first input | Mean outer continuations | Mean eval latency (s) | Mean prep latency (s) |",
    "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
  ];
  for (const [arm, usage] of [...usages.entries()].sort()) {
    const mean = (value: number): string => (usage.cells ? (value / usage.cells).toFixed(1) : "n/a");
    lines.push(
      `| ${arm} | ${usage.cells} | ${Math.round(usage.input).toLocaleString("en-US")} | ${Math.round(usage.cached).toLocaleString("en-US")} | ` +
        `${Math.round(usage.input - usage.cached).toLocaleString("en-US")} | ${usage.cacheWriteKnown ? Math.round(usage.cacheWrite).toLocaleString("en-US") : "n/a"} | ` +
        `${Math.round(usage.output).toLocaleString("en-US")} | ${mean(usage.firstInput)} | ${mean(usage.outerContinuations)} | ` +
        `${mean(usage.evalLatencyMs / 1000)} | ${mean(usage.prepLatencyMs / 1000)} |`,
    );
  }
  lines.push("", "Nested attempt totals are reported when telemetry is available. An outer code-mode continuation may contain multiple nested attempts.");
  writeFileSync(path, `${lines.join("\n")}\n`);
}

function finalize(runDir: string, destination: string): void {
  const { manifest, cells } = loadRun(runDir);
  mkdirSync(destination, { recursive: true });
  for (const name of ["manifest.json", "summary.json", "scores.csv", "GENERATED_RESULTS.md"]) {
    const source = join(runDir, name);
    if (!existsSync(source)) throw new Error(`${runDir}: missing ${name}; run analyze first`);
    const target = join(destination, name);
    if (name === "GENERATED_RESULTS.md") {
      const report = readFileSync(source, "utf8").replace(
        /Nested recovery telemetry is unavailable for (\d+) cell\(s\); nested attempt counts and derived page bytes are shown as n\/a, and noread compliance is unverified for those cells\./,
        "Nested recovery telemetry is unavailable for $1 cell(s); nested attempt counts and derived page bytes are shown as n/a, and nested-tool identity/exclusivity is unverified for those wrapper cells.",
      );
      writeFileSync(target, report);
    } else {
      copyFileSync(source, target);
    }
  }
  writeUsageReport(join(destination, "CACHE_USAGE.md"), manifest.model, manifest.variant, usageByArm(cells));
}

const [plainDir = "results/full-plain-2026-09-11", echoDir = "results/full-echo-2026-09-11", output = "reports/2026-09-11"] = process.argv.slice(2);
finalize(resolve(plainDir), join(resolve(output), "plain"));
finalize(resolve(echoDir), join(resolve(output), "echo"));
console.log(`wrote curated reports to ${resolve(output)}`);
