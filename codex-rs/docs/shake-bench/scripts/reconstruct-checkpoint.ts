#!/usr/bin/env node
// Reconstruct the dirty working tree of a Codex coding session at a chosen
// cutoff timestamp, as a cutoff-only repo copy with no later git history.
//
// Phase 1 of the shake replay benchmark. Given
//   (a) a base commit in a read-only source repo,
//   (b) a cutoff timestamp, and
//   (c) the orchestrator rollout plus every subagent rollout it spawned,
// this script exports the base tree, replays every file-mutating tool call
// whose timestamp is <= the cutoff in global timestamp order, and then
// re-initialises git so the copy carries exactly one commit and no history
// from after the cutoff.
//
// Record shapes are the same ones `scripts/mine-checkpoint.ts` parses. The
// mutation-bearing record is:
//   {type:"response_item", timestamp, ordinal,
//    payload:{type:"custom_tool_call", name:"exec", call_id, input:<JS source>}}
// where <JS source> is Codex "code mode": JavaScript that calls
//   tools.apply_patch(<patch string>)     -> file mutation
//   tools.exec_command({cmd, workdir})    -> shell command
//   tools.write_stdin({...})              -> input to a background session
// The matching {type:"custom_tool_call_output", call_id, output:[{text}...]}
// record says whether the script completed or failed; a failed apply_patch
// never touched the worktree, so it is skipped.
//
// Shell commands are NEVER replayed unless they match SAFE_SHELL_ALLOWLIST.
// Anything else that looks like it could write to the worktree is recorded in
// the manual-review list instead of guessed at.
//
// Usage:
//   tsx scripts/reconstruct-checkpoint.ts \
//     --base ceff9549eb2b78d11f8560992991d1cd4259fba1 \
//     --cutoff 2026-09-08T01:57:36.059Z \
//     --out $BENCH_CHECKPOINTS/01a07e54-r264 \
//     --label "checkpoint 01a07e54@r264" \
//     [--repo <path to the fork checkout>] \
//     [--rollout <file> ...]
//
// The script is idempotent: the output dir is wiped and rebuilt on entry.

import { execFileSync, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync, symlinkSync, realpathSync } from "node:fs";
import { basename, dirname, join, resolve } from "node:path";
import { tmpdir } from "node:os";

// --- CLI ------------------------------------------------------------------

type Args = {
  base: string;
  cutoff: string;
  out: string;
  repo: string;
  label: string;
  rollouts: string[];
  applyPatchBin?: string;
};

const DEFAULT_REPO = process.env.CODEX_FORK_REPO ?? resolve(process.cwd(), "..");
const SESSION_DIR = join(process.env.CODEX_HOME ?? join(process.env.HOME ?? "", ".codex"), "sessions/2026/09/07");

/**
 * Orchestrator 01a07e54 plus the four subagents it spawned. The subagent set
 * was verified from each rollout's `session_meta.payload`:
 * `parent_thread_id == 01a07e54-...` and `agent_path` in
 * {shake_investigation, artifact_implementation, artifact_review_fixes,
 *  artifact_delete_lifecycle}. Other rollouts from the same hour belong to
 * unrelated sessions from other private repos and are excluded.
 */
const DEFAULT_ROLLOUTS = [
  "rollout-2026-09-07T17-04-21-01a07e54-7c97-7541-b618-af295f393780.jsonl", // orchestrator
  "rollout-2026-09-07T17-07-37-01a07e57-793b-7a60-85eb-541e2c26bc22.jsonl", // shake_investigation
  "rollout-2026-09-07T17-21-12-01a07e63-ebd6-7020-8a28-ee13803e26fd.jsonl", // artifact_implementation
  "rollout-2026-09-07T17-53-16-01a07e81-455e-7680-bb1f-af433136d256.jsonl", // artifact_review_fixes
  "rollout-2026-09-07T17-54-17-01a07e82-32e7-7972-933a-bb6d8d38d32a.jsonl", // artifact_delete_lifecycle
].map((f) => join(SESSION_DIR, f));

