import { createReadStream } from "node:fs";
import type { Dirent } from "node:fs";
import { lstat, realpath, readdir } from "node:fs/promises";
import { createInterface } from "node:readline";
import { resolve } from "node:path";
import { createZstdDecompress } from "node:zlib";

/** Categories assigned once to each usage-bearing model response. */
export type PollingActionCategory =
  | "pure_wait_agent"
  | "wait_containing_mixed_calls"
  | "pure_write_stdin"
  | "other"
  | "unknown_no_attribution";

/** Input is the provider-reported total; cached input is a subset of it. */
export interface TokenTotals {
  inputTokens: number;
  cachedInputTokens: number;
  uncachedInputTokens: number;
  outputTokens: number;
  responseCount: number;
}

export type CategoryTotals = Record<PollingActionCategory, TokenTotals>;

/** Local-only provenance for a thread's rollout files. */
export interface ThreadSummary {
  threadId: string;
  parentThreadId: string | null;
  sourceRolloutPaths: string[];
  totals: TokenTotals;
  byCategory: CategoryTotals;
}

/** Root session totals keep the root thread separate from every descendant. */
export interface SessionSummary {
  sessionId: string;
  root: ThreadSummary;
  descendants: ThreadSummary[];
  /** False means the rollout had no provider usage records, not zero token use. */
  usageTraceAvailable: boolean;
  totals: TokenTotals;
  byCategory: CategoryTotals;
  /** Strict wait_agent total across root and descendants. */
  waitAttributedInputTokens: number | null;
  /** Pure write_stdin total; this can include non-polling stdin writes. */
  writeStdinAttributedInputTokens: number | null;
  /** null when usage is absent or action attribution is incomplete. */
  pollingCandidateInputTokens: number | null;
  /** False if usage is absent or any response has mixed or unknown action attribution. */
  actionAttributionComplete: boolean;
}

/** Results are returned to the local caller; this module has no network or write path. */
export interface PollingScanResult {
  sessions: SessionSummary[];
  totals: TokenTotals;
  byCategory: CategoryTotals;
  scannedRolloutCount: number;
  skippedRolloutCount: number;
}

interface SessionMetadata {
  threadId: string;
  sessionId: string | null;
  parentThreadId: string | null;
}

interface UsageValues {
  inputTokens: number;
  cachedInputTokens: number;
  outputTokens: number;
}

interface UsageCandidate {
  threadId: string;
  responseId: string;
  usage: UsageValues;
  actionNames: Array<string | null>;
  nonToolResponseItem: boolean;
}

interface ParsedRollout {
  metadata: SessionMetadata;
  sourcePath: string;
  usageCandidates: UsageCandidate[];
}

interface MutableThread {
  threadId: string;
  parentThreadId: string | null;
  sourceRolloutPaths: Set<string>;
  usageByResponse: Map<string, UsageCandidate>;
}

const ACTION_CATEGORIES: PollingActionCategory[] = [
  "pure_wait_agent",
  "wait_containing_mixed_calls",
  "pure_write_stdin",
  "other",
  "unknown_no_attribution",
];

/**
 * Scan only the Codex homes passed by the caller. The scanner never consults
 * environment variables, uploads data, or writes to the scanned homes.
 *
 * Rollout usage is counted once per (thread, response_id), using `usage` from
 * token_usage_record (or the corresponding raw_response_completed event), not
 * the cumulative thread/turn fields. Input is split once: cached is clamped to
 * the reported input range and uncached is the remainder. Output counts only
 * `output_tokens`, not reasoning_output_tokens.
 *
 * Action attribution follows response-item order up to raw completion or usage
 * records. A response with several calls contributes its token usage once. A
 * response containing wait_agent plus another call is kept in the mixed bucket;
 * its tokens are not split among tools. The ranking field sums only pure
 * wait_agent input across the root and descendants, so ambiguous mixed and
 * unknown rows do not inflate the rank. Pure `write_stdin` is also ranked as a
 * candidate because polling sessions can use it, but it may instead write
 * stdin; callers must inspect that local transcript context before attributing
 * it as polling. Missing or malformed call attribution remains visible in
 * `unknown_no_attribution`.
 */
