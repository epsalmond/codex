#!/usr/bin/env node
// Read-only sizing/analysis helper for the flattened 01a07e54 history.
// Reproduces flatten-history.ts's emit ordering (same rollouts, same cutoff,
// same plumbing/redaction rules) but keeps the per-emit metadata (timestamp,
// source agent, kind) that the shipped script discards once it sorts to a
// flat item list. Used to build reports/2026-09-11/flattened-trim-candidates.md.
//
// No tokenizer is vendored in this repo (grep turned up none), and calling
// thread/shake/preview once per candidate span would mean spinning up the
// codex-next driver hundreds of times just to size text — too slow/costly for
// a read-only sizing pass. Estimate is bytes/3.844, a constant calibrated so
// that summing over all 949 items reproduces the product's own measured
// 880,020-token count for this exact fixture (replay-proof.md §1); this
// is noted wherever we quote a token number below.
//
// Usage: tsx scripts/history-sizes.ts > /tmp/history-sizes.json

import { readFileSync } from "node:fs";
import { basename, join } from "node:path";

const SESSION_DIR = join(process.env.CODEX_HOME ?? join(process.env.HOME ?? "", ".codex"), "sessions/2026/09/07");
const DEFAULT_ROLLOUTS = [
  "rollout-2026-09-07T17-04-21-01a07e54-7c97-7541-b618-af295f393780.jsonl",
  "rollout-2026-09-07T17-07-37-01a07e57-793b-7a60-85eb-541e2c26bc22.jsonl",
  "rollout-2026-09-07T17-21-12-01a07e63-ebd6-7020-8a28-ee13803e26fd.jsonl",
  "rollout-2026-09-07T17-53-16-01a07e81-455e-7680-bb1f-af433136d256.jsonl",
  "rollout-2026-09-07T17-54-17-01a07e82-32e7-7972-933a-bb6d8d38d32a.jsonl",
].map((f) => join(SESSION_DIR, f));
const CUTOFF = "2026-09-08T01:57:36.059Z";

// Calibration: 3,382,594 bytes (redacted fixture, meta.bytes) / 880,020 tokens
// (replay-proof.md measured via thread/shake/preview) = 3.844 bytes/token.
const BYTES_PER_TOKEN = 3.844;
const TARGET_TOTAL_TOKENS = 880_020;

const PLUMBING_TOOLS = new Set(["spawn_agent", "send_message", "followup_task", "interrupt_agent", "list_agents", "sleep"]);

type Row = { agent: string; isOrchestrator: boolean; timestamp: string; ordinal: number; payload: Record<string, any> };

function textOf(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content.map((p: any) => (p && typeof p === "object" && typeof p.text === "string" ? p.text : "")).join("");
}
function outputToText(output: unknown): string {
  if (typeof output === "string") return output;
  if (Array.isArray(output)) return output.map((p: any) => (p && typeof p === "object" && "text" in p ? String(p.text) : "")).join("\n");
  return output == null ? "" : JSON.stringify(output);
}
function readRollout(path: string) {
  const recs = readFileSync(path, "utf8").split("\n").filter((l) => l.trim().length > 0).map((l) => JSON.parse(l) as any);
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

type EmitItem = { timestamp: string; agent: string; kind: string; name?: string; text: string };

function main(): void {
  const files = DEFAULT_ROLLOUTS.map(readRollout);
  const outputs = new Map<string, string>();
  for (const file of files) {
    for (const row of file.rows) {
      const p = row.payload;
      if ((p?.type === "custom_tool_call_output" || p?.type === "function_call_output") && p.call_id) {
        outputs.set(`${file.agent}:${p.call_id}`, outputToText(p.output ?? p.result));
      }
    }
  }

  type Emit = { timestamp: string; agent: string; ordinal: number; items: EmitItem[] };
  const emits: Emit[] = [];

  for (const file of files) {
    for (const row of file.rows) {
      if (row.timestamp > CUTOFF) continue;
      const p = row.payload;
      const type = p?.type;
      const push = (items: EmitItem[]) => emits.push({ timestamp: row.timestamp, agent: file.agent, ordinal: row.ordinal, items });

      if (type === "reasoning") continue;
      if (type === "agent_message") continue; // dropped in the flattened variant (see flatten-history.ts)
      if (type === "message") {
        const role = p.role;
        const text = textOf(p.content);
        if (!text.trim()) continue;
        if (role === "developer") continue;
        if (role === "user") {
          if (!file.isOrchestrator) continue;
          push([{ timestamp: row.timestamp, agent: file.agent, kind: "message.user", text }]);
          continue;
        }
        if (role === "assistant") {
          push([{ timestamp: row.timestamp, agent: file.agent, kind: file.isOrchestrator ? "message.assistant" : "message.assistant.subagent", text }]);
          continue;
        }
        continue;
      }
      if (type === "function_call" || type === "custom_tool_call") {
        const name = String(p.name ?? "");
        if (PLUMBING_TOOLS.has(name)) continue;
        const callId = `${file.agent}:${p.call_id}`;
        const args = type === "custom_tool_call" ? JSON.stringify({ script: String(p.input ?? "") }) : typeof p.arguments === "string" ? p.arguments : JSON.stringify(p.arguments ?? {});
        const output = outputs.get(callId) ?? "";
        push([
          { timestamp: row.timestamp, agent: file.agent, kind: "function_call", name, text: args },
          { timestamp: row.timestamp, agent: file.agent, kind: "function_call_output", name, text: output },
        ]);
        continue;
      }
      if (type === "function_call_output" || type === "custom_tool_call_output") continue;
    }
  }

  emits.sort((a, b) => (a.timestamp < b.timestamp ? -1 : a.timestamp > b.timestamp ? 1 : a.agent < b.agent ? -1 : a.agent > b.agent ? 1 : a.ordinal - b.ordinal));
  const flat = emits.flatMap((e) => e.items);

  // Raw bytes/token estimate, then rescale so the total matches the product's
  // measured 880,020 tokens for this exact fixture (see BYTES_PER_TOKEN note).
  const sized = flat.map((it, i) => ({ index: i, timestamp: it.timestamp, agent: it.agent, kind: it.kind, name: it.name, bytes: Buffer.byteLength(it.text, "utf8") }));
  const rawTotal = sized.reduce((s, it) => s + it.bytes, 0);
  const scale = TARGET_TOTAL_TOKENS / (rawTotal / BYTES_PER_TOKEN);
  const out = sized.map((it) => ({ ...it, tokensEst: Math.round((it.bytes / BYTES_PER_TOKEN) * scale) }));

  const dumpArg = process.argv.find((a) => a.startsWith("--dump="));
  if (dumpArg) {
    for (const rangeStr of dumpArg.slice("--dump=".length).split(",")) {
      const [a, b] = rangeStr.split("-").map((n) => parseInt(n, 10));
      for (let i = a; i <= (b ?? a); i += 1) {
        const it = flat[i];
        if (!it) continue;
        process.stderr.write(`--- [${i}] ${it.kind} ${it.name ?? ""} ${it.agent} ${it.timestamp} ---\n`);
        process.stderr.write(it.text.slice(0, 1200) + (it.text.length > 1200 ? `\n...[${it.text.length} chars total]...\n` + it.text.slice(-400) : "") + "\n");
      }
    }
    return;
  }

  process.stdout.write(JSON.stringify({ itemCount: out.length, rawBytesTotal: rawTotal, tokensEstTotal: out.reduce((s, it) => s + it.tokensEst, 0), items: out }, null, 0) + "\n");
}

main();
