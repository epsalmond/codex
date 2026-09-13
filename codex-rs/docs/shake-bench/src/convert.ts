// Conversion of the upstream fixture items into `thread/inject_items` payloads,
// plus the evaluation prompt, output schema, answer parsing and exact-match
// scoring.
//
// The prompt text, answer schema shape, `normalizeAnswer` and `parseAnswers`
// logic are adapted from algal/pi-openai-server-compaction,
// benchmarks/native-vs-text/run.ts (MIT licence).

import type { BenchmarkQuestion, ResponseItem } from "./fixtures.ts";

export const SYSTEM_INSTRUCTIONS =
  "You are the assistant responsible for one synthetic software project. Treat statements marked authoritative as binding, preserve exact identifiers and tool outputs, apply later corrections over superseded values, and maintain task state.";

/** Item types the shake `elide` mode keys on, and that inject_items accepts. */
const ALLOWED_TYPES = new Set(["message", "function_call", "function_call_output"]);

export class ConversionError extends Error {}

/**
 * Validate and normalize one generator item into the raw Responses-API shape
 * that `thread/inject_items` persists into a thread's model-visible history.
 *
 * The mapping is 1:1: the upstream generator already emits codex `ResponseItem`
 * shapes. Tool names (`read`, `bash`, `edit`, `database_query`, `deploy`) are
 * kept verbatim, because shake's elide mode keys on the item *type*, not on the
 * tool name.
 */
export function convertItem(item: ResponseItem, index: number): Record<string, unknown> {
  const where = `item[${index}] (type=${String(item.type)})`;
  if (!ALLOWED_TYPES.has(item.type)) {
    throw new ConversionError(`${where}: unsupported item type`);
  }
  if (item.type === "message") {
    const role = item.role;
    if (role !== "user" && role !== "assistant") {
      throw new ConversionError(`${where}: message role must be user or assistant, got ${String(role)}`);
    }
    const content = item.content;
    if (!Array.isArray(content) || content.length === 0) {
      throw new ConversionError(`${where}: message content must be a non-empty array`);
    }
    const wanted = role === "user" ? "input_text" : "output_text";
    for (const part of content as Array<Record<string, unknown>>) {
      if (!part || typeof part !== "object") throw new ConversionError(`${where}: bad content part`);
      if (part.type !== wanted) {
        throw new ConversionError(`${where}: ${role} content part must be ${wanted}, got ${String(part.type)}`);
      }
      if (typeof part.text !== "string") throw new ConversionError(`${where}: content part text must be a string`);
    }
    return { type: "message", role, content };
  }
  if (item.type === "function_call") {
    if (typeof item.call_id !== "string" || item.call_id.length === 0) {
      throw new ConversionError(`${where}: function_call needs a non-empty call_id`);
    }
    if (typeof item.name !== "string" || item.name.length === 0) {
      throw new ConversionError(`${where}: function_call needs a non-empty name`);
    }
    if (typeof item.arguments !== "string") {
      throw new ConversionError(`${where}: function_call arguments must be a JSON string`);
    }
    return { type: "function_call", call_id: item.call_id, name: item.name, arguments: item.arguments };
  }
  if (typeof item.call_id !== "string" || item.call_id.length === 0) {
    throw new ConversionError(`${where}: function_call_output needs a non-empty call_id`);
  }
  if (typeof item.output !== "string") {
    throw new ConversionError(`${where}: function_call_output output must be a string`);
  }
  return { type: "function_call_output", call_id: item.call_id, output: item.output };
}

/** Convert a whole item list, checking that every call_id is paired. */
export function toInjectItems(items: ResponseItem[]): Array<Record<string, unknown>> {
  const converted = items.map((item, index) => convertItem(item, index));
  const calls = new Set<string>();
  for (const item of converted) {
    if (item.type === "function_call") {
      if (calls.has(item.call_id as string)) {
        throw new ConversionError(`duplicate call_id ${String(item.call_id)}`);
      }
      calls.add(item.call_id as string);
    }
  }
  for (const item of converted) {
    if (item.type === "function_call_output" && !calls.has(item.call_id as string)) {
      throw new ConversionError(`function_call_output with unmatched call_id ${String(item.call_id)}`);
    }
  }
  return converted;
}

