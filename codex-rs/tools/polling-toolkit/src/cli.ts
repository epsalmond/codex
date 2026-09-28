#!/usr/bin/env node
import { createHash } from "node:crypto";
import { chmodSync, existsSync, readFileSync, realpathSync, statSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { isAbsolute, relative, resolve, sep } from "node:path";
import { pathToFileURL } from "node:url";
import { runReplay } from "./replay.ts";
import { scanCodexHomes } from "./scanner.ts";

const DEFAULT_LIMITS = {
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

function fail(message: string): never {
  throw new Error(message);
}

function parsePairs(args: string[]): Map<string, string[]> {
  const result = new Map<string, string[]>();
  for (let index = 0; index < args.length; index += 1) {
    const key = args[index];
    const value = args[index + 1];
    if (!key?.startsWith("--") || !value || value.startsWith("--")) fail("expected --option value pairs");
    const values = result.get(key) ?? [];
    values.push(value);
    result.set(key, values);
    index += 1;
  }
  return result;
}

function one(options: Map<string, string[]>, name: string, required = true): string | undefined {
  const values = options.get(name) ?? [];
  if (values.length > 1) fail(`${name} may be specified once`);
  if (required && !values[0]) fail(`${name} is required`);
  return values[0];
}

function isInside(root: string, path: string): boolean {
  const rel = relative(root, path);
  return rel === "" || (rel !== ".." && !rel.startsWith(".." + sep) && !isAbsolute(rel));
}

function git(cwd: string, args: string[]): string {
  const result = spawnSync("git", args, { cwd, encoding: "utf8", timeout: 20_000 });
  if (result.status !== 0) fail("checkpoint Git validation failed");
  return result.stdout.trim();
}

function createFixture(args: string[]): void {
  const options = parsePairs(args);
  const directory = realpathSync(one(options, "--directory")!);
  if ((statSync(directory).mode & 0o077) !== 0) fail("fixture directory must be private (mode 0700); restrict it before continuing");
  const id = one(options, "--id")!;
  if (!/^[a-z0-9][a-z0-9._-]{0,63}$/.test(id)) fail("--id must be a generic lowercase label with letters, digits, dots, dashes, or underscores");
  const model = one(options, "--model")!;
  if (!/^gpt-[a-z0-9.-]+$/.test(model)) fail("--model must be an exact gpt-* model slug");
  const effort = one(options, "--effort")!;
  const snapshotValue = one(options, "--snapshot")!;
  const promptValue = one(options, "--prompt")!;
  for (const [label, value] of [["--snapshot", snapshotValue], ["--prompt", promptValue]] as const) {
    if (isAbsolute(value)) fail(`${label} must be relative to --directory`);
  }
  const snapshot = realpathSync(resolve(directory, snapshotValue));
  const promptPath = realpathSync(resolve(directory, promptValue));
  if (!isInside(directory, snapshot) || !isInside(directory, promptPath)) fail("snapshot and prompt must be inside --directory");
  if (!statSync(snapshot).isDirectory() || !statSync(promptPath).isFile()) fail("snapshot or prompt is not the expected file type");
  if (!existsSync(resolve(snapshot, ".git"))) fail("snapshot must be a Git checkout");
  if (git(snapshot, ["status", "--porcelain", "--untracked-files=all"])) fail("snapshot working tree must be clean");
  if (git(snapshot, ["rev-list", "--all", "--count"]) !== "1") fail("snapshot must expose exactly one commit");
  if (git(snapshot, ["remote"]) || git(snapshot, ["tag", "--list"])) fail("snapshot must have no remotes or tags");
  const fsck = spawnSync("git", ["fsck", "--full", "--no-reflogs", "--unreachable", "--no-progress"], {
    cwd: snapshot, encoding: "utf8", timeout: 20_000,
  });
  if (fsck.status !== 0 || fsck.stdout.trim()) fail("snapshot has invalid or unreachable Git objects");

  const acceptanceText = one(options, "--acceptance")!;
  const acceptance = JSON.parse(acceptanceText) as { argv?: unknown; cwd?: unknown; timeoutMs?: unknown };
  if (!Array.isArray(acceptance.argv) || acceptance.argv.length === 0 || acceptance.argv.some((value) => typeof value !== "string" || !value)) {
    fail("--acceptance must contain a non-empty argv array");
  }
  if (typeof acceptance.cwd !== "string" || !acceptance.cwd || isAbsolute(acceptance.cwd)) fail("acceptance cwd must be relative to the checkpoint");
  const acceptanceCwd = resolve(snapshot, acceptance.cwd);
  if (!isInside(snapshot, acceptanceCwd)) fail("acceptance cwd must stay inside the checkpoint");
  if (!Number.isInteger(acceptance.timeoutMs) || Number(acceptance.timeoutMs) <= 0 || Number(acceptance.timeoutMs) > 1_800_000) {
    fail("acceptance timeout must be between 1 and 1800000 milliseconds");
  }
  const limitsText = one(options, "--limits", false);
  const limits = limitsText ? { ...DEFAULT_LIMITS, ...JSON.parse(limitsText) } : DEFAULT_LIMITS;
  for (const [key, maximum] of Object.entries(DEFAULT_LIMITS)) {
    const value = limits[key as keyof typeof DEFAULT_LIMITS];
    if (!Number.isFinite(value) || value <= 0 || value > maximum) fail(`limit ${key} must be positive and no greater than ${maximum}`);
  }
  const promptText = readFileSync(promptPath, "utf8");
  if (/rollout[-_].*\.jsonl|token_usage_record|response_item/i.test(promptText)) fail("task prompt appears to contain transcript material");
  const commit = git(snapshot, ["rev-parse", "HEAD"]);
  const tree = git(snapshot, ["rev-parse", "HEAD^{tree}"]);
  const promptRelative = relative(directory, promptPath);
  const snapshotRelative = relative(directory, snapshot);
  const manifest = {
    schemaVersion: 1,
    id,
    snapshotCommit: commit,
    snapshotTree: tree,
    snapshotSha256: sha256(tree),
    snapshot: snapshotRelative,
    prompt: promptRelative,
    promptSha256: sha256(promptText),
    model,
    effort,
    acceptance,
    limits,
  };
  const manifestPath = resolve(directory, "manifest.json");
  writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + "\n", { encoding: "utf8", mode: 0o600, flag: "wx" });
  chmodSync(manifestPath, 0o600);
  process.stdout.write(JSON.stringify({ manifest: manifestPath, checkpointCommit: commit, checkpointTree: tree, promptSha256: manifest.promptSha256 }, null, 2) + "\n");
}

async function main(argv: string[]): Promise<void> {
  const [command, ...args] = argv;
  if (!command || command === "--help" || command === "help") {
    process.stdout.write([
      "Codex polling toolkit",
      "  scan --home <codex-home> [--home <another-codex-home>]",
      "  fixture --directory <private-dir> --id <label> --snapshot <relative-path> --prompt <relative-path>",
      "          --model <model-slug> --effort <effort> --acceptance <json> [--limits <json>]",
      "  replay --manifest <manifest.json> [--dry-run | --probe-config | --isolation-check | --live] ...",
      "",
      "Scanning is local and read-only. Replay never replays transcripts; --live is the only provider-backed command.",
    ].join("\n") + "\n");
    return;
  }
  if (command === "scan") {
    const options = parsePairs(args);
    const homes = options.get("--home") ?? [];
    if (homes.length === 0) fail("provide one or more explicit --home paths; the scanner does not infer Codex homes");
    const result = await scanCodexHomes(homes.map((home) => realpathSync(home)));
    process.stdout.write(JSON.stringify(result, null, 2) + "\n");
    return;
  }
  if (command === "fixture") {
    createFixture(args);
    return;
  }
  if (command === "replay") {
    await runReplay(args);
    return;
  }
  fail(`unknown command: ${command}`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main(process.argv.slice(2)).catch((error: unknown) => {
    process.stderr.write((error instanceof Error ? error.message : "polling toolkit failed") + "\n");
    process.exitCode = 1;
  });
}
