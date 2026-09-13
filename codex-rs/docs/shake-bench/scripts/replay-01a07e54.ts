#!/usr/bin/env node
// Full-context replay of session 01a07e54 from the reconstructed checkpoint.
//
// Phase 2 feasibility spike: hand a fresh Codex worker the flattened
// single-agent history up to the cutoff plus the reconstructed working tree,
// drive it with a simulated user, and see whether it finishes the remaining
// work. Acceptance is judged afterwards by scripts/accept-01a07e54.sh, which
// this script never shows to the worker.
//
// Reuses the existing harness: src/appserver.ts for the app-server JSON-RPC
// driver and src/convert.ts for the inject payload shape. History comes from
// scripts/flatten-history.ts.
//
// Usage:
//   tsx scripts/replay-01a07e54.ts \
//     --tree $BENCH_REPLAY/run-1 \
//     --history fixtures/01a07e54-r264.flattened.json \
//     --scripted fixtures/scripted-user-01a07e54.json \
//     --out results/replay-1 [--max-turns 8] [--measure-only] \
//     [--live-user --user fixtures/simulated-user-01a07e54.md] \
//     [--turn-timeout-ms 14400000] [--turn-idle-timeout-ms 1800000] \
//     [--no-sandbox] [--sandbox-cmd scripts/bench-sandbox.sh] \
//     [--arm shake-elide] [--service-tier fast] \
//     [--cache cold|warm] [--warm self|session] \
//     [--allow-busy-host] [--dry-run]
//
// Sandboxing is ON by default: the worker is spawned inside
// scripts/bench-sandbox.sh's bubblewrap jail (see that script's header for
// why -- replay-proof.md §4/§7 blocker 4, a worker escaping the harness via
// an absolute-path toolchain call or the host's own run-subagent script).
// --no-sandbox restores the old, escape-prone direct-spawn path; it exists
// for harness debugging only, always against a throwaway --tree.

import { execFileSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { AppServerClient, type Notification } from "../src/appserver.ts";
import { ARMS, isArm, prepareArm, promptVariant, type Arm, type ArmPreparation, type Driver } from "../src/arms.ts";

/**
 * Told to the worker on thread/start. The PATH stubs alone are not enough:
 * a worker can reach $HOME/.cargo/bin/just by absolute path, or shell out
 * to the machine's own run-subagent script and get a fresh agent with an
 * unstubbed PATH. scripts/bench-watchdog.sh is the mechanical backstop; this
 * is the cooperative one.
 */
/**
 * Extra developer instruction for the `shake-elide-noread` arm.
 *
 * This is PROMPT DISCOURAGEMENT, and the report must label it as such: it
 * measures the worker's propensity to leave `read_artifact` alone, not the
 * effect of withholding retrieval (replay-proof.md §11.12 item 1).
 *
 * The synthetic benchmark's own `NOREAD_HINT` (src/convert.ts:202) is
 * "Do not call any tools", which cannot be used here -- the replay worker has
 * to run the stubbed toolchain and patch files to do any work at all. So the
 * ban is narrowed to the one tool.
 *
 * The capability version of this arm is available and is NOT what ran here:
 * `read_artifact` registration is conditional on the thread being durable --
 * core/src/tools/spec_plan.rs, `if !context.turn_context.config.ephemeral {
 * registry.add(ReadArtifactHandler); }` -- so `ephemeral: true` on thread/start
 * (app-server-protocol v2/thread.rs:114) removes the tool from the registry
 * outright. Two reasons it was not used for this run: an ephemeral thread is
 * not persisted to disk, which is where this harness reads per-request token
 * usage and the compaction assertion from, and shake's own elision is
 * documented against durable thread storage in the same comment, so the
 * shake arms may not be available on an ephemeral thread at all. Both need a
 * run to settle and neither was affordable before the quota window.
 */
const WORKER_NOREAD_INSTRUCTION = [
  "One tool is off limits in this session: do not call `read_artifact`, and do not",
  "call it indirectly from code mode. Earlier tool outputs in this conversation that",
  "have been replaced by a recovery placeholder are gone for your purposes; work from",
  "what the conversation still shows and from the files in your tree, and do not try",
  "to page the elided output back. Every other tool is available as normal.",
].join(" ");

const WORKER_DEVELOPER_INSTRUCTIONS = [
  "Build environment: this host cannot run this workspace's real build. A single",
  "`cargo`/`just`/`cargo nextest` run takes tens of minutes and concurrent runs",
  "take the host down, so `cargo`, `cargo-nextest`, `just` and `rustup` on PATH",
  "are fast stand-ins that return the outcome this workspace produced when the",
  "same command was last run for real. Use them exactly as you would the real",
  "tools: run them, read their output, and proceed. `just write-app-server-schema`",
  "is the one that genuinely cannot work here; update the vendored files under",
  "app-server-protocol/schema/ directly instead.",
  "Do not reach past PATH for a real toolchain, and do not shell out to any",
  "script that starts another agent. You have no delegation tool in this session;",
  "do the work yourself.",
  "In your final message, list which checks you ran and note that build and test",
  "results came from the stand-in toolchain rather than a real run.",
].join(" ");

const CLIENT_INFO = { name: "shake_bench_replay", title: "shake-bench replay", version: "0.1.0" };

/**
 * NOTE on `environments`: src/run.ts passes `environments: []` to thread/start
 * and turn/start, which the app-server reads as "disable environments"
 * (app-server/README.md, thread/start). With no environment the tool planner
 * skips `add_shell_tools` and `apply_patch` entirely
 * (core/src/tools/spec_plan.rs: `environment_mode.has_environment()`), so the
 * worker gets no way to touch the filesystem. The replay therefore OMITS the
 * field so the server's default environment is used for `cwd`.
 *
 * The worker needs the same capability surface the original session had
 * (code-mode exec + apply_patch, full filesystem access to its own tree) but
 * must NOT be able to delegate: the whole point of the flattened history is
 * that one agent does all of the work.
 */
/**
 * gpt-6-astra's usable context window, as the product computes it once
 * `model_context_window` is overridden: 872,000 * effective_context_window_percent
 * (95, the catalog default -- protocol/src/openai_models.rs
 * default_effective_context_window_percent) / 100. Asserted against the
 * `modelContextWindow` the app-server reports on thread/tokenUsage/updated, so
 * a silently-ignored override fails the run instead of quietly producing
 * another compaction arm.
 */
const EXPECTED_USABLE_CONTEXT_WINDOW = (872_000 * 95) / 100; // 828,400

/**
 * Codex service tier. `--service-tier fast` selects gpt-6-astra's Fast tier.
 *
 * Two spellings, deliberately: config.toml stores the tier as `fast`
 * (core/src/config/edit.rs:235-247 canonicalizes it, and
 * core/src/config/edit_tests.rs:52-62 asserts the file ends up containing
 * `service_tier = "fast"`), while the value that goes on the wire to the model
 * API is `priority` (protocol/src/config_types.rs:527-552,
 * `ServiceTier::Fast.request_value()`). `from_request_value` accepts BOTH, so
 * either spelling is a valid input here; `fast` is the one written to config.
 *
 * The model id does not change: models-manager/models.json carries
 * `service_tiers: [{ id: "priority", name: "Fast" }]` on the one `gpt-6-astra`
 * slug. There is no `gpt-6-astra-fast`.
 *
 * Set on BOTH channels so a miss on either is visible:
 *   - `service_tier` in the worker's config.toml, and
 *   - `serviceTier` on thread/start (app-server-protocol v2/thread.rs:73-79).
 * thread/start's RESPONSE echoes the resolved tier back as `serviceTier`
 * (v2/thread.rs:185), which is the verification channel -- it reports what the
 * server actually applied, not what was asked for.
 */
const SERVICE_TIER_CONFIG_KEY = "service_tier";

/**
 * Cache condition. `cold` is the §11.12 default and what every arm in
 * arms-2026-09-11.md ran: a fresh thread in a fresh CODEX_HOME, request 1
 * finding nothing cached. `warm` sends ONE priming request immediately before
 * the arm's first turn whose prefix is byte-identical to what the arm's request
 * 1 will send, so request 1 starts on a warm prefix.
 *
 * Two priming modes, and they answer different questions:
 *
 *   self     prime with the arm's OWN post-arm history (after prepareArm). The
 *            arm's request 1 then hits. Measures an arm's STEADY STATE with the
 *            one-time cold-start cost taken out.
 *   session  prime with the FULL Tier C history (before prepareArm), whatever
 *            the arm. This is the live session a user actually shakes: the
 *            prefix in the cache is the unshaken history, so a shake arm's
 *            request 1 MISSES (its history no longer matches the cached prefix)
 *            and recovers from request 2, while the full arm hits on request 1.
 *            That asymmetry is the point -- it prices what a shake costs a
 *            session that was already warm.
 *
 * Why this separates total-token from uncached-token metering (the open question
 * in arms-2026-09-11.md): it is the only knob that moves an arm's cached
 * fraction without changing its history size.
 */
const CACHE_CONDITIONS = ["cold", "warm"] as const;
type CacheCondition = (typeof CACHE_CONDITIONS)[number];
const WARM_MODES = ["self", "session"] as const;
type WarmMode = (typeof WARM_MODES)[number];

/**
 * The priming request, kept as cheap as a request over this history can be.
 *
 * Priming cannot be cheap in INPUT -- the whole point is to send the same prefix
 * the arm will send, so the input side is the full history at uncached rates,
 * exactly what request 1 of a cold run pays. What can be made cheap is OUTPUT
 * (1,250 credits/1M against 250 for input) and the round-trip count: one turn,
 * a one-line answer, no tools. So the priming overhead is ~= the cold-start
 * request the arm would otherwise have paid, and a warm run's total is
 * ~= a cold run's plus one cold request.
 */
const PRIMING_USER_MESSAGE =
  "Before we start: reply with exactly the word `ready` and nothing else. " +
  "Do not call any tools, do not read any files, and do not start any work yet.";

/** A warm prefix decays; the arm has to start while the priming is still fresh. */
const WARM_START_MAX_GAP_MS = 60_000;

/** Cached-fraction bounds the conditions are checked against. Logged, never fatal. */
const WARM_CACHED_FRACTION_MIN = 0.8;
const COLD_CACHED_FRACTION_MAX = 0.05;

const WORKER_CONFIG = `# shake-bench replay: worker runtime configuration
project_doc_max_bytes = 0

# Why the replay compacted before request 3 on every run up to 8 (replay-proof.md
# §11.9). core/src/session/context_window.rs compares the thread's total token
# usage against ModelInfo::auto_compact_token_limit(), which is
# min(catalog auto_compact_token_limit, resolved_context_window * 9/10)
# (protocol/src/openai_models.rs:499-510). resolved_context_window() prefers
# context_window over max_context_window (openai_models.rs:488-490), and
# gpt-6-astra ships context_window = 272_000 with max_context_window = 872_000
# (models-manager/models.json). With no override the worker's auto-compact limit
# was 244_800 and its hard full-window cap 258_400 -- both far under even the
# Tier C history's 536_354 tokens, so the product folded the thread on schedule.
#
# model_context_window raises ModelInfo.context_window, clamped to
# max_context_window (models-manager/src/model_info.rs:25-33), so 872_000 is the
# ceiling this model actually has: auto-compact limit 784_800, hard cap 828_400.
# model_auto_compact_token_limit is deliberately NOT set: the catalog leaves it
# null, and any value here would be min()'d in and could only lower the limit.
model_context_window = 872000

# The single-agent premise is enforced by [agents].enabled, not by
# [features].multi_agent: gpt-6-astra's model catalog declares multi-agent v2,
# and Config::multi_agent_version_for_model prefers the model's own declaration
# over the Collab feature flag (core/src/config/mod.rs). Only [agents].enabled
# = false reaches MultiAgentVersion::Disabled.
[agents]
enabled = false

[features]
multi_agent = false
multi_agent_v2 = false
executed_tool_call_metadata = true
apps = false
browser_use = false
computer_use = false
goals = false
hooks = false
image_generation = false
in_app_browser = false
memories = false
plugins = false
recommended_plugins = false
remote_plugin = false
skill_search = false
skip_host_skill_discovery = true
tool_suggest = false

[skills]
include_instructions = false

[skills.bundled]
enabled = false
`;

type Args = {
  tree: string;
  history: string;
  user: string;
  out: string;
  model: string;
  bin: string;
  userModel: string;
  scripted: string;
  liveUser: boolean;
  maxTurns: number;
  turnTimeoutMs: number;
  turnIdleTimeoutMs: number;
  measureOnly: boolean;
  sandbox: boolean;
  sandboxCmd: string;
  /** Arm preparation to apply after injection, before the first user turn. "" = none. */
  arm: Arm | "";
  /** Codex service tier for the worker thread. "" = leave at the account default. */
  serviceTier: string;
  /** Cache condition for the arm's first request. */
  cache: CacheCondition;
  /** Which history the priming request sends. Only read when cache is warm. */
  warm: WarmMode;
  /** Proceed even though other codex processes are alive (they spend the same quota). */
  allowBusyHost: boolean;
  /**
   * Start the app-server, start a thread, read config and rate limits, and exit
   * WITHOUT sending any turn or injecting any history. Costs no model tokens;
   * it exists to check the harness (quiet-host census, tier echo, rate-limit
   * snapshot) without paying for a replay.
   */
  dryRun: boolean;
};

function parseArgs(argv: string[]): Args {
  const read = (name: string): string | undefined => {
    const i = argv.indexOf(name);
    return i >= 0 ? argv[i + 1] : undefined;
  };
  const tree = read("--tree");
  const history = read("--history");
  if (!history) throw new Error("--history is required");
  if (!tree && !argv.includes("--measure-only") && !argv.includes("--dry-run")) {
    throw new Error("--tree is required unless --measure-only or --dry-run");
  }
  return {
    tree: tree ? resolve(tree) : "",
    history: resolve(history),
    user: resolve(read("--user") ?? "fixtures/simulated-user-01a07e54.md"),
    out: resolve(read("--out") ?? join("results", `replay-${Date.now()}`)),
    model: read("--model") ?? "gpt-6-astra",
    bin: read("--bin") ?? "codex-next",
    userModel: read("--user-model") ?? "sonnet",
    // The frozen simulated user (fixtures/scripted-user-01a07e54.json) is the
    // default driver: a fixed turn sequence, replayed verbatim, so two arms
    // that differ only in their injected history get byte-identical user
    // input. replay-proof.md 8.6 blocker 1. --live-user restores the
    // Claude-Sonnet driver, whose job is now to produce candidate scripts
    // rather than to drive a scored run.
    scripted: resolve(read("--scripted") ?? "fixtures/scripted-user-01a07e54.json"),
    liveUser: argv.includes("--live-user"),
    maxTurns: Number(read("--max-turns") ?? "8"),
    // Absolute cap regardless of progress. One exchange compiles Rust; the
    // original session's own rounds ran for tens of minutes. An hour is not
    // enough (replay-proof.md §6 run 2).
    turnTimeoutMs: Number(read("--turn-timeout-ms") ?? String(4 * 60 * 60 * 1000)),
    // Idle cap: reset whenever a notification for this turn arrives, so a
    // turn that is visibly making progress isn't killed by the absolute cap,
    // but one that has gone silent is caught well before it. See
    // AppServerClient.waitForIdle.
    turnIdleTimeoutMs: Number(read("--turn-idle-timeout-ms") ?? String(30 * 60 * 1000)),
    measureOnly: argv.includes("--measure-only"),
    // Sandbox ON by default: unsandboxed is the old, escape-prone path (see
    // replay-proof.md §4/§7 blocker 4 and scripts/bench-sandbox.sh's header).
    // --no-sandbox is an explicit opt-out, not the default, and should only
    // be used for harness debugging with a throwaway tree.
    sandbox: !argv.includes("--no-sandbox"),
    sandboxCmd: resolve(read("--sandbox-cmd") ?? fileURLToPath(new URL("./bench-sandbox.sh", import.meta.url))),
    // Arm preparation. Wiring only: the procedures themselves live in
    // src/arms.ts and are called once, after thread/inject_items and before
    // the first scripted user turn (replay-proof.md §11.12 item 1). Omitting
    // --arm is NOT the same as "--arm full" in the record: "" means the
    // runner was never told, `full` means the no-preparation arm was chosen.
    arm: (() => {
      const value = read("--arm");
      if (value === undefined) return "";
      if (!isArm(value)) throw new Error(`--arm must be one of ${ARMS.join(", ")}; got ${value}`);
      return value;
    })(),
    serviceTier: read("--service-tier") ?? "",
    cache: (() => {
      const value = read("--cache") ?? "cold";
      if (!(CACHE_CONDITIONS as readonly string[]).includes(value)) {
        throw new Error(`--cache must be one of ${CACHE_CONDITIONS.join(", ")}; got ${value}`);
      }
      return value as CacheCondition;
    })(),
    warm: (() => {
      const value = read("--warm") ?? "self";
      if (!(WARM_MODES as readonly string[]).includes(value)) {
        throw new Error(`--warm must be one of ${WARM_MODES.join(", ")}; got ${value}`);
      }
      return value as WarmMode;
    })(),
    allowBusyHost: argv.includes("--allow-busy-host"),
    dryRun: argv.includes("--dry-run"),
  };
}

/**
 * Resolve `bin` (a name on PATH, e.g. the `codex-next` wrapper) down to the
 * real worker binary it eventually execs. `codex-next` is a shell wrapper
 * (`dir="..."; exec "$dir/codex" "$@"`), and `readlink -f` alone does not
 * follow that -- it only resolves symlinks in the path, and the wrapper is a
 * regular file, so plain `which` + `readlink -f` would report the wrapper
 * script itself, not codex-cli. Parsed generically (any `dir="X"` wrapper
 * that execs "$dir/codex") rather than hardcoding the current install path,
 * so a reinstall of codex-next does not silently go stale here.
 */
function resolveCodexBinary(bin: string): string {
  const which = execFileSync("which", [bin], { encoding: "utf8" }).trim();
  try {
    const content = readFileSync(which, "utf8");
    const dirMatch = /\bdir="([^"]+)"/.exec(content);
    if (dirMatch) {
      const candidate = join(dirMatch[1], "codex");
      if (existsSync(candidate)) return execFileSync("readlink", ["-f", candidate], { encoding: "utf8" }).trim();
    }
  } catch {
    // Not a readable text wrapper; fall through to resolving `which`'s own
    // result, which is correct when `bin` is already the real binary.
  }
  return execFileSync("readlink", ["-f", which], { encoding: "utf8" }).trim();
}

type Usage = {
  inputTokens: number;
  cachedInputTokens: number;
  outputTokens: number;
  reasoningOutputTokens?: number;
  totalTokens?: number;
};

function prepareCodexHome(target: string, serviceTier = ""): void {
  mkdirSync(target, { recursive: true });
  const auth = join(process.env.HOME ?? "", ".codex/auth.json");
  if (!existsSync(auth)) throw new Error(`missing ${auth}; log in with codex first`);
  copyFileSync(auth, join(target, "auth.json"));
  const extra = serviceTier ? `\n${SERVICE_TIER_CONFIG_KEY} = ${JSON.stringify(serviceTier)}\n` : "";
  writeFileSync(join(target, "config.toml"), `${WORKER_CONFIG}${extra}`);
}

// --- simulated user --------------------------------------------------------

type UserReply = { text: string; stop: string | null; usage: Record<string, unknown>; costUsd: number };

/**
 * Drive the simulated user with Claude Sonnet via the `claude` CLI in print
 * mode. The spec file replaces the default system prompt; the CLI runs in an
 * empty scratch cwd with no tools so the driver cannot read the repository,
 * the final diff, or the acceptance script.
 */
