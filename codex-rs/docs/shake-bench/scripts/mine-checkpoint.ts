#!/usr/bin/env node
// Mines a Codex rollout JSONL transcript for a replay-benchmark checkpoint
// analysis: per-request token timeline, tool calls (exec/apply_patch),
// file-mutating operations, subagent (multi-agent) FINAL_ANSWER reports, and
// top-level user messages.
//
// Record shapes observed in this rollout (see README notes below):
//   {type:"token_usage_record", ordinal, timestamp, payload:{usage:{...}}}
//   {type:"response_item", ordinal, timestamp,
//     payload:{type:"message"|"agent_message"|"reasoning"|
//               "function_call"|"function_call_output"|
//               "custom_tool_call"|"custom_tool_call_output", ...}}
//   {type:"turn_context", ...}  {type:"world_state", ...}
//
// Usage: tsx scripts/mine-checkpoint.ts <rollout.jsonl> [outFile.json]

import { readFileSync, writeFileSync } from "node:fs";

type Json = Record<string, unknown>;

const inPath = process.argv[2];
const outPath = process.argv[3];
if (!inPath) {
  console.error("usage: mine-checkpoint.ts <rollout.jsonl> [out.json]");
  process.exit(1);
}

const lines = readFileSync(inPath, "utf8").split("\n").filter((l) => l.trim().length > 0);

type Rec = {
  timestamp: string;
  ordinal: number;
  type: string;
  payload: Json;
};

const recs: Rec[] = lines.map((l) => JSON.parse(l) as Rec);

// --- 1. Build the request (token_usage_record) timeline ---------------------
type RequestPoint = {
  index: number; // 1-based
  ordinal: number;
  timestamp: string;
  inputTokens: number;
  cachedInputTokens: number;
  outputTokens: number;
  reasoningOutputTokens: number;
};

const requests: RequestPoint[] = [];
for (const r of recs) {
  if (r.type !== "token_usage_record") continue;
  const usage = (r.payload as any).usage ?? {};
  requests.push({
    index: requests.length + 1,
    ordinal: r.ordinal,
    timestamp: r.timestamp,
    inputTokens: usage.input_tokens ?? 0,
    cachedInputTokens: usage.cached_input_tokens ?? 0,
    outputTokens: usage.output_tokens ?? 0,
    reasoningOutputTokens: usage.reasoning_output_tokens ?? 0,
  });
}

function requestIndexForOrdinal(ordinal: number): number {
  // The request whose token_usage_record is the first with ordinal > this event's ordinal.
  // (i.e. "this event is part of building toward request N").
  let lo = 0;
  let hi = requests.length - 1;
  let ans = requests.length + 1; // past the end -> after the last recorded request
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (requests[mid].ordinal > ordinal) {
      ans = requests[mid].index;
      hi = mid - 1;
    } else {
      lo = mid + 1;
    }
  }
  return ans;
}

// --- 2. Walk response_items, classify tool calls, extract files -------------
const MUTATING_PATTERNS = [
  /apply_patch/i,
  /\bsed\s+-i\b/,
  /\bcargo\s+fmt\b/,
  /\bcargo\s+insta\s+accept\b/,
  /\bgit\s+commit\b/,
  /\bgit\s+apply\b/,
  /\bmv\s+/,
  /\brm\s+-rf?\s+/,
  /\bcp\s+.*\s+\S+\.(rs|ts|md|toml|json)\b/,
  /\bwrite_stdin\b.*cat\s*>/,
  />\s*\S+\.(rs|ts|md|toml|json)\b/,
];

function isBackgroundish(input: string): boolean {
  return (
    /session_id/i.test(input) ||
    /write_stdin/i.test(input) ||
    /yield_time_ms/i.test(input) ||
    /nohup|&\s*$/i.test(input)
  );
}

function extractApplyPatchFiles(input: string): { file: string; op: string }[] {
  // `input` is a JS source string, so patch text embedded as a JS string
  // literal contains literal two-character "\n" escapes, not real newlines.
  // Stop the filename capture at whichever comes first: a real newline or a
  // literal backslash-n escape.
  const out: { file: string; op: string }[] = [];
  const re = /\*\*\*\s*(Add|Update|Delete)\s+File:\s*([^\n]+)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(input))) {
    const raw = m[2];
    const cut = raw.indexOf("\\n");
    const file = (cut === -1 ? raw : raw.slice(0, cut)).trim();
    out.push({ op: m[1].toLowerCase(), file });
  }
  return out;
}

