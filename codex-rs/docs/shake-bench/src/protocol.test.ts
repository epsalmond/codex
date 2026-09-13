import assert from "node:assert/strict";
import test from "node:test";

import { AppServerClient, buildChildEnvironment, type Notification } from "./appserver.ts";
import { prepareArm, type Driver } from "./arms.ts";

type Waiter = {
  predicate: (notification: Notification) => boolean;
  resolve: (notification: Notification) => void;
  reject: (error: Error) => void;
  timer: NodeJS.Timeout;
};

class MockDriver implements Driver {
  readonly requests: Array<{ method: string; params: Record<string, unknown> }> = [];
  private readonly waiters = new Set<Waiter>();
  private clock = 0;

  constructor(
    private readonly preview: Record<string, unknown> = {
      tokensBefore: 1_000,
      tokensAfter: 400,
      toolOutputs: 2,
      textBlocks: 0,
      thinkingBlocks: 0,
      images: 0,
      fingerprint: "fingerprint",
    },
    private readonly compactEvents: Notification[] = [
      { method: "turn/completed", params: { threadId: "other-thread", turn: { id: "other-turn" } } },
      { method: "turn/started", params: { threadId: "target-thread", turn: { id: "compact-turn" } } },
      {
        method: "item/completed",
        params: {
          threadId: "target-thread",
          turnId: "compact-turn",
          item: { type: "contextCompaction", id: "compact-item" },
        },
      },
      { method: "turn/completed", params: { threadId: "target-thread", turn: { id: "compact-turn" } } },
    ],
    private readonly shakeEvents: Notification[] = [
      { method: "warning", params: { threadId: "other-thread", message: "⛭ shake: Shook other" } },
      { method: "warning", params: { threadId: "target-thread", message: "ordinary warning" } },
      { method: "warning", params: { threadId: "target-thread", message: "⛭ shake: Shook target" } },
    ],
  ) {}

  async request<T>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    this.requests.push({ method, params });
    if (method === "thread/shake/preview") return { preview: this.preview } as T;
    if (method === "thread/shake/start") {
      for (const event of this.shakeEvents) this.emit(event);
    }
    if (method === "thread/compact/start") {
      for (const event of this.compactEvents) this.emit(event);
    }
    return {} as T;
  }

  waitFor(predicate: (notification: Notification) => boolean, timeoutMs = 900_000, label = "notification") {
    return new Promise<Notification>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.waiters.delete(waiter);
        reject(new Error(`timed out waiting for ${label}`));
      }, Math.min(timeoutMs, 100));
      const waiter: Waiter = { predicate, resolve, reject, timer };
      this.waiters.add(waiter);
    });
  }

  now(): number {
    this.clock += 1;
    return this.clock;
  }

  private emit(notification: Notification): void {
    for (const waiter of [...this.waiters]) {
      if (!waiter.predicate(notification)) continue;
      this.waiters.delete(waiter);
      clearTimeout(waiter.timer);
      waiter.resolve(notification);
    }
  }
}

test("shake waits for the matching prefixed warning and requires tool outputs", async () => {
  const driver = new MockDriver();
  const preparation = await prepareArm(driver, "target-thread", "shake-elide", 2);

  assert.equal(preparation.shake?.warning, "⛭ shake: Shook target");
  assert.equal(preparation.shake?.applied, true);
  assert.equal(preparation.shake?.preview.toolOutputs, 2);
  assert.deepEqual(
    driver.requests.map(({ method }) => method),
    ["thread/shake/preview", "thread/shake/start"],
  );
});

test("shake rejects a preview that cannot elide any tool outputs", async () => {
  const driver = new MockDriver({
    tokensBefore: 1_000,
    tokensAfter: 1_000,
    toolOutputs: 0,
    textBlocks: 0,
    thinkingBlocks: 0,
    images: 0,
    fingerprint: "fingerprint",
  });

  await assert.rejects(() => prepareArm(driver, "target-thread", "shake-elide"), /no tool outputs/);
  assert.deepEqual(driver.requests.map(({ method }) => method), ["thread/shake/preview"]);
});

test("shake rejects noop and failure warnings before returning", async () => {
  for (const message of ["⛭ shake: Nothing to shake.", "⛭ shake: failed to apply"]) {
    const driver = new MockDriver(undefined, undefined, [{ method: "warning", params: { threadId: "target-thread", message } }]);
    await assert.rejects(() => prepareArm(driver, "target-thread", "shake-elide"), /did not apply/);
  }
});