export async function scanCodexHomes(
  explicitHomes: readonly string[],
): Promise<PollingScanResult> {
  const rolloutPaths = new Set<string>();
  for (const home of explicitHomes) {
    for (const rolloutPath of await discoverRollouts(resolve(home))) {
      try {
        rolloutPaths.add(await realpath(rolloutPath));
      } catch {
        rolloutPaths.add(rolloutPath);
      }
    }
  }

  const parsedRollouts: ParsedRollout[] = [];
  let skippedRolloutCount = 0;
  for (const sourcePath of [...rolloutPaths].sort()) {
    const parsed = await parseRolloutFile(sourcePath);
    if (parsed) parsedRollouts.push(parsed);
    else skippedRolloutCount += 1;
  }

  const roots = new Map<string, MutableThread>();
  const threads = new Map<string, MutableThread>();
  for (const rollout of parsedRollouts) {
    const metadata = rollout.metadata;
    const thread = getOrCreateThread(threads, metadata);
    thread.sourceRolloutPaths.add(rollout.sourcePath);
    for (const candidate of rollout.usageCandidates) {
      mergeCandidate(thread.usageByResponse, candidate);
    }
    if (
      metadata.parentThreadId === null &&
      (metadata.sessionId === null || metadata.sessionId === metadata.threadId)
    ) {
      roots.set(metadata.threadId, thread);
    }
  }

  const sessionThreads = new Map<string, MutableThread[]>();
  for (const thread of threads.values()) {
    const metadata = parsedRollouts.find(
      (rollout) => rollout.metadata.threadId === thread.threadId,
    )?.metadata;
    if (!metadata) continue;
    const rootId = findRootId(metadata, roots, threads);
    if (rootId === null) continue;
    const group = sessionThreads.get(rootId) ?? [];
    group.push(thread);
    sessionThreads.set(rootId, group);
  }

  const sessions: SessionSummary[] = [];
  for (const [rootId, group] of sessionThreads) {
    const rootThread = roots.get(rootId);
    if (!rootThread) continue;
    const descendants = group
      .filter((thread) => thread.threadId !== rootId)
      .sort((left, right) => left.threadId.localeCompare(right.threadId))
      .map(summarizeThread);
    const root = summarizeThread(rootThread);
    const byCategory = emptyCategoryTotals();
    addCategoryTotals(byCategory, root.byCategory);
    for (const descendant of descendants) {
      addCategoryTotals(byCategory, descendant.byCategory);
    }
    const totals = totalsFromCategories(byCategory);
    const actionAttributionComplete = totals.responseCount > 0 &&
      byCategory.wait_containing_mixed_calls.responseCount === 0 &&
      byCategory.unknown_no_attribution.responseCount === 0;
    sessions.push({
      sessionId: rootId,
      root,
      descendants,
      usageTraceAvailable: totals.responseCount > 0,
      totals,
      byCategory,
      waitAttributedInputTokens: totals.responseCount > 0
        ? sumThreadCategory(root, descendants, "pure_wait_agent")
        : null,
      writeStdinAttributedInputTokens: totals.responseCount > 0
        ? sumThreadCategory(root, descendants, "pure_write_stdin")
        : null,
      pollingCandidateInputTokens: actionAttributionComplete
        ? sumTokenTotals(
          sumThreadCategory(root, descendants, "pure_wait_agent"),
          sumThreadCategory(root, descendants, "pure_write_stdin"),
        )
        : null,
      actionAttributionComplete,
    });
  }

  sessions.sort(
    (left, right) =>
      (right.pollingCandidateInputTokens ?? -1) - (left.pollingCandidateInputTokens ?? -1) ||
      (right.waitAttributedInputTokens ?? -1) - (left.waitAttributedInputTokens ?? -1) ||
      left.sessionId.localeCompare(right.sessionId),
  );
  const byCategory = emptyCategoryTotals();
  for (const session of sessions) addCategoryTotals(byCategory, session.byCategory);
  return {
    sessions,
    totals: totalsFromCategories(byCategory),
    byCategory,
    scannedRolloutCount: parsedRollouts.length,
    skippedRolloutCount,
  };
}

