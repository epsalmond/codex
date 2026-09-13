#!/usr/bin/env node
// Flatten a multi-agent Codex session into a single-agent history that the
// shake-bench harness can inject with `thread/inject_items`.
//
// Phase 2 of the shake replay benchmark. Session 01a07e54 ran as an
// orchestrator plus four `spawn_agent` subagents; the orchestrator's own
// rollout contains only its `exec` calls plus the multi-agent plumbing
// (`spawn_agent`, `send_message`, `followup_task`, `sleep`, `interrupt_agent`,
// and the subagents' `agent_message` FINAL_ANSWER reports). The real
// implementation work lives in the subagents' own rollout files.
//
// This script interleaves every agent's tool calls and tool results into one
// timeline ordered by wall-clock timestamp and drops the plumbing, so the
// result reads as one agent that did all of the work itself.
//
// Output format is the one `src/convert.ts` already validates and
// `src/run.ts` already injects:
//   {type:"message", role:"user"|"assistant", content:[{type:"input_text"|"output_text", text}]}
//   {type:"function_call", call_id, name, arguments}   (arguments is a JSON string)
//   {type:"function_call_output", call_id, output}
// No new history format is invented.
//
// Usage:
//   tsx scripts/flatten-history.ts \
//     --cutoff 2026-09-08T01:57:36.059Z \
//     --out fixtures/01a07e54-r264 \
//     --redact /path/to/codex-artifacts=$BENCH_REPLAY/worktree \
//     --redact codex-artifacts=worktree-work \
//     --trim fixtures/trim-01a07e54-tierB.json
//
// --trim <spec.json> truncates selected function_call_output items in the
// FLATTENED variant only (the orchestrator variant is always emitted
// untrimmed; its meta.trim is recorded as null). The spec file is either
//   { "bytesPerToken": 3.844, "defaults": {"keepHead": 800, "keepTail": 800},
//     "items": [ {"index": 15}, {"index": 17, "keepHead": 800, "keepTail": 800} ] }
// or a bare array shorthand for `items` (defaults 800/800 tokens,
// bytesPerToken 3.844). `index` is 0-based into the flattened variant's item
// list, using the same ordering scripts/history-sizes.ts reports. keepHead/
// keepTail are token-equivalents, converted to bytes via
// Math.round(tokens * bytesPerToken), the same proxy history-sizes.ts uses.
// Trimming happens right after the flatten() emit/sort/flatMap and before
// toInjectItems validation and redaction. Only function_call_output items
// may be listed; an out-of-range or wrong-type index throws.
//
// A spec may also carry a `drop` list:
//   "drop": [ {"call": 845, "output": 846}, {"call": 847, "output": 848} ]
// which DELETES a function_call together with its function_call_output. Both
// halves are required and must share a call_id, so a drop can never leave a
// dangling call or an orphan result. Only those two item types can appear in
// a drop, so user messages and assistant messages (where the session's
// decisions live) cannot be dropped at all. Drops are applied AFTER the
// truncations, so every index in one spec -- `items` and `drop` alike --
// refers to the same untrimmed flattened item list.
//
// Writes <out>.flattened.json and <out>.orchestrator.json, each
//   { meta: {...}, items: [...] }
// plus a combined <out>.sizes.json with byte sizes and item counts.

import { readFileSync, mkdirSync, writeFileSync } from "node:fs";
import { basename, dirname, join, resolve } from "node:path";
import { toInjectItems, payloadBytes } from "../src/convert.ts";
import type { ResponseItem } from "../src/fixtures.ts";

const SESSION_DIR = join(process.env.CODEX_HOME ?? join(process.env.HOME ?? "", ".codex"), "sessions/2026/09/07");

/** Orchestrator first; the four subagents it spawned, verified by
 *  `session_meta.payload.parent_thread_id` in scripts/reconstruct-checkpoint.ts. */
