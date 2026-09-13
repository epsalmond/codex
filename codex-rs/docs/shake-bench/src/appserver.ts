// Minimal newline-delimited JSON-RPC client for `codex-next app-server`.
//
// The app-server speaks one JSON object per line over stdio. Clients must send
// exactly one `initialize` request per connection and then an `initialized`
// notification before any other method; anything earlier is rejected with
// "Not initialized". See codex-rs/app-server/README.md.

import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createWriteStream, type WriteStream } from "node:fs";
import assert from "node:assert/strict";

export type Notification = { method: string; params: Record<string, unknown> };
export type NotificationHandler = (notification: Notification) => void;

export class AppServerError extends Error {
  constructor(message: string, readonly code?: number, readonly data?: unknown) {
    super(message);
  }
}

/** Split a byte stream into complete newline-delimited JSON values. */
export class LineDecoder {
  private buffer = "";

  push(chunk: string): Record<string, unknown>[] {
    this.buffer += chunk;
    const out: Record<string, unknown>[] = [];
    let index = this.buffer.indexOf("\n");
    while (index >= 0) {
      const line = this.buffer.slice(0, index).trim();
      this.buffer = this.buffer.slice(index + 1);
      if (line.length > 0) {
        try {
          out.push(JSON.parse(line) as Record<string, unknown>);
        } catch {
          // Non-JSON chatter on stdout is ignored; stderr carries real logs.
        }
      }
      index = this.buffer.indexOf("\n");
    }
    return out;
  }
}

export function encodeMessage(message: Record<string, unknown>): string {
  return `${JSON.stringify(message)}\n`;
}

type Pending = {
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
  method: string;
  timer: NodeJS.Timeout;
};

export type SpawnOptions = {
  bin?: string;
  args?: string[];
  codexHome: string;
  /** Extra `-c key=value` config overrides passed to the binary. */
  configOverrides?: string[];
  stderrLogPath?: string;
};

const INHERITED_ENV_KEYS = [
  "PATH",
  "HOME",
  "USER",
  "LOGNAME",
  "LANG",
  "LANGUAGE",
  "LC_ALL",
  "LC_CTYPE",
  "LC_MESSAGES",
  "TMPDIR",
  "TMP",
  "TEMP",
  "SSL_CERT_FILE",
  "SSL_CERT_DIR",
  "NODE_EXTRA_CA_CERTS",
  "HTTP_PROXY",
  "HTTPS_PROXY",
  "ALL_PROXY",
  "NO_PROXY",
  "http_proxy",
  "https_proxy",
  "all_proxy",
  "no_proxy",
  "USERPROFILE",
  "HOMEDRIVE",
  "HOMEPATH",
  "SYSTEMROOT",
  "WINDIR",
  "COMSPEC",
  "PATHEXT",
] as const;

/** Build the child environment without inheriting agent or parent Codex state. */
export function buildChildEnvironment(codexHome: string): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = {};
  for (const key of INHERITED_ENV_KEYS) {
    if (process.env[key] !== undefined) env[key] = process.env[key];
  }
  for (const [key, value] of Object.entries(process.env)) {
    if (key.startsWith("LC_") && value !== undefined) env[key] = value;
  }
  env.CODEX_HOME = codexHome;
  env.RUST_LOG = "error";
  return env;
}

export class AppServerClient {
  private nextId = 1;
  private readonly pending = new Map<number, Pending>();
  private readonly handlers = new Set<NotificationHandler>();
  private readonly decoder = new LineDecoder();
  private exited: { code: number | null; signal: NodeJS.Signals | null } | undefined;
  private stderrLog: WriteStream | undefined;
  readonly stderrTail: string[] = [];

  private constructor(private readonly child: ChildProcessWithoutNullStreams) {}

