#!/usr/bin/env node
/**
 * Bounded app-server continuation replay for root-agent polling versus wake
 * mode. A replay starts from a task and source snapshot, never recorded model
 * messages.
 */
import { createHash, randomUUID } from "node:crypto";
import {
  accessSync, chmodSync, copyFileSync, cpSync, createReadStream, existsSync, lstatSync, mkdirSync, mkdtempSync,
  openSync, closeSync, readSync,
  readFileSync, readlinkSync, realpathSync, renameSync, rmSync, statSync,
  writeFileSync, symlinkSync,
} from "node:fs";
import { constants as fsConstants } from "node:fs";
import { spawnSync } from "node:child_process";
import { homedir, tmpdir } from "node:os";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { AppServerClient, type Notification } from "./appserver.ts";
import { scanCodexHomes, type CategoryTotals, type PollingActionCategory, type PollingScanResult, type TokenTotals } from "./scanner.ts";

const CLIENT_INFO = { name: "codex_polling_toolkit", title: "Codex polling toolkit", version: "0.1.0" };
const TOOLKIT_VERSION = "0.1.0";
const COMPLETE_MARKER = "BENCHMARK_COMPLETE";
const AUTH_CANARY = "polling-toolkit-must-not-read";
const POSITIVE_MARKER = "POLLING-TOOLKIT-WORKSPACE-READ-CONTROL";
const OUTPUT_ROOT = resolve(process.env.XDG_STATE_HOME ?? join(homedir(), ".local/state"), "codex-polling-toolkit/runs");
const BWRAP = "/usr/bin/bwrap";
const ISOLATED_CODEX_HOME = "/tmp/codex-home";
const ISOLATED_WORKSPACE = "/workspace";
const ISOLATED_CARGO_HOME = "/cargo-home";
export type PollingMode = "enabled" | "disabled";
export type ArmName = "polling" | "wake";
const MODES: Record<ArmName, PollingMode> = { polling: "enabled", wake: "disabled" };

export type ReplayFixture = {
  schemaVersion: 1;
  id: string;
  snapshotCommit: string;
  snapshotTree: string;
  snapshotSha256: string;
  snapshot: string;
  prompt: string;
  promptSha256: string;
  model: string;
  effort: string;
  acceptance: { argv: string[]; cwd: string; timeoutMs: number };
  limits: {
    maxInputTokens: number;
    maxOutputTokens: number;
    maxModelRequests: number;
    maxRootTurns: number;
    maxChildren: number;
    maxWallMs: number;
    idleMs: number;
  };
};

export type Metrics = Pick<TokenTotals, "inputTokens" | "cachedInputTokens" | "uncachedInputTokens" | "outputTokens"> & {
  modelRequests: number;
};

export type PollingMeasurement = {
  totals: TokenTotals;
  byCategory: CategoryTotals;
  root: { totals: TokenTotals; byCategory: CategoryTotals };
  descendants: Array<{ totals: TokenTotals; byCategory: CategoryTotals }>;
  coverage: TraceCoverage;
};

export type TraceCoverage = {
  complete: boolean;
  expectedThreadCount: number;
  observedThreadCount: number;
  expectedDescendantCount: number;
  observedDescendantCount: number;
  runtimeThreadCountMismatch: boolean;
  completeThreadCount: number;
  missingThreadCount: number;
  unexpectedThreadCount: number;
  responseCountMismatchThreadCount: number;
  tokenTotalsMismatchThreadCount: number;
  noUsageThreadCount: number;
  mixedActionResponseCount: number;
  unattributedResponseCount: number;
  skippedRolloutCount: number;
};

type ArmResult = {
  mode: ArmName;
  status: string;
  benchmarkValid: boolean;
  metrics: Metrics;
  pollingAttribution: PollingMeasurement | null;
};

export type Outcome = {
  status: "complete" | "incomplete" | "failed";
  reason: string;
  rootThreadId: string;
  childThreads: number;
  rootTurns: number;
  terminalChildren: number;
  /** Private runtime identifiers used only to validate the local rollout trace. */
  childThreadIds: string[];
  threadUsageById: Record<string, Metrics>;
  taskElapsedMs: number;
  metrics: Metrics;
  finalMessageSha256?: string;
};

type LoadedFixture = { fixture: ReplayFixture; directory: string; snapshot: string; promptPath: string; promptText: string };
type Options = {
  manifest: string;
  bin?: string;
  out?: string;
  order: ArmName[];
  mode: "dry-run" | "probe-config" | "isolation-check" | "live";
  authHome?: string;
  toolchainRoot?: string;
  help: boolean;
};

function hash(data: string | Buffer): string {
  return createHash("sha256").update(data).digest("hex");
}

function inside(root: string, path: string): boolean {
  const rel = relative(root, path);
  return rel === "" || (rel !== ".." && !rel.startsWith(".." + sep) && !isAbsolute(rel));
}