type ToolEvent = {
  ordinal: number;
  timestamp: string;
  requestIndex: number;
  kind: "exec" | "apply_patch" | "sleep" | "spawn_agent" | "send_message" | "followup_task" | "interrupt_agent" | "list_agents" | "other_function_call";
  mutating: boolean;
  backgroundish: boolean;
  name?: string;
  summary: string;
  filesTouched?: { file: string; op: string }[];
};

type AgentMessage = {
  ordinal: number;
  timestamp: string;
  requestIndex: number;
  author: string;
  recipient: string;
  text: string;
};

type UserMessage = {
  ordinal: number;
  timestamp: string;
  requestIndex: number;
  text: string;
};

const toolEvents: ToolEvent[] = [];
const agentMessages: AgentMessage[] = [];
const userMessages: UserMessage[] = [];
const fileMutations: { ordinal: number; timestamp: string; requestIndex: number; file: string; op: string }[] = [];

for (const r of recs) {
  if (r.type !== "response_item") continue;
  const p = r.payload as any;
  const reqIdx = requestIndexForOrdinal(r.ordinal);

  if (p.type === "message" && p.role === "user") {
    const text = (p.content?.[0]?.text ?? "").toString();
    userMessages.push({ ordinal: r.ordinal, timestamp: r.timestamp, requestIndex: reqIdx, text });
    continue;
  }

  if (p.type === "agent_message") {
    const text = (p.content?.[0]?.text ?? "").toString();
    agentMessages.push({
      ordinal: r.ordinal,
      timestamp: r.timestamp,
      requestIndex: reqIdx,
      author: p.author ?? "",
      recipient: p.recipient ?? "",
      text,
    });
    continue;
  }

  if (p.type === "custom_tool_call" && p.name === "exec") {
    const input: string = p.input ?? "";
    const mutating = MUTATING_PATTERNS.some((re) => re.test(input));
    const patchFiles = extractApplyPatchFiles(input);
    if (patchFiles.length) {
      for (const f of patchFiles) {
        fileMutations.push({ ordinal: r.ordinal, timestamp: r.timestamp, requestIndex: reqIdx, file: f.file, op: f.op });
      }
    }
    toolEvents.push({
      ordinal: r.ordinal,
      timestamp: r.timestamp,
      requestIndex: reqIdx,
      kind: patchFiles.length ? "apply_patch" : "exec",
      mutating: mutating || patchFiles.length > 0,
      backgroundish: isBackgroundish(input),
      summary: input.slice(0, 300),
      filesTouched: patchFiles.length ? patchFiles : undefined,
    });
    continue;
  }

  if (p.type === "function_call") {
    const name = p.name as string;
    let kind: ToolEvent["kind"] = "other_function_call";
    if (name === "sleep") kind = "sleep";
    else if (name === "spawn_agent") kind = "spawn_agent";
    else if (name === "send_message") kind = "send_message";
    else if (name === "followup_task") kind = "followup_task";
    else if (name === "interrupt_agent") kind = "interrupt_agent";
    else if (name === "list_agents") kind = "list_agents";
    toolEvents.push({
      ordinal: r.ordinal,
      timestamp: r.timestamp,
      requestIndex: reqIdx,
      kind,
      mutating: false,
      backgroundish: kind === "sleep",
      name,
      summary: (p.arguments ?? "").toString().slice(0, 300),
    });
    continue;
  }
}

// --- 3. Serialize -------------------------------------------------------
const timeline = {
  sourceFile: inPath,
  requestCount: requests.length,
  requests,
  toolEvents,
  agentMessages,
  userMessages,
  fileMutations,
};

const json = JSON.stringify(timeline, null, 2);
if (outPath) {
  writeFileSync(outPath, json);
  console.error(`wrote ${outPath} (${requests.length} requests, ${toolEvents.length} tool events, ${agentMessages.length} agent messages, ${fileMutations.length} apply_patch file ops)`);
} else {
  process.stdout.write(json);
}