function askUser(args: Args, spec: string, transcript: { who: string; text: string }[], cwd: string): UserReply {
  const conversation = transcript
    .map((t) => `### ${t.who}\n${t.text}`)
    .join("\n\n");
  const prompt =
    `You are the simulated user. Follow the brief in your system prompt exactly.\n\n` +
    `Conversation so far (you are "User", the worker is "WORKER"):\n\n${conversation}\n\n` +
    `Write the user's next message. Under 60 words. If a stop condition in the brief is met, ` +
    `put the control token on its own final line. Output only the message.`;
  const raw = execFileSync(
    "claude",
    ["-p", "--model", args.userModel, "--output-format", "json", "--system-prompt-file", args.user],
    { cwd, input: prompt, encoding: "utf8", maxBuffer: 32 * 1024 * 1024, env: { ...process.env } },
  );
  const parsed = JSON.parse(raw) as { result?: string; usage?: Record<string, unknown>; total_cost_usd?: number };
  const text = String(parsed.result ?? "").trim();
  const stop = /^STOP:\s*([A-Z_]+)\s*$/m.exec(text)?.[1] ?? null;
  void spec;
  return { text, stop, usage: parsed.usage ?? {}, costUsd: Number(parsed.total_cost_usd ?? 0) };
}

// --- scripted simulated user ----------------------------------------------

type ScriptedTurn = { n: number; text: string; final?: boolean; stop?: string; rationale?: string };
type ScriptedUser = { version: number; turns: ScriptedTurn[]; changelog?: unknown[] };

/**
 * Load the frozen user script. Validated on load rather than trusted: an
 * empty turn, a missing `text`, or a script whose last turn is not marked
 * `final` would silently change the exchange count, which is exactly the
 * variance this fixture exists to remove.
 */
function loadScriptedUser(path: string): ScriptedUser {
  const raw = JSON.parse(readFileSync(path, "utf8")) as ScriptedUser;
  if (!Array.isArray(raw.turns) || raw.turns.length === 0) {
    throw new Error(`--scripted ${path}: no turns`);
  }
  raw.turns.forEach((t, i) => {
    if (typeof t.text !== "string" || !t.text.trim()) throw new Error(`--scripted ${path}: turn ${i + 1} has no text`);
  });
  const last = raw.turns[raw.turns.length - 1]!;
  if (!last.final) throw new Error(`--scripted ${path}: the last turn must be marked "final": true`);
  return raw;
}

// --- compaction assertion --------------------------------------------------

/**
 * Read the worker's own rollout and decide whether the thread stayed
 * full-context for the whole run.
 *
 * Two independent signals, because they fail at different times:
 *  - a `compacted` record, which the product writes when it folds the thread;
 *  - an input-token drop of more than 50% between consecutive requests, which
 *    is what compaction looks like in the usage stream even if the record is
 *    missing, renamed, or written to a different rollout.
 *
 * A third signal was tried and REMOVED: an `auto-compact-*` `turn_context`,
 * read as the product having decided at thread start that the injected history
 * needed compacting (replay-proof.md §11.5). It is a naming collision, not a
 * decision. `Session::next_internal_sub_id` (core/src/session/mod.rs:1282-1286)
 * formats EVERY internal turn id as `auto-compact-{n}` unconditionally, and
 * `thread/inject_items` mints one via `new_default_turn`
 * (core/src/codex_thread.rs:634 -> core/src/session/turn_context.rs:1082-1085),
 * so `auto-compact-0` appears at the start of any thread that had items
 * injected, compaction or no. It is still RECORDED, for the report's sake, but
 * it is no longer a violation. See replay-proof.md §11.9.
 *
 * replay-proof.md 8.4: run 7's request 1 was NOT compacted and looked clean;
 * compaction fired one request later. A run is only full-context if every
 * request in it is, so this checks the whole sequence and fails the run
 * loudly rather than reporting a green first request.
 *
 * THE SHAKE FALSE POSITIVE (arms-2026-09-11.md, finding 3). A shake writes a
 * `compacted` record of its own -- that is the rollout mechanism it rewrites
 * history through -- carrying the message "[shake] context reduced surgically",
 * and an arm's shake runs BEFORE the arm's first model request. Arms A and C
 * both exited 2 on it while being, demonstrably, full-context runs: 51 and 59
 * monotonically climbing requests with no input-token drop at all.
 *
 * So a `compacted` record is excused only when BOTH hold: it precedes the arm's
 * first model request, and it carries the shake marker. Either one alone is
 * still a violation -- a pre-request record with no marker means the injected
 * history was really compacted before the arm started (the arm's context is
 * then not what it is recorded as), and a marked record after the first request
 * means history was rewritten mid-run, which is equally not a full-context run.
 * Compaction after the first arm request and >50% input drops fail exactly as
 * before.
 */
const COMPACTION_DROP_RATIO = 0.5;

/**
 * Message the shake path writes on its own `compacted` record
 * (core/src/codex_thread.rs; observed verbatim in every shake-arm rollout).
 */
export const SHAKE_COMPACTION_MESSAGE = "[shake] context reduced surgically";

type CompactionCheck = {
  passed: boolean;
  rolloutPaths: string[];
  requests: Array<{ i: number; inputTokens: number; cachedInputTokens: number; outputTokens: number; timestamp: string }>;
  compactedEvents: Array<{
    timestamp: string;
    message: string;
    /** How many model requests had already been recorded when it was written. */
    requestsBefore: number;
    /** True when it carries the shake marker. */
    shake: boolean;
    /** True when it is excused: pre-first-arm-request AND shake-marked. */
    ignored: boolean;
  }>;
  /** Requests before the arm's first one (a --cache warm run's priming). */
  primingRequests: number;
  autoCompactTurnContexts: Array<{ timestamp: string; turnId: string }>;
  drops: Array<{ from: number; to: number; fromTokens: number; toTokens: number; ratio: number }>;
  violations: string[];
};

function findRollouts(dir: string): string[] {
  const found: string[] = [];
  const walk = (d: string): void => {
    let entries: string[];
    try {
      entries = readdirSync(d);
    } catch {
      return;
    }
    for (const name of entries) {
      const full = join(d, name);
      let isDir = false;
      try {
        isDir = statSync(full).isDirectory();
      } catch {
        continue;
      }
      if (isDir) walk(full);
      else if (name.startsWith("rollout-") && name.endsWith(".jsonl")) found.push(full);
    }
  };
  walk(join(dir, "sessions"));
  return found.sort();
}

export function checkCompaction(
  codexHome: string,
  threadId: string,
  options: {
    /**
     * 1-based index of the arm's FIRST model request. > 1 only for a warm-cache
     * run, whose priming request(s) precede the arm: their own input size is
     * not the arm's, so neither the drop check nor the compaction cutoff may
     * include them.
     */
    firstArmRequestIndex?: number;
  } = {},
): CompactionCheck {
  const firstArmRequestIndex = Math.max(1, options.firstArmRequestIndex ?? 1);
  const rolloutPaths = findRollouts(codexHome);
  const requests: CompactionCheck["requests"] = [];
  const compactedEvents: CompactionCheck["compactedEvents"] = [];
  const autoCompactTurnContexts: CompactionCheck["autoCompactTurnContexts"] = [];
  for (const path of rolloutPaths) {
    for (const line of readFileSync(path, "utf8").split("\n")) {
      if (!line.trim()) continue;
      let rec: Record<string, any>;
      try {
        rec = JSON.parse(line);
      } catch {
        continue;
      }
      if (rec.type === "compacted") {
        // Ordering is the signal: `requests` holds every model request counted
        // so far, so a record written while it is still short of the arm's
        // first request predates the arm.
        const message = String(rec.payload?.message ?? "");
        const shake = message.includes(SHAKE_COMPACTION_MESSAGE);
        const requestsBefore = requests.length;
        compactedEvents.push({
          timestamp: String(rec.timestamp ?? ""),
          message,
          requestsBefore,
          shake,
          ignored: requestsBefore < firstArmRequestIndex && shake,
        });
        continue;
      }
      if (rec.type === "turn_context") {
        const turnId = String(rec.payload?.turn_id ?? "");
        if (turnId.startsWith("auto-compact")) {
          autoCompactTurnContexts.push({ timestamp: String(rec.timestamp ?? ""), turnId });
        }
        continue;
      }
      if (rec.type !== "token_usage_record") continue;
      // threadId filter is best-effort: if the record does not name a thread,
      // keep it rather than silently dropping a request from the sequence.
      const recThread = rec.payload?.thread_id;
      if (threadId && recThread && String(recThread) !== threadId) continue;
      const usage = rec.payload?.usage ?? {};
      requests.push({
        i: requests.length + 1,
        inputTokens: Number(usage.input_tokens ?? 0),
        cachedInputTokens: Number(usage.cached_input_tokens ?? 0),
        outputTokens: Number(usage.output_tokens ?? 0),
        timestamp: String(rec.timestamp ?? ""),
      });
    }
  }

  const drops: CompactionCheck["drops"] = [];
  // Start at the arm's second request: the step from a priming request into the
  // arm's first is expected to be large (a warm-session priming primes the FULL
  // history and the arm may then shake it), and is not compaction.
  for (let i = Math.max(1, firstArmRequestIndex); i < requests.length; i += 1) {
    const prev = requests[i - 1]!.inputTokens;
    const cur = requests[i]!.inputTokens;
    if (prev > 0 && cur < prev * (1 - COMPACTION_DROP_RATIO)) {
      drops.push({ from: i, to: i + 1, fromTokens: prev, toTokens: cur, ratio: Number((cur / prev).toFixed(4)) });
    }
  }

  const violations: string[] = [];
  for (const e of compactedEvents) {
    if (e.ignored) continue;
    violations.push(
      `compaction event in the worker rollout at ${e.timestamp} ` +
        `(after ${e.requestsBefore} request(s), message ${JSON.stringify(e.message)}` +
        (e.requestsBefore < firstArmRequestIndex
          ? "; it precedes the arm's first request but is NOT the shake path, so the injected history was compacted"
          : e.shake
            ? "; it is the shake path but fires mid-run, so history was rewritten after the arm began"
            : "") +
        ")",
    );
  }
  for (const d of drops) {
    violations.push(
      `input tokens dropped ${Math.round((1 - d.ratio) * 100)}% between requests ${d.from} and ${d.to} ` +
        `(${d.fromTokens} -> ${d.toTokens})`,
    );
  }
  if (rolloutPaths.length === 0) violations.push(`no worker rollout found under ${codexHome} -- compaction could not be checked`);

  return {
    passed: violations.length === 0,
    rolloutPaths,
    requests,
    compactedEvents,
    primingRequests: firstArmRequestIndex - 1,
    autoCompactTurnContexts,
    drops,
    violations,
  };
}