function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function string(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function count(value: unknown): number {
  return Number.isFinite(Number(value)) ? Math.max(0, Number(value)) : 0;
}

function tomlString(value: string): string {
  return JSON.stringify(value);
}

export function buildReplayConfig(fixture: ReplayFixture, mode: PollingMode): string {
  return [
    "# paired app-server config; agent_polling is the only arm-specific value",
    "model = " + tomlString(fixture.model),
    "model_reasoning_effort = " + tomlString(fixture.effort),
    'approval_policy = "never"',
    'sandbox_mode = "workspace-write"',
    "",
    "[sandbox_workspace_write]",
    'writable_roots = ["/workspace", "/cargo-home"]',
    "network_access = false",
    "exclude_tmpdir_env_var = true",
    "exclude_slash_tmp = true",
    "",
    "[features]",
    "apps = false",
    "browser_use = false",
    "browser_use_external = false",
    "browser_use_full_cdp_access = false",
    "computer_use = false",
    "hooks = false",
    "in_app_browser = false",
    "memories = false",
    "plugins = false",
    "recommended_plugins = false",
    "remote_plugin = false",
    "skill_search = false",
    "skip_host_skill_discovery = true",
    "tool_suggest = false",
    "workspace_dependencies = false",
    "",
    "[features.multi_agent_v2]",
    "enabled = true",
    "agent_polling = " + tomlString(mode),
    "",
    "[skills]",
    "include_instructions = false",
    "",
    "[skills.bundled]",
    "enabled = false",
    "",
  ].join("\n");
}

export function fingerprintSnapshot(root: string): string {
  return hash(git(root, ["rev-parse", "HEAD^{tree}"]));
}

function git(cwd: string, args: string[]): string {
  const result = spawnSync("git", args, { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
  if (result.status !== 0) throw new Error("private checkpoint Git validation failed");
  return result.stdout.trim();
}

export function parseReplayArgs(argv: string[]): Options {
  const get = (flag: string) => {
    const at = argv.indexOf(flag);
    return at >= 0 ? argv[at + 1] : undefined;
  };
  const modes = ["--dry-run", "--probe-config", "--isolation-check", "--live"].filter((flag) => argv.includes(flag));
  if (modes.length > 1) throw new Error("choose one of --dry-run, --probe-config, --isolation-check, or --live");
  const order = (get("--order") ?? "polling,wake").split(",").map((part) => part.trim());
  if (order.length !== 2 || new Set(order).size !== 2 || order.some((part) => part !== "polling" && part !== "wake")) {
    throw new Error("--order must contain polling,wake exactly once each");
  }
  const out = get("--out");
  if (out && !isAbsolute(out)) throw new Error("--out must be an absolute path");
  const manifest = get("--manifest");
  if (!manifest && !argv.includes("--help") && modes[0] !== "--isolation-check") throw new Error("--manifest is required for replay configuration checks and live runs");
  return {
    manifest: manifest ? resolve(manifest) : "",
    bin: get("--bin") ? resolve(get("--bin")!) : undefined,
    out: out ? resolve(out) : undefined,
    order: order as ArmName[],
    mode: modes[0] === "--probe-config" ? "probe-config" : modes[0] === "--isolation-check" ? "isolation-check" : modes[0] === "--live" ? "live" : "dry-run",
    authHome: get("--auth-home") ? resolve(get("--auth-home")!) : undefined,
    toolchainRoot: get("--toolchain-root") ? resolve(get("--toolchain-root")!) : undefined,
    help: argv.includes("--help"),
  };
}

export function loadFixture(path: string): LoadedFixture {
  const manifestPath = realpathSync(path);
  const directory = dirname(manifestPath);
  const fixture = JSON.parse(readFileSync(manifestPath, "utf8")) as ReplayFixture;
  if (fixture.schemaVersion !== 1 || !fixture.id) throw new Error("invalid replay manifest");
  if (!/^[0-9a-f]{40}$/.test(fixture.snapshotCommit)) throw new Error("snapshot commit must be a full commit hash");
  if (!/^[0-9a-f]{40}$/.test(fixture.snapshotTree) || !/^[0-9a-f]{64}$/.test(fixture.snapshotSha256)) {
    throw new Error("manifest requires snapshot tree hashes");
  }
  if (!/^[0-9a-f]{64}$/.test(fixture.promptSha256)) throw new Error("manifest requires a task prompt hash");
  if (!/^gpt-[a-z0-9.-]+$/.test(fixture.model)) throw new Error("model must be pinned to a gpt-* slug");
  if (
    !fixture.effort ||
    !fixture.acceptance?.argv?.length ||
    fixture.acceptance.argv.some((arg) => typeof arg !== "string" || arg.length === 0) ||
    !fixture.acceptance.cwd ||
    !Number.isFinite(fixture.acceptance.timeoutMs) ||
    fixture.acceptance.timeoutMs <= 0 ||
    fixture.acceptance.timeoutMs > 1_800_000
  ) throw new Error("manifest requires a pinned, bounded acceptance command");
  const hardCaps = {
    maxInputTokens: 5_000_000,
    maxOutputTokens: 250_000,
    maxModelRequests: 400,
    maxRootTurns: 60,
    maxChildren: 8,
    maxWallMs: 2_700_000,
    idleMs: 240_000,
  };
  for (const [name, maximum] of Object.entries(hardCaps)) {
    const value = fixture.limits?.[name as keyof ReplayFixture["limits"]];
    if (!Number.isFinite(value) || value! <= 0 || value! > maximum) {
      throw new Error("missing, invalid, or over-cap replay limit: " + name);
    }
  }
  const resolveFixtureFile = (value: string) => {
    if (isAbsolute(value)) throw new Error("fixture paths must be relative to their directory");
    const result = resolve(directory, value);
    if (!inside(directory, result) || !inside(directory, realpathSync(result))) {
      throw new Error("fixture paths must stay inside their directory");
    }
    return realpathSync(result);
  };
  const snapshot = resolveFixtureFile(fixture.snapshot);
  const promptPath = resolveFixtureFile(fixture.prompt);
  if (!statSync(snapshot).isDirectory() || !statSync(promptPath).isFile()) throw new Error("snapshot or prompt is missing");
  const promptText = readFileSync(promptPath, "utf8");
  if (hash(promptText) !== fixture.promptSha256) throw new Error("task prompt hash does not match manifest");
  if (/rollout[-_].*\.jsonl|token_usage_record|response_item/i.test(promptText)) {
    throw new Error("task prompt contains rollout/transcript material");
  }
  if (hash(git(snapshot, ["rev-parse", "HEAD^{tree}"])) !== fixture.snapshotSha256) {
    throw new Error("snapshot tree hash does not match manifest");
  }
  if (!existsSync(join(snapshot, ".git"))) throw new Error("snapshot must be a one-commit isolated repository");
  if (git(snapshot, ["status", "--porcelain", "--untracked-files=all"])) {
    throw new Error("checkpoint working tree must be clean; ignored files are excluded from the replay clone");
  }
  if (git(snapshot, ["rev-list", "--all", "--count"]) !== "1") throw new Error("checkpoint exposes more than one commit");
  const objects = spawnSync("git", ["fsck", "--full", "--no-reflogs", "--unreachable", "--no-progress"], {
    cwd: snapshot, encoding: "utf8", timeout: 20_000,
  });
  if (objects.status !== 0 || objects.stdout.trim()) throw new Error("checkpoint contains invalid or unreachable Git objects");
  if (git(snapshot, ["rev-parse", "HEAD"]) !== fixture.snapshotCommit) throw new Error("snapshot commit differs from manifest");
  if (git(snapshot, ["rev-parse", "HEAD^{tree}"]) !== fixture.snapshotTree) throw new Error("snapshot Git tree differs from manifest");
  if (git(snapshot, ["remote"]) || git(snapshot, ["tag", "--list"])) throw new Error("checkpoint must have no remotes or tags");
  return { fixture, directory, snapshot, promptPath, promptText };
}

/** Clone only the validated one-commit history, never ignored or untracked checkout files. */
export function materializeSnapshot(source: string, destination: string): void {
  const cloned = spawnSync("git", ["clone", "--quiet", "--no-local", "--no-hardlinks", "--no-tags", source, destination], {
    encoding: "utf8", timeout: 60_000,
  });
  if (cloned.status !== 0) throw new Error("could not create isolated checkpoint clone");
  const remotes = spawnSync("git", ["remote"], { cwd: destination, encoding: "utf8" });
  if (remotes.status !== 0) throw new Error("could not inspect isolated checkpoint clone");
  for (const remote of remotes.stdout.trim().split(/\s+/).filter(Boolean)) {
    const removed = spawnSync("git", ["remote", "remove", remote], { cwd: destination, encoding: "utf8" });
    if (removed.status !== 0) throw new Error("could not remove clone remote");
  }
  const refs = spawnSync("git", ["for-each-ref", "--format=%(refname)", "refs/remotes"], { cwd: destination, encoding: "utf8" });
  if (refs.status !== 0) throw new Error("could not inspect clone references");
  for (const ref of refs.stdout.trim().split(/\s+/).filter(Boolean)) {
    const deleted = spawnSync("git", ["update-ref", "-d", ref], { cwd: destination, encoding: "utf8" });
    if (deleted.status !== 0) throw new Error("could not remove clone tracking ref");
  }
  const config = "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n" +
    "[user]\n\tname = Codex Polling Benchmark\n\temail = polling-toolkit@invalid\n";
  writeFileSync(join(destination, ".git", "config"), config, { encoding: "utf8", mode: 0o600 });
  const check = spawnSync("git", ["status", "--porcelain", "--untracked-files=all"], { cwd: destination, encoding: "utf8" });
  if (check.status !== 0 || check.stdout.trim()) throw new Error("isolated checkpoint clone is not clean");
  if (spawnSync("git", ["remote"], { cwd: destination, encoding: "utf8" }).stdout.trim()) {
    throw new Error("isolated checkpoint clone retained a remote");
  }
  if (git(destination, ["rev-list", "--all", "--count"]) !== "1" || git(destination, ["tag", "--list"])) {
    throw new Error("isolated checkpoint clone retained extra history");
  }
  const objects = spawnSync("git", ["fsck", "--full", "--no-reflogs", "--unreachable", "--no-progress"], {
    cwd: destination, encoding: "utf8", timeout: 20_000,
  });
  if (objects.status !== 0 || objects.stdout.trim()) throw new Error("isolated checkpoint clone retained unreachable objects");
}

function setPrivateDirectory(path: string): void {
  mkdirSync(path, { recursive: true, mode: 0o700 });
  if (lstatSync(path).isSymbolicLink()) throw new Error("private directories cannot be symlinks");
  chmodSync(path, 0o700);
}

function makePrivateTemp(prefix: string): string {
  const path = mkdtempSync(join(tmpdir(), prefix));
  chmodSync(path, 0o700);
  return path;
}

function writePrivate(path: string, text: string): void {
  setPrivateDirectory(dirname(path));
  const partial = path + ".partial";
  writeFileSync(partial, text, { encoding: "utf8", mode: 0o600, flag: "wx" });
  chmodSync(partial, 0o600);
  renameSync(partial, path);
}

function prepareHome(path: string, config: string, authHome?: string): void {
  setPrivateDirectory(path);
  writeFileSync(join(path, "config.toml"), config, { encoding: "utf8", mode: 0o600 });
  if (authHome) {
    const auth = join(authHome, "auth.json");
    if (!existsSync(auth)) throw new Error("auth.json missing in --auth-home");
    copyFileSync(auth, join(path, "auth.json"));
    chmodSync(join(path, "auth.json"), 0o600);
  } else {
    writeFileSync(join(path, "auth.json"), JSON.stringify({ isolationProbeCanary: AUTH_CANARY }) + "\n", { encoding: "utf8", mode: 0o600 });
  }
}

type SandboxLayout = {
  runtimeBinHost: string;
  runtimeCodeModeHost?: string;
  systemEtcDir: string;
  cargoHomeDir: string;
  workspaceHost: string;
  codexHomeHost: string;
  toolchainRoot?: string;
};

function denyReadPaths(): string[] {
  return [homedir() + "/**", ISOLATED_CODEX_HOME + "/**"];
}

function prepareSandboxLayout(
  bin: string,
  workspaceHost: string,
  codexHomeHost: string,
  layoutRoot: string,
  toolchainRoot?: string,
): SandboxLayout {
  if (process.platform !== "linux") {
    throw new Error("live replay and isolation probes require Linux with bubblewrap; transcript scanning works on other platforms");
  }
  if (!existsSync(BWRAP)) throw new Error("bubblewrap is required for host-read isolation");
  accessSync(BWRAP, fsConstants.X_OK);
  const systemEtcDir = join(layoutRoot, "isolated-etc");
  const cargoHomeDir = join(layoutRoot, "cargo-home");
  mkdirSync(join(systemEtcDir, "codex"), { recursive: true, mode: 0o700 });
  mkdirSync(join(systemEtcDir, "ssl"), { recursive: true, mode: 0o700 });
  mkdirSync(join(cargoHomeDir, "target", "tmp"), { recursive: true, mode: 0o700 });

  let isolatedToolchain: string | undefined;
  if (toolchainRoot) {
    isolatedToolchain = realpathSync(toolchainRoot);
    if (!statSync(isolatedToolchain).isDirectory()) throw new Error("--toolchain-root must be a directory");
    if (!existsSync(join(isolatedToolchain, "lib", "rustlib"))) {
      throw new Error("--toolchain-root must be one exact Rust toolchain directory, not a home or dependency cache");
    }
    for (const tool of ["cargo", "rustc"]) {
      const executable = join(isolatedToolchain, "bin", tool);
      if (!existsSync(executable)) throw new Error("--toolchain-root is missing bin/" + tool);
      accessSync(executable, fsConstants.X_OK);
    }
  }

  const runtimeBinHost = realpathSync(bin);
  const fd = openSync(runtimeBinHost, "r");
  const magic = Buffer.alloc(4);
  const bytesRead = readSync(fd, magic, 0, magic.length, 0);
  closeSync(fd);
  if (bytesRead !== magic.length || magic.toString("hex") !== "7f454c46") {
    throw new Error("--bin must point to the standalone Codex ELF binary, not a shell wrapper");
  }
  const siblingPath = join(dirname(runtimeBinHost), "codex-code-mode-host");
  const runtimeCodeModeHost = existsSync(siblingPath) ? realpathSync(siblingPath) : undefined;

  const requirements = "[permissions.filesystem]\ndeny_read = " + JSON.stringify(denyReadPaths()) + "\n";
  writeFileSync(join(systemEtcDir, "codex", "requirements.toml"), requirements, { encoding: "utf8", mode: 0o600 });
  writeFileSync(join(systemEtcDir, "hosts"), "127.0.0.1 localhost\n::1 localhost\n", { encoding: "utf8", mode: 0o644 });
  writeFileSync(join(systemEtcDir, "passwd"), "root:x:0:0:root:/safe-home:/bin/sh\n", { encoding: "utf8", mode: 0o644 });
  writeFileSync(join(systemEtcDir, "group"), "root:x:0:\n", { encoding: "utf8", mode: 0o644 });
  writeFileSync(join(systemEtcDir, "nsswitch.conf"), "passwd: files\ngroup: files\nhosts: files dns\n", { encoding: "utf8", mode: 0o644 });
  for (const name of ["ld.so.cache", "services", "protocols", "gai.conf", "os-release"]) {
    const systemFile = join("/etc", name);
    if (existsSync(systemFile)) writeFileSync(join(systemEtcDir, name), readFileSync(systemFile), { mode: 0o644 });
  }
  const resolver = ["/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"].find(existsSync);
  if (resolver) {
    const contents = readFileSync(resolver, "utf8").replace(/^search\s+.*(?:\r?\n|$)/gm, "");
    writeFileSync(join(systemEtcDir, "resolv.conf"), contents, { mode: 0o644 });
  }
  if (existsSync("/etc/ssl/certs")) cpSync("/etc/ssl/certs", join(systemEtcDir, "ssl", "certs"), { recursive: true, dereference: false });
  if (existsSync("/etc/ssl/openssl.cnf")) copyFileSync("/etc/ssl/openssl.cnf", join(systemEtcDir, "ssl", "openssl.cnf"));
  if (existsSync("/etc/localtime")) writeFileSync(join(systemEtcDir, "localtime"), readFileSync("/etc/localtime"), { mode: 0o644 });
  return { runtimeBinHost, runtimeCodeModeHost, systemEtcDir, cargoHomeDir, workspaceHost, codexHomeHost, toolchainRoot: isolatedToolchain };
}

function bubblewrapArgs(
  layout: SandboxLayout,
  command: string[],
  options: { readOnlyWorkspace?: boolean; noNetwork?: boolean; cwd?: string } = {},
): string[] {
  const args = ["--unshare-user", "--unshare-pid", "--unshare-ipc", "--unshare-uts", "--die-with-parent"];
  if (options.noNetwork) args.push("--unshare-net");
  for (const path of ["/usr", "/bin", "/lib", "/lib64"]) {
    if (existsSync(path)) args.push("--ro-bind", path, path);
  }
  args.push(
    "--dev", "/dev",
    "--proc", "/proc",
    "--tmpfs", "/tmp",
    "--dir", "/home",
    "--dir", "/safe-home",
    "--dir", ISOLATED_CODEX_HOME,
    "--dir", ISOLATED_WORKSPACE,
    "--dir", "/runtime",
    "--dir", ISOLATED_CARGO_HOME,
    "--dir", "/toolchain",
    options.readOnlyWorkspace ? "--ro-bind" : "--bind", layout.workspaceHost, ISOLATED_WORKSPACE,
    "--bind", layout.codexHomeHost, ISOLATED_CODEX_HOME,
    "--ro-bind", layout.runtimeBinHost, "/runtime/codex",
    "--ro-bind", layout.systemEtcDir, "/etc",
    "--bind", layout.cargoHomeDir, ISOLATED_CARGO_HOME,
  );
  if (layout.runtimeCodeModeHost) args.push("--ro-bind", layout.runtimeCodeModeHost, "/runtime/codex-code-mode-host");
  if (layout.toolchainRoot) args.push("--ro-bind", layout.toolchainRoot, "/toolchain");
  args.push(
    "--tmpfs", "/home",
    "--clearenv",
    "--setenv", "HOME", "/safe-home",
    "--setenv", "CODEX_HOME", ISOLATED_CODEX_HOME,
    "--setenv", "CARGO_HOME", ISOLATED_CARGO_HOME,
    "--setenv", "CARGO_TARGET_DIR", join(ISOLATED_CARGO_HOME, "target"),
    "--setenv", "CARGO_NET_OFFLINE", "true",
    "--setenv", "PATH", "/runtime:/toolchain/bin:/usr/bin:/bin",
    "--setenv", "TMPDIR", join(ISOLATED_CARGO_HOME, "target", "tmp"),
    "--setenv", "LANG", "C.UTF-8",
    "--chdir", options.cwd ?? ISOLATED_WORKSPACE,
    "--",
    ...command,
  );
  return args;
}

async function newClient(
  bin: string,
  home: string,
  workspaceHost: string,
  armDir: string,
  stderrPath?: string,
  sandboxOptions: {
    readOnlyWorkspace?: boolean;
    noNetwork?: boolean;
    toolchainRoot?: string;
  } = {},
): Promise<AppServerClient> {
  const authPath = join(home, "auth.json");
  if (!statSync(authPath).isFile() || statSync(authPath).size === 0) throw new Error("private CODEX_HOME auth file is absent or empty");
  const layout = prepareSandboxLayout(bin, workspaceHost, home, armDir, sandboxOptions.toolchainRoot);
  const authMount = spawnSync(BWRAP, bubblewrapArgs(layout, [
    "/bin/sh", "-c", 'test -s "$CODEX_HOME/auth.json" && test -r "$CODEX_HOME/auth.json"',
  ], { noNetwork: true }), { cwd: "/", encoding: "utf8", timeout: 10_000 });
  if (authMount.status !== 0) throw new Error("isolated CODEX_HOME auth mount failed its positive read control");
  const args = bubblewrapArgs(layout, ["/runtime/codex", "app-server"], sandboxOptions);
  const client = AppServerClient.spawn({ bin: BWRAP, codexHome: ISOLATED_CODEX_HOME, stderrLogPath: stderrPath, args });
  try {
    await client.initialize(CLIENT_INFO);
    return client;
  } catch (error) {
    await client.close();
    throw error;
  }
}

async function identity(bin: string): Promise<{ version: string; sha256: string }> {
  accessSync(bin, fsConstants.X_OK);
  const version = spawnSync(bin, ["--version"], { encoding: "utf8", timeout: 10_000 });
  if (version.status !== 0) throw new Error("binary --version failed");
  const digest = createHash("sha256");
  for await (const chunk of createReadStream(realpathSync(bin))) digest.update(chunk);
  return { version: version.stdout.trim(), sha256: digest.digest("hex") };
}

function toolkitRevision(): string {
  const repository = resolve(dirname(fileURLToPath(import.meta.url)), "../../../..");
  const result = spawnSync("git", ["rev-parse", "--verify", "HEAD"], { cwd: repository, encoding: "utf8" });
  if (result.status !== 0) return "unversioned";
  const status = spawnSync("git", ["status", "--porcelain", "--untracked-files=all", "--", "codex-rs/tools/polling-toolkit"], {
    cwd: repository, encoding: "utf8",
  });
  return result.stdout.trim() + (status.status === 0 && status.stdout.trim() ? "+dirty" : "");
}

export async function probeReadBoundary(
  client: AppServerClient,
  workspaceHost: string,
  codexHomeHost: string,
  positiveFilename = ".polling-toolkit-isolation-positive",
  expectedCanary = true,
): Promise<void> {
  const command = async (script: string, path: string): Promise<{ exitCode: number; stdout: string; stderr: string }> => {
    let result: { exitCode: number; stdout: string; stderr: string };
    try {
      result = await client.request("command/exec", {
        command: ["/bin/sh", "-c", script, "polling-toolkit-isolation-probe", path],
      cwd: ISOLATED_WORKSPACE,
      timeoutMs: 10_000,
      sandboxPolicy: {
        type: "workspaceWrite",
        writableRoots: [ISOLATED_WORKSPACE, ISOLATED_CARGO_HOME],
        networkAccess: false,
        excludeTmpdirEnvVar: true,
        excludeSlashTmp: true,
      },
      });
    } catch (error) {
      return { exitCode: 1, stdout: "", stderr: error instanceof Error ? error.message : String(error) };
    }
    if (!Number.isInteger(result.exitCode)) throw new Error("command/exec isolation probe returned no exit code");
    return result;
  };
  const positivePath = join(workspaceHost, positiveFilename);
  if (!existsSync(positivePath) || readFileSync(positivePath, "utf8") !== POSITIVE_MARKER + "\n") {
    throw new Error("host-side workspace positive control is absent");
  }
  const authPath = join(codexHomeHost, "auth.json");
  if (!existsSync(authPath) || !statSync(authPath).isFile() || statSync(authPath).size === 0) {
    throw new Error("host-side mounted auth control is absent");
  }
  if (expectedCanary && readFileSync(authPath, "utf8") !== JSON.stringify({ isolationProbeCanary: AUTH_CANARY }) + "\n") {
    throw new Error("host-side synthetic auth canary does not match");
  }
  const workspaceRead = await command('cat "$1"', join(ISOLATED_WORKSPACE, positiveFilename));
  if (workspaceRead.exitCode !== 0 || workspaceRead.stdout !== POSITIVE_MARKER + "\n") {
    throw new Error("app-server command/exec workspace positive read control failed");
  }
  const authRead = await command('cat "$1"', join(ISOLATED_CODEX_HOME, "auth.json"));
  if (
    authRead.exitCode === 0 ||
    authRead.stdout.includes(AUTH_CANARY) ||
    !/(permission denied|operation not permitted|access denied)/i.test(authRead.stderr)
  ) {
    throw new Error("managed deny_read did not prove a permission denial on the mounted app-server auth file");
  }
  if (existsSync(homedir()) && (await command('test -e "$1"', homedir())).exitCode === 0) {
    throw new Error("bubblewrap exposed the host home directory to task commands");
  }
  if (!existsSync(workspaceHost)) throw new Error("isolation probe lost its private workspace");
}

export async function probeModes(loaded: LoadedFixture, bin: string, toolchainRoot?: string): Promise<void> {
  for (const mode of ["enabled", "disabled"] as const) {
    const root = makePrivateTemp("probe-");
    const home = join(root, "codex-home");
    const armDir = join(root, "probe");
    const workspace = join(root, "workspace");
    mkdirSync(home, { mode: 0o700 });
    mkdirSync(armDir, { mode: 0o700 });
    materializeSnapshot(loaded.snapshot, workspace);
    chmodSync(workspace, 0o700);
    writeFileSync(join(workspace, ".polling-toolkit-isolation-positive"), POSITIVE_MARKER + "\n", { encoding: "utf8", mode: 0o600 });
    const config = buildReplayConfig(loaded.fixture, mode);
    prepareHome(home, config);
    let client: AppServerClient | undefined;
    try {
      client = await newClient(bin, home, workspace, armDir, undefined, {
        noNetwork: true,
        toolchainRoot,
      });
      const result = await client.request<Record<string, unknown>>("config/read", { cwd: ISOLATED_WORKSPACE, includeLayers: false });
      const text = JSON.stringify(result);
      if (!text.includes('"agent_polling":"' + mode + '"')) {
        throw new Error("binary does not report agent_polling=" + mode + " in config/read");
      }
      await client.request("thread/start", {
        cwd: ISOLATED_WORKSPACE,
        approvalPolicy: "never",
        sandbox: "workspace-write",
        threadSource: "user",
        model: loaded.fixture.model,
      });
      await probeReadBoundary(client, workspace, home);
    } finally {
      await client?.close();
      rmSync(root, { recursive: true, force: true });
    }
  }
}

export async function isolationCheck(bin: string, toolchainRoot?: string): Promise<void> {
  const root = makePrivateTemp("isolation-check-");
  const home = join(root, "codex-home");
  const workspace = join(root, "workspace");
  const armDir = join(root, "probe");
  mkdirSync(home, { mode: 0o700 });
  mkdirSync(workspace, { mode: 0o700 });
  writeFileSync(join(workspace, ".polling-toolkit-isolation-positive"), POSITIVE_MARKER + "\n", { encoding: "utf8", mode: 0o600 });
  mkdirSync(armDir, { mode: 0o700 });
  const config = [
    'approval_policy = "never"',
    'sandbox_mode = "workspace-write"',
    "",
    "[sandbox_workspace_write]",
    'writable_roots = ["/workspace", "/cargo-home"]',
    "network_access = false",
    "exclude_tmpdir_env_var = true",
    "exclude_slash_tmp = true",
    "",
  ].join("\n");
  prepareHome(home, config);
  let client: AppServerClient | undefined;
  try {
    client = await newClient(bin, home, workspace, armDir, undefined, { noNetwork: true, toolchainRoot });
    await client.request("thread/start", {
      cwd: ISOLATED_WORKSPACE,
      approvalPolicy: "never",
      sandbox: "workspace-write",
      threadSource: "user",
    });
    await probeReadBoundary(client, workspace, home);
  } finally {
    await client?.close();
    rmSync(root, { recursive: true, force: true });
  }
}

function threadId(event: Notification): string | undefined {
  const direct = string(event.params.threadId);
  if (direct) return direct;
  const thread = event.params.thread;
  return object(thread) ? string(thread.id) : undefined;
}

function turnId(event: Notification): string | undefined {
  const direct = string(event.params.turnId);
  if (direct) return direct;
  const turn = event.params.turn;
  return object(turn) ? string(turn.id) : undefined;
}

function usage(event: Notification): Pick<TokenTotals, "cachedInputTokens" | "uncachedInputTokens" | "outputTokens"> | undefined {
  const outer = event.params.tokenUsage;
  if (!object(outer) || !object(outer.last)) return undefined;
  const last = outer.last;
  const input = count(last.inputTokens ?? last.input_tokens);
  const cached = Math.min(input, count(last.cachedInputTokens ?? last.cached_input_tokens));
  return {
    cachedInputTokens: cached,
    uncachedInputTokens: input - cached,
    outputTokens: count(last.outputTokens ?? last.output_tokens),
  };
}

function finalTexts(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((item) => object(item) &&
    (item.type === "agentMessage" || item.type === "message") &&
    typeof item.text === "string" ? [item.text] : []);
}

function eventText(event: Notification): string[] {
  const item = event.params.item;
  return object(item) && (item.type === "agentMessage" || item.type === "message") && typeof item.text === "string"
    ? [item.text]
    : [];
}

function emptyMetrics(): Metrics {
  return { inputTokens: 0, cachedInputTokens: 0, uncachedInputTokens: 0, outputTokens: 0, modelRequests: 0 };
}

class Monitor {
  private readonly client: AppServerClient;
  private readonly limits: ReplayFixture["limits"];
  readonly metrics = emptyMetrics();
  readonly threadUsageById = new Map<string, Metrics>();
  readonly descendants = new Set<string>();
  readonly finishedChildren = new Set<string>();
  readonly active = new Map<string, Set<string>>();
  readonly pending: Notification[] = [];
  root = "";
  rootTurns = 0;
  rootText = "";
  finalHash: string | undefined;
  private eventSequence = 0;
  private rootTextSequence = 0;
  private lastChildTerminalSequence = 0;
  private done: ((value: Outcome) => void) | undefined;
  private result: Outcome | undefined;
  private unsubscribe: (() => void) | undefined;
  private idleTimer: NodeJS.Timeout | undefined;
  private wallTimer: NodeJS.Timeout | undefined;
  private taskStartedAt: number | undefined;

  constructor(client: AppServerClient, limits: ReplayFixture["limits"]) {
    this.client = client;
    this.limits = limits;
  }

  attach(): void {
    this.unsubscribe = this.client.onNotification((event) => {
      if (!this.root) this.pending.push(event);
      else this.onEvent(event);
    });
  }

  setRoot(id: string): void {
    this.root = id;
    this.threadUsageById.set(id, emptyMetrics());
    for (const event of this.pending.splice(0)) this.onEvent(event);
  }

  wait(): Promise<Outcome> {
    return new Promise((resolvePromise) => {
      this.done = resolvePromise;
      this.taskStartedAt = Date.now();
      this.armTimers();
      this.check();
    });
  }

  dispose(): void {
    this.unsubscribe?.();
    if (this.idleTimer) clearTimeout(this.idleTimer);
    if (this.wallTimer) clearTimeout(this.wallTimer);
  }

  private armTimers(): void {
    if (this.idleTimer) clearTimeout(this.idleTimer);
    this.idleTimer = setTimeout(() => this.stop("idle_limit"), this.limits.idleMs);
    this.idleTimer.unref?.();
    if (!this.wallTimer) {
      this.wallTimer = setTimeout(() => this.stop("wall_limit"), this.limits.maxWallMs);
      this.wallTimer.unref?.();
    }
  }

  private onEvent(event: Notification): void {
    this.armTimers();
    this.eventSequence += 1;
    const thread = threadId(event);
    const turn = turnId(event);
    if (event.method === "thread/started" && thread && thread !== this.root) this.descendants.add(thread);
    if (thread && event.method === "turn/started") {
      const turns = this.active.get(thread) ?? new Set<string>();
      if (turn) turns.add(turn);
      this.active.set(thread, turns);
      if (thread === this.root) this.rootTurns += 1;
      else {
        this.descendants.add(thread);
        this.finishedChildren.delete(thread);
      }
    }
    if (thread && event.method === "thread/tokenUsage/updated") {
      if (thread !== this.root) this.descendants.add(thread);
      const last = usage(event);
      if (last) {
        this.metrics.cachedInputTokens += last.cachedInputTokens;
        this.metrics.uncachedInputTokens += last.uncachedInputTokens;
        this.metrics.inputTokens += last.cachedInputTokens + last.uncachedInputTokens;
        this.metrics.outputTokens += last.outputTokens;
        this.metrics.modelRequests += 1;
        const threadMetrics = this.threadUsageById.get(thread) ?? emptyMetrics();
        threadMetrics.cachedInputTokens += last.cachedInputTokens;
        threadMetrics.uncachedInputTokens += last.uncachedInputTokens;
        threadMetrics.inputTokens += last.cachedInputTokens + last.uncachedInputTokens;
        threadMetrics.outputTokens += last.outputTokens;
        threadMetrics.modelRequests += 1;
        this.threadUsageById.set(thread, threadMetrics);
      }
    }
    if (thread && turn && (event.method === "turn/completed" || event.method === "turn/failed")) {
      const turns = this.active.get(thread);
      turns?.delete(turn);
      if (turns?.size === 0) this.active.delete(thread);
      if (thread !== this.root) {
        this.finishedChildren.add(thread);
        this.lastChildTerminalSequence = this.eventSequence;
      }
      if (thread === this.root && object(event.params.turn)) {
        const texts = finalTexts(event.params.turn.items);
        if (texts.length) this.setText(texts.at(-1)!);
        if (event.method === "turn/failed" || event.params.turn.status === "failed") {
          this.stop("root_turn_failed");
          return;
        }
      }
    }
    if (thread === this.root) {
      const texts = eventText(event);
      if (texts.length) this.setText(texts.at(-1)!);
    }
    this.checkLimits();
    this.check();
  }

  private setText(text: string): void {
    this.rootText = text;
    this.rootTextSequence = this.eventSequence;
    if (text.includes(COMPLETE_MARKER)) this.finalHash = hash(text);
  }

  private checkLimits(): void {
    const totalInput = this.metrics.cachedInputTokens + this.metrics.uncachedInputTokens;
    if (totalInput >= this.limits.maxInputTokens) return this.stop("input_token_limit");
    if (this.metrics.outputTokens >= this.limits.maxOutputTokens) return this.stop("output_token_limit");
    if (this.metrics.modelRequests >= this.limits.maxModelRequests) return this.stop("model_request_limit");
    if (this.rootTurns > this.limits.maxRootTurns) return this.stop("root_turn_limit");
    if (this.descendants.size > this.limits.maxChildren) return this.stop("child_limit");
  }

  private check(): void {
    if (this.result || !this.rootText.includes(COMPLETE_MARKER)) return;
    const activeChildTurns = [...this.active.entries()]
      .filter(([thread]) => thread !== this.root)
      .reduce((n, [, turns]) => n + turns.size, 0);
    const rootActive = (this.active.get(this.root)?.size ?? 0) > 0;
    if (this.descendants.size === 0) return this.finish("incomplete", "task_did_not_use_subagents");
    const allReturned = [...this.descendants].every((thread) => this.finishedChildren.has(thread));
    if (
      !rootActive &&
      activeChildTurns === 0 &&
      allReturned &&
      this.rootTextSequence > this.lastChildTerminalSequence
    ) {
      this.finish("complete", "completed_marker_after_children");
    }
  }

  stop(reason: string): void {
    if (this.result) return;
    this.finish("incomplete", reason);
  }

  fail(reason: string): void {
    if (this.result) return;
    this.finish("failed", reason);
  }

  private finish(status: Outcome["status"], reason: string): void {
    if (this.result) return;
    this.result = {
      status,
      reason,
      rootThreadId: this.root,
      childThreads: this.descendants.size,
      rootTurns: this.rootTurns,
      terminalChildren: this.finishedChildren.size,
      childThreadIds: [...this.descendants],
      threadUsageById: Object.fromEntries(this.threadUsageById),
      taskElapsedMs: this.taskStartedAt ? Date.now() - this.taskStartedAt : 0,
      metrics: { ...this.metrics },
      ...(this.finalHash ? { finalMessageSha256: this.finalHash } : {}),
    };
    this.unsubscribe?.();
    if (this.idleTimer) clearTimeout(this.idleTimer);
    if (this.wallTimer) clearTimeout(this.wallTimer);
    this.done?.(this.result);
  }
}

export async function driveTask(
  client: AppServerClient,
  fixture: ReplayFixture,
  cwd: string,
  prompt: string,
): Promise<Outcome> {
  const monitor = new Monitor(client, fixture.limits);
  monitor.attach();
  try {
    const response = await client.request<{ thread: { id: string } }>("thread/start", {
      cwd,
      approvalPolicy: "never",
      sandbox: "workspace-write",
      threadSource: "user",
      model: fixture.model,
    });
    if (!response.thread?.id) throw new Error("thread/start did not return a thread id");
    monitor.setRoot(response.thread.id);
    const outcomePromise = monitor.wait();
    const startPromise = client.request("turn/start", {
      threadId: response.thread.id,
      input: [{ type: "text", text: prompt }],
      cwd,
      effort: fixture.effort,
    }, Math.max(1, fixture.limits.maxWallMs));
    void startPromise.catch(() => undefined);
    const first = await Promise.race([
      outcomePromise.then((outcome) => ({ kind: "outcome" as const, outcome })),
      startPromise.then(() => ({ kind: "started" as const }), (error: unknown) => ({ kind: "start_error" as const, error })),
    ]);
    if (first.kind === "start_error") monitor.fail("turn_start_failed");
    const outcome = first.kind === "outcome" ? first.outcome : await outcomePromise;
    if (outcome.status === "complete") await client.close();
    else await client.killImmediately();
    return outcome;
  } finally {
    monitor.dispose();
  }
}

export function fingerprintDiff(cwd: string, baselineHead = "HEAD"): { files: number; sha256: string } {
  const patch = spawnSync("git", ["diff", "--binary", "--no-ext-diff", baselineHead], {
    cwd, encoding: "buffer", timeout: 20_000, maxBuffer: 16 * 1024 * 1024,
  });
  const names = spawnSync("git", ["diff", "--name-only", "-z", baselineHead], { cwd, encoding: "buffer", timeout: 20_000 });
  const untracked = spawnSync("git", ["ls-files", "--others", "--exclude-standard", "-z"], { cwd, encoding: "buffer", timeout: 20_000 });
  if (patch.status !== 0 || names.status !== 0 || untracked.status !== 0) throw new Error("isolated workspace diff failed");
  const changed = new Set([...names.stdout.toString("utf8").split("\0"), ...untracked.stdout.toString("utf8").split("\0")].filter(Boolean));
  const digest = createHash("sha256").update(patch.stdout);
  for (const name of [...changed].filter((path) => untracked.stdout.toString("utf8").split("\0").includes(path)).sort()) {
    const path = join(cwd, name);
    const metadata = lstatSync(path);
    digest.update("untracked\0" + name + "\0");
    digest.update("mode\0" + (metadata.mode & 0o777).toString(8) + "\0");
    if (metadata.isSymbolicLink()) digest.update("link\0" + readlinkSync(path) + "\0");
    else if (metadata.isFile()) digest.update(readFileSync(path));
    else digest.update("directory\0");
    digest.update("\0");
  }
  return { files: changed.size, sha256: digest.digest("hex") };
}

export function benchmarkValidity(status: string, acceptancePassed: boolean, usageTracePresent: boolean, changesPresent: boolean): boolean {
  return status === "complete" && acceptancePassed && usageTracePresent && changesPresent;
}

export function runAcceptance(
  fixture: ReplayFixture,
  cwd: string,
  logDir: string,
  bin: string,
  layoutRoot: string,
  toolchainRoot?: string,
): { passed: boolean; exitCode: number | null; elapsedMs: number } {
  const acceptanceCwd = resolve(cwd, fixture.acceptance.cwd);
  if (!inside(cwd, acceptanceCwd)) throw new Error("acceptance cwd escaped the private worktree");
  const home = join(layoutRoot, "acceptance-home");
  setPrivateDirectory(home);
  const layout = prepareSandboxLayout(bin, cwd, home, layoutRoot, toolchainRoot);
  const cwdRelative = relative(cwd, acceptanceCwd);
  const isolatedCwd = cwdRelative ? join(ISOLATED_WORKSPACE, cwdRelative) : ISOLATED_WORKSPACE;
  const started = Date.now();
  const result = spawnSync(BWRAP, bubblewrapArgs(layout, fixture.acceptance.argv, { noNetwork: true, cwd: isolatedCwd }), {
    cwd: "/",
    encoding: "utf8",
    timeout: fixture.acceptance.timeoutMs,
    killSignal: "SIGKILL",
    maxBuffer: 2 * 1024 * 1024,
  });
  mkdirSync(logDir, { recursive: true, mode: 0o700 });
  writeFileSync(join(logDir, "acceptance.log"), (result.stdout ?? "") + (result.stderr ?? ""), { encoding: "utf8", mode: 0o600 });
  return { passed: result.status === 0, exitCode: result.status, elapsedMs: Date.now() - started };
}

export function safeMeasurement(
  scan: PollingScanResult,
  outcome: Pick<Outcome, "rootThreadId" | "childThreads" | "childThreadIds" | "threadUsageById">,
): PollingMeasurement | undefined {
  const rootThreadId = outcome.rootThreadId;
  const session = scan.sessions.find((candidate) => candidate.sessionId === rootThreadId);
  if (!session) return undefined;
  const sessionThreads = [session.root, ...session.descendants];
  const threadsById = new Map(sessionThreads.map((thread) => [thread.threadId, thread]));
  const expectedIds = [rootThreadId, ...outcome.childThreadIds];
  const expectedIdSet = new Set(expectedIds);
  const observedIds = new Set(threadsById.keys());
  const missingThreadCount = expectedIds.filter((id) => !observedIds.has(id)).length;
  const unexpectedThreadCount = [...observedIds].filter((id) => !expectedIdSet.has(id)).length;
  const runtimeThreadCountMismatch = outcome.childThreadIds.length !== outcome.childThreads;
  let completeThreadCount = 0;
  let responseCountMismatchThreadCount = 0;
  let tokenTotalsMismatchThreadCount = 0;
  let noUsageThreadCount = 0;
  for (const id of expectedIds) {
    const expected = outcome.threadUsageById[id] ?? emptyMetrics();
    const observed = threadsById.get(id);
    if (!observed) continue;
    const responseCountMatches = observed.totals.responseCount === expected.modelRequests;
    const totalsMatch =
      observed.totals.inputTokens === expected.inputTokens &&
      observed.totals.cachedInputTokens === expected.cachedInputTokens &&
      observed.totals.uncachedInputTokens === expected.uncachedInputTokens &&
      observed.totals.outputTokens === expected.outputTokens;
    if (!responseCountMatches) responseCountMismatchThreadCount += 1;
    if (!totalsMatch) tokenTotalsMismatchThreadCount += 1;
    if (expected.modelRequests === 0 || observed.totals.responseCount === 0) noUsageThreadCount += 1;
    if (
      responseCountMatches && totalsMatch && expected.modelRequests > 0 &&
      observed.byCategory.unknown_no_attribution.responseCount === 0 &&
      observed.byCategory.wait_containing_mixed_calls.responseCount === 0
    ) completeThreadCount += 1;
  }
  const unattributedResponseCount = sessionThreads.reduce(
    (sum, thread) => sum + thread.byCategory.unknown_no_attribution.responseCount,
    0,
  );
  const mixedActionResponseCount = sessionThreads.reduce(
    (sum, thread) => sum + thread.byCategory.wait_containing_mixed_calls.responseCount,
    0,
  );
  const coverage: TraceCoverage = {
    complete: !runtimeThreadCountMismatch && session.descendants.length === outcome.childThreads &&
      missingThreadCount === 0 && unexpectedThreadCount === 0 &&
      responseCountMismatchThreadCount === 0 && tokenTotalsMismatchThreadCount === 0 &&
      noUsageThreadCount === 0 && mixedActionResponseCount === 0 &&
      unattributedResponseCount === 0 && scan.skippedRolloutCount === 0,
    expectedThreadCount: expectedIds.length,
    observedThreadCount: sessionThreads.length,
    expectedDescendantCount: outcome.childThreads,
    observedDescendantCount: session.descendants.length,
    runtimeThreadCountMismatch,
    completeThreadCount,
    missingThreadCount,
    unexpectedThreadCount,
    responseCountMismatchThreadCount,
    tokenTotalsMismatchThreadCount,
    noUsageThreadCount,
    mixedActionResponseCount,
    unattributedResponseCount,
    skippedRolloutCount: scan.skippedRolloutCount,
  };
  const cleanThread = (thread: { totals: TokenTotals; byCategory: CategoryTotals }) => ({
    totals: thread.totals,
    byCategory: thread.byCategory,
  });
  return {
    totals: session.totals,
    byCategory: session.byCategory,
    root: cleanThread(session.root),
    descendants: session.descendants.map(cleanThread),
    coverage,
  };
}

function measured(measurement: PollingMeasurement): Metrics {
  return {
    inputTokens: measurement.totals.inputTokens,
    cachedInputTokens: measurement.totals.cachedInputTokens,
    uncachedInputTokens: measurement.totals.uncachedInputTokens,
    outputTokens: measurement.totals.outputTokens,
    modelRequests: measurement.totals.responseCount,
  };
}

function emptyTotals(): TokenTotals {
  return { inputTokens: 0, cachedInputTokens: 0, uncachedInputTokens: 0, outputTokens: 0, responseCount: 0 };
}

function combineDescendantMeasurement(measurement: PollingMeasurement): { totals: TokenTotals; byCategory: CategoryTotals } {
  const categoryNames: PollingActionCategory[] = [
    "pure_wait_agent",
    "wait_containing_mixed_calls",
    "pure_write_stdin",
    "other",
    "unknown_no_attribution",
  ];
  const byCategory = Object.fromEntries(categoryNames.map((category) => [category, emptyTotals()])) as CategoryTotals;
  const totals = emptyTotals();
  for (const thread of measurement.descendants) {
    for (const category of categoryNames) {
      for (const field of ["inputTokens", "cachedInputTokens", "uncachedInputTokens", "outputTokens", "responseCount"] as const) {
        byCategory[category][field] += thread.byCategory[category][field];
      }
    }
    for (const field of ["inputTokens", "cachedInputTokens", "uncachedInputTokens", "outputTokens", "responseCount"] as const) {
      totals[field] += thread.totals[field];
    }
  }
  return { totals, byCategory };
}

function compareTokenTotals(polling: TokenTotals, wake: TokenTotals): Record<string, { polling: number; wake: number; pollingMinusWake: number }> {
  const fields = ["inputTokens", "cachedInputTokens", "uncachedInputTokens", "outputTokens", "responseCount"] as const;
  return Object.fromEntries(fields.map((field) => [field, {
    polling: polling[field],
    wake: wake[field],
    pollingMinusWake: polling[field] - wake[field],
  }]));
}

export function pairedComparison(results: ArmResult[]): Record<string, unknown> | null {
  const polling = results.find((result) => result.mode === "polling");
  const wake = results.find((result) => result.mode === "wake");
  if (!polling || !wake || !polling.pollingAttribution || !wake.pollingAttribution) return null;
  const pollingDescendants = combineDescendantMeasurement(polling.pollingAttribution);
  const wakeDescendants = combineDescendantMeasurement(wake.pollingAttribution);
  const categoryNames: PollingActionCategory[] = [
    "pure_wait_agent",
    "wait_containing_mixed_calls",
    "pure_write_stdin",
    "other",
    "unknown_no_attribution",
  ];
  return {
    validPair: polling.benchmarkValid && wake.benchmarkValid,
    pollingStatus: polling.status,
    wakeStatus: wake.status,
    root: {
      totals: compareTokenTotals(polling.pollingAttribution.root.totals, wake.pollingAttribution.root.totals),
      byCategory: Object.fromEntries(categoryNames.map((category) => [category, compareTokenTotals(
        polling.pollingAttribution!.root.byCategory[category],
        wake.pollingAttribution!.root.byCategory[category],
      )])),
    },
    descendants: {
      totals: compareTokenTotals(pollingDescendants.totals, wakeDescendants.totals),
      byCategory: Object.fromEntries(categoryNames.map((category) => [category, compareTokenTotals(
        pollingDescendants.byCategory[category],
        wakeDescendants.byCategory[category],
      )])),
    },
    allThreads: {
      totals: compareTokenTotals(polling.pollingAttribution.totals, wake.pollingAttribution.totals),
      byCategory: Object.fromEntries(categoryNames.map((category) => [category, compareTokenTotals(
        polling.pollingAttribution!.byCategory[category],
        wake.pollingAttribution!.byCategory[category],
      )])),
    },
  };
}

async function runArm(
  loaded: LoadedFixture,
  mode: ArmName,
  out: string,
  bin: string,
  authHome: string,
  binary: { version: string; sha256: string },
  toolchainRoot?: string,
): Promise<ArmResult> {
  const fixture = loaded.fixture;
  const armDir = join(out, mode);
  mkdirSync(armDir, { recursive: true, mode: 0o700 });
  chmodSync(armDir, 0o700);
  const sandboxDir = makePrivateTemp("arm-");
  const cwd = join(sandboxDir, "workspace");
  const home = join(sandboxDir, "codex-home");
  const config = buildReplayConfig(fixture, MODES[mode]);
  const configSha256 = hash(config);
  const started = Date.now();
  let baselineHead = "";
  try {
    materializeSnapshot(loaded.snapshot, cwd);
    chmodSync(cwd, 0o700);
    mkdirSync(home, { mode: 0o700 });
    baselineHead = git(cwd, ["rev-parse", "HEAD"]);
  } catch (error) {
    rmSync(sandboxDir, { recursive: true, force: true });
    throw error;
  }
  let client: AppServerClient | undefined;
  let outcome: Outcome | undefined;
  let measurement: PollingMeasurement | undefined;
  let acceptance = { passed: false, exitCode: null as number | null, elapsedMs: 0 };
  let status: Outcome["status"] = "failed";
  let reason = "runner_error";
  let sandboxExitConfirmed = true;
  let changed = { files: 0, sha256: hash("") };
  const positiveFilename = ".polling-toolkit-isolation-positive-" + randomUUID();
  const logPath = join(armDir, "app-server.stderr.log");
  writePrivate(logPath, "");
  try {
    prepareHome(home, config, authHome);
    client = await newClient(bin, home, cwd, sandboxDir, logPath, { toolchainRoot });
    writeFileSync(join(cwd, positiveFilename), POSITIVE_MARKER + "\n", { encoding: "utf8", mode: 0o600, flag: "wx" });
    await probeReadBoundary(client, cwd, home, positiveFilename, false);
    rmSync(join(cwd, positiveFilename), { force: true });
    outcome = await driveTask(client, fixture, ISOLATED_WORKSPACE, loaded.promptText);
    status = outcome.status;
    reason = outcome.reason;
  } catch (error) {
    status = "failed";
    reason = error instanceof Error ? error.name : "runner_error";
  } finally {
    try {
      await client?.close();
    } catch {
      status = "failed";
      reason = "appserver_shutdown_timeout";
      sandboxExitConfirmed = false;
      try {
        await client?.killImmediately();
        sandboxExitConfirmed = true;
      } catch {
        reason = "appserver_exit_unconfirmed";
      }
    }
  }
  if (sandboxExitConfirmed && outcome?.rootThreadId) {
    try {
      measurement = safeMeasurement(await scanCodexHomes([home]), outcome);
      if (!measurement) {
        status = "incomplete";
        reason = "usage_trace_missing";
      } else if (!measurement.coverage.complete) {
        status = "incomplete";
        reason = "usage_trace_incomplete";
      } else {
        const metrics = measured(measurement);
        if (metrics.inputTokens >= fixture.limits.maxInputTokens) {
          status = "incomplete";
          reason = "input_token_limit";
        } else if (metrics.outputTokens >= fixture.limits.maxOutputTokens) {
          status = "incomplete";
          reason = "output_token_limit";
        } else if (metrics.modelRequests >= fixture.limits.maxModelRequests) {
          status = "incomplete";
          reason = "model_request_limit";
        } else if (outcome.rootTurns > fixture.limits.maxRootTurns) {
          status = "incomplete";
          reason = "root_turn_limit";
        } else if (measurement.descendants.length > fixture.limits.maxChildren) {
          status = "incomplete";
          reason = "child_limit";
        }
      }
      if (status === "complete" && measurement) {
        acceptance = runAcceptance(fixture, cwd, join(armDir, "logs"), bin, sandboxDir, toolchainRoot);
        if (!acceptance.passed) {
          status = "incomplete";
          reason = "acceptance_failed";
        }
      }
      changed = fingerprintDiff(cwd, baselineHead);
      if (status === "complete" && changed.files === 0) {
        status = "incomplete";
        reason = "no_workspace_changes";
      }
    } catch (error) {
      status = "failed";
      reason = error instanceof Error ? error.name : "usage_profile_failed";
    }
  }
  const metrics = measurement ? measured(measurement) : outcome?.metrics ?? {
    inputTokens: 0, cachedInputTokens: 0, uncachedInputTokens: 0, outputTokens: 0, modelRequests: 0,
  };
  if (sandboxExitConfirmed) {
    const savedWorkspace = join(armDir, "workspace");
    if (existsSync(savedWorkspace)) rmSync(savedWorkspace, { recursive: true, force: true });
    if (existsSync(cwd)) cpSync(cwd, savedWorkspace, {
      recursive: true,
      preserveTimestamps: true,
      verbatimSymlinks: true,
      filter: (source) => {
        const path = relative(cwd, source);
        return path === "" || (path !== "target" && !path.startsWith("target" + sep) && path !== "node_modules" && !path.startsWith("node_modules" + sep));
      },
    });
    rmSync(sandboxDir, { recursive: true, force: true });
  }
  const traceCoverageComplete = Boolean(measurement?.coverage.complete);
  const result = {
    schemaVersion: 1,
    fixtureId: fixture.id,
    checkpointCommit: fixture.snapshotCommit,
    checkpointTree: fixture.snapshotTree,
    checkpointSha256: fixture.snapshotSha256,
    promptSha256: fixture.promptSha256,
    workspaceArtifact: join(armDir, "workspace"),
    mode,
    agentPolling: MODES[mode],
    model: fixture.model,
    effort: fixture.effort,
    binaryVersion: binary.version,
    binarySha256: binary.sha256,
    configSha256,
    status,
    reason,
    sandboxExitConfirmed,
    ...(sandboxExitConfirmed ? {} : { privateSandboxDirectory: sandboxDir }),
    runnerElapsedMs: Date.now() - started,
    taskElapsedMs: outcome?.taskElapsedMs ?? null,
    metrics,
    pollingAttribution: measurement ? {
      root: measurement.root,
      descendants: measurement.descendants,
      totals: measurement.totals,
      byCategory: measurement.byCategory,
      coverage: measurement.coverage,
    } : null,
    acceptance,
    changedFiles: changed.files,
    changedPatchSha256: changed.sha256,
    changesPresent: changed.files > 0,
    traceCoverageComplete,
    benchmarkValid: benchmarkValidity(
      status,
      acceptance.passed,
      traceCoverageComplete,
      changed.files > 0,
    ),
  };
  writePrivate(join(armDir, "result.json"), JSON.stringify(result, null, 2) + "\n");
  return result;
}

export async function runReplay(argv = process.argv.slice(2)): Promise<void> {
  const options = parseReplayArgs(argv);
  if (options.help) {
    process.stdout.write("Usage: node --experimental-strip-types src/cli.ts replay --manifest <fixture/manifest.json> [--dry-run | --probe-config | --isolation-check | --live] [--bin <codex-elf>] [--auth-home <codex-home>] [--toolchain-root <rust-toolchain>] [--out <private-dir>] [--order polling,wake]\n");
    return;
  }
  if (options.mode === "isolation-check") {
    if (!options.bin) throw new Error("--bin is required for --isolation-check");
    const bin = realpathSync(options.bin);
    await isolationCheck(bin, options.toolchainRoot);
    process.stdout.write(JSON.stringify({
      mode: "isolation-check",
      toolkitVersion: TOOLKIT_VERSION,
      toolkitRevision: toolkitRevision(),
      nodeVersion: process.version,
      runner: `${process.platform}-${process.arch}`,
      providerCalls: 0,
      binary: await identity(bin),
      result: "bubblewrap hid the host home and managed deny_read rejected reads of the mounted auth file",
    }, null, 2) + "\n");
    return;
  }
  const loaded = loadFixture(options.manifest);
  const fixture = loaded.fixture;
  const enabled = buildReplayConfig(fixture, "enabled").replace('agent_polling = "enabled"', 'agent_polling = "MODE"');
  const disabled = buildReplayConfig(fixture, "disabled").replace('agent_polling = "disabled"', 'agent_polling = "MODE"');
  if (enabled !== disabled) throw new Error("paired config differs outside agent_polling");
  const acceptanceSha256 = hash(JSON.stringify(fixture.acceptance));
  const summary = {
    toolkitVersion: TOOLKIT_VERSION,
    toolkitRevision: toolkitRevision(),
    nodeVersion: process.version,
    runner: `${process.platform}-${process.arch}`,
    fixtureId: fixture.id,
    checkpointCommit: fixture.snapshotCommit,
    checkpointTree: fixture.snapshotTree,
    checkpointSha256: fixture.snapshotSha256,
    wakePolicyScope: "root-only; subagent polling remains enabled and is reported separately",
    promptSha256: fixture.promptSha256,
    acceptanceSha256,
    model: fixture.model,
    effort: fixture.effort,
    order: options.order.map((mode) => ({ mode, agentPolling: MODES[mode] })),
    limitsPerArm: fixture.limits,
    inputOutputAndRequestLimitsIncludeAllDescendants: true,
    transcriptReplay: false,
  };
  if (options.mode === "dry-run") {
    let binary: unknown = "not checked; pass --bin for executable identity";
    if (options.bin) binary = await identity(realpathSync(options.bin));
    process.stdout.write(JSON.stringify({ ...summary, dryRun: true, providerCalls: 0, binary }, null, 2) + "\n");
    return;
  }
  if (!options.bin) throw new Error("--bin is required");
  const bin = realpathSync(options.bin);
  const binary = await identity(bin);
  if (options.mode === "probe-config") {
    await probeModes(loaded, bin, options.toolchainRoot);
    process.stdout.write(JSON.stringify({ ...summary, probeConfig: true, isolationCheck: true, providerCalls: 0, binary, result: "both polling settings parsed; read-isolation probes passed; app-server thread/start succeeded" }, null, 2) + "\n");
    return;
  }
  if (!options.authHome) throw new Error("--auth-home is required for --live; it must contain the Codex auth.json you intend to use");
  if (!existsSync(join(options.authHome, "auth.json"))) throw new Error("--auth-home does not contain auth.json");
  await probeModes(loaded, bin, options.toolchainRoot);
  const out = options.out ?? join(OUTPUT_ROOT, new Date().toISOString().replace(/[:.]/g, "-") + "-" + fixture.id);
  setPrivateDirectory(OUTPUT_ROOT);
  const privateRoot = realpathSync(OUTPUT_ROOT);
  const requestedOut = resolve(out);
  if (dirname(requestedOut) !== resolve(OUTPUT_ROOT)) {
    throw new Error("live output must be a new direct child of the private toolkit state directory");
  }
  if (existsSync(requestedOut)) throw new Error("live output path already exists");
  mkdirSync(requestedOut, { mode: 0o700 });
  chmodSync(requestedOut, 0o700);
  const privateOut = realpathSync(requestedOut);
  if (!inside(privateRoot, privateOut)) throw new Error("live output must resolve under the private toolkit state directory");
  const lock = join(privateOut, ".running");
  writeFileSync(lock, String(process.pid) + "\n", { encoding: "utf8", mode: 0o600, flag: "wx" });
  const results: ArmResult[] = [];
  try {
    writePrivate(join(privateOut, "manifest.json"), JSON.stringify({ ...summary, binary, startedAt: new Date().toISOString() }, null, 2) + "\n");
    for (const mode of options.order) {
      const result = await runArm(loaded, mode, privateOut, bin, options.authHome, binary, options.toolchainRoot);
      results.push(result);
      if (result.status === "failed") break;
    }
    const comparison = pairedComparison(results);
    writePrivate(join(privateOut, "paired-results.json"), JSON.stringify({ summary, results, comparison }, null, 2) + "\n");
    process.stdout.write(JSON.stringify({
      out: privateOut,
      comparison,
      results: results.map((result) => ({
        mode: result.mode,
        status: result.status,
        reason: result.reason,
        metrics: result.metrics,
        waitAttribution: result.waitAttribution,
        acceptance: result.acceptance,
        usageTracePresent: result.usageTracePresent,
        benchmarkValid: result.benchmarkValid,
      })),
    }, null, 2) + "\n");
  } finally {
    rmSync(lock, { force: true });
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  runReplay().catch((error: unknown) => {
    process.stderr.write((error instanceof Error ? error.message : "wait replay failed") + "\n");
    process.exitCode = 1;
  });
}