  static spawn(options: SpawnOptions): AppServerClient {
    const bin = options.bin ?? "codex-next";
    const overrides = (options.configOverrides ?? []).flatMap((value) => ["-c", value]);
    const args = [...overrides, ...(options.args ?? ["app-server"])];
    const child = spawn(bin, args, {
      stdio: ["pipe", "pipe", "pipe"],
      env: buildChildEnvironment(options.codexHome),
    }) as ChildProcessWithoutNullStreams;
    const client = new AppServerClient(child);
    if (options.stderrLogPath) client.stderrLog = createWriteStream(options.stderrLogPath, { flags: "a" });
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => client.onStdout(chunk));
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk: string) => {
      client.stderrLog?.write(chunk);
      client.stderrTail.push(chunk);
      if (client.stderrTail.length > 200) client.stderrTail.splice(0, client.stderrTail.length - 200);
    });
    child.on("exit", (code, signal) => {
      client.exited = { code, signal };
      const error = new AppServerError(`app-server exited (code=${code}, signal=${signal})`);
      for (const [id, pending] of client.pending) {
        clearTimeout(pending.timer);
        client.pending.delete(id);
        pending.reject(error);
      }
    });
    child.on("error", (error) => {
      client.exited = { code: null, signal: null };
      for (const [id, pending] of client.pending) {
        clearTimeout(pending.timer);
        client.pending.delete(id);
        pending.reject(error);
      }
    });
    return client;
  }

  private onStdout(chunk: string): void {
    for (const message of this.decoder.push(chunk)) {
      const id = message.id;
      if (typeof id === "number" && (("result" in message) || ("error" in message))) {
        const pending = this.pending.get(id);
        if (!pending) continue;
        clearTimeout(pending.timer);
        this.pending.delete(id);
        if ("error" in message) {
          const error = message.error as { message?: string; code?: number; data?: unknown } | undefined;
          pending.reject(
            new AppServerError(`${pending.method}: ${error?.message ?? "unknown error"}`, error?.code, error?.data),
          );
        } else {
          pending.resolve(message.result);
        }
        continue;
      }
      if (typeof message.method === "string") {
        if ("id" in message) {
          // Server-initiated requests (approvals, elicitations) need a JSON-RPC
          // response. Leaving them as notifications makes the server wait
          // until its request timeout and can strand the benchmark turn.
          this.child.stdin.write(
            encodeMessage({
              jsonrpc: "2.0",
              id: message.id,
              error: {
                code: -32601,
                message: `shake-bench does not support server request ${message.method}`,
              },
            }),
          );
          continue;
        }
        const notification: Notification = {
          method: message.method,
          params: (message.params ?? {}) as Record<string, unknown>,
        };
        for (const handler of [...this.handlers]) handler(notification);
      }
    }
  }

  request<T = unknown>(method: string, params: Record<string, unknown> = {}, timeoutMs = 600_000): Promise<T> {
    if (this.exited) {
      return Promise.reject(new AppServerError(`app-server already exited before ${method}`));
    }
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new AppServerError(`${method} timed out after ${timeoutMs}ms`));
      }, timeoutMs);
      timer.unref?.();
      this.pending.set(id, { resolve: resolve as (value: unknown) => void, reject, method, timer });
      this.child.stdin.write(encodeMessage({ jsonrpc: "2.0", id, method, params }));
    });
  }

  notify(method: string, params: Record<string, unknown> = {}): void {
    this.child.stdin.write(encodeMessage({ jsonrpc: "2.0", method, params }));
  }

  onNotification(handler: NotificationHandler): () => void {
    this.handlers.add(handler);
    return () => this.handlers.delete(handler);
  }

  /** Resolve when a notification matching `predicate` arrives. */
  waitFor(predicate: (notification: Notification) => boolean, timeoutMs = 900_000, label = "notification"): Promise<Notification> {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        off();
        reject(new AppServerError(`timed out after ${timeoutMs}ms waiting for ${label}`));
      }, timeoutMs);
      timer.unref?.();
      const off = this.onNotification((notification) => {
        if (!predicate(notification)) return;
        clearTimeout(timer);
        off();
        resolve(notification);
      });
    });
  }

  /**
   * Like `waitFor`, but the wait is governed by two clocks instead of one:
   * an idle clock that resets every time a notification matching `progress`
   * arrives (so a turn that is visibly working is not killed just because it
   * is slow), and an absolute clock that fires regardless of progress (so a
   * notification stream that "progresses" forever without ever completing
   * still terminates the wait). Whichever fires first produces an error that
   * says which one it was and how long the wait had been running.
   *
   * This exists because a single absolute deadline (the old `waitFor`) has
   * to be set high enough for the slowest real turn, which means a turn that
   * is actually stuck produces no signal until that same multi-hour ceiling
   * — see replay-proof.md §6 run 2 (killed by a since-raised 1h deadline
   * mid-turn) and §7 blocker 4.
   */
  waitForIdle(
    predicate: (notification: Notification) => boolean,
    progress: (notification: Notification) => boolean,
    opts: { idleMs: number; absoluteMs: number },
    label = "notification",
  ): Promise<Notification> {
    return new Promise((resolve, reject) => {
      const startedAt = Date.now();
      let idleTimer: NodeJS.Timeout;
      const settle = (fn: () => void) => {
        clearTimeout(idleTimer);
        clearTimeout(absoluteTimer);
        off();
        fn();
      };
      const absoluteTimer = setTimeout(() => {
        settle(() =>
          reject(
            new AppServerError(
              `timed out after ${opts.absoluteMs}ms (absolute cap) waiting for ${label}; ` +
                `running for ${Date.now() - startedAt}ms`,
            ),
          ),
        );
      }, opts.absoluteMs);
      absoluteTimer.unref?.();
      const armIdle = () => {
        clearTimeout(idleTimer);
        idleTimer = setTimeout(() => {
          settle(() =>
            reject(
              new AppServerError(
                `timed out after ${opts.idleMs}ms with no progress (idle cap) waiting for ${label}; ` +
                  `running for ${Date.now() - startedAt}ms`,
              ),
            ),
          );
        }, opts.idleMs);
        idleTimer.unref?.();
      };
      armIdle();
      const off = this.onNotification((notification) => {
        if (predicate(notification)) {
          settle(() => resolve(notification));
          return;
        }
        if (progress(notification)) armIdle();
      });
    });
  }

  async initialize(clientInfo: { name: string; title: string; version: string }): Promise<Record<string, unknown>> {
    const result = await this.request<Record<string, unknown>>(
      "initialize",
      { clientInfo, capabilities: { experimentalApi: true } },
      120_000,
    );
    this.notify("initialized");
    return result;
  }

  async close(): Promise<void> {
    this.stderrLog?.end();
    if (this.exited) return;
    this.child.stdin.end();
    await new Promise<void>((resolve) => {
      const timer = setTimeout(() => {
        this.child.kill("SIGKILL");
        resolve();
      }, 5_000);
      timer.unref?.();
      this.child.once("exit", () => {
        clearTimeout(timer);
        resolve();
      });
    });
  }
}