// --- quota measurement -----------------------------------------------------
//
// The worker authenticates through ChatGPT login (auth_mode chatgpt), so the
// real, non-shadow price of a replay is PLAN QUOTA, not dollars. The app-server
// exposes it: `account/rateLimits/read` (app-server-protocol
// src/protocol/common.rs:1234, response v2::GetAccountRateLimitsResponse) is
// the pull channel, and `account/rateLimits/updated` the rolling push one. Both
// carry a RateLimitSnapshot of two RateLimitWindows -- primary and secondary --
// each `{ usedPercent, windowDurationMins, resetsAt }` (v2/account.rs:565,650).
// We take the snapshot by pull, twice: right before the first scripted user
// turn and right after the last, and store both plus the delta.
//
// `resetsAt` matters as much as the delta: a window that rolls over mid-run
// makes the delta meaningless (it can even go negative), so the reset instants
// are recorded on both ends and the report crosses them off by hand.

type RateLimitWindowSnapshot = {
  usedPercent: number;
  windowDurationMins: number | null;
  resetsAt: number | null;
};
type RateLimitSnapshot = {
  limitId?: string | null;
  limitName?: string | null;
  primary?: RateLimitWindowSnapshot | null;
  secondary?: RateLimitWindowSnapshot | null;
  planType?: string | null;
  rateLimitReachedType?: string | null;
};
type QuotaSnapshot = {
  at: string;
  atEpochMs: number;
  rateLimits?: RateLimitSnapshot;
  rateLimitsByLimitId?: Record<string, RateLimitSnapshot> | null;
  /** Every `codex` process alive at this instant, argv included. */
  codexProcesses: string[];
  error?: string;
};

/**
 * `pgrep -af codex` at both ends of the run. A concurrent Codex session --
 * the operator's own TUI, another agent -- spends the same plan quota as the
 * worker, so the delta is an upper bound on what THIS run cost unless nothing
 * else was running. Recording the census makes that a documented fact rather
 * than an assumption; it does not correct for it.
 */
function codexProcessCensus(): string[] {
  let lines: string[];
  try {
    lines = execFileSync("pgrep", ["-af", "codex"], { encoding: "utf8" })
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line.length > 0);
  } catch {
    // pgrep exits 1 when nothing matches; that is an empty census, not a fault.
    return [];
  }
  // Exclude this harness's own lineage. Every wrapper in it (npm exec, tsx, the
  // sandbox launcher) carries "codex" in a path or an argument, so a census that
  // counts them can never report a quiet host, however quiet the host is.
  const own = new Set<number>();
  for (let pid: number | undefined = process.pid; pid && !own.has(pid); ) {
    own.add(pid);
    try {
      const stat = readFileSync(`/proc/${pid}/stat`, "utf8");
      pid = Number(stat.slice(stat.lastIndexOf(")") + 1).trim().split(/\s+/)[1]);
    } catch {
      break;
    }
  }
  return lines.filter((line) => {
    const pid = Number(line.split(/\s+/)[0]);
    if (own.has(pid)) return false;
    return !line.includes("replay-01a07e54.ts") && !line.includes("codex-census.py");
  });
}

/**
 * Classified census: which of those processes spend the SAME plan quota this run
 * will. Not every codex process on this host does -- the personal account lives
 * in ~/.codex and a second Codex account/home lives elsewhere (path
 * configurable), and only same-account processes pollute a quota delta.
 * scripts/codex-census.py reads each process's
 * CODEX_HOME from /proc/<pid>/environ to decide. Best-effort: if it cannot run,
 * the unclassified census stands and every process counts against the host.
 */
function classifiedCensus(): Record<string, unknown> | null {
  try {
    const script = fileURLToPath(new URL("./codex-census.py", import.meta.url));
    const out = execFileSync("python3", [script, "--json"], { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] });
    return JSON.parse(out) as Record<string, unknown>;
  } catch (error) {
    // Exit 1 means "a personal-account process is alive", which is a RESULT, so
    // the JSON it printed on stdout is still what we want.
    const stdout = (error as { stdout?: string }).stdout;
    if (typeof stdout === "string" && stdout.trim().startsWith("{")) {
      try {
        return JSON.parse(stdout) as Record<string, unknown>;
      } catch {
        return null;
      }
    }
    return null;
  }
}

/**
 * Quiet-host precheck. Every concurrent codex session -- the operator's own TUI,
 * another agent -- authenticates as the same ChatGPT account and spends the same
 * plan quota, so any quota delta measured next to one is an UPPER BOUND on what
 * the run itself cost and not a measurement of it. That was the largest
 * methodological debt arms-2026-09-11.md left behind (36-65 other processes
 * alive across those runs), and this is the mechanical fix: refuse to start.
 *
 * Runs BEFORE the app-server is spawned, so this run contributes nothing to its
 * own census and an empty list means a genuinely quiet host.
 */
function quietHostPrecheck(allowBusyHost: boolean): Record<string, unknown> {
  const census = codexProcessCensus();
  const classified = classifiedCensus();
  // The number that matters is same-account processes, since those are the ones
  // spending this run's quota. Without a classification every process counts.
  const sameAccount = classified ? Number(classified.personal ?? 0) : census.length;
  const result: Record<string, unknown> = {
    checkedAt: new Date().toISOString(),
    processes: census,
    count: census.length,
    classified,
    sameAccountCount: sameAccount,
    quiet: sameAccount === 0,
    allowBusyHost,
  };
  if (sameAccount === 0) {
    console.error(
      `quiet-host precheck OK: no other same-account codex processes alive` +
        (census.length ? ` (${census.length} codex process(es) on other accounts, which spend other quota)` : ""),
    );
    return result;
  }
  console.error(
    `\n!!! HOST IS NOT QUIET: ${sameAccount} same-account codex process(es) alive before this run\n` +
      `    started (${census.length} codex process(es) in total) !!!\n` +
      `    They authenticate as the same ChatGPT account and spend the same plan quota, so this\n` +
      `    run's quota delta will be an UPPER BOUND on its own cost, not a measurement of it.\n`,
  );
  for (const line of census) console.error(`    ${line}`);
  if (!allowBusyHost) {
    console.error(
      `\n    Refusing to start. Close them, or pass --allow-busy-host to proceed with an\n` +
        `    unattributable quota number (which the report must then label as such).\n`,
    );
    result.refused = true;
    return result;
  }
  console.error(`\n    --allow-busy-host given: proceeding. The quota delta is NOT attributable to this run.\n`);
  return result;
}

async function readQuota(client: AppServerClient): Promise<QuotaSnapshot> {
  const at = new Date();
  const snapshot: QuotaSnapshot = {
    at: at.toISOString(),
    atEpochMs: at.getTime(),
    codexProcesses: codexProcessCensus(),
  };
  try {
    const response = await client.request<{
      rateLimits: RateLimitSnapshot;
      rateLimitsByLimitId?: Record<string, RateLimitSnapshot> | null;
    }>("account/rateLimits/read", undefined, 60_000);
    snapshot.rateLimits = response.rateLimits;
    snapshot.rateLimitsByLimitId = response.rateLimitsByLimitId ?? null;
  } catch (error) {
    snapshot.error = String(error instanceof Error ? error.message : error);
  }
  return snapshot;
}

function windowDelta(
  before: RateLimitWindowSnapshot | null | undefined,
  after: RateLimitWindowSnapshot | null | undefined,
): Record<string, unknown> | null {
  if (!before || !after) return null;
  // A window whose reset instant moved forward between the two reads rolled
  // over during the run: the percentages are then measured against different
  // windows and their difference is not a cost.
  const resetCrossed =
    before.resetsAt !== null && after.resetsAt !== null && before.resetsAt !== after.resetsAt;
  return {
    usedPercentBefore: before.usedPercent,
    usedPercentAfter: after.usedPercent,
    usedPercentDelta: after.usedPercent - before.usedPercent,
    windowDurationMins: after.windowDurationMins ?? before.windowDurationMins ?? null,
    resetsAtBefore: before.resetsAt,
    resetsAtAfter: after.resetsAt,
    resetsAtBeforeIso: before.resetsAt === null ? null : new Date(before.resetsAt * 1000).toISOString(),
    resetsAtAfterIso: after.resetsAt === null ? null : new Date(after.resetsAt * 1000).toISOString(),
    resetCrossed,
    valid: !resetCrossed,
  };
}

function quotaDelta(before: QuotaSnapshot | undefined, after: QuotaSnapshot | undefined): Record<string, unknown> {
  if (!before || !after) return { available: false, reason: "one or both snapshots missing" };
  if (before.error || after.error) {
    return { available: false, reason: `rateLimits read failed: ${before.error ?? ""} ${after.error ?? ""}`.trim() };
  }
  const b = before.rateLimits ?? {};
  const a = after.rateLimits ?? {};
  // Concurrent sessions are the delta's confound, so name them next to it.
  const concurrent = [...new Set([...before.codexProcesses, ...after.codexProcesses])].filter(
    (line) => !/app-server/.test(line) || false,
  );
  return {
    available: true,
    wallMs: after.atEpochMs - before.atEpochMs,
    planType: a.planType ?? b.planType ?? null,
    primary: windowDelta(b.primary, a.primary),
    secondary: windowDelta(b.secondary, a.secondary),
    codexProcessesBefore: before.codexProcesses,
    codexProcessesAfter: after.codexProcesses,
    concurrentCodexObserved: concurrent,
  };
}