function parseArgs(argv: string[]): Args {
  const out: Partial<Args> & { rollouts: string[] } = { rollouts: [] };
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    const next = () => {
      const v = argv[++i];
      if (v === undefined) throw new Error(`${a} needs a value`);
      return v;
    };
    if (a === "--base") out.base = next();
    else if (a === "--cutoff") out.cutoff = next();
    else if (a === "--out") out.out = next();
    else if (a === "--repo") out.repo = next();
    else if (a === "--label") out.label = next();
    else if (a === "--rollout") out.rollouts.push(next());
    else if (a === "--apply-patch-bin") out.applyPatchBin = next();
    else throw new Error(`unknown argument ${a}`);
  }
  if (!out.base || !out.cutoff || !out.out) {
    throw new Error("usage: reconstruct-checkpoint.ts --base <commit> --cutoff <iso8601> --out <dir> [--repo <path>] [--label <msg>] [--rollout <file>]...");
  }
  return {
    base: out.base,
    cutoff: out.cutoff,
    out: resolve(out.out),
    repo: resolve(out.repo ?? DEFAULT_REPO),
    label: out.label ?? `checkpoint @ ${out.cutoff}`,
    rollouts: out.rollouts.length ? out.rollouts.map((r) => resolve(r)) : DEFAULT_ROLLOUTS,
    applyPatchBin: out.applyPatchBin,
  };
}

// --- JS source scanning ---------------------------------------------------

const ESCAPES: Record<string, string> = {
  n: "\n", t: "\t", r: "\r", b: "\b", f: "\f", v: "\v", "0": "\0",
  "\\": "\\", '"': '"', "'": "'", "`": "`", "\n": "",
};

type Literal = { start: number; end: number; quote: string; raw: boolean; value: string };

/**
 * Scan a JavaScript source string for string/template literals, skipping
 * comments. Returns each literal with its decoded value. This is deliberately
 * a lexer rather than a full parser: the only thing the replay needs out of a
 * code-mode script is the set of literal arguments (patch texts, `cmd:`
 * strings), and every observed script passes those as plain literals.
 *
 * `String.raw` tagged templates are handled separately and NOT unescaped:
 * later patches in this session embed Rust source containing `\n` inside
 * `String.raw\`...\`` precisely so the backslash survives, and cooking those
 * escapes turns a Rust newline escape into a real line break, which corrupts
 * the patch body (a `+`-prefixed line loses its prefix) and makes apply_patch
 * reject the hunk.
 */
export function scanStringLiterals(src: string): Literal[] {
  const out: Literal[] = [];
  let i = 0;
  const n = src.length;
  while (i < n) {
    const c = src[i];
    if (c === "/" && src[i + 1] === "/") {
      const j = src.indexOf("\n", i);
      i = j < 0 ? n : j + 1;
      continue;
    }
    if (c === "/" && src[i + 1] === "*") {
      const j = src.indexOf("*/", i + 2);
      i = j < 0 ? n : j + 2;
      continue;
    }
    if (c === '"' || c === "'" || c === "`") {
      const quote = c;
      const isRaw = quote === "`" && /String\.raw\s*$/.test(src.slice(Math.max(0, i - 24), i));
      if (isRaw) {
        // Raw tagged template: no escape processing at all. A backslash still
        // prevents the next character from closing the template, but both
        // characters are kept verbatim.
        let j = i + 1;
        let buf = "";
        while (j < n) {
          const ch = src[j];
          if (ch === "\\") { buf += ch + (src[j + 1] ?? ""); j += 2; continue; }
          if (ch === "`") { j += 1; break; }
          buf += ch;
          j += 1;
        }
        out.push({ start: i, end: j, quote, raw: true, value: buf });
        i = j;
        continue;
      }
      let j = i + 1;
      let buf = "";
      while (j < n) {
        const ch = src[j];
        if (ch === "\\") {
          const nx = src[j + 1] ?? "";
          if (nx === "u") {
            if (src[j + 2] === "{") {
              const k = src.indexOf("}", j + 3);
              buf += String.fromCodePoint(parseInt(src.slice(j + 3, k), 16));
              j = k + 1;
              continue;
            }
            buf += String.fromCharCode(parseInt(src.slice(j + 2, j + 6), 16));
            j += 6;
            continue;
          }
          if (nx === "x") {
            buf += String.fromCharCode(parseInt(src.slice(j + 2, j + 4), 16));
            j += 4;
            continue;
          }
          buf += ESCAPES[nx] ?? nx;
          j += 2;
          continue;
        }
        if (ch === quote) { j += 1; break; }
        buf += ch;
        j += 1;
      }
      out.push({ start: i, end: j, quote, raw: false, value: buf });
      i = j;
      continue;
    }
    i += 1;
  }
  return out;
}

