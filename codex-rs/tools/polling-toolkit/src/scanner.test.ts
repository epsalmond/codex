import assert from "node:assert/strict";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { zstdCompressSync } from "node:zlib";

import { scanCodexHomes } from "./scanner.ts";

type JsonRecord = Record<string, unknown>;

const rootId = "00000000-0000-4000-8000-000000000001";
const childId = "00000000-0000-4000-8000-000000000002";
const grandchildId = "00000000-0000-4000-8000-000000000003";

function metadata(
  id: string,
  sessionId: string,
  parentThreadId: string | null = null,
): JsonRecord {
  return {
    type: "session_meta",
    payload: {
      id,
      session_id: sessionId,
      parent_thread_id: parentThreadId,
    },
  };
}

function call(name?: string): JsonRecord {
  return {
    type: "response_item",
    payload: {
      type: "function_call",
      ...(name === undefined ? {} : { name }),
      call_id: `call-${name ?? "unknown"}`,
      arguments: "{}",
    },
  };
}

function completed(
  responseId: string,
  usage: { input_tokens: number; cached_input_tokens: number; output_tokens: number },
): JsonRecord {
  return {
    type: "event_msg",
    payload: {
      type: "raw_response_completed",
      response_id: responseId,
      token_usage: usage,
    },
  };
}

function usageRecord(
  threadId: string,
  responseId: string,
  usage: { input_tokens: number; cached_input_tokens: number; output_tokens: number },
): JsonRecord {
  return {
    type: "token_usage_record",
    payload: {
      thread_id: threadId,
      response_id: responseId,
      usage,
      turn_token_usage: usage,
      thread_token_usage: usage,
    },
  };
}

function rollout(...rows: JsonRecord[]): string {
  return `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`;
}