const DEFAULT_ROLLOUTS = [
  "rollout-2026-09-07T17-04-21-01a07e54-7c97-7541-b618-af295f393780.jsonl",
  "rollout-2026-09-07T17-07-37-01a07e57-793b-7a60-85eb-541e2c26bc22.jsonl",
  "rollout-2026-09-07T17-21-12-01a07e63-ebd6-7020-8a28-ee13803e26fd.jsonl",
  "rollout-2026-09-07T17-53-16-01a07e81-455e-7680-bb1f-af433136d256.jsonl",
  "rollout-2026-09-07T17-54-17-01a07e82-32e7-7972-933a-bb6d8d38d32a.jsonl",
].map((f) => join(SESSION_DIR, f));

/**
 * Multi-agent plumbing. These calls only exist because the session delegated;
 * a single agent doing the same work makes none of them. Their payloads are
 * Fernet-encrypted in the rollout anyway, so they carry no recoverable content.
 */
const PLUMBING_TOOLS = new Set([
  "spawn_agent",
  "send_message",
  "followup_task",
  "interrupt_agent",
  "list_agents",
  "sleep",
]);

type TrimItemSpec = { index: number; keepHead?: number; keepTail?: number };
/** A `drop` entry removes a function_call and its function_call_output as a
 *  unit. Both indexes are required and both are 0-based into the flattened
 *  variant's untrimmed item list, the same numbering `items[]` uses. Dropping
 *  one half alone would leave a dangling call or a result with no call, which
 *  toInjectItems rejects and which no real history can contain. */
type TrimDropSpec = { call: number; output: number };
type TrimSpec = {
  specPath: string;
  source?: string;
  note?: string;
  bytesPerToken: number;
  defaults: { keepHead: number; keepTail: number };
  items: TrimItemSpec[];
  drop: TrimDropSpec[];
};

const TRIM_DEFAULT_BYTES_PER_TOKEN = 3.844;
const TRIM_DEFAULT_KEEP_TOKENS = 800;

function loadTrimSpec(path: string): TrimSpec {
  // Recorded in meta.trim.specPath verbatim as given on the command line
  // (not resolved to an absolute path) so the fixture doesn't embed this
  // machine's checkout path into the benchmark payload's bookkeeping.
  const abs = resolve(path);
  const raw = JSON.parse(readFileSync(abs, "utf8"));
  if (Array.isArray(raw)) {
    return {
      specPath: path,
      bytesPerToken: TRIM_DEFAULT_BYTES_PER_TOKEN,
      defaults: { keepHead: TRIM_DEFAULT_KEEP_TOKENS, keepTail: TRIM_DEFAULT_KEEP_TOKENS },
      items: raw as TrimItemSpec[],
      drop: [],
    };
  }
  if (!raw || typeof raw !== "object" || !Array.isArray(raw.items)) {
    throw new Error(`--trim spec at ${abs} must be a JSON array or an object with an "items" array`);
  }
  return {
    specPath: path,
    source: typeof raw.source === "string" ? raw.source : undefined,
    note: typeof raw.note === "string" ? raw.note : undefined,
    bytesPerToken: typeof raw.bytesPerToken === "number" ? raw.bytesPerToken : TRIM_DEFAULT_BYTES_PER_TOKEN,
    defaults: {
      keepHead: raw.defaults?.keepHead ?? TRIM_DEFAULT_KEEP_TOKENS,
      keepTail: raw.defaults?.keepTail ?? TRIM_DEFAULT_KEEP_TOKENS,
    },
    items: raw.items as TrimItemSpec[],
    drop: Array.isArray(raw.drop) ? (raw.drop as TrimDropSpec[]) : [],
  };
}

type Args = {
  cutoff: string;
  out: string;
  rollouts: string[];
  redactions: { from: string; to: string }[];
  trim: TrimSpec | null;
};