// --- rollout parsing ------------------------------------------------------

type ExecCall = {
  rollout: string;
  agent: string;
  timestamp: string;
  ordinal: number;
  callId: string;
  input: string;
  outputText: string;
  ok: boolean;
};

function outputToText(output: unknown): string {
  if (typeof output === "string") return output;
  if (Array.isArray(output)) {
    return output.map((p) => (p && typeof p === "object" && "text" in (p as any) ? String((p as any).text) : "")).join("\n");
  }
  return output == null ? "" : JSON.stringify(output);
}

function readRollout(path: string): ExecCall[] {
  const lines = readFileSync(path, "utf8").split("\n").filter((l) => l.trim().length > 0);
  const recs = lines.map((l) => JSON.parse(l) as any);
  const meta = recs.find((r) => r.type === "session_meta");
  const agent = meta?.payload?.agent_path ?? meta?.payload?.id ?? basename(path);
  const outputs = new Map<string, string>();
  for (const r of recs) {
    if (r.type !== "response_item") continue;
    const p = r.payload;
    if (p?.type === "custom_tool_call_output" && p.call_id) {
      outputs.set(p.call_id, outputToText(p.output));
    }
  }
  const calls: ExecCall[] = [];
  for (const r of recs) {
    if (r.type !== "response_item") continue;
    const p = r.payload;
    if (p?.type !== "custom_tool_call" || p.name !== "exec") continue;
    const outputText = outputs.get(p.call_id) ?? "";
    calls.push({
      rollout: basename(path),
      agent: String(agent),
      timestamp: r.timestamp,
      ordinal: r.ordinal,
      callId: p.call_id,
      input: String(p.input ?? ""),
      outputText,
      // Codex code-mode reports "Script completed" / "Script failed" in the
      // first output chunk. A failed script left the worktree untouched for
      // the apply_patch it was attempting (apply_patch is all-or-nothing).
      ok: /Script completed/.test(outputText),
    });
  }
  return calls;
}

// --- classification -------------------------------------------------------

/**
 * Shell commands that are safe and deterministic to re-execute during a
 * replay: they rewrite files purely as a function of the files already on
 * disk. Everything else (snapshot accepts that depend on a test run, package
 * managers, build outputs, anything with a network or clock dependency) is
 * reported for manual review rather than guessed at.
 */
const SAFE_SHELL_ALLOWLIST: RegExp[] = [
  /^\s*(rtk\s+(proxy\s+)?)?cargo\s+fmt\b/,
  /^\s*(rtk\s+(proxy\s+)?)?just\s+fmt\b/,
];

/** Commands that plausibly write into the worktree and therefore need a look. */
const SHELL_WRITE_PATTERNS: RegExp[] = [
  /\bapply_patch\b/,
  /\bsed\s+-i\b/,
  /\bcargo\s+fmt\b/,
  /\bjust\s+fmt\b/,
  /\binsta\s+accept\b/,
  /\bgit\s+(apply|checkout|stash|commit|restore|reset|clean|mv|rm)\b/,
  /\b(mv|cp|rm|mkdir|touch|chmod|ln)\s+(-\S+\s+)*\S*\.(rs|ts|md|toml|json|py|snap|sh)\b/,
  /\btee\b/,
  /(^|[^0-9>])>>?\s*\S+\.(rs|ts|md|toml|json|py|snap|sh)\b/,
  /\bcat\s*>\s*/,
];