function sumThreadCategory(
  root: ThreadSummary,
  descendants: ThreadSummary[],
  category: PollingActionCategory,
): number {
  return descendants.reduce(
    (sum, thread) => sumTokenTotals(sum, thread.byCategory[category].inputTokens),
    root.byCategory[category].inputTokens,
  );
}

async function discoverRollouts(home: string): Promise<string[]> {
  const paths: string[] = [];
  for (const directory of ["sessions", "archived_sessions"]) {
    await walkRolloutTree(resolve(home, directory), paths);
  }
  return paths;
}

async function walkRolloutTree(directory: string, paths: string[]): Promise<void> {
  try {
    const stats = await lstat(directory);
    if (!stats.isDirectory() || stats.isSymbolicLink()) return;
  } catch {
    return;
  }
  let entries: Dirent[];
  try {
    entries = await readdir(directory, { withFileTypes: true });
  } catch {
    return;
  }
  entries.sort((left, right) => left.name.localeCompare(right.name));
  for (const entry of entries) {
    const entryPath = resolve(directory, entry.name);
    if (entry.isDirectory()) {
      await walkRolloutTree(entryPath, paths);
    } else if (entry.isFile() && isRolloutFile(entry.name)) {
      paths.push(entryPath);
    }
  }
}

function isRolloutFile(name: string): boolean {
  return name.startsWith("rollout-") && (name.endsWith(".jsonl") || name.endsWith(".jsonl.zst"));
}

async function parseRolloutFile(sourcePath: string): Promise<ParsedRollout | null> {
  const input = createReadStream(sourcePath);
  const readable = sourcePath.endsWith(".zst") ? input.pipe(createZstdDecompress()) : input;
  const lines = createInterface({ input: readable, crlfDelay: Infinity });
  let metadata: SessionMetadata | null = null;
  let pendingActionNames: Array<string | null> = [];
  let pendingNonToolResponseItem = false;
  const candidates = new Map<string, UsageCandidate>();

  try {
    for await (const line of lines) {
      const row = parseJsonObject(line);
      if (!row) continue;
      if (row.type === "session_meta") {
        metadata = parseSessionMetadata(row.payload);
      } else if (row.type === "response_item") {
        const action = responseItemActionName(row.payload);
        if (action !== undefined) pendingActionNames.push(action);
        else if (nonEmptyString(asRecord(row.payload)?.type)) pendingNonToolResponseItem = true;
      } else if (row.type === "event_msg") {
        const event = asRecord(row.payload);
        if (event?.type === "raw_response_completed") {
          const responseId = nonEmptyString(event.response_id);
          const usage = parseUsage(event.token_usage);
          const actionNames = pendingActionNames;
          const nonToolResponseItem = pendingNonToolResponseItem;
          pendingActionNames = [];
          pendingNonToolResponseItem = false;
          if (responseId && usage) {
            mergeCandidate(candidates, {
              threadId: metadata?.threadId ?? "",
              responseId,
              usage,
              actionNames,
              nonToolResponseItem,
            });
          }
        }
      } else if (row.type === "token_usage_record") {
        const record = asRecord(row.payload);
        if (record) {
          const candidate = parseTokenUsageRecord(
            record,
            pendingActionNames,
            pendingNonToolResponseItem,
            metadata?.threadId,
          );
          pendingActionNames = [];
          pendingNonToolResponseItem = false;
          if (candidate) mergeCandidate(candidates, candidate);
        }
      } else if (row.type === "compacted") {
        const payload = asRecord(row.payload);
        const record = asRecord(payload?.latest_token_usage_record);
        if (record) {
          const candidate = parseTokenUsageRecord(record, [], false, metadata?.threadId);
          if (candidate) mergeCandidate(candidates, candidate);
        }
      }
    }
  } catch {
    lines.close();
    input.destroy();
    return null;
  }

  if (!metadata) return null;
  const usageCandidates = [...candidates.values()]
    .filter((candidate) => candidate.threadId === metadata?.threadId)
    .map((candidate) => ({ ...candidate, threadId: metadata?.threadId ?? "" }));
  return { metadata, sourcePath, usageCandidates };
}