function parseArgs(argv: string[]): Args {
  const out: Partial<Args> & { rollouts: string[]; redactions: { from: string; to: string }[] } = {
    rollouts: [],
    redactions: [],
  };
  let trimPath: string | undefined;
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    const next = () => {
      const v = argv[++i];
      if (v === undefined) throw new Error(`${a} needs a value`);
      return v;
    };
    if (a === "--cutoff") out.cutoff = next();
    else if (a === "--redact") {
      const value = next();
      const eq = value.indexOf("=");
      if (eq <= 0) throw new Error(`--redact wants from=to, got ${value}`);
      out.redactions.push({ from: value.slice(0, eq), to: value.slice(eq + 1) });
    }
    else if (a === "--out") out.out = next();
    else if (a === "--rollout") out.rollouts.push(next());
    else if (a === "--trim") trimPath = next();
    else throw new Error(`unknown argument ${a}`);
  }
  if (!out.cutoff || !out.out) {
    throw new Error("usage: flatten-history.ts --cutoff <iso8601> --out <path-prefix> [--rollout <file>]... [--trim <spec.json>]");
  }
  return {
    cutoff: out.cutoff,
    out: resolve(out.out),
    rollouts: out.rollouts.length ? out.rollouts.map((r) => resolve(r)) : DEFAULT_ROLLOUTS,
    redactions: out.redactions,
    trim: trimPath ? loadTrimSpec(trimPath) : null,
  };
}

// --- rollout reading -------------------------------------------------------

type Row = {
  agent: string;
  isOrchestrator: boolean;
  timestamp: string;
  ordinal: number;
  payload: Record<string, any>;
};

function textOf(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .map((part: any) => (part && typeof part === "object" && typeof part.text === "string" ? part.text : ""))
    .join("");
}

function outputToText(output: unknown): string {
  if (typeof output === "string") return output;
  if (Array.isArray(output)) {
    return output
      .map((p: any) => (p && typeof p === "object" && "text" in p ? String(p.text) : ""))
      .join("\n");
  }
  return output == null ? "" : JSON.stringify(output);
}