/**
 * Minimum serialized output size used by the product's elide mode is about
 * 400 tokens. The upstream fixture intentionally has compact tool outputs,
 * so a benchmark that injects them unchanged cannot exercise tool-output
 * elision at all. Keep the fixture and the 1:1 conversion above untouched;
 * this explicit preparation step adds deterministic, answer-free filler to
 * the injected payload only.
 */
export const SHAKE_TOOL_OUTPUT_TARGET_BYTES = 2_048;

const SHAKE_PADDING_LINE = "shake-bench deterministic answer-free retention padding.";

function expandToolOutput(output: string): string {
  const currentBytes = Buffer.byteLength(output, "utf8");
  if (currentBytes >= SHAKE_TOOL_OUTPUT_TARGET_BYTES) return output;
  const needed = SHAKE_TOOL_OUTPUT_TARGET_BYTES - currentBytes;
  const lines = Math.ceil(needed / Buffer.byteLength(`${SHAKE_PADDING_LINE}\n`, "utf8"));
  return `${output}\n${Array.from({ length: lines }, () => SHAKE_PADDING_LINE).join("\n")}`;
}

/**
 * Prepare converted fixture items for the shipped shake implementation.
 * Original output text stays at the beginning of every payload, followed by
 * deterministic filler that carries no benchmark answer.
 */
export function prepareInjectItems(items: ResponseItem[]): Array<Record<string, unknown>> {
  return toInjectItems(items).map((item) =>
    item.type === "function_call_output" && typeof item.output === "string"
      ? { ...item, output: expandToolOutput(item.output) }
      : item,
  );
}

/** Count the tool outputs that the prepared payload makes eligible for elision. */
export function eligibleToolOutputCount(items: Array<Record<string, unknown>>): number {
  return items.filter(
    (item) =>
      item.type === "function_call_output" &&
      typeof item.output === "string" &&
      Buffer.byteLength(item.output, "utf8") >= SHAKE_TOOL_OUTPUT_TARGET_BYTES,
  ).length;
}

/** Rough byte size of the injected payload; used for manifest bookkeeping. */
export function payloadBytes(items: Array<Record<string, unknown>>): number {
  return Buffer.byteLength(JSON.stringify(items), "utf8");
}

export type FixtureVariant = "plain" | "echo";

export const VARIANTS: readonly FixtureVariant[] = ["plain", "echo"];

export function isVariant(value: string): value is FixtureVariant {
  return (VARIANTS as readonly string[]).includes(value);
}

/** Flatten a tool output to a single prose-safe line. */
function condense(output: string): string {
  const line = output.split(/\r?\n/).map((part) => part.trim()).filter(Boolean).join("; ").trim();
  return line.length > 400 ? `${line.slice(0, 400)}...` : line;
}

/**
 * Post-process a generated history into a fixture variant.
 *
 * `plain` is the upstream behaviour, unchanged. `echo` inserts an assistant
 * message after every tool exchange that restates the tool result in prose,
 * which is what real agents do. Because shake's elide mode removes
 * `function_call_output` items but not assistant messages, the echoed prose
 * survives a shake; comparing the two variants bounds how much of elide's
 * measured loss is an artifact of a transcript that never restates anything.
 *
 * Questions and expected answers are untouched: only the history grows.
 * `src/fixtures.ts` stays verbatim upstream.
 */
