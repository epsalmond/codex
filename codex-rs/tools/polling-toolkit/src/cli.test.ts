import assert from "node:assert/strict";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const CLI = fileURLToPath(new URL("./cli.ts", import.meta.url));

function git(cwd: string, ...args: string[]): string {
  const result = spawnSync("git", args, { cwd, encoding: "utf8" });
  if (result.status !== 0) throw new Error(result.stderr || "git command failed");
  return result.stdout.trim();
}

test("fixture command writes private hash-only metadata and dry-run makes no provider calls", () => {
  const directory = mkdtempSync(join(tmpdir(), "codex-polling-cli-test-"));
  chmodSync(directory, 0o700);
  try {
    const snapshot = join(directory, "snapshot");
    mkdirSync(snapshot, { mode: 0o700 });
    git(snapshot, "init", "--quiet", "--initial-branch=main");
    git(snapshot, "config", "user.name", "Synthetic Benchmark");
    git(snapshot, "config", "user.email", "synthetic@example.invalid");
    writeFileSync(join(snapshot, "README.md"), "Synthetic checkpoint.\n");
    git(snapshot, "add", "README.md");
    git(snapshot, "commit", "--quiet", "--no-gpg-sign", "-m", "synthetic checkpoint");
    const prompt = "Private fixture task wording must never be embedded in the manifest.\n";
    writeFileSync(join(directory, "task.md"), prompt, { mode: 0o600 });
    const acceptance = JSON.stringify({ argv: ["true"], cwd: ".", timeoutMs: 10_000 });
    const create = spawnSync(process.execPath, ["--experimental-strip-types", CLI, "fixture",
      "--directory", directory,
      "--id", "synthetic-local",
      "--snapshot", "snapshot",
      "--prompt", "task.md",
      "--model", "gpt-test-model",
      "--effort", "medium",
      "--acceptance", acceptance,
    ], { encoding: "utf8" });
    assert.equal(create.status, 0, create.stderr);
    const manifestPath = join(directory, "manifest.json");
    assert.equal(statSync(manifestPath).mode & 0o777, 0o600);
    const manifestText = readFileSync(manifestPath, "utf8");
    assert.equal(manifestText.includes(prompt), false);
    const manifest = JSON.parse(manifestText) as { snapshotSha256: string; promptSha256: string };
    assert.match(manifest.snapshotSha256, /^[0-9a-f]{64}$/);
    assert.match(manifest.promptSha256, /^[0-9a-f]{64}$/);

    const dryRun = spawnSync(process.execPath, ["--experimental-strip-types", CLI, "replay", "--manifest", manifestPath, "--dry-run"], {
      encoding: "utf8",
    });
    assert.equal(dryRun.status, 0, dryRun.stderr);
    assert.equal(dryRun.stdout.includes(prompt), false);
    assert.match(dryRun.stdout, /"providerCalls": 0/);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("scan command requires explicit homes and returns an empty local report for an empty home", () => {
  const missing = spawnSync(process.execPath, ["--experimental-strip-types", CLI, "scan"], { encoding: "utf8" });
  assert.notEqual(missing.status, 0);
  assert.match(missing.stderr, /explicit --home/);

  const directory = mkdtempSync(join(tmpdir(), "codex-polling-empty-home-"));
  chmodSync(directory, 0o700);
  try {
    const result = spawnSync(process.execPath, ["--experimental-strip-types", CLI, "scan", "--home", directory], { encoding: "utf8" });
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(JSON.parse(result.stdout).sessions, []);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