// ------------------------------------------------------------------ self-test

/**
 * Framing check against a fake stdio server: verifies request/response
 * correlation by id, error propagation, and notification delivery, without
 * launching codex.
 */
export async function runFramingSelfTest(): Promise<void> {
  const fake = `
    let buffer = "";
    process.stdin.setEncoding("utf8");
    process.stdin.on("data", (chunk) => {
      buffer += chunk;
      let index;
      while ((index = buffer.indexOf("\\n")) >= 0) {
        const line = buffer.slice(0, index).trim();
        buffer = buffer.slice(index + 1);
        if (!line) continue;
        const message = JSON.parse(line);
        if (message.method === "initialize") {
          // Reply out of order with a notification first.
          process.stdout.write(JSON.stringify({ jsonrpc: "2.0", method: "thread/started", params: { threadId: "t1" } }) + "\\n");
          process.stdout.write(
            JSON.stringify({
              jsonrpc: "2.0",
              id: message.id,
              result: { userAgent: "fake", experimentalApi: message.params.capabilities?.experimentalApi },
            }) + "\\n",
          );
        } else if (message.method === "boom") {
          process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: message.id, error: { code: -32000, message: "nope" } }) + "\\n");
        } else if (message.method === "echo") {
          process.stdout.write("not json at all\\n");
          process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: message.id, result: message.params }) + "\\n");
        }
      }
    });
  `;
  const client = AppServerClient.spawn({
    bin: process.execPath,
    args: ["-e", fake],
    codexHome: process.cwd(),
  });
  const seen: Notification[] = [];
  client.onNotification((notification) => seen.push(notification));
  try {
    const initialized = await client.initialize({ name: "self_test", title: "self test", version: "0" });
    assert.deepEqual(initialized, { userAgent: "fake", experimentalApi: true });
    const echoed = await client.request<Record<string, unknown>>("echo", { a: 1, b: ["c"] }, 10_000);
    assert.deepEqual(echoed, { a: 1, b: ["c"] });
    await assert.rejects(() => client.request("boom", {}, 10_000), /boom: nope/);
    assert.deepEqual(seen.map((notification) => notification.method), ["thread/started"]);
    assert.deepEqual(seen[0]!.params, { threadId: "t1" });
  } finally {
    await client.close();
  }
}