test("shake rejects a successful warning when the preview did not reduce tokens", async () => {
  const driver = new MockDriver({
    tokensBefore: 1_000,
    tokensAfter: 1_000,
    toolOutputs: 2,
    textBlocks: 0,
    thinkingBlocks: 0,
    images: 0,
    fingerprint: "fingerprint",
  });

  await assert.rejects(() => prepareArm(driver, "target-thread", "shake-elide"), /did not reduce tokens/);
});

test("compact waits for its turn completion despite unrelated and same-chunk events", async () => {
  const driver = new MockDriver();
  const preparation = await prepareArm(driver, "target-thread", "compact");

  assert.deepEqual(preparation.compact, { latencyMs: 1, completedVia: "turn/completed" });
  assert.deepEqual(driver.requests.map(({ method }) => method), ["thread/compact/start"]);
});

test("compact can identify its turn from a context-compaction item", async () => {
  const driver = new MockDriver(undefined, [
    {
      method: "item/completed",
      params: {
        threadId: "target-thread",
        turnId: "compact-from-item",
        item: { type: "contextCompaction", id: "compact-item" },
      },
    },
    { method: "turn/completed", params: { threadId: "target-thread", turn: { id: "compact-from-item" } } },
  ]);

  const preparation = await prepareArm(driver, "target-thread", "compact");

  assert.equal(preparation.compact?.completedVia, "turn/completed");
});

test("compact rejects failed terminal notifications", async () => {
  for (const terminal of [
    { method: "turn/failed", params: { threadId: "target-thread", turnId: "failed-turn", error: { message: "model failed" } } },
    { method: "turn/completed", params: { threadId: "target-thread", turn: { id: "failed-turn", status: "failed" } } },
  ] satisfies Notification[]) {
    const driver = new MockDriver(undefined, [
      { method: "turn/started", params: { threadId: "target-thread", turn: { id: "failed-turn" } } },
      terminal,
    ]);
    await assert.rejects(() => prepareArm(driver, "target-thread", "compact"), /compaction.*failed/);
  }
});

test("server-initiated JSON-RPC requests receive an explicit rejection", async () => {
  const fake = String.raw`
    let buffer = "";
    let echoId;
    process.stdin.setEncoding("utf8");
    process.stdin.on("data", (chunk) => {
      buffer += chunk;
      let index;
      while ((index = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, index).trim();
        buffer = buffer.slice(index + 1);
        if (!line) continue;
        const message = JSON.parse(line);
        if (message.method === "echo") {
          echoId = message.id;
          process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: "server-request", method: "client/approve", params: {} }) + "\n");
        } else if (message.id === "server-request") {
          process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: echoId, result: { rejection: message.error } }) + "\n");
        }
      }
    });
  `;
  const client = AppServerClient.spawn({ bin: process.execPath, args: ["-e", fake], codexHome: process.cwd() });
  const notifications: Notification[] = [];
  client.onNotification((notification) => notifications.push(notification));

  try {
    const result = await client.request<{ rejection: { code: number; message: string } }>("echo", {}, 5_000);
    assert.equal(result.rejection.code, -32601);
    assert.match(result.rejection.message, /does not support server request client\/approve/);
    assert.deepEqual(notifications, []);
  } finally {
    await client.close();
  }
});

test("app-server child environment excludes parent agent state", () => {
  const environment = buildChildEnvironment("/tmp/shake-bench-private-home");

  assert.equal(environment.CODEX_HOME, "/tmp/shake-bench-private-home");
  assert.equal(environment.RUST_LOG, "error");
  assert.equal(environment.CODEX_THREAD_ID, undefined);
  assert.equal(environment.CODEX_SESSION_ID, undefined);
  assert.equal(environment.CODEX_CI, undefined);
  assert.equal(environment.CLAUDE_CODE, undefined);
  assert.equal(environment.OMP_NUM_THREADS, undefined);
  for (const key of ["PATH", "HOME", "LANG", "TMPDIR", "HTTP_PROXY", "HTTPS_PROXY"]) {
    if (process.env[key] !== undefined) assert.equal(environment[key], process.env[key]);
  }
});