/** Commands that only read, even though they trip a write pattern above. */
const SHELL_READ_ONLY_HINTS: RegExp[] = [
  /^\s*(rtk\s+)?(rg|grep|find|ls|sed\s+-n|awk|cat|head|tail|wc|git\s+(status|log|diff|show|branch|reflog|fsck|for-each-ref|show-ref))\b/,
];

type Mutation =
  | { kind: "apply_patch"; call: ExecCall; patch: string; files: { op: string; path: string }[] }
  | { kind: "shell"; call: ExecCall; cmd: string; workdir?: string; safe: boolean };

function extractExecCommands(src: string, literals: Literal[]): { cmd: string; workdir?: string }[] {
  const out: { cmd: string; workdir?: string }[] = [];
  const byStart = new Map(literals.map((l) => [l.start, l] as const));
  const findAfter = (re: RegExp) => {
    const found: { pos: number; lit: Literal }[] = [];
    let m: RegExpExecArray | null;
    const r = new RegExp(re.source, "g");
    while ((m = r.exec(src))) {
      const lit = byStart.get(m.index + m[0].length);
      if (lit) found.push({ pos: m.index, lit });
    }
    return found;
  };
  const cmds = findAfter(/cmd\s*:\s*(?:String\.raw\s*)?/);
  const workdirs = findAfter(/workdir\s*:\s*(?:String\.raw\s*)?/);
  for (const c of cmds) {
    // Nearest workdir literal that follows this cmd literal within the same
    // object literal; good enough because exec_command always spells them
    // adjacently in the observed scripts.
    const wd = workdirs.find((w) => w.pos > c.lit.end && w.pos - c.lit.end < 200);
    out.push({ cmd: c.lit.value, workdir: wd?.lit.value });
  }
  return out;
}

