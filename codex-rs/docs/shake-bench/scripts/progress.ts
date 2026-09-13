#!/usr/bin/env node

import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

type Score = { correct?: unknown };
type Cell = {
  fixtureId?: unknown;
  trial?: unknown;
  arm?: unknown;
  evaluation?: {
    turnStatus?: unknown;
    parseFailed?: unknown;
    scores?: unknown;
    turnInputTokens?: unknown;
    turnOutputTokens?: unknown;
    forbiddenToolCalls?: unknown;
  };
  recovery?: {
    violation?: unknown;
    forbiddenToolCalls?: unknown;
  };
};

type Manifest = {
  expectedCells?: unknown;
  fixtureCount?: unknown;
  trials?: unknown;
  arms?: unknown;
};

function object(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : undefined;
}

function finiteNumber(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function loadJson(path: string): unknown {
  return JSON.parse(readFileSync(path, "utf8"));
}

function expectedCount(manifest: Manifest): number | null {
  if (Array.isArray(manifest.expectedCells)) return manifest.expectedCells.length;
  const fixtures = finiteNumber(manifest.fixtureCount);
  const trials = finiteNumber(manifest.trials);
  const arms = Array.isArray(manifest.arms) ? manifest.arms.length : 0;
  return fixtures > 0 && trials > 0 && arms > 0 ? fixtures * trials * arms : null;
}

function percent(correct: number, total: number): string {
  return total === 0 ? "n/a" : `${((correct / total) * 100).toFixed(1)}%`;
}

function summarize(runDir: string): string[] {
  const manifestPath = join(runDir, "manifest.json");
  const trialsDir = join(runDir, "trials");
  const manifest = existsSync(manifestPath) ? (object(loadJson(manifestPath)) as Manifest | undefined) : undefined;
  if (!manifest) return [`${runDir}`, "  manifest: missing or invalid"];
  if (!existsSync(trialsDir)) return [`${runDir}`, `  cells: 0/${expectedCount(manifest) ?? "?"}; trials directory missing`];

  const cells: Cell[] = [];
  let malformed = 0;
  for (const name of readdirSync(trialsDir).filter((entry) => entry.endsWith(".json") && !entry.endsWith(".partial"))) {
    try {
      const value = object(loadJson(join(trialsDir, name)));
      if (!value) malformed++;
      else cells.push(value as Cell);
    } catch {
      malformed++;
    }
  }

  const failed = cells.filter((cell) => cell.evaluation?.turnStatus !== "completed").length;
  const parseFailed = cells.filter((cell) => cell.evaluation?.parseFailed === true).length;
  const protocolViolations = cells.filter((cell) => {
    const recovery = cell.recovery;
    const evaluation = cell.evaluation;
    return recovery?.violation === true ||
      (Array.isArray(recovery?.forbiddenToolCalls) && recovery.forbiddenToolCalls.length > 0) ||
      (Array.isArray(evaluation?.forbiddenToolCalls) && evaluation.forbiddenToolCalls.length > 0);
  }).length;
  const controls = cells.filter((cell) => cell.arm === "full");
  const validControls = controls.filter((cell) => {
    const scores = Array.isArray(cell.evaluation?.scores) ? cell.evaluation!.scores as Score[] : [];
    return cell.evaluation?.turnStatus === "completed" &&
      cell.evaluation?.parseFailed === false &&
      scores.length === 75 &&
      scores.every((score) => score.correct === true);
  }).length;
  const arms = new Set<string>([
    ...(Array.isArray(manifest.arms) ? manifest.arms.filter((arm): arm is string => typeof arm === "string") : []),
    ...cells.map((cell) => typeof cell.arm === "string" ? cell.arm : "unknown"),
  ]);
  const accuracy = [...arms].sort().map((arm) => {
    const scores = cells
      .filter((cell) => cell.arm === arm)
      .flatMap((cell) => Array.isArray(cell.evaluation?.scores) ? cell.evaluation!.scores as Score[] : []);
    const correct = scores.filter((score) => score.correct === true).length;
    return `${arm}=${correct}/${scores.length} ${percent(correct, scores.length)}`;
  });
  const inputTokens = cells.reduce((sum, cell) => sum + finiteNumber(cell.evaluation?.turnInputTokens), 0);
  const outputTokens = cells.reduce((sum, cell) => sum + finiteNumber(cell.evaluation?.turnOutputTokens), 0);
  return [
    runDir,
    `  cells: ${cells.length}/${expectedCount(manifest) ?? "?"}; full controls: ${validControls}/${controls.length}; ` +
      `failed: ${failed}; parse: ${parseFailed}; protocol: ${protocolViolations}; malformed: ${malformed}`,
    `  accuracy: ${accuracy.join("; ") || "n/a"}`,
    `  evaluation tokens: input=${inputTokens.toLocaleString("en-US")}; output=${outputTokens.toLocaleString("en-US")}`,
  ];
}

const runDirs = process.argv.slice(2).map((runDir) => resolve(runDir));
if (runDirs.length === 0) throw new Error("Usage: progress.ts <results-dir> [...]");
for (const runDir of runDirs) console.log(summarize(runDir).join("\n"));