function parseSessionMetadata(value: unknown): SessionMetadata | null {
  const payload = asRecord(value);
  const metadata = asRecord(payload?.meta) ?? payload;
  const threadId = nonEmptyString(metadata.id);
  if (!threadId) return null;
  return {
    threadId,
    sessionId: nonEmptyString(metadata.session_id),
    parentThreadId: nonEmptyString(metadata.parent_thread_id),
  };
}

function parseTokenUsageRecord(
  record: Record<string, unknown>,
  actionNames: Array<string | null>,
  nonToolResponseItem: boolean,
  fallbackThreadId?: string,
): UsageCandidate | null {
  const responseId = nonEmptyString(record.response_id);
  const threadId = nonEmptyString(record.thread_id) ?? fallbackThreadId;
  const usage = parseUsage(record.usage);
  if (!responseId || !threadId || !usage) return null;
  return { threadId, responseId, usage, actionNames: [...actionNames], nonToolResponseItem };
}

function parseUsage(value: unknown): UsageValues | null {
  const usage = asRecord(value);
  if (!usage) return null;
  return {
    inputTokens: nonNegativeCount(usage.input_tokens),
    cachedInputTokens: nonNegativeCount(usage.cached_input_tokens),
    outputTokens: nonNegativeCount(usage.output_tokens),
  };
}

function responseItemActionName(value: unknown): string | null | undefined {
  const item = asRecord(value);
  const type = nonEmptyString(item?.type);
  if (!type) return undefined;
  if (type === "function_call" || type === "custom_tool_call") {
    return nonEmptyString(item?.name) ?? null;
  }
  if (
    type === "local_shell_call" ||
    type === "tool_search_call" ||
    (type.endsWith("_call") && !type.endsWith("_call_output"))
  ) {
    return type;
  }
  return undefined;
}

function parseJsonObject(line: string): Record<string, unknown> | null {
  try {
    return asRecord(JSON.parse(line));
  } catch {
    return null;
  }
}

function asRecord(value: unknown): Record<string, unknown> | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  return value as Record<string, unknown>;
}

function nonEmptyString(value: unknown): string | null {
  return typeof value === "string" && value.trim().length > 0 ? value : null;
}

function nonNegativeCount(value: unknown): number {
  if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) return 0;
  return Math.min(Math.trunc(value), Number.MAX_SAFE_INTEGER);
}

function classifyActions(actionNames: Array<string | null>, nonToolResponseItem: boolean): PollingActionCategory {
  if (actionNames.length === 0) return nonToolResponseItem ? "other" : "unknown_no_attribution";
  const waitCount = actionNames.filter((name) => name === "wait_agent").length;
  if (waitCount > 0) {
    return actionNames.every((name) => name === "wait_agent")
      ? "pure_wait_agent"
      : "wait_containing_mixed_calls";
  }
  if (actionNames.every((name) => name === "write_stdin")) return "pure_write_stdin";
  if (actionNames.every((name) => name !== null)) return "other";
  return "unknown_no_attribution";
}

function mergeCandidate(
  candidates: Map<string, UsageCandidate>,
  candidate: UsageCandidate,
): void {
  if (!candidate.threadId) return;
  const key = `${candidate.threadId}\0${candidate.responseId}`;
  const previous = candidates.get(key);
  if (!previous || attributionQuality(candidate.actionNames) > attributionQuality(previous.actionNames)) {
    candidates.set(key, candidate);
  }
}

function attributionQuality(actionNames: Array<string | null>): number {
  if (actionNames.length === 0) return 0;
  const knownCount = actionNames.filter((name) => name !== null).length;
  return knownCount * 2 + actionNames.length;
}