export function applyVariant(items: ResponseItem[], variant: FixtureVariant): ResponseItem[] {
  if (variant === "plain") return items;
  const out: ResponseItem[] = [];
  const callNames = new Map<string, string>();
  for (const item of items) {
    out.push(item);
    if (item.type === "function_call" && typeof item.call_id === "string" && typeof item.name === "string") {
      callNames.set(item.call_id, item.name);
    }
    if (item.type !== "function_call_output") continue;
    const callId = typeof item.call_id === "string" ? item.call_id : "";
    const name = callNames.get(callId) ?? "tool";
    const condensed = condense(String(item.output ?? ""));
    if (!condensed) continue;
    out.push({
      type: "message",
      role: "assistant",
      content: [{ type: "output_text", text: `Noting the \`${name}\` result verbatim: ${condensed}` }],
    });
  }
  return out;
}

const BASE_PROMPT =
  "Answer every benchmark question from the conversation above. " +
  "Return only the JSON object required by the response schema. " +
  "Each value must be the exact canonical value, with no explanation, labels, units, or extra punctuation. " +
  "Later authoritative corrections supersede earlier or archival values.";

const RECOVERY_HINT =
  "Some earlier tool outputs may appear as recovery placeholders of the form " +
  "`[shaken ~N tokens from <label> (recover: artifact://<id>)]`. " +
  "If a placeholder covers information you need, recover the original text before answering.";

const NOREAD_HINT =
  "Do not call any tools. Answer only from the conversation exactly as it appears, " +
  "including any recovery placeholders, which you must not attempt to recover.";

export type PromptVariant = "default" | "noread";

export function evaluationPrompt(questions: BenchmarkQuestion[], variant: PromptVariant = "default"): string {
  const rendered = questions.map((question) => `${question.id}: ${question.question}`).join("\n");
  const hint = variant === "noread" ? NOREAD_HINT : RECOVERY_HINT;
  return `${BASE_PROMPT}\n\n${hint}\n\n${rendered}`;
}

/**
 * JSON Schema for `turn/start.outputSchema`. Unlike the upstream Responses-API
 * `text.format` payload, app-server takes the bare object schema.
 */
export function answerSchema(questions: BenchmarkQuestion[]): Record<string, unknown> {
  const properties = Object.fromEntries(questions.map((question) => [question.id, { type: "string" }]));
  return {
    type: "object",
    properties: {
      answers: {
        type: "object",
        properties,
        required: questions.map((question) => question.id),
        additionalProperties: false,
      },
    },
    required: ["answers"],
    additionalProperties: false,
  };
}

export function normalizeAnswer(value: unknown): string {
  return typeof value === "string" ? value.trim().replace(/^['"]|['"]$/g, "") : String(value ?? "").trim();
}

export function parseAnswers(text: string): Record<string, string> {
  const fromObject = (parsed: unknown): Record<string, string> | undefined => {
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return undefined;
    const answers = (parsed as { answers?: unknown }).answers;
    const source = answers && typeof answers === "object" && !Array.isArray(answers) ? answers : parsed;
    return Object.fromEntries(
      Object.entries(source as Record<string, unknown>).map(([key, value]) => [key, normalizeAnswer(value)]),
    );
  };
  try {
    const direct = fromObject(JSON.parse(text));
    if (direct) return direct;
  } catch {
    // fall through to lenient extraction
  }
  const start = text.indexOf("{");
  const end = text.lastIndexOf("}");
  if (start < 0 || end <= start) return {};
  try {
    return fromObject(JSON.parse(text.slice(start, end + 1))) ?? {};
  } catch {
    return {};
  }
}

export type ScoreRow = {
  questionId: string;
  category: string;
  expected: string;
  actual: string;
  correct: boolean;
};

export function scoreAnswers(questions: BenchmarkQuestion[], answers: Record<string, string>): ScoreRow[] {
  return questions.map((question) => {
    const actual = normalizeAnswer(answers[question.id]);
    const expected = normalizeAnswer(question.expected);
    return { questionId: question.id, category: question.category, expected, actual, correct: actual === expected };
  });
}
