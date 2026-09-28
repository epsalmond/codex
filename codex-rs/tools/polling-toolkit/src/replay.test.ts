import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import { AppServerClient } from "./appserver.ts";
import {
  benchmarkValidity,
  buildReplayConfig,
  driveTask,
  fingerprintSnapshot,
  loadFixture,
  materializeSnapshot,
  pairedComparison,
  parseReplayArgs,
  safeMeasurement,
  type ReplayFixture,
} from "./replay.ts";
import { scanCodexHomes, type PollingScanResult } from "./scanner.ts";

const LIMITS: ReplayFixture["limits"] = {
  maxInputTokens: 5_000_000,
  maxOutputTokens: 250_000,
  maxModelRequests: 400,
  maxRootTurns: 60,
  maxChildren: 8,
  maxWallMs: 2_700_000,
  idleMs: 240_000,
};

function sha256(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}

function git(cwd: string, ...args: string[]): string {
  const result = spawnSync("git", args, { cwd, encoding: "utf8" });
  if (result.status !== 0) throw new Error(result.stderr || "git command failed");
  return result.stdout.trim();
}

async function withTemp<T>(action: (directory: string) => Promise<T> | T): Promise<T> {
  const directory = mkdtempSync(join(tmpdir(), "codex-polling-replay-test-"));
  chmodSync(directory, 0o700);
  try {
    return await action(directory);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

function makeCheckpoint(directory: string): { snapshot: string; promptPath: string; fixture: ReplayFixture } {
  const snapshot = join(directory, "snapshot");
  mkdirSync(snapshot, { mode: 0o700 });
  git(snapshot, "init", "--quiet", "--initial-branch=main");
  git(snapshot, "config", "user.name", "Synthetic Benchmark");
  git(snapshot, "config", "user.email", "synthetic@example.invalid");
  writeFileSync(join(snapshot, "README.md"), "Synthetic one-commit benchmark checkpoint.\n");
  writeFileSync(join(snapshot, ".gitignore"), "target/\n");
  git(snapshot, "add", "README.md", ".gitignore");
  git(snapshot, "commit", "--quiet", "--no-gpg-sign", "-m", "synthetic checkpoint");
  const promptPath = join(directory, "task.md");
  const prompt = "Solve the synthetic task and finish with BENCHMARK_COMPLETE.\n";
  writeFileSync(promptPath, prompt, { mode: 0o600 });
  const commit = git(snapshot, "rev-parse", "HEAD");
  const tree = git(snapshot, "rev-parse", "HEAD^{tree}");
  const fixture: ReplayFixture = {
    schemaVersion: 1,
    id: "synthetic-example",
    snapshotCommit: commit,
    snapshotTree: tree,
    snapshotSha256: sha256(tree),
    snapshot: "snapshot",
    prompt: "task.md",
    promptSha256: sha256(prompt),
    model: "gpt-test-model",
    effort: "medium",
    acceptance: { argv: ["true"], cwd: ".", timeoutMs: 10_000 },
    limits: LIMITS,
  };
  return { snapshot, promptPath, fixture };
}

test("paired configs differ only in agent_polling", () => {
  const fixture = {
    model: "gpt-test-model",
    effort: "medium",
  } as ReplayFixture;
  const polling = buildReplayConfig(fixture, "enabled").replace('agent_polling = "enabled"', 'agent_polling = "MODE"');
  const wake = buildReplayConfig(fixture, "disabled").replace('agent_polling = "disabled"', 'agent_polling = "MODE"');
  assert.equal(polling, wake);
  assert.match(buildReplayConfig(fixture, "enabled"), /agent_polling = "enabled"/);
  assert.match(buildReplayConfig(fixture, "disabled"), /agent_polling = "disabled"/);
});

test("loadFixture accepts an isolated one-commit checkpoint and rejects dirty files", async () => {
  await withTemp((directory) => {
    const { fixture } = makeCheckpoint(directory);
    const manifestPath = join(directory, "manifest.json");
    writeFileSync(manifestPath, JSON.stringify(fixture));
    assert.equal(loadFixture(manifestPath).fixture.id, "synthetic-example");
    writeFileSync(join(directory, "snapshot", "untracked.txt"), "synthetic untracked data");
    assert.throws(() => loadFixture(manifestPath), /working tree must be clean/);
  });
});

test("materializeSnapshot excludes ignored and untracked checkout files and removes clone history links", async () => {
  await withTemp((directory) => {
    const { snapshot } = makeCheckpoint(directory);
    writeFileSync(join(snapshot, "untracked-private-note.txt"), "not committed");
    mkdirSync(join(snapshot, "target"));
    writeFileSync(join(snapshot, "target", "generated-answer.txt"), "ignored file");
    const destination = join(directory, "replay-workspace");
    materializeSnapshot(snapshot, destination);
    assert.equal(existsSync(join(destination, "untracked-private-note.txt")), false);
    assert.equal(existsSync(join(destination, "target", "generated-answer.txt")), false);
    assert.equal(git(destination, "rev-list", "--all", "--count"), "1");
    assert.equal(git(destination, "remote"), "");
    assert.equal(git(destination, "tag", "--list"), "");
    assert.equal(git(destination, "status", "--porcelain", "--untracked-files=all"), "");
  });
});

test("snapshot fingerprint follows committed Git tree, not ambient ignored files", async () => {
  await withTemp((directory) => {
    const { snapshot } = makeCheckpoint(directory);
    const before = fingerprintSnapshot(snapshot);
    writeFileSync(join(snapshot, "ignored-but-untracked.txt"), "must not change checkpoint fingerprint");
    assert.equal(fingerprintSnapshot(snapshot), before);
  });
});

test("isolation check does not require a replay manifest", () => {
  const options = parseReplayArgs(["--isolation-check", "--bin", "/tmp/codex"]);
  assert.equal(options.mode, "isolation-check");
  assert.equal(options.manifest, "");
});

test("only a complete accepted run with usage and a workspace change is valid", () => {
  assert.equal(benchmarkValidity("complete", true, true, true), true);
  assert.equal(benchmarkValidity("incomplete", true, true, true), false);
  assert.equal(benchmarkValidity("complete", false, true, true), false);
  assert.equal(benchmarkValidity("complete", true, false, true), false);
  assert.equal(benchmarkValidity("complete", true, true, false), false);
});

test("requires complete root and descendant rollout coverage before accepting a trace", () => {
  const rootId = "synthetic-root";
  const childId = "synthetic-child";
  const usage = (inputTokens: number, responseCount = 1) => ({
    inputTokens,
    cachedInputTokens: 0,
    uncachedInputTokens: inputTokens,
    outputTokens: 2,
    responseCount,
  });
  const categories = (category: string, totals: ReturnType<typeof usage>) => ({
    pure_wait_agent: category === "pure_wait_agent" ? totals : usage(0, 0),
    wait_containing_mixed_calls: category === "wait_containing_mixed_calls" ? totals : usage(0, 0),
    pure_write_stdin: category === "pure_write_stdin" ? totals : usage(0, 0),
    other: category === "other" ? totals : usage(0, 0),
    unknown_no_attribution: category === "unknown_no_attribution" ? totals : usage(0, 0),
  });
  const thread = (id: string, parentThreadId: string | null, category: string, totals: ReturnType<typeof usage>) => ({
    threadId: id,
    parentThreadId,
    sourceRolloutPaths: [],
    totals,
    byCategory: categories(category, totals),
  });
  const scan = (children: ReturnType<typeof thread>[], skippedRolloutCount = 0, rootCategory = "pure_wait_agent") => {
    const root = thread(rootId, null, rootCategory, usage(10));
    const all = [root, ...children];
    const byCategory = categories("none", usage(0, 0));
    for (const item of all) {
      for (const key of Object.keys(byCategory) as Array<keyof typeof byCategory>) {
        for (const field of Object.keys(item.byCategory[key]) as Array<keyof ReturnType<typeof usage>>) {
          byCategory[key][field] += item.byCategory[key][field];
        }
      }
    }
    return {
      sessions: [{
        sessionId: rootId,
        root,
        descendants: children,
        usageTraceAvailable: all.some((entry) => entry.totals.responseCount > 0),
        totals: all.reduce((total, entry) => ({
          inputTokens: total.inputTokens + entry.totals.inputTokens,
          cachedInputTokens: total.cachedInputTokens + entry.totals.cachedInputTokens,
          uncachedInputTokens: total.uncachedInputTokens + entry.totals.uncachedInputTokens,
          outputTokens: total.outputTokens + entry.totals.outputTokens,
          responseCount: total.responseCount + entry.totals.responseCount,
        }), usage(0, 0)),
        byCategory,
        waitAttributedInputTokens: 10,
        writeStdinAttributedInputTokens: 0,
        pollingCandidateInputTokens: 10,
        actionAttributionComplete: true,
      }],
      totals: usage(0, 0),
      byCategory,
      scannedRolloutCount: all.length,
      skippedRolloutCount,
    } as unknown as PollingScanResult;
  };
  const expected = (childThreadIds: string[], childInputTokens = 20) => ({
    rootThreadId: rootId,
    childThreads: childThreadIds.length,
    childThreadIds,
    threadUsageById: {
      [rootId]: { ...usage(10), modelRequests: 1 },
      ...(childThreadIds.length ? { [childId]: { ...usage(childInputTokens), modelRequests: 1 } } : {}),
    },
  });

  const complete = safeMeasurement(scan([thread(childId, rootId, "pure_wait_agent", usage(20))]), expected([childId]));
  assert.equal(complete?.coverage.complete, true);
  assert.equal(complete?.coverage.completeThreadCount, 2);

  const wrongRuntimeCount = safeMeasurement(
    scan([thread(childId, rootId, "pure_wait_agent", usage(20))]),
    { ...expected([childId]), childThreads: 2 },
  );
  assert.equal(wrongRuntimeCount?.coverage.complete, false);
  assert.equal(wrongRuntimeCount?.coverage.runtimeThreadCountMismatch, true);

  const missing = safeMeasurement(scan([]), expected([childId]));
  assert.equal(missing?.coverage.complete, false);
  assert.equal(missing?.coverage.missingThreadCount, 1);

  const malformed = safeMeasurement(scan([], 1), expected([childId]));
  assert.equal(malformed?.coverage.complete, false);
  assert.equal(malformed?.coverage.skippedRolloutCount, 1);

  const zeroUsage = safeMeasurement(scan([thread(childId, rootId, "other", usage(0, 0))]), expected([childId]));
  assert.equal(zeroUsage?.coverage.complete, false);
  assert.equal(zeroUsage?.coverage.noUsageThreadCount, 1);

  const unknown = safeMeasurement(
    scan([thread(childId, rootId, "unknown_no_attribution", usage(20))]),
    expected([childId]),
  );
  assert.equal(unknown?.coverage.complete, false);
  assert.equal(unknown?.coverage.unattributedResponseCount, 1);

  const mixedRoot = safeMeasurement(
    scan([thread(childId, rootId, "pure_wait_agent", usage(20))], 0, "wait_containing_mixed_calls"),
    expected([childId]),
  );
  assert.equal(mixedRoot?.coverage.complete, false);
  assert.equal(mixedRoot?.coverage.mixedActionResponseCount, 1);

  const mixedChild = safeMeasurement(
    scan([thread(childId, rootId, "wait_containing_mixed_calls", usage(20))]),
    expected([childId]),
  );
  assert.equal(mixedChild?.coverage.complete, false);
  assert.equal(mixedChild?.coverage.mixedActionResponseCount, 1);
});

test("scanner-skipped malformed child rollout invalidates measured coverage", async () => {
  await withTemp(async (directory) => {
    const home = join(directory, "codex-home");
    const rootId = "synthetic-root";
    const childId = "synthetic-child";
    const rootRollout = join(home, "sessions", "rollout-root.jsonl");
    const childRollout = join(home, "sessions", "rollout-child.jsonl");
    mkdirSync(join(home, "sessions"), { recursive: true });
    const metadata = (id: string, sessionId: string, parentThreadId?: string) => ({
      type: "session_meta",
      payload: { id, session_id: sessionId, ...(parentThreadId ? { parent_thread_id: parentThreadId } : {}) },
    });
    const usageRecord = (threadId: string, responseId: string, inputTokens: number) => ({
      type: "token_usage_record",
      payload: {
        thread_id: threadId,
        response_id: responseId,
        usage: { input_tokens: inputTokens, cached_input_tokens: 0, output_tokens: 2 },
      },
    });
    writeFileSync(rootRollout, [
      metadata(rootId, rootId),
      { type: "response_item", payload: { type: "function_call", name: "wait_agent" } },
      usageRecord(rootId, "root-response", 10),
    ].map((row) => JSON.stringify(row)).join("\n") + "\n");
    writeFileSync(childRollout, "malformed child rollout\n");

    const scan = await scanCodexHomes([home]);
    const measurement = safeMeasurement(scan, {
      rootThreadId: rootId,
      childThreads: 1,
      childThreadIds: [childId],
      threadUsageById: {
        [rootId]: { inputTokens: 10, cachedInputTokens: 0, uncachedInputTokens: 10, outputTokens: 2, modelRequests: 1 },
        [childId]: { inputTokens: 5, cachedInputTokens: 0, uncachedInputTokens: 5, outputTokens: 2, modelRequests: 1 },
      },
    });
    assert.equal(scan.skippedRolloutCount, 1);
    assert.equal(measurement?.coverage.complete, false);
    assert.equal(measurement?.coverage.missingThreadCount, 1);
    assert.equal(measurement?.coverage.skippedRolloutCount, 1);
  });
});

test("paired comparison reports root and descendant reductions separately", () => {
  const totals = (inputTokens: number, cachedInputTokens: number, outputTokens: number, responseCount: number) => ({
    inputTokens,
    cachedInputTokens,
    uncachedInputTokens: inputTokens - cachedInputTokens,
    outputTokens,
    responseCount,
  });
  const categories = (waitInput: number, stdinInput: number) => ({
    pure_wait_agent: totals(waitInput, 0, 0, 1),
    wait_containing_mixed_calls: totals(0, 0, 0, 0),
    pure_write_stdin: totals(stdinInput, 0, 0, 1),
    other: totals(0, 0, 0, 0),
    unknown_no_attribution: totals(0, 0, 0, 0),
  });
  const measurement = (rootWait: number, childWait: number) => ({
    totals: totals(rootWait + childWait, 0, 0, 2),
    byCategory: categories(rootWait + childWait, 0),
    root: { totals: totals(rootWait, 0, 0, 1), byCategory: categories(rootWait, 0) },
    descendants: [{ totals: totals(childWait, 0, 0, 1), byCategory: categories(childWait, 0) }],
    coverage: {
      complete: true,
      expectedThreadCount: 2,
      observedThreadCount: 2,
      expectedDescendantCount: 1,
      observedDescendantCount: 1,
      runtimeThreadCountMismatch: false,
      completeThreadCount: 2,
      missingThreadCount: 0,
      unexpectedThreadCount: 0,
      responseCountMismatchThreadCount: 0,
      tokenTotalsMismatchThreadCount: 0,
      noUsageThreadCount: 0,
      mixedActionResponseCount: 0,
      unattributedResponseCount: 0,
      skippedRolloutCount: 0,
    },
  });
  const result = (mode: "polling" | "wake", attribution: ReturnType<typeof measurement>) => ({
    mode,
    status: "complete",
    benchmarkValid: true,
    metrics: { inputTokens: attribution.totals.inputTokens, cachedInputTokens: 0, uncachedInputTokens: attribution.totals.inputTokens, outputTokens: 0, modelRequests: 2 },
    pollingAttribution: attribution,
  });
  const comparison = pairedComparison([result("polling", measurement(100, 80)), result("wake", measurement(20, 75))]);
  assert.ok(comparison);
  assert.equal(comparison.validPair, true);
  const root = comparison.root as { totals: { inputTokens: { polling: number; wake: number; pollingMinusWake: number } } };
  const descendants = comparison.descendants as { totals: { inputTokens: { polling: number; wake: number; pollingMinusWake: number } } };
  assert.deepEqual(root.totals.inputTokens, { polling: 100, wake: 20, pollingMinusWake: 80 });
  assert.deepEqual(descendants.totals.inputTokens, { polling: 80, wake: 75, pollingMinusWake: 5 });
});

test("persisted measurements strip local session IDs and rollout paths", () => {
  const totals = { inputTokens: 1, cachedInputTokens: 0, uncachedInputTokens: 1, outputTokens: 0, responseCount: 1 };
  const categories = {
    pure_wait_agent: totals,
    wait_containing_mixed_calls: { inputTokens: 0, cachedInputTokens: 0, uncachedInputTokens: 0, outputTokens: 0, responseCount: 0 },
    pure_write_stdin: { inputTokens: 0, cachedInputTokens: 0, uncachedInputTokens: 0, outputTokens: 0, responseCount: 0 },
    other: { inputTokens: 0, cachedInputTokens: 0, uncachedInputTokens: 0, outputTokens: 0, responseCount: 0 },
    unknown_no_attribution: { inputTokens: 0, cachedInputTokens: 0, uncachedInputTokens: 0, outputTokens: 0, responseCount: 0 },
  };
  const privateId = "synthetic-session-id-to-strip";
  const privatePath = "/private/local/rollout-synthetic.jsonl";
  const scan = {
    sessions: [{
      sessionId: privateId,
      root: { threadId: privateId, parentThreadId: null, sourceRolloutPaths: [privatePath], totals, byCategory: categories },
      descendants: [],
      totals,
      byCategory: categories,
      waitAttributedInputTokens: 1,
      writeStdinAttributedInputTokens: 0,
      pollingCandidateInputTokens: 1,
    }],
    totals,
    byCategory: categories,
    scannedRolloutCount: 1,
    skippedRolloutCount: 0,
  } as unknown as PollingScanResult;
  const measurement = safeMeasurement(scan, {
    rootThreadId: privateId,
    childThreads: 0,
    childThreadIds: [],
    threadUsageById: {
      [privateId]: { inputTokens: 1, cachedInputTokens: 0, uncachedInputTokens: 1, outputTokens: 0, modelRequests: 1 },
    },
  });
  assert.ok(measurement);
  const persisted = JSON.stringify(measurement);
  assert.equal(persisted.includes(privateId), false);
  assert.equal(persisted.includes(privatePath), false);
});

test("app-server task monitor waits for child completion and counts all threads without a provider", async () => {
  await withTemp(async (directory) => {
    const codexHome = join(directory, "codex-home");
    mkdirSync(codexHome, { mode: 0o700 });
    writeFileSync(join(codexHome, "config.toml"), "synthetic mock config\n");
    const client = AppServerClient.spawn({
      bin: process.execPath,
      args: [fileURLToPath(new URL("./mock-app-server.mjs", import.meta.url))],
      codexHome,
      shutdownTimeoutMs: 1_000,
    });
    const fixture = {
      model: "gpt-test-model",
      effort: "medium",
      limits: LIMITS,
    } as ReplayFixture;
    try {
      await client.initialize({ name: "test", title: "test", version: "0" });
      const outcome = await driveTask(client, fixture, "/workspace", "synthetic task");
      assert.equal(outcome.status, "complete");
      assert.equal(outcome.childThreads, 1);
      assert.equal(outcome.terminalChildren, 1);
      assert.equal(outcome.rootTurns, 2);
      assert.equal(outcome.metrics.modelRequests, 3);
      assert.equal(outcome.metrics.inputTokens, 220);
      assert.equal(outcome.metrics.outputTokens, 9);
    } finally {
      await client.close();
    }
  });
});