function readRollout(path: string): { agent: string; isOrchestrator: boolean; rows: Row[] } {
  const recs = readFileSync(path, "utf8")
    .split("\n")
    .filter((l) => l.trim().length > 0)
    .map((l) => JSON.parse(l) as any);
  const meta = recs.find((r) => r.type === "session_meta")?.payload;
  const isOrchestrator = !meta?.parent_thread_id;
  const agent = isOrchestrator ? "root" : String(meta?.agent_path ?? basename(path)).replace(/^\/root\//, "");
  const rows: Row[] = [];
  for (const r of recs) {
    if (r.type !== "response_item") continue;
    rows.push({ agent, isOrchestrator, timestamp: r.timestamp, ordinal: r.ordinal, payload: r.payload });
  }
  return { agent, isOrchestrator, rows };
}

// --- flattening ------------------------------------------------------------

type Stats = Record<string, number>;

function bump(stats: Stats, key: string): void {
  stats[key] = (stats[key] ?? 0) + 1;
}

/**
 * Build the single-agent item list.
 *
 * `orchestratorOnly` produces the secondary variant: exactly what the
 * orchestrator's own rollout contains, with the same plumbing removed but
 * without any subagent content. That variant is what a naive
 * "replay the session's own transcript" would give, and is much smaller
 * because the implementation work happened elsewhere.
 */
export function flatten(
  files: { agent: string; isOrchestrator: boolean; rows: Row[] }[],
  cutoff: string,
  orchestratorOnly: boolean,
): { items: ResponseItem[]; stats: Stats } {
  const stats: Stats = {};
  const sources = orchestratorOnly ? files.filter((f) => f.isOrchestrator) : files;

  // Tool outputs are keyed by call id inside one rollout.
  const outputs = new Map<string, string>();
  for (const file of sources) {
    for (const row of file.rows) {
      const p = row.payload;
      if ((p?.type === "custom_tool_call_output" || p?.type === "function_call_output") && p.call_id) {
        outputs.set(`${file.agent}:${p.call_id}`, outputToText(p.output ?? p.result));
      }
    }
  }

  type Emit = { timestamp: string; agent: string; ordinal: number; items: ResponseItem[] };
  const emits: Emit[] = [];

  for (const file of sources) {
    for (const row of file.rows) {
      if (row.timestamp > cutoff) continue;
      const p = row.payload;
      const type = p?.type;
      const push = (items: ResponseItem[]) =>
        emits.push({ timestamp: row.timestamp, agent: file.agent, ordinal: row.ordinal, items });

      if (type === "reasoning") { bump(stats, "dropped.reasoning"); continue; }
      if (type === "agent_message") {
        // In the flattened variant the subagents do not exist: their work is
        // inlined above, so their FINAL_ANSWER reports back to the orchestrator
        // are redundant plumbing. In the orchestrator-only variant those
        // reports are the orchestrator's ONLY visibility into the
        // implementation, so they are kept, rendered as inbound messages.
        const inbound = p.recipient === "/root" && p.author !== "/root";
        const reportText = textOf(p.content);
        if (!orchestratorOnly || !inbound || !reportText.trim()) {
          bump(stats, "dropped.agent_message");
          continue;
        }
        bump(stats, "kept.agent_report");
        push([
          {
            type: "message",
            role: "user",
            content: [{ type: "input_text", text: `[report from ${String(p.author)}]\n${reportText}` }],
          },
        ]);
        continue;
      }

      if (type === "message") {
        const role = p.role;
        const text = textOf(p.content);
        if (!text.trim()) { bump(stats, "dropped.empty_message"); continue; }
        if (role === "developer") { bump(stats, "dropped.developer_message"); continue; }
        if (role === "user") {
          // Only the orchestrator has real user input. A subagent's "user"
          // message is the (encrypted, empty) task assignment from the parent.
          if (!file.isOrchestrator) { bump(stats, "dropped.subagent_task_message"); continue; }
          bump(stats, "kept.user_message");
          push([{ type: "message", role: "user", content: [{ type: "input_text", text }] }]);
          continue;
        }
        if (role === "assistant") {
          bump(stats, file.isOrchestrator ? "kept.assistant_message" : "kept.subagent_assistant_message");
          push([{ type: "message", role: "assistant", content: [{ type: "output_text", text }] }]);
          continue;
        }
        bump(stats, `dropped.message_role_${String(role)}`);
        continue;
      }

      if (type === "function_call" || type === "custom_tool_call") {
        const name = String(p.name ?? "");
        if (PLUMBING_TOOLS.has(name)) { bump(stats, `dropped.plumbing.${name}`); continue; }
        const callId = `${file.agent}:${p.call_id}`;
        // custom tools carry a freeform `input`; function tools carry a JSON
        // `arguments` string. inject_items wants a string either way.
        const args =
          type === "custom_tool_call"
            ? JSON.stringify({ script: String(p.input ?? "") })
            : typeof p.arguments === "string"
              ? p.arguments
              : JSON.stringify(p.arguments ?? {});
        const output = outputs.get(callId) ?? "";
        bump(stats, `kept.tool.${name}`);
        // Emit the result immediately after its call so the pairing stays
        // valid no matter how two agents' timestamps interleave.
        push([
          { type: "function_call", call_id: callId, name, arguments: args },
          { type: "function_call_output", call_id: callId, output },
        ]);
        continue;
      }

      if (type === "function_call_output" || type === "custom_tool_call_output") continue; // emitted with its call
      bump(stats, `dropped.other_${String(type)}`);
    }
  }

  emits.sort((a, b) =>
    a.timestamp < b.timestamp ? -1 : a.timestamp > b.timestamp ? 1
      : a.agent < b.agent ? -1 : a.agent > b.agent ? 1
      : a.ordinal - b.ordinal,
  );
  return { items: emits.flatMap((e) => e.items), stats };
}

// --- trimming ----------------------------------------------------------

const TRIM_MARKER_PREFIX = "[... ";
const TRIM_MARKER_SUFFIX = " bytes elided by benchmark trim; rerun the command to regenerate ...]";

/**
 * Split `str` into a head prefix (<= headBudget bytes) and a tail suffix
 * (<= tailBudget bytes), both UTF-8 safe: cuts always land on codepoint
 * boundaries (via Array.from, which is surrogate-pair aware), so neither
 * slice can end/start mid-character.
 */
function splitByByteBudget(
  str: string,
  headBudget: number,
  tailBudget: number,
): { head: string; tail: string; headBytes: number; tailBytes: number } {
  const chars = Array.from(str);
  let headBytes = 0;
  let headEnd = 0;
  for (; headEnd < chars.length; headEnd += 1) {
    const b = Buffer.byteLength(chars[headEnd]!, "utf8");
    if (headBytes + b > headBudget) break;
    headBytes += b;
  }
  let tailBytes = 0;
  let tailStart = chars.length;
  for (; tailStart > headEnd; tailStart -= 1) {
    const b = Buffer.byteLength(chars[tailStart - 1]!, "utf8");
    if (tailBytes + b > tailBudget) break;
    tailBytes += b;
  }
  return {
    head: chars.slice(0, headEnd).join(""),
    tail: chars.slice(tailStart).join(""),
    headBytes,
    tailBytes,
  };
}

type TrimItemResult = {
  index: number;
  kind: string;
  name?: string;
  bytesBefore: number;
  bytesAfter: number;
  elided: number;
  headBytes: number;
  tailBytes: number;
  skipped?: string;
};

/**
 * Apply a --trim spec to the flattened item list, right after flatten()'s
 * emit/sort/flatMap and before toInjectItems validation. Only
 * function_call_output items may be trimmed; an out-of-range or wrong-type
 * index throws rather than silently skipping.
 */
function applyTrim(
  items: ResponseItem[],
  spec: TrimSpec,
): { items: ResponseItem[]; summary: Record<string, unknown> } {
  const result = items.slice();
  const trimmedItems: TrimItemResult[] = [];
  let trimmed = 0;
  let skipped = 0;

  for (const req of spec.items) {
    const idx = req.index;
    if (!Number.isInteger(idx) || idx < 0 || idx >= result.length) {
      throw new Error(`--trim: index ${idx} is out of range (0..${result.length - 1})`);
    }
    const item = result[idx] as Record<string, unknown> & { type: string };
    if (item.type !== "function_call_output") {
      throw new Error(`--trim: index ${idx} is not a function_call_output (found type=${String(item.type)})`);
    }
    const output = typeof item.output === "string" ? item.output : "";
    const bytesBefore = Buffer.byteLength(output, "utf8");

    const keepHeadTokens = req.keepHead ?? spec.defaults.keepHead;
    const keepTailTokens = req.keepTail ?? spec.defaults.keepTail;
    const headBudget = Math.round(keepHeadTokens * spec.bytesPerToken);
    const tailBudget = Math.round(keepTailTokens * spec.bytesPerToken);

    const prev = idx > 0 ? (result[idx - 1] as Record<string, unknown> & { type: string }) : undefined;
    const name =
      prev && prev.type === "function_call" && prev.call_id === item.call_id
        ? String(prev.name ?? "")
        : undefined;

    const split = splitByByteBudget(output, headBudget, tailBudget);
    const elided = bytesBefore - split.headBytes - split.tailBytes;
    const markerLine = `${TRIM_MARKER_PREFIX}${elided}${TRIM_MARKER_SUFFIX}`;
    const markerBytes = Buffer.byteLength(`\n${markerLine}\n`, "utf8");

    if (bytesBefore <= headBudget + tailBudget + markerBytes) {
      skipped += 1;
      trimmedItems.push({
        index: idx,
        kind: item.type,
        name,
        bytesBefore,
        bytesAfter: bytesBefore,
        elided: 0,
        headBytes: 0,
        tailBytes: 0,
        skipped: "already-small",
      });
      continue;
    }

    const newOutput = `${split.head}\n${markerLine}\n${split.tail}`;
    const bytesAfter = Buffer.byteLength(newOutput, "utf8");
    result[idx] = { ...item, output: newOutput };
    trimmed += 1;
    trimmedItems.push({
      index: idx,
      kind: item.type,
      name,
      bytesBefore,
      bytesAfter,
      elided,
      headBytes: split.headBytes,
      tailBytes: split.tailBytes,
    });
  }

  // Drops run after truncation so both refer to the SAME (untrimmed) index
  // space -- a drop shifts every later index, so doing it first would silently
  // move the truncation targets.
  const { items: afterDrop, dropped } = applyDrop(result, spec);

  const summary = {
    specPath: spec.specPath,
    bytesPerToken: spec.bytesPerToken,
    defaults: spec.defaults,
    requested: spec.items.length,
    trimmed,
    skipped,
    items: trimmedItems,
    dropRequested: spec.drop.length,
    dropped,
  };
  return { items: afterDrop, summary };
}

type TrimDropResult = { call: number; output: number; name?: string; callId: string; bytes: number };

/**
 * Remove `{call, output}` pairs named by the spec's `drop` list. A pair is
 * removed as a unit and only ever as a unit: the call index must be a
 * `function_call`, the output index a `function_call_output`, and the two must
 * share a `call_id`, or this throws. Messages (user or assistant) can never be
 * dropped -- the type check makes that structurally impossible, which is the
 * point: assistant messages are where the session's decisions live, and
 * dropping a user message would rewrite the task.
 */
function applyDrop(
  items: ResponseItem[],
  spec: TrimSpec,
): { items: ResponseItem[]; dropped: TrimDropResult[] } {
  if (spec.drop.length === 0) return { items, dropped: [] };
  const remove = new Set<number>();
  const dropped: TrimDropResult[] = [];

  for (const pair of spec.drop) {
    const { call, output } = pair;
    for (const [label, idx] of [["call", call], ["output", output]] as const) {
      if (!Number.isInteger(idx) || idx < 0 || idx >= items.length) {
        throw new Error(`--trim drop: ${label} index ${idx} is out of range (0..${items.length - 1})`);
      }
      if (remove.has(idx)) throw new Error(`--trim drop: index ${idx} is listed twice`);
    }
    const callItem = items[call] as Record<string, unknown> & { type: string };
    const outItem = items[output] as Record<string, unknown> & { type: string };
    if (callItem.type !== "function_call") {
      throw new Error(`--trim drop: call index ${call} is not a function_call (found type=${String(callItem.type)})`);
    }
    if (outItem.type !== "function_call_output") {
      throw new Error(`--trim drop: output index ${output} is not a function_call_output (found type=${String(outItem.type)})`);
    }
    if (String(callItem.call_id ?? "") !== String(outItem.call_id ?? "") || !callItem.call_id) {
      throw new Error(
        `--trim drop: call ${call} and output ${output} do not share a call_id ` +
          `(${String(callItem.call_id)} vs ${String(outItem.call_id)}); dropping them would leave a dangling pair`,
      );
    }
    remove.add(call);
    remove.add(output);
    dropped.push({
      call,
      output,
      name: typeof callItem.name === "string" ? callItem.name : undefined,
      callId: String(callItem.call_id),
      bytes:
        Buffer.byteLength(typeof callItem.arguments === "string" ? callItem.arguments : "", "utf8") +
        Buffer.byteLength(typeof outItem.output === "string" ? outItem.output : "", "utf8"),
    });
  }

  return { items: items.filter((_, i) => !remove.has(i)), dropped };
}

// --- main ------------------------------------------------------------------

function main(): void {
  const args = parseArgs(process.argv.slice(2));
  const files = args.rollouts.map(readRollout);
  if (files.filter((f) => f.isOrchestrator).length !== 1) {
    throw new Error("expected exactly one orchestrator rollout (a rollout with no parent_thread_id)");
  }
  mkdirSync(dirname(args.out), { recursive: true });

  const sizes: Record<string, unknown> = { cutoff: args.cutoff, rollouts: args.rollouts };
  for (const [variant, orchestratorOnly] of [["flattened", false], ["orchestrator", true]] as const) {
    const { items: rawItems, stats } = flatten(files, args.cutoff, orchestratorOnly);

    // Trimming (--trim) applies only to the flattened variant, and happens
    // here: after flatten()'s emit/sort/flatMap, before validation/redaction,
    // keyed on the same 0-based index this variant's item list uses.
    let items = rawItems;
    let trimMeta: Record<string, unknown> | null = null;
    if (args.trim) {
      if (variant === "flattened") {
        const bytesBeforeTrim = payloadBytes(toInjectItems(rawItems));
        const { items: trimmedItems, summary } = applyTrim(rawItems, args.trim);
        items = trimmedItems;
        const bytesAfterTrim = payloadBytes(toInjectItems(trimmedItems));
        trimMeta = {
          ...summary,
          bytesBefore: bytesBeforeTrim,
          bytesAfter: bytesAfterTrim,
          bytesElided: bytesBeforeTrim - bytesAfterTrim,
        };
        console.error(
          `trim          requested ${summary.requested} trimmed ${summary.trimmed} skipped ${summary.skipped} ` +
            `dropped ${(summary.dropped as unknown[]).length} pairs  ` +
            `${bytesBeforeTrim} -> ${bytesAfterTrim} bytes (-${bytesBeforeTrim - bytesAfterTrim})`,
        );
      } else {
        trimMeta = null;
      }
    }

    // Validate against the harness's own converter; it throws on anything
    // inject_items would reject, including unmatched call ids.
    let injectItems = toInjectItems(items);
    if (args.redactions.length > 0) {
      // Every apply_patch body, shell command and subagent report in this
      // session names its repositories by absolute path, and the bare repo
      // names survive in scratch-script names and sibling directory listings.
      // Left alone, a replay worker can `cd` to the real checkout and read the
      // finished branch, which invalidates the replay. Redactions are applied
      // to the serialized payload in order, longest-first so a full path is
      // rewritten before its bare basename.
      let serialized = JSON.stringify(injectItems);
      for (const { from, to } of [...args.redactions].sort((a, b) => b.from.length - a.from.length)) {
        serialized = serialized.split(JSON.stringify(from).slice(1, -1)).join(JSON.stringify(to).slice(1, -1));
      }
      injectItems = JSON.parse(serialized) as Array<Record<string, unknown>>;
    }
    const bytes = payloadBytes(injectItems);
    const meta: Record<string, unknown> = {
      variant,
      cutoff: args.cutoff,
      rollouts: args.rollouts,
      agents: files.filter((f) => !orchestratorOnly || f.isOrchestrator).map((f) => f.agent),
      redactions: args.redactions,
      itemCount: injectItems.length,
      bytes,
      stats,
    };
    if (args.trim) meta.trim = trimMeta;
    const path = `${args.out}.${variant}.json`;
    writeFileSync(path, `${JSON.stringify({ meta, items: injectItems }, null, 0)}\n`);
    sizes[variant] = { path, ...meta };
    console.error(
      `${variant.padEnd(13)} ${String(injectItems.length).padStart(5)} items  ${String(bytes).padStart(9)} bytes  -> ${path}`,
    );
  }
  const sizesPath = `${args.out}.sizes.json`;
  writeFileSync(sizesPath, `${JSON.stringify(sizes, null, 2)}\n`);
  console.error(`sizes         ${sizesPath}`);
}

if (process.argv[1] && resolve(process.argv[1]).endsWith("flatten-history.ts")) main();