function printQuotaDelta(delta: Record<string, unknown>): void {
  if (!delta.available) {
    console.error(`quota delta unavailable: ${delta.reason}`);
    return;
  }
  for (const name of ["primary", "secondary"] as const) {
    const w = delta[name] as Record<string, unknown> | null;
    if (!w) {
      console.error(`quota ${name}: not reported`);
      continue;
    }
    console.error(
      `quota ${name}: ${w.usedPercentBefore}% -> ${w.usedPercentAfter}% ` +
        `(delta ${(w.usedPercentDelta as number) >= 0 ? "+" : ""}${w.usedPercentDelta} pp, ` +
        `window ${w.windowDurationMins} min, resets ${w.resetsAtAfterIso})` +
        (w.resetCrossed ? "  !!! WINDOW RESET CROSSED MID-RUN -- DELTA INVALID !!!" : ""),
    );
  }
  const before = (delta.codexProcessesBefore as string[]) ?? [];
  const after = (delta.codexProcessesAfter as string[]) ?? [];
  console.error(`codex processes: ${before.length} at start, ${after.length} at end`);
}

// --- main ------------------------------------------------------------------

async function main(): Promise<void> {
  const args = parseArgs(process.argv.slice(2));
  mkdirSync(args.out, { recursive: true });

  // Quiet-host precheck first: nothing else has run yet, so the census is
  // purely other people's processes. A busy host is refused unless
  // --allow-busy-host, because the whole point of the quota channel is a number
  // that belongs to this run.
  const quietHost = quietHostPrecheck(args.allowBusyHost);
  if (quietHost.refused) {
    writeFileSync(join(args.out, "quiet-host-refused.json"), `${JSON.stringify(quietHost, null, 2)}\n`);
    process.exitCode = 3;
    return;
  }
  const history = JSON.parse(readFileSync(args.history, "utf8")) as {
    meta: Record<string, unknown>;
    items: Array<Record<string, unknown>>;
  };
  const spec = args.liveUser ? readFileSync(args.user, "utf8") : "";
  const scripted = args.liveUser ? null : loadScriptedUser(args.scripted);
  // A scripted run's exchange count is the script's length, full stop --
  // --max-turns cannot shorten or extend it, because a variable exchange
  // count is one of the things freezing the user is meant to remove.
  const plannedTurns = scripted ? scripted.turns.length : args.maxTurns;
  // Not under /tmp: codex refuses to create its PATH helper aliases (notably
  // the arg0 `apply_patch` shim) when CODEX_HOME is a temporary directory.
  const homesRoot = resolve(args.out, "..", "codex-homes");
  mkdirSync(homesRoot, { recursive: true });
  const codexHome = mkdtempSync(join(homesRoot, "replay-home-"));
  const userCwd = mkdtempSync(join(tmpdir(), "shake-replay-user-"));
  prepareCodexHome(codexHome, args.serviceTier);

  // Stubbed build toolchain. Real cargo/nextest/just runs of this workspace
  // take tens of minutes each and three at once take the machine down, so the
  // worker gets canned outputs taken from the original session's own tool
  // results. Every invocation is logged so the report can say what it tried.
  const stubDir = join(args.out, "stubs");
  rmSync(stubDir, { recursive: true, force: true });
  mkdirSync(stubDir, { recursive: true });
  const stubSource = fileURLToPath(new URL("../fixtures/stubs/", import.meta.url));
  for (const file of readdirSync(stubSource)) copyFileSync(join(stubSource, file), join(stubDir, file));
  for (const file of readdirSync(stubDir)) chmodSync(join(stubDir, file), 0o755);
  const stubLog = join(args.out, "stub-invocations.log");
  writeFileSync(join(stubDir, "stub-env"), `SHAKE_STUB_LOG=${JSON.stringify(stubLog)}\n`);
  writeFileSync(stubLog, "");
  process.env.PATH = `${stubDir}:${process.env.PATH ?? ""}`;
  // Probe --version through the host wrapper (codex-next), same as before
  // sandboxing existed: it still needs to resolve on the un-sandboxed PATH,
  // and this is only a version string, not part of the worker's run.
  const binVersion = execFileSync(args.bin, ["--version"], { encoding: "utf8" }).trim();
  const workerBinRealPath = (() => {
    try {
      return resolveCodexBinary(args.bin);
    } catch {
      return null;
    }
  })();

  // AppServerClient.spawn only forwards an env allowlist to the child
  // (src/appserver.ts INHERITED_ENV_KEYS), so sandbox parameters cannot ride
  // along as environment variables -- they have to be baked into what gets
  // exec'd. sandbox-exec.sh is a tiny generated launcher that bakes in the
  // absolute paths for this run and execs bench-sandbox.sh, which is itself
  // what actually execs the (sandboxed) codex binary.
  let spawnBin = args.bin;
  const sandboxRecord: Record<string, unknown> = { enabled: args.sandbox };
  if (args.sandbox) {
    if (!workerBinRealPath) throw new Error("--sandbox requires resolving the real codex binary; could not resolve it");
    const launcherPath = join(args.out, "sandbox-exec.sh");
    const launcherScript = [
      "#!/usr/bin/env bash",
      "set -euo pipefail",
      `exec ${JSON.stringify(args.sandboxCmd)} \\`,
      `  --tree ${JSON.stringify(args.tree)} \\`,
      `  --codex-home ${JSON.stringify(codexHome)} \\`,
      `  --out ${JSON.stringify(args.out)} \\`,
      `  --stubs ${JSON.stringify(stubDir)} \\`,
      `  --codex-bin ${JSON.stringify(workerBinRealPath)} \\`,
      `  -- "$@"`,
      "",
    ].join("\n");
    writeFileSync(launcherPath, launcherScript);
    chmodSync(launcherPath, 0o755);
    spawnBin = launcherPath;
    sandboxRecord.launcher = launcherPath;
    sandboxRecord.sandboxCmd = args.sandboxCmd;
    sandboxRecord.codexBin = workerBinRealPath;
    try {
      sandboxRecord.dryRunArgv = execFileSync(
        args.sandboxCmd,
        [
          "--dry-run",
          "--tree", args.tree,
          "--codex-home", codexHome,
          "--out", args.out,
          "--stubs", stubDir,
          "--codex-bin", workerBinRealPath,
          "--", "app-server",
        ],
        { encoding: "utf8" },
      ).trim();
    } catch (error) {
      sandboxRecord.dryRunError = String(error instanceof Error ? error.message : error);
    }
  }

  const client = AppServerClient.spawn({
    bin: spawnBin,
    codexHome,
    stderrLogPath: join(args.out, "app-server.stderr.log"),
  });

  const record: Record<string, unknown> = {
    startedAt: new Date().toISOString(),
    tree: args.tree,
    historyPath: args.history,
    historyMeta: history.meta,
    workerBin: args.bin,
    workerBinVersion: binVersion,
    workerBinRealPath,
    model: args.model,
    userModel: args.liveUser ? args.userModel : null,
    userDriver: args.liveUser ? { kind: "live", spec: args.user, model: args.userModel } : { kind: "scripted", script: args.scripted, version: scripted?.version, turns: scripted?.turns.length },
    maxTurns: args.maxTurns,
    plannedTurns,
    turnTimeoutMs: args.turnTimeoutMs,
    turnIdleTimeoutMs: args.turnIdleTimeoutMs,
    sandbox: sandboxRecord,
    stubDir,
    stubLog,
    arm: args.arm || null,
    // Recorded so a noread run cannot later be mistaken for a capability test.
    promptVariant: promptVariant((args.arm || "full") as Arm),
    noreadIsPromptOnly: promptVariant((args.arm || "full") as Arm) === "noread",
    serviceTier: args.serviceTier || null,
    serviceTierConfigKey: args.serviceTier ? SERVICE_TIER_CONFIG_KEY : null,
    cache: args.cache,
    warmMode: args.cache === "warm" ? args.warm : null,
    quietHost,
    dryRun: args.dryRun,
  };

  const transcript: { who: string; text: string }[] = [];
  const turns: Record<string, unknown>[] = [];
  const userUsage: Record<string, unknown>[] = [];
  const usageByTurn: Usage[] = [];
  let currentTurnId: string | undefined;
  let finalMessage = "";
  const warnings: string[] = [];
  let threadId = "";
  let compactionFailed = false;
  const observedContextWindows: number[] = [];
  const readArtifactItems = new Map<string, number>();
  const readArtifactByTurn: Array<{ turn: number; itemType: string }> = [];
  let quotaBefore: QuotaSnapshot | undefined;
  let quotaAfter: QuotaSnapshot | undefined;
  // 1-based index of the arm's first model request; > 1 only when a warm-cache
  // priming request precedes it. Hoisted because the finally block needs it.
  let firstArmRequestIndex = 1;
  /** Priming turns, in order. Hoisted so the finally block can price them. */
  const primingRunsForRecord: Record<string, unknown>[] = [];

  client.onNotification((n: Notification) => {
    if (n.params.threadId !== threadId) return;
    if (n.method === "warning") warnings.push(String(n.params.message ?? ""));
    if (n.method === "thread/tokenUsage/updated") {
      const tokenUsage = n.params.tokenUsage as { last?: Usage; modelContextWindow?: number | null } | undefined;
      const last = tokenUsage?.last;
      // A `last` carrying no input AND no output tokens is not a model request:
      // shake emits one after it rewrites history, to republish the thread's
      // recomputed size. Counting it shifted the warm-session run's
      // firstArmRequestIndex by one and mislabelled the arm's own first request
      // as priming (warm-2026-09-12.md, run (d)).
      const isRequest = last !== undefined && ((last.inputTokens ?? 0) > 0 || (last.outputTokens ?? 0) > 0);
      if (last && currentTurnId && isRequest) usageByTurn.push(last);
      // The product's own view of this thread's usable context window
      // (ThreadTokenUsage.model_context_window, app-server-protocol
      // v2/thread.rs:1871-1890, fed by TurnContext::model_context_window() =
      // ModelInfo::usable_context_window()). This is the only channel that
      // reports the window AFTER the catalog entry, the config override and
      // the effective-percent haircut have all been applied, so it -- not the
      // config file -- is what proves the override reached inference.
      const window = tokenUsage?.modelContextWindow;
      if (typeof window === "number" && !observedContextWindows.includes(window)) {
        observedContextWindows.push(window);
      }
    }
    if (n.method === "item/completed") {
      const item = n.params.item as { type?: string; text?: string } | undefined;
      if (item?.type === "agentMessage" && typeof item.text === "string") finalMessage = item.text;
      // Retrieval census. Counts the OUTER read_artifact calls the app-server
      // reports as completed items -- which is all this channel can see. A
      // read_artifact the worker issues from inside code mode (a nested call
      // within one exec item) does not surface as its own item/completed, so
      // this is a lower bound on retrieval, not a total. Recorded by item type
      // so the report can say which channel each count came from.
      if (item && item.type !== "agentMessage") {
        let serialized = "";
        try {
          serialized = JSON.stringify(item);
        } catch {
          serialized = "";
        }
        if (serialized.includes("read_artifact")) {
          const key = `${item.type ?? "unknown"}`;
          readArtifactItems.set(key, (readArtifactItems.get(key) ?? 0) + 1);
          readArtifactByTurn.push({ turn: turns.length + 1, itemType: key });
        }
      }
    }
  });

  try {
    await client.initialize(CLIENT_INFO);
    const started = await client.request<{ thread: { id: string }; serviceTier?: string | null }>("thread/start", {
      cwd: args.tree || process.cwd(),
      approvalPolicy: "never",
      sandbox: "danger-full-access",
      threadSource: "user",
      model: args.model,
      developerInstructions:
        promptVariant((args.arm || "full") as Arm) === "noread"
          ? `${WORKER_DEVELOPER_INSTRUCTIONS}\n\n${WORKER_NOREAD_INSTRUCTION}`
          : WORKER_DEVELOPER_INSTRUCTIONS,
      // Double-optional on the wire: omitted = account default, null =
      // explicitly no tier, string = that tier. Omit rather than send null so
      // an un-flagged run is byte-identical to run 9's request.
      ...(args.serviceTier ? { serviceTier: args.serviceTier } : {}),
    });
    threadId = started.thread.id;
    record.threadId = threadId;
    // Evidence channel 0: the tier the server resolved, echoed back. A
    // requested tier that does not come back is a tier that did not apply.
    record.serviceTierApplied = started.serviceTier ?? null;
    console.error(
      `service tier: requested ${args.serviceTier || "(account default)"}, ` +
        `server reports ${JSON.stringify(started.serviceTier ?? null)}`,
    );
    if (args.serviceTier && !started.serviceTier) {
      record.serviceTierWarning =
        `requested service tier ${args.serviceTier} but thread/start echoed none; ` +
        `the run may be on the account default tier`;
      console.error(`!!! ${record.serviceTierWarning} !!!`);
    }

    // Evidence channel 1 (pre-turn): the effective config the server actually
    // loaded from this run's CODEX_HOME. Proves the override was parsed and is
    // in force, and -- just as importantly -- that
    // model_auto_compact_token_limit is still unset, since
    // ModelInfo::auto_compact_token_limit() min()s any configured value in and
    // so could only ever lower the limit back down.
    try {
      const cfg = await client.request<{ config: Record<string, unknown> }>("config/read", { cwd: args.tree || process.cwd() });
      record.effectiveConfig = {
        model_context_window: cfg.config?.model_context_window ?? null,
        model_auto_compact_token_limit: cfg.config?.model_auto_compact_token_limit ?? null,
        model_auto_compact_token_limit_scope: cfg.config?.model_auto_compact_token_limit_scope ?? null,
        service_tier: cfg.config?.service_tier ?? null,
      };
      console.error(`effective config: ${JSON.stringify(record.effectiveConfig)}`);
    } catch (error) {
      record.effectiveConfigError = String(error instanceof Error ? error.message : error);
      console.error(`config/read failed: ${record.effectiveConfigError}`);
    }

    // --dry-run stops here: a live app-server, a real thread on the worker's own
    // config and tier, config/read, and both rate-limit snapshots -- and not one
    // model token. This is the smoke test for the quiet-host census, the tier
    // echo and the quota channel.
    if (args.dryRun) {
      quotaBefore = await readQuota(client);
      record.quotaBefore = quotaBefore;
      console.error(
        `quota before: primary ${quotaBefore.rateLimits?.primary?.usedPercent ?? "?"}%, ` +
          `secondary ${quotaBefore.rateLimits?.secondary?.usedPercent ?? "?"}%, ` +
          `${quotaBefore.codexProcesses.length} codex processes alive` +
          (quotaBefore.error ? `  (read failed: ${quotaBefore.error})` : ""),
      );
      record.stopReason = "DRY_RUN";
      console.error(`\ndry run: thread started, config and rate limits read, no turn sent`);
      return;
    }

    // Inject the pre-built history in chunks; one 3 MB JSON-RPC line is
    // needlessly fragile.
    const CHUNK = 100;
    for (let i = 0; i < history.items.length; i += CHUNK) {
      await client.request("thread/inject_items", { threadId, items: history.items.slice(i, i + CHUNK) });
    }

    // Token size of the injected history, measured with the product's own
    // counter (the same call src/arms.ts uses). `preview` never mutates.
    const preview = await client.request<{ preview: { tokensBefore: number; tokensAfter: number; toolOutputs: number; unavailableReason?: string | null } }>(
      "thread/shake/preview",
      { threadId, mode: "elide" },
      120_000,
    );
    record.historyTokens = preview.preview.tokensBefore;
    record.historyTokensAfterElide = preview.preview.tokensAfter;
    record.historyToolOutputs = preview.preview.toolOutputs;
    // What prepareArm will assert the shake sees. Re-measured after a session
    // priming turn, which appends an exchange to the history before the shake.
    let expectedToolOutputs = preview.preview.toolOutputs;
    console.error(
      `history ${history.items.length} items, ${history.meta.bytes} bytes, ` +
        `${preview.preview.tokensBefore} tokens (elide would leave ${preview.preview.tokensAfter})`,
    );
    if (args.measureOnly) {
      writeFileSync(join(args.out, "measure.json"), `${JSON.stringify(record, null, 2)}\n`);
      return;
    }

    // --- cache priming ----------------------------------------------------
    //
    // ONE request, immediately before the arm's first turn, whose prefix is
    // byte-identical to what the arm's request 1 will send: same thread, same
    // developer instructions, same model, same tier, same history -- the only
    // way to warm a prefix is to send it. The user message is a one-line
    // no-tools answer, so the only part of the priming that is not already the
    // arm's own prefix is a handful of output tokens.
    //
    // Its cost is recorded SEPARATELY as priming overhead: it is paid, but it is
    // not the arm, and the arm's per-request figures would be wrong if it were
    // folded in.
    let primingFinishedAt = 0;

    const readQuotaBefore = async (why: string): Promise<void> => {
      if (quotaBefore) return;
      quotaBefore = await readQuota(client);
      record.quotaBefore = quotaBefore;
      record.quotaBeforeReadAt = why;
      console.error(
        `quota before (${why}): primary ${quotaBefore.rateLimits?.primary?.usedPercent ?? "?"}%, ` +
          `secondary ${quotaBefore.rateLimits?.secondary?.usedPercent ?? "?"}%, ` +
          `${quotaBefore.codexProcesses.length} codex processes alive` +
          (quotaBefore.error ? `  (read failed: ${quotaBefore.error})` : ""),
      );
    };

    const prime = async (mode: WarmMode): Promise<void> => {
      const usageBefore = usageByTurn.length;
      const t0 = Date.now();
      console.error(
        `\npriming the cache (--cache warm --warm ${mode}): one turn over the ` +
          `${mode === "session" ? "FULL, pre-arm" : "post-arm"} history, one-line reply expected ...`,
      );
      const startedTurn = await client.request<{ turn: { id: string } }>("turn/start", {
        threadId,
        input: [{ type: "text", text: PRIMING_USER_MESSAGE }],
        effort: "medium",
        cwd: args.tree,
      });
      currentTurnId = startedTurn.turn.id;
      const isThisTurn = (n: Notification) =>
        n.params.threadId === threadId &&
        ((n.params.turn as { id?: string } | undefined)?.id === currentTurnId || n.params.turnId === currentTurnId);
      const completed = await client.waitForIdle(
        (n) => (n.method === "turn/completed" || n.method === "turn/failed") && isThisTurn(n),
        (n) => n.params.threadId === threadId,
        // A priming turn is one request and a one-line answer; it does not get
        // the multi-hour budget a working turn does.
        { idleMs: Math.min(args.turnIdleTimeoutMs, 15 * 60 * 1000), absoluteMs: Math.min(args.turnTimeoutMs, 30 * 60 * 1000) },
        `priming turn ${currentTurnId}`,
      );
      const latencyMs = Date.now() - t0;
      primingFinishedAt = Date.now();
      const turnObj = completed.params.turn as { status?: string } | undefined;
      const usage = usageByTurn.slice(usageBefore);
      const entry = {
        mode,
        turnId: currentTurnId,
        status: turnObj?.status ?? completed.method,
        latencyMs,
        requests: usage.length,
        inputTokens: usage.reduce((sum, u) => sum + (u.inputTokens ?? 0), 0),
        cachedInputTokens: usage.reduce((sum, u) => sum + (u.cachedInputTokens ?? 0), 0),
        outputTokens: usage.reduce((sum, u) => sum + (u.outputTokens ?? 0), 0),
        reply: finalMessage.slice(0, 200),
      };
      primingRunsForRecord.push(entry);
      record.priming = primingRunsForRecord;
      console.error(
        `primed in ${Math.round(latencyMs / 1000)}s: ${entry.requests} request(s), ` +
          `${entry.inputTokens} input / ${entry.cachedInputTokens} cached / ${entry.outputTokens} output tokens, ` +
          `reply ${JSON.stringify(entry.reply)}`,
      );
      if (entry.requests !== 1) {
        console.error(`    note: priming took ${entry.requests} requests, not 1 -- its overhead is that much larger`);
      }
      finalMessage = "";
      // Nothing between the priming turn and the arm's first turn belongs to a
      // turn, so no notification in that window may be attributed to one.
      currentTurnId = undefined;
    };

    // warm/session primes BEFORE the arm, on the unshaken history, which is what
    // makes a shake arm miss on request 1 and recover from request 2.
    if (args.cache === "warm" && args.warm === "session") {
      await readQuotaBefore("before the session priming request, so the delta includes priming");
      await prime("session");
      // The priming exchange is now part of the history the shake will see, so
      // re-measure rather than asserting the pre-priming tool-output count.
      try {
        const afterPriming = await client.request<{ preview: { tokensBefore: number; toolOutputs: number } }>(
          "thread/shake/preview",
          { threadId, mode: "elide" },
          120_000,
        );
        record.historyTokensAfterPriming = afterPriming.preview.tokensBefore;
        record.historyToolOutputsAfterPriming = afterPriming.preview.toolOutputs;
        expectedToolOutputs = afterPriming.preview.toolOutputs;
        console.error(
          `history after session priming: ${afterPriming.preview.tokensBefore} tokens, ` +
            `${afterPriming.preview.toolOutputs} tool outputs` +
            (afterPriming.preview.toolOutputs === (record.historyToolOutputs as number)
              ? " (unchanged, as expected for a no-tools priming turn)"
              : " (CHANGED: the priming turn used tools)"),
        );
      } catch (error) {
        record.historyTokensAfterPrimingError = String(error instanceof Error ? error.message : error);
      }
    }

    // Arm preparation: exactly one call, after thread/inject_items and before
    // the first scripted user turn (replay-proof.md §11.12 item 1). Wiring
    // only -- the procedures are src/arms.ts's, unchanged.
    if (args.arm) {
      const driver: Driver = {
        request: (method, params, timeoutMs) => client.request(method, params, timeoutMs),
        waitFor: (predicate, timeoutMs, label) => client.waitFor(predicate, timeoutMs, label),
        now: () => Date.now(),
      };
      console.error(`\npreparing arm ${args.arm} ...`);
      const preparation: ArmPreparation = await prepareArm(driver, threadId, args.arm, expectedToolOutputs);
      record.armPreparation = preparation;
      console.error(
        `arm ${args.arm} prepared in ${Math.round(preparation.totalLatencyMs / 1000)}s` +
          (preparation.shake
            ? `: shake ${preparation.shake.preview.tokensBefore} -> ${preparation.shake.preview.tokensAfter} tokens ` +
              `over ${preparation.shake.preview.toolOutputs} tool outputs ("${preparation.shake.warning}")`
            : "") +
          (preparation.compact ? `: compacted via ${preparation.compact.completedVia}` : ""),
      );
      // Post-preparation history size, by the product's own counter, so the
      // arm's actual starting context is recorded and not inferred from the
      // preview it was planned from.
      try {
        const after = await client.request<{ preview: { tokensBefore: number; toolOutputs: number } }>(
          "thread/shake/preview",
          { threadId, mode: "elide" },
          120_000,
        );
        record.historyTokensAfterArm = after.preview.tokensBefore;
        console.error(`history after arm ${args.arm}: ${after.preview.tokensBefore} tokens`);
      } catch (error) {
        record.historyTokensAfterArmError = String(error instanceof Error ? error.message : error);
      }
    }

    // warm/self primes AFTER the arm, on the arm's own post-arm history, so the
    // arm's request 1 hits and what is measured is its steady state.
    if (args.cache === "warm" && args.warm === "self") {
      await readQuotaBefore("before the self priming request, so the delta includes priming");
      await prime("self");
    }

    // Quota, read 1 of 2: right before the first user turn, after every
    // preparation that could itself spend quota (the `compact` arms run a real
    // model turn). A no-op if a priming read already happened above. Paired with
    // read 2 in the finally block.
    await readQuotaBefore("after arm preparation, before the first user turn");

    // The arm's own first model request. Everything before it -- a priming
    // request -- is overhead, and both the compaction cutoff and the drop check
    // have to know where the arm starts.
    firstArmRequestIndex = usageByTurn.length + 1;
    record.firstArmRequestIndex = firstArmRequestIndex;
    if (args.cache === "warm") {
      const gapMs = primingFinishedAt ? Date.now() - primingFinishedAt : null;
      record.primingToFirstTurnMs = gapMs;
      if (gapMs !== null && gapMs > WARM_START_MAX_GAP_MS) {
        console.error(
          `\n!!! WARM START WINDOW MISSED: ${Math.round(gapMs / 1000)}s between the priming request and the\n` +
            `    first arm turn, against a ${WARM_START_MAX_GAP_MS / 1000}s budget. The prefix may have decayed;\n` +
            `    check the request-1 cached fraction below before quoting this as a warm run.\n`,
        );
      } else if (gapMs !== null) {
        console.error(`warm start: ${(gapMs / 1000).toFixed(1)}s from priming to the first arm turn`);
      }
    }

    let stopReason = "TURN_LIMIT";

    for (let turn = 1; turn <= plannedTurns; turn += 1) {
      // Scripted: the turn is sent as soon as the previous worker turn ends,
      // whatever it said. Nothing reads the worker's output, so the user side
      // of the transcript is identical in every arm.
      const scriptTurn = scripted?.turns[turn - 1];
      const user = scriptTurn
        ? { text: scriptTurn.text, stop: null as string | null, usage: {} as Record<string, unknown>, costUsd: 0 }
        : askUser(args, spec, transcript, userCwd);
      if (!scriptTurn) userUsage.push({ turn, ...user.usage, costUsd: user.costUsd });
      const userText = user.text.replace(/^STOP:\s*[A-Z_]+\s*$/m, "").trim();
      transcript.push({ who: "User", text: userText });
      console.error(`\n--- exchange ${turn} / User ---\n${userText}\n`);
      if (user.stop && turn > 1) {
        stopReason = user.stop;
        break;
      }

      const usageBefore = usageByTurn.length;
      finalMessage = "";
      const startedTurn = await client.request<{ turn: { id: string } }>("turn/start", {
        threadId,
        input: [{ type: "text", text: userText }],
        effort: "medium",
        cwd: args.tree,
      });
      currentTurnId = startedTurn.turn.id;
      const t0 = Date.now();
      const isThisTurn = (n: Notification) =>
        n.params.threadId === threadId &&
        ((n.params.turn as { id?: string } | undefined)?.id === currentTurnId || n.params.turnId === currentTurnId);
      let completed: Notification;
      try {
        completed = await client.waitForIdle(
          (n) => (n.method === "turn/completed" || n.method === "turn/failed") && isThisTurn(n),
          // Any notification scoped to this thread counts as progress, not
          // just ones scoped to the turn id: item/completed and
          // thread/tokenUsage/updated are the two that matter in practice,
          // and both carry threadId (turnId is not always present on them).
          (n) => n.params.threadId === threadId,
          { idleMs: args.turnIdleTimeoutMs, absoluteMs: args.turnTimeoutMs },
          `worker turn ${currentTurnId}`,
        );
      } catch (error) {
        // A timed-out turn still needs to show up as an exchange (with
        // whatever partial signal we have) so the record's `turns` array and
        // the finally block's replay.json write reflect that this exchange
        // was attempted, not silently dropped.
        record.turnTimeoutError = String(error instanceof Error ? error.message : error);
        throw error;
      }
      const latencyMs = Date.now() - t0;
      const turnObj = completed.params.turn as { status?: string; items?: Array<Record<string, unknown>> } | undefined;
      if (!finalMessage) {
        for (const item of turnObj?.items ?? []) {
          if (item.type === "agentMessage" && typeof item.text === "string") finalMessage = item.text;
        }
      }
      const usage = usageByTurn.slice(usageBefore);
      turns.push({
        turn,
        turnId: currentTurnId,
        status: turnObj?.status ?? completed.method,
        latencyMs,
        requests: usage.length,
        inputTokens: usage.reduce((s, u) => s + (u.inputTokens ?? 0), 0),
        cachedInputTokens: usage.reduce((s, u) => s + (u.cachedInputTokens ?? 0), 0),
        outputTokens: usage.reduce((s, u) => s + (u.outputTokens ?? 0), 0),
        finalMessage,
      });
      console.error(`--- exchange ${turn} / worker (${turnObj?.status}, ${Math.round(latencyMs / 1000)}s) ---\n${finalMessage.slice(0, 1500)}\n`);
      transcript.push({ who: "WORKER", text: finalMessage || "(no message)" });
      if ((turnObj?.status ?? completed.method) === "failed") {
        stopReason = "WORKER_TURN_FAILED";
        break;
      }
      // The script's last turn tells the worker to stop and summarize; the run
      // ends once that summary lands, so the exchange count is fixed.
      if (scriptTurn?.final) {
        stopReason = scriptTurn.stop ?? "SCRIPT_END";
        break;
      }
    }

    record.stopReason = stopReason;
    record.workerTotals = {
      inputTokens: turns.reduce((s, t) => s + (t.inputTokens as number), 0),
      cachedInputTokens: turns.reduce((s, t) => s + (t.cachedInputTokens as number), 0),
      outputTokens: turns.reduce((s, t) => s + (t.outputTokens as number), 0),
      wallMs: turns.reduce((s, t) => s + (t.latencyMs as number), 0),
    };
    record.userTotals = {
      inputTokens: userUsage.reduce((s, u) => s + Number(u.input_tokens ?? 0), 0),
      cacheReadInputTokens: userUsage.reduce((s, u) => s + Number(u.cache_read_input_tokens ?? 0), 0),
      cacheCreationInputTokens: userUsage.reduce((s, u) => s + Number(u.cache_creation_input_tokens ?? 0), 0),
      outputTokens: userUsage.reduce((s, u) => s + Number(u.output_tokens ?? 0), 0),
      costUsd: userUsage.reduce((s, u) => s + Number(u.costUsd ?? 0), 0),
    };
  } catch (error) {
    record.error = String(error instanceof Error ? error.stack ?? error.message : error);
    record.stopReason = record.stopReason ?? "ERROR";
    console.error(`replay error: ${record.error}`);
  } finally {
    // These are filled in as the run proceeds, so a timed-out or failed run
    // still reports every completed exchange and every token it spent.
    record.turns = turns;
    record.userUsage = userUsage;
    record.warnings = warnings;
    record.transcript = transcript;
    record.usageByTurn = usageByTurn;
    // Quota, read 2 of 2: as soon after the last turn as possible, and before
    // the app-server is closed (the method is served by that process). In the
    // finally block on purpose: a run that errored or timed out still spent
    // quota, and that spend has to be recorded.
    if (quotaBefore) {
      quotaAfter = await readQuota(client);
      record.quotaAfter = quotaAfter;
      const delta = quotaDelta(quotaBefore, quotaAfter);
      record.quotaDelta = delta;
      console.error("");
      printQuotaDelta(delta);
    }
    // Retrieval census: outer read_artifact calls only -- see the handler.
    record.readArtifactCalls = {
      total: [...readArtifactItems.values()].reduce((a, b) => a + b, 0),
      byItemType: Object.fromEntries(readArtifactItems),
      byTurn: readArtifactByTurn,
      note:
        "Outer item/completed calls mentioning read_artifact only. A read_artifact " +
        "issued from inside code mode does not surface as its own completed item, " +
        "so this is a lower bound on retrieval, not a total.",
    };
    console.error(
      `read_artifact (outer calls): ${[...readArtifactItems.values()].reduce((a, b) => a + b, 0)}` +
        ` ${JSON.stringify(Object.fromEntries(readArtifactItems))}`,
    );
    // Close the app-server before reading its rollout: the worker's last
    // token_usage_record (and any `compacted` record) is only guaranteed on
    // disk once the process that writes it has exited.
    await client.close();
    // Keep the worker's own rollout: it is the only record of per-request
    // token usage, auto-compaction, and whether the worker wandered outside
    // its tree. Only the credential copy is removed.
    rmSync(join(codexHome, "auth.json"), { force: true });
    record.codexHome = codexHome;
    rmSync(userCwd, { recursive: true, force: true });
    // Compaction assertion. Runs even on an errored or timed-out run: a run
    // that compacted is not a full-context run and must not be quoted as one,
    // whatever else happened to it. Recorded in replay.json AND made loud on
    // stderr; main() exits non-zero when it fails.
    // Evidence channel 2 (per-request): what the product reported as this
    // thread's usable context window while it was actually running.
    record.contextWindow = {
      expectedUsable: EXPECTED_USABLE_CONTEXT_WINDOW,
      observed: observedContextWindows,
      // Not an error on a run that never got a usage notification (e.g.
      // --measure-only or a thread/start failure); only a mismatch is.
      ok: observedContextWindows.length === 0 || observedContextWindows.every((w) => w === EXPECTED_USABLE_CONTEXT_WINDOW),
    };
    if (!(record.contextWindow as { ok: boolean }).ok) {
      console.error(
        `\n!!! CONTEXT WINDOW OVERRIDE DID NOT TAKE EFFECT !!!\n` +
          `    expected usable window ${EXPECTED_USABLE_CONTEXT_WINDOW}, server reported ${observedContextWindows.join(", ")}`,
      );
      compactionFailed = true;
    } else if (observedContextWindows.length > 0) {
      console.error(`context window OK: server reports usable window ${observedContextWindows.join(", ")}`);
    }
    if (threadId && !args.dryRun) {
      try {
        record.compaction = checkCompaction(codexHome, threadId, { firstArmRequestIndex });
      } catch (error) {
        record.compaction = { passed: false, error: String(error instanceof Error ? error.message : error) };
      }
      // --- cache condition -------------------------------------------------
      //
      // The mechanical check §11.12 asked for, now run in both directions: a
      // cold arm's request 1 must find essentially nothing cached, and a
      // warm/self arm's must find almost everything. warm/session is asymmetric
      // by design -- a shake arm MISSES on request 1, because the prefix in the
      // cache is the history the shake just rewrote, and recovers from request
      // 2 -- so it is reported rather than bounded.
      //
      // Logged, never fatal: a missed cache condition is a fact about the run
      // that the report has to carry, not a reason to throw away six exchanges
      // of worker output.
      const reqs = (record.compaction as CompactionCheck | undefined)?.requests ?? [];
      const fraction = (r: { inputTokens: number; cachedInputTokens: number }): number =>
        r.inputTokens > 0 ? Number((r.cachedInputTokens / r.inputTokens).toFixed(6)) : 0;
      const priming = reqs.slice(0, firstArmRequestIndex - 1);
      const armRequests = reqs.slice(firstArmRequestIndex - 1);
      const shakeArm = args.arm === "shake-elide" || args.arm === "shake-elide-noread" || args.arm === "shake-then-compact";
      const first = armRequests[0];
      const firstFraction = first ? fraction(first) : null;
      const secondFraction = armRequests[1] ? fraction(armRequests[1]!) : null;
      let rule = "";
      let met: boolean | null = null;
      if (args.cache === "cold") {
        rule = `cold: request 1 cached fraction < ${COLD_CACHED_FRACTION_MAX}`;
        met = firstFraction === null ? null : firstFraction < COLD_CACHED_FRACTION_MAX;
      } else if (args.warm === "self") {
        rule = `warm/self: request 1 cached fraction > ${WARM_CACHED_FRACTION_MIN}`;
        met = firstFraction === null ? null : firstFraction > WARM_CACHED_FRACTION_MIN;
      } else if (shakeArm) {
        rule =
          `warm/session on a shake arm: request 1 cached fraction < ${COLD_CACHED_FRACTION_MAX} (the shake ` +
          `invalidated the primed prefix) and request 2 > ${WARM_CACHED_FRACTION_MIN} (recovered)`;
        met =
          firstFraction === null || secondFraction === null
            ? null
            : firstFraction < COLD_CACHED_FRACTION_MAX && secondFraction > WARM_CACHED_FRACTION_MIN;
      } else {
        rule = `warm/session on a non-shake arm: request 1 cached fraction > ${WARM_CACHED_FRACTION_MIN}`;
        met = firstFraction === null ? null : firstFraction > WARM_CACHED_FRACTION_MIN;
      }
      const sum = (rows: typeof reqs, key: "inputTokens" | "cachedInputTokens" | "outputTokens"): number =>
        rows.reduce((total, r) => total + r[key], 0);
      record.cacheCondition = {
        cache: args.cache,
        warm: args.cache === "warm" ? args.warm : null,
        firstArmRequestIndex,
        primingRequests: priming.length,
        // Paid, but not part of the arm. Priced by scripts/run-cost.py and
        // scripts/arms-row.py as "priming overhead".
        primingOverhead: {
          requests: priming.length,
          inputTokens: sum(priming, "inputTokens"),
          cachedInputTokens: sum(priming, "cachedInputTokens"),
          outputTokens: sum(priming, "outputTokens"),
          runs: primingRunsForRecord,
        },
        primingToFirstTurnMs: record.primingToFirstTurnMs ?? null,
        warmStartWindowMs: WARM_START_MAX_GAP_MS,
        firstArmRequestCachedFraction: firstFraction,
        secondArmRequestCachedFraction: secondFraction,
        perRequestCachedFraction: armRequests.map((r) => ({ i: r.i, inputTokens: r.inputTokens, cachedInputTokens: r.cachedInputTokens, fraction: fraction(r) })),
        assertionRule: rule,
        assertionMet: met,
      };
      console.error(
        `\ncache condition: ${args.cache}${args.cache === "warm" ? `/${args.warm}` : ""} — ` +
          `arm request 1 cached fraction ${firstFraction ?? "?"}` +
          (secondFraction === null ? "" : `, request 2 ${secondFraction}`) +
          (priming.length ? `; ${priming.length} priming request(s), ${sum(priming, "inputTokens")} input tokens of overhead` : ""),
      );
      console.error(`    rule: ${rule} — ${met === null ? "NOT EVALUATED (no requests)" : met ? "MET" : "NOT MET"}`);
      if (met === false) {
        console.error(
          `    !!! the cache condition this run was launched under was NOT achieved. The run is still\n` +
            `        usable, but it must be reported as ${args.cache === "cold" ? "partially warm" : "not warm"}, not as ${args.cache}. !!!`,
        );
      }
    }
    record.finishedAt = new Date().toISOString();
    writeFileSync(join(args.out, "replay.json"), `${JSON.stringify(record, null, 2)}\n`);
    console.error(`\nwrote ${join(args.out, "replay.json")}`);
    const c = record.compaction as CompactionCheck | { passed: boolean; error?: string } | undefined;
    if (c && !c.passed) {
      console.error("\n!!! COMPACTION ASSERTION FAILED — this run is NOT full-context !!!");
      for (const v of (c as CompactionCheck).violations ?? [String((c as { error?: string }).error)]) console.error(`    ${v}`);
      compactionFailed = true;
    } else if (c) {
      const reqs = (c as CompactionCheck).requests;
      const ignored = ((c as CompactionCheck).compactedEvents ?? []).filter((e) => e.ignored);
      console.error(
        `compaction assertion PASSED: ${reqs.length} requests, no compaction event, ` +
          `no >${Math.round(COMPACTION_DROP_RATIO * 100)}% input-token drop ` +
          `(peak input ${Math.max(0, ...reqs.map((r) => r.inputTokens))})` +
          (ignored.length
            ? `; ignored ${ignored.length} pre-run shake \`compacted\` record(s) at ` +
              `${ignored.map((e) => e.timestamp).join(", ")}`
            : ""),
      );
    }
  }
  if (compactionFailed) process.exitCode = 2;
}

if (process.argv[1] && resolve(process.argv[1]).endsWith("replay-01a07e54.ts")) {
  main().catch((error) => {
    console.error(error);
    process.exit(1);
  });
}