function getOrCreateThread(
  threads: Map<string, MutableThread>,
  metadata: SessionMetadata,
): MutableThread {
  let thread = threads.get(metadata.threadId);
  if (!thread) {
    thread = {
      threadId: metadata.threadId,
      parentThreadId: metadata.parentThreadId,
      sourceRolloutPaths: new Set(),
      usageByResponse: new Map(),
    };
    threads.set(metadata.threadId, thread);
  } else if (thread.parentThreadId === null && metadata.parentThreadId !== null) {
    thread.parentThreadId = metadata.parentThreadId;
  }
  return thread;
}

function findRootId(
  metadata: SessionMetadata,
  roots: Map<string, MutableThread>,
  threads: Map<string, MutableThread>,
): string | null {
  if (roots.has(metadata.threadId)) return metadata.threadId;
  if (metadata.sessionId && roots.has(metadata.sessionId)) return metadata.sessionId;
  let parentId = metadata.parentThreadId;
  const seen = new Set<string>();
  while (parentId && !seen.has(parentId)) {
    if (roots.has(parentId)) return parentId;
    seen.add(parentId);
    parentId = threads.get(parentId)?.parentThreadId ?? null;
  }
  return null;
}

function summarizeThread(thread: MutableThread): ThreadSummary {
  const byCategory = emptyCategoryTotals();
  for (const candidate of thread.usageByResponse.values()) {
    const category = classifyActions(candidate.actionNames, candidate.nonToolResponseItem);
    addUsage(byCategory[category], candidate.usage);
  }
  return {
    threadId: thread.threadId,
    parentThreadId: thread.parentThreadId,
    sourceRolloutPaths: [...thread.sourceRolloutPaths].sort(),
    totals: totalsFromCategories(byCategory),
    byCategory,
  };
}

function emptyCategoryTotals(): CategoryTotals {
  return {
    pure_wait_agent: emptyTotals(),
    wait_containing_mixed_calls: emptyTotals(),
    pure_write_stdin: emptyTotals(),
    other: emptyTotals(),
    unknown_no_attribution: emptyTotals(),
  };
}

function emptyTotals(): TokenTotals {
  return { inputTokens: 0, cachedInputTokens: 0, uncachedInputTokens: 0, outputTokens: 0, responseCount: 0 };
}

function addUsage(target: TokenTotals, usage: UsageValues): void {
  const inputTokens = nonNegativeCount(usage.inputTokens);
  const cachedInputTokens = Math.min(inputTokens, nonNegativeCount(usage.cachedInputTokens));
  target.inputTokens = sumTokenTotals(target.inputTokens, inputTokens);
  target.cachedInputTokens = sumTokenTotals(target.cachedInputTokens, cachedInputTokens);
  target.uncachedInputTokens = sumTokenTotals(
    target.uncachedInputTokens,
    inputTokens - cachedInputTokens,
  );
  target.outputTokens = sumTokenTotals(target.outputTokens, nonNegativeCount(usage.outputTokens));
  target.responseCount = sumTokenTotals(target.responseCount, 1);
}

function addCategoryTotals(target: CategoryTotals, source: CategoryTotals): void {
  for (const category of ACTION_CATEGORIES) addTotals(target[category], source[category]);
}

function addTotals(target: TokenTotals, source: TokenTotals): void {
  target.inputTokens = sumTokenTotals(target.inputTokens, source.inputTokens);
  target.cachedInputTokens = sumTokenTotals(target.cachedInputTokens, source.cachedInputTokens);
  target.uncachedInputTokens = sumTokenTotals(target.uncachedInputTokens, source.uncachedInputTokens);
  target.outputTokens = sumTokenTotals(target.outputTokens, source.outputTokens);
  target.responseCount = sumTokenTotals(target.responseCount, source.responseCount);
}

function totalsFromCategories(categories: CategoryTotals): TokenTotals {
  const totals = emptyTotals();
  for (const category of ACTION_CATEGORIES) addTotals(totals, categories[category]);
  return totals;
}

function sumTokenTotals(left: number, right: number): number {
  return Math.min(left + right, Number.MAX_SAFE_INTEGER);
}