function parseMutations(call: ExecCall): Mutation[] {
  const literals = scanStringLiterals(call.input);
  const mutations: Mutation[] = [];
  if (/tools\.apply_patch\s*\(/.test(call.input)) {
    for (const lit of literals) {
      if (!lit.value.includes("*** Begin Patch")) continue;
      const files: { op: string; path: string }[] = [];
      const re = /^\*\*\* (Add|Update|Delete) File: (.+)$/gm;
      let m: RegExpExecArray | null;
      while ((m = re.exec(lit.value))) files.push({ op: m[1].toLowerCase(), path: m[2].trim() });
      mutations.push({ kind: "apply_patch", call, patch: lit.value, files });
    }
  }
  for (const { cmd, workdir } of extractExecCommands(call.input, literals)) {
    if (SHELL_READ_ONLY_HINTS.some((re) => re.test(cmd))) continue;
    if (!SHELL_WRITE_PATTERNS.some((re) => re.test(cmd))) continue;
    mutations.push({ kind: "shell", call, cmd, workdir, safe: SAFE_SHELL_ALLOWLIST.some((re) => re.test(cmd)) });
  }
  return mutations;
}

// --- apply_patch driver ---------------------------------------------------

/**
 * Resolve the `apply_patch` executable. Codex ships apply_patch as an arg0
 * dispatch on the main `codex` binary, so a symlink named `apply_patch` that
 * points at the codex binary is the real, in-product implementation rather
 * than a reimplementation of the patch format.
 */
function resolveApplyPatch(override: string | undefined, scratch: string): { bin: string; provenance: string } {
  if (override) return { bin: override, provenance: `--apply-patch-bin ${override}` };
  const candidates = [
    process.env.CODEX_BIN,
    join(process.env.HOME ?? "", ".local/share/codex-statusline/current/codex"),
    "/usr/local/bin/codex",
  ].filter(Boolean) as string[];
  for (const c of candidates) {
    if (!existsSync(c)) continue;
    const real = realpathSync(c);
    const probe = spawnSync(real, ["--version"], { encoding: "utf8" });
    if (probe.status !== 0) continue;
    const link = join(scratch, "apply_patch");
    rmSync(link, { force: true });
    symlinkSync(real, link);
    return { bin: link, provenance: `${real} (${probe.stdout.trim()}) via arg0 apply_patch dispatch` };
  }
  throw new Error("could not find a codex binary to provide apply_patch; pass --apply-patch-bin");
}

// --- main -----------------------------------------------------------------

function run(cmd: string, args: string[], cwd?: string): string {
  return execFileSync(cmd, args, { cwd, encoding: "utf8", maxBuffer: 256 * 1024 * 1024 });
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  const scratch = join(tmpdir(), `reconstruct-checkpoint-${process.pid}`);
  mkdirSync(scratch, { recursive: true });

  const repoPrefix = args.repo.endsWith("/") ? args.repo : `${args.repo}/`;
  const { bin: applyPatchBin, provenance } = resolveApplyPatch(args.applyPatchBin, scratch);

  // (a) Export the base tree. `git archive` reads the source repo without
  //     writing to it and without carrying any history into the copy.
  rmSync(args.out, { recursive: true, force: true });
  mkdirSync(args.out, { recursive: true });
  const tar = join(scratch, "base.tar");
  writeFileSync(tar, execFileSync("git", ["-C", args.repo, "archive", "--format=tar", args.base], { maxBuffer: 2 * 1024 * 1024 * 1024 }));
  run("tar", ["-xf", tar, "-C", args.out]);

  // A scratch git repo during the replay, because the repo's own formatter
  // (`just fmt` -> scripts/format.py) shells out to git to enumerate files and
  // fails outside a work tree. It is destroyed again in step (d), so nothing
  // from it reaches the final checkpoint.
  run("git", ["init", "-q", "-b", "replay"], args.out);
  run("git", ["-C", args.out, "config", "user.name", "shake-bench checkpoint"]);
  run("git", ["-C", args.out, "config", "user.email", "shake-bench@localhost"]);
  run("git", ["-C", args.out, "add", "-A"]);
  run("git", ["-C", args.out, "commit", "-q", "-m", `base ${args.base}`]);

  // (b) Collect every file-mutating tool call at or before the cutoff.
  const calls: ExecCall[] = [];
  for (const r of args.rollouts) calls.push(...readRollout(r));
  calls.sort((a, b) => (a.timestamp < b.timestamp ? -1 : a.timestamp > b.timestamp ? 1 : a.ordinal - b.ordinal));
  const inWindow = calls.filter((c) => c.timestamp <= args.cutoff);

  const log: Record<string, unknown>[] = [];
  const manualReview: Record<string, unknown>[] = [];
  let applied = 0;
  let skipped = 0;
  const touched = new Set<string>();

  // (c) Replay in timestamp order.
  for (const call of inWindow) {
    for (const mut of parseMutations(call)) {
      const common = { timestamp: call.timestamp, agent: call.agent, rollout: call.rollout, callId: call.callId };
      if (mut.kind === "shell") {
        if (mut.safe) {
          const res = spawnSync("bash", ["-lc", mut.cmd], { cwd: args.out, encoding: "utf8" });
          applied += 1;
          log.push({ ...common, kind: "shell", cmd: mut.cmd, status: res.status === 0 ? "applied" : "failed", stderr: (res.stderr ?? "").slice(0, 400) });
        } else {
          skipped += 1;
          manualReview.push({ ...common, kind: "shell", cmd: mut.cmd, workdir: mut.workdir, reason: "not on the safe/deterministic allowlist" });
          log.push({ ...common, kind: "shell", cmd: mut.cmd, status: "manual-review" });
        }
        continue;
      }

      // apply_patch
      if (!call.ok) {
        skipped += 1;
        log.push({ ...common, kind: "apply_patch", files: mut.files, status: "skipped", reason: "the session's own script failed; the worktree was never modified" });
        continue;
      }
      const outside = mut.files.filter((f) => !f.path.startsWith(repoPrefix));
      if (outside.length) {
        skipped += 1;
        manualReview.push({ ...common, kind: "apply_patch", files: mut.files, reason: `patch targets paths outside ${repoPrefix}` });
        log.push({ ...common, kind: "apply_patch", files: mut.files, status: "skipped", reason: "outside the reconstructed repo" });
        continue;
      }
      // Rewrite absolute source-repo paths to paths relative to the copy and
      // run the real apply_patch with cwd = the copy.
      const rewritten = mut.patch.split(repoPrefix).join("");
      const res = spawnSync(applyPatchBin, [rewritten], { cwd: args.out, encoding: "utf8" });
      if (res.status === 0) {
        applied += 1;
        for (const f of mut.files) touched.add(f.path.slice(repoPrefix.length));
        log.push({ ...common, kind: "apply_patch", files: mut.files, status: "applied" });
      } else {
        skipped += 1;
        manualReview.push({ ...common, kind: "apply_patch", files: mut.files, reason: `replay apply_patch failed: ${(res.stderr || res.stdout || "").slice(0, 600)}` });
        log.push({ ...common, kind: "apply_patch", files: mut.files, status: "replay-failed", stderr: (res.stderr || res.stdout || "").slice(0, 600) });
      }
    }
  }

  // (d) Re-init git so the copy carries no history from after the cutoff.
  //     `git archive` never wrote a .git; remove one anyway so a re-run over a
  //     previously built dir cannot inherit refs, remotes or worktree pointers.
  rmSync(join(args.out, ".git"), { recursive: true, force: true });
  run("git", ["init", "-q", "-b", "checkpoint"], args.out);
  run("git", ["-C", args.out, "config", "user.name", "shake-bench checkpoint"]);
  run("git", ["-C", args.out, "config", "user.email", "shake-bench@localhost"]);
  run("git", ["-C", args.out, "add", "-A"]);
  run("git", ["-C", args.out, "commit", "-q", "-m", args.label]);
  // No remotes, no worktree pointers, no extra refs exist in a fresh init;
  // assert that rather than assume it.
  const remotes = run("git", ["-C", args.out, "remote"]).trim();
  const refs = run("git", ["-C", args.out, "for-each-ref", "--format=%(refname)"]).trim().split("\n").filter(Boolean);
  if (remotes) throw new Error(`unexpected remote in the checkpoint copy: ${remotes}`);
  if (refs.length !== 1) throw new Error(`expected exactly one ref in the checkpoint copy, found ${refs.join(", ")}`);

  const manifest = {
    base: args.base,
    baseRepo: args.repo,
    cutoff: args.cutoff,
    label: args.label,
    rollouts: args.rollouts,
    applyPatch: provenance,
    execCallsTotal: calls.length,
    execCallsAtOrBeforeCutoff: inWindow.length,
    mutationsApplied: applied,
    mutationsSkipped: skipped,
    filesTouched: [...touched].sort(),
    manualReview,
    replayLog: log,
  };
  // The manifest lives beside the checkpoint, not inside it, so the copy's
  // tree is exactly the reconstructed worktree and diffs against the source
  // repo stay clean.
  const manifestPath = `${args.out}.manifest.json`;
  writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);

  rmSync(scratch, { recursive: true, force: true });

  console.error(
    [
      `base           ${args.base}`,
      `cutoff         ${args.cutoff}`,
      `apply_patch    ${provenance}`,
      `exec calls     ${inWindow.length} of ${calls.length} at or before the cutoff`,
      `applied        ${applied}`,
      `skipped        ${skipped}`,
      `files touched  ${touched.size}`,
      `manual review  ${manualReview.length}`,
      `out            ${args.out}`,
      `manifest       ${manifestPath}`,
    ].join("\n"),
  );
}

if (process.argv[1] && resolve(process.argv[1]).endsWith("reconstruct-checkpoint.ts")) main();
