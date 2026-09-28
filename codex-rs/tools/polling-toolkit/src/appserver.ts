// Minimal newline-delimited JSON-RPC client for `codex-next app-server`.
//
// The app-server speaks one JSON object per line over stdio. Clients must send
// exactly one `initialize` request per connection and then an `initialized`
// notification before any other method; anything earlier is rejected with
// "Not initialized". See codex-rs/app-server/README.md.

import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createWriteStream, type WriteStream } from "node:fs";

export type Notification = { method: string; params: Record<string, unknown> };
export type NotificationHandler = (notification: Notification) => void;

export class AppServerError extends Error {
  readonly code?: number;
  readonly data?: unknown;

  constructor(message: string, code?: number, data?: unknown) {
    super(message);
    this.code = code;
    this.data = data;
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
  /** Override bounded shutdown waits in tests. */
  shutdownTimeoutMs?: number;
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
  private readonly child: ChildProcessWithoutNullStreams;
  private readonly shutdownTimeoutMs: number;

  private constructor(
    child: ChildProcessWithoutNullStreams,
    shutdownTimeoutMs = 5_000,
  ) {
    this.child = child;
    this.shutdownTimeoutMs = shutdownTimeoutMs;
  }

  static spawn(options: SpawnOptions): AppServerClient {
    const bin = options.bin ?? "codex-next";
    const overrides = (options.configOverrides ?? []).flatMap((value) => ["-c", value]);
    const args = [...overrides, ...(options.args ?? ["app-server"])];
    const child = spawn(bin, args, {
      stdio: ["pipe", "pipe", "pipe"],
      env: buildChildEnvironment(options.codexHome),
    }) as ChildProcessWithoutNullStreams;
    const client = new AppServerClient(child, options.shutdownTimeoutMs);
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
    child.once("close", () => client.stderrLog?.end());
    return client;
  }

  private waitForExit(timeoutMs: number, after: string): Promise<void> {
    if (this.exited) return Promise.resolve();
    return new Promise<void>((resolve, reject) => {
      let settled = false;
      const cleanup = () => {
        clearTimeout(timer);
        this.child.removeListener("exit", onExit);
        this.child.removeListener("error", onError);
      };
      const finish = () => {
        if (settled) return;
        settled = true;
        cleanup();
        resolve();
      };
      const onExit = () => finish();
      const onError = () => {
        // A spawn error means there is no child process to wait for.
        if (this.child.pid === undefined) finish();
      };
      const timer = setTimeout(() => {
        if (settled) return;
        settled = true;
        cleanup();
        reject(new AppServerError(`app-server did not exit within ${timeoutMs}ms ${after}`));
      }, timeoutMs);
      timer.unref?.();
      this.child.once("exit", onExit);
      this.child.once("error", onError);
      if (this.exited || this.child.exitCode !== null || this.child.signalCode !== null) finish();
    });
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
                message: `polling toolkit does not support server request ${message.method}`,
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
    if (this.exited) return;
    this.child.stdin.end();
    try {
      await this.waitForExit(this.shutdownTimeoutMs, "after stdin close");
    } catch {
      this.child.kill("SIGKILL");
      await this.waitForExit(this.shutdownTimeoutMs, "after SIGKILL");
    }
  }

  /** Kill the isolated app-server process tree after a task limit fires. */
  async killImmediately(): Promise<void> {
    if (this.exited) return;
    this.child.kill("SIGKILL");
    await this.waitForExit(this.shutdownTimeoutMs, "after SIGKILL");
  }
}