async function withHomes<T>(
  action: (directory: string) => Promise<T>,
): Promise<T> {
  const directory = await mkdtemp(join(tmpdir(), "codex-polling-scanner-"));
  try {
    return await action(directory);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

async function writeRollout(
  home: string,
  relativePath: string,
  contents: string,
): Promise<string> {
  const sourcePath = resolve(home, relativePath);
  await mkdir(dirname(sourcePath), { recursive: true });
  await writeFile(sourcePath, contents, "utf8");
  return sourcePath;
}

test("discovers nested descendants and ranks only pure wait_agent input", async () => {
  await withHomes(async (directory) => {
    const home = join(directory, "codex-home");
    await writeRollout(
      home,
      "sessions/2026/09/27/rollout-root.jsonl",
      rollout(
        metadata(rootId, rootId),
        call("wait_agent"),
        completed("response-root", { input_tokens: 11, cached_input_tokens: 4, output_tokens: 2 }),
        usageRecord(rootId, "response-root", {
          input_tokens: 11,
          cached_input_tokens: 4,
          output_tokens: 2,
        }),
      ),
    );
    await writeRollout(
      home,
      "sessions/2026/09/27/rollout-child.jsonl",
      rollout(
        metadata(childId, rootId, rootId),
        call("wait_agent"),
        call("write_stdin"),
        completed("response-child", { input_tokens: 20, cached_input_tokens: 5, output_tokens: 4 }),
        call("write_stdin"),
        completed("response-child-poll", { input_tokens: 5, cached_input_tokens: 2, output_tokens: 1 }),
      ),
    );
    await writeRollout(
      home,
      "archived_sessions/2026/09/27/rollout-grandchild.jsonl",
      rollout(
        metadata(grandchildId, rootId, childId),
        call("wait_agent"),
        completed("response-grandchild", {
          input_tokens: 30,
          cached_input_tokens: 8,
          output_tokens: 5,
        }),
      ),
    );

    const result = await scanCodexHomes([home]);
    assert.equal(result.sessions.length, 1);
    const session = result.sessions[0];
    assert.ok(session);
    assert.equal(session.sessionId, rootId);
    assert.equal(session.root.threadId, rootId);
    assert.deepEqual(
      session.descendants.map((thread) => thread.threadId),
      [childId, grandchildId],
    );
    assert.equal(session.root.byCategory.pure_wait_agent.inputTokens, 11);
    assert.equal(session.descendants[0]?.byCategory.wait_containing_mixed_calls.inputTokens, 20);
    assert.equal(session.descendants[0]?.byCategory.pure_write_stdin.inputTokens, 5);
    assert.equal(session.descendants[1]?.byCategory.pure_wait_agent.inputTokens, 30);
    assert.equal(session.waitAttributedInputTokens, 41);
    assert.equal(session.writeStdinAttributedInputTokens, 5);
    assert.equal(session.pollingCandidateInputTokens, null);
    assert.equal(session.usageTraceAvailable, true);
    assert.equal(session.actionAttributionComplete, false);
    assert.deepEqual(session.totals, {
      inputTokens: 66,
      cachedInputTokens: 19,
      uncachedInputTokens: 47,
      outputTokens: 12,
      responseCount: 4,
    });
    assert.equal(result.sessions[0]?.byCategory.wait_containing_mixed_calls.inputTokens, 20);
    assert.equal(result.sessions[0]?.byCategory.pure_wait_agent.inputTokens, 41);
  });
});

test("clamps negative usage and cached input to the reported input total", async () => {
  await withHomes(async (directory) => {
    const home = join(directory, "codex-home");
    await writeRollout(
      home,
      "sessions/rollout-clamped.jsonl",
      rollout(
        metadata(rootId, rootId),
        completed("response-high-cache", {
          input_tokens: 10,
          cached_input_tokens: 14,
          output_tokens: -2,
        }),
        completed("response-negative", {
          input_tokens: -5,
          cached_input_tokens: 2,
          output_tokens: 4,
        }),
      ),
    );

    const result = await scanCodexHomes([home]);
    assert.deepEqual(result.totals, {
      inputTokens: 10,
      cachedInputTokens: 10,
      uncachedInputTokens: 0,
      outputTokens: 4,
      responseCount: 2,
    });
    assert.equal(result.byCategory.unknown_no_attribution.inputTokens, 10);
  });
});

test("ranks pure write_stdin candidates alongside strict wait_agent usage", async () => {
  await withHomes(async (directory) => {
    const home = join(directory, "codex-home");
    const waitId = "00000000-0000-4000-8000-000000000011";
    const stdinId = "00000000-0000-4000-8000-000000000012";
    await writeRollout(home, "sessions/rollout-wait.jsonl", rollout(
      metadata(waitId, waitId),
      call("wait_agent"),
      completed("wait-response", { input_tokens: 10, cached_input_tokens: 0, output_tokens: 0 }),
    ));
    await writeRollout(home, "sessions/rollout-stdin.jsonl", rollout(
      metadata(stdinId, stdinId),
      call("write_stdin"),
      completed("stdin-response", { input_tokens: 11, cached_input_tokens: 0, output_tokens: 0 }),
    ));
    const result = await scanCodexHomes([home]);
    assert.deepEqual(result.sessions.map((session) => session.sessionId), [stdinId, waitId]);
    assert.equal(result.sessions[0]?.pollingCandidateInputTokens, 11);
    assert.equal(result.sessions[0]?.actionAttributionComplete, true);
    assert.equal(result.sessions[0]?.waitAttributedInputTokens, 0);
  });
});

test("assigns a multiple-action response once to the mixed bucket", async () => {
  await withHomes(async (directory) => {
    const home = join(directory, "codex-home");
    const usage = { input_tokens: 17, cached_input_tokens: 3, output_tokens: 8 };
    await writeRollout(
      home,
      "sessions/rollout-mixed.jsonl",
      rollout(
        metadata(rootId, rootId),
        call("wait_agent"),
        call("write_stdin"),
        completed("response-mixed", usage),
        usageRecord(rootId, "response-mixed", usage),
      ),
    );

    const result = await scanCodexHomes([home]);
    assert.equal(result.totals.responseCount, 1);
    assert.deepEqual(result.byCategory.wait_containing_mixed_calls, {
      inputTokens: 17,
      cachedInputTokens: 3,
      uncachedInputTokens: 14,
      outputTokens: 8,
      responseCount: 1,
    });
    assert.equal(result.byCategory.pure_wait_agent.responseCount, 0);
    assert.equal(result.byCategory.pure_write_stdin.responseCount, 0);
    assert.equal(result.sessions[0]?.waitAttributedInputTokens, 0);
  });
});

test("keeps missing and malformed action attribution in the unknown bucket", async () => {
  await withHomes(async (directory) => {
    const home = join(directory, "codex-home");
    await writeRollout(
      home,
      "sessions/rollout-unknown.jsonl",
      rollout(
        metadata(rootId, rootId),
        call(),
        completed("response-malformed-call", {
          input_tokens: 7,
          cached_input_tokens: 2,
          output_tokens: 1,
        }),
        completed("response-no-call", {
          input_tokens: 4,
          cached_input_tokens: 0,
          output_tokens: 2,
        }),
      ),
    );

    const result = await scanCodexHomes([home]);
    assert.deepEqual(result.byCategory.unknown_no_attribution, {
      inputTokens: 11,
      cachedInputTokens: 2,
      uncachedInputTokens: 9,
      outputTokens: 3,
      responseCount: 2,
    });
    assert.equal(result.sessions[0]?.waitAttributedInputTokens, 0);
  });
});

test("treats an observed non-tool response as other rather than missing attribution", async () => {
  await withHomes(async (directory) => {
    const home = join(directory, "codex-home");
    await writeRollout(
      home,
      "sessions/rollout-text-only.jsonl",
      rollout(
        metadata(rootId, rootId),
        { type: "response_item", payload: { type: "message", role: "assistant" } },
        completed("response-text-only", { input_tokens: 12, cached_input_tokens: 0, output_tokens: 2 }),
      ),
    );

    const result = await scanCodexHomes([home]);
    assert.equal(result.byCategory.other.inputTokens, 12);
    assert.equal(result.byCategory.unknown_no_attribution.responseCount, 0);
    assert.equal(result.sessions[0]?.pollingCandidateInputTokens, 0);
    assert.equal(result.sessions[0]?.actionAttributionComplete, true);
  });
});

test("returns local root IDs and rollout paths from only explicitly supplied homes", async () => {
  await withHomes(async (directory) => {
    const suppliedHome = join(directory, "selected-home");
    const omittedHome = join(directory, "omitted-home");
    const sourcePath = await writeRollout(
      suppliedHome,
      "sessions/2026/09/27/rollout-selected.jsonl",
      rollout(
        metadata(rootId, rootId),
        call("wait_agent"),
        completed("response-selected", {
          input_tokens: 9,
          cached_input_tokens: 2,
          output_tokens: 3,
        }),
      ),
    );
    await writeRollout(
      omittedHome,
      "sessions/rollout-omitted.jsonl",
      rollout(metadata(childId, childId)),
    );

    const result = await scanCodexHomes([suppliedHome, suppliedHome]);
    assert.equal(result.scannedRolloutCount, 1);
    assert.deepEqual(
      result.sessions.map((session) => session.sessionId),
      [rootId],
    );
    assert.deepEqual(result.sessions[0]?.root.sourceRolloutPaths, [sourcePath]);
  });
});

test("reads a synthetic compressed rollout", async () => {
  await withHomes(async (directory) => {
    const home = join(directory, "codex-home");
    const sourcePath = resolve(home, "sessions/rollout-compressed.jsonl.zst");
    await mkdir(dirname(sourcePath), { recursive: true });
    await writeFile(
      sourcePath,
      zstdCompressSync(
        Buffer.from(
          rollout(
            metadata(rootId, rootId),
            call("wait_agent"),
            completed("response-compressed", {
              input_tokens: 13,
              cached_input_tokens: 4,
              output_tokens: 2,
            }),
          ),
        ),
      ),
    );

    const result = await scanCodexHomes([home]);
    assert.equal(result.sessions[0]?.sessionId, rootId);
    assert.equal(result.sessions[0]?.waitAttributedInputTokens, 13);
    assert.deepEqual(result.sessions[0]?.root.sourceRolloutPaths, [sourcePath]);
  });
});

test("marks sessions without persisted usage as unavailable instead of zero polling", async () => {
  await withHomes(async (directory) => {
    const home = join(directory, "codex-home");
    await writeRollout(home, "sessions/rollout-no-usage.jsonl", rollout(metadata(rootId, rootId)));

    const result = await scanCodexHomes([home]);
    const session = result.sessions[0];
    assert.equal(session?.usageTraceAvailable, false);
    assert.equal(session?.waitAttributedInputTokens, null);
    assert.equal(session?.writeStdinAttributedInputTokens, null);
    assert.equal(session?.pollingCandidateInputTokens, null);
    assert.equal(session?.actionAttributionComplete, false);
  });
});

test("reports malformed rollout files without exposing their contents", async () => {
  await withHomes(async (directory) => {
    const home = join(directory, "codex-home");
    await writeRollout(home, "sessions/rollout-malformed.jsonl", "this is not JSON\n");
    const result = await scanCodexHomes([home]);
    assert.equal(result.scannedRolloutCount, 0);
    assert.equal(result.skippedRolloutCount, 1);
    assert.deepEqual(result.sessions, []);
  });
});
