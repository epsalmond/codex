// Deterministic stdio JSON-RPC peer used only by waitReplay offline tests.
// It never contacts a model provider.
import { readFileSync } from "node:fs";
import { join } from "node:path";

let buffer = "";
process.stdin.setEncoding("utf8");

function send(value) {
  process.stdout.write(JSON.stringify(value) + "\n");
}

function reply(id, result) {
  send({ jsonrpc: "2.0", id, result });
}

function notify(method, params) {
  send({ jsonrpc: "2.0", method, params });
}

function emitWakeStyleLifecycle() {
  notify("turn/started", { threadId: "root", turn: { id: "root-1", status: "inProgress" } });
  notify("thread/started", { threadId: "child" });
  notify("turn/started", { threadId: "child", turn: { id: "child-1", status: "inProgress" } });
  notify("thread/tokenUsage/updated", {
    threadId: "root",
    tokenUsage: { last: { inputTokens: 100, cachedInputTokens: 20, outputTokens: 3 } },
  });
  notify("turn/completed", { threadId: "root", turn: { id: "root-1", status: "completed", items: [] } });
  notify("thread/tokenUsage/updated", {
    threadId: "child",
    tokenUsage: { last: { inputTokens: 40, cachedInputTokens: 10, outputTokens: 2 } },
  });
  notify("turn/completed", { threadId: "child", turn: { id: "child-1", status: "completed", items: [] } });
  notify("turn/started", { threadId: "root", turn: { id: "root-2", status: "inProgress" } });
  notify("thread/tokenUsage/updated", {
    threadId: "root",
    tokenUsage: { last: { inputTokens: 80, cachedInputTokens: 0, outputTokens: 4 } },
  });
  const answer = "The tests pass. BENCHMARK_COMPLETE";
  notify("item/completed", {
    threadId: "root",
    turnId: "root-2",
    item: { type: "agentMessage", text: answer },
  });
  notify("turn/completed", {
    threadId: "root",
    turn: { id: "root-2", status: "completed", items: [{ type: "agentMessage", text: answer }] },
  });
}

process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline = buffer.indexOf("\n");
  while (newline >= 0) {
    const line = buffer.slice(0, newline).trim();
    buffer = buffer.slice(newline + 1);
    newline = buffer.indexOf("\n");
    if (!line) continue;
    const message = JSON.parse(line);
    if (message.method === "initialize") {
      reply(message.id, { userAgent: "mock", experimentalApi: true });
    } else if (message.method === "config/read") {
      const config = readFileSync(join(process.env.CODEX_HOME, "config.toml"), "utf8");
      const mode = config.match(/agent_polling\s*=\s*"([^"]+)"/)?.[1] ?? "missing";
      reply(message.id, { config: { features: { multi_agent_v2: { agent_polling: mode } } } });
    } else if (message.method === "thread/start") {
      reply(message.id, { thread: { id: "root" } });
      setTimeout(() => notify("thread/started", { threadId: "root" }), 0);
    } else if (message.method === "turn/start") {
      reply(message.id, { turn: { id: "root-1", status: "inProgress" } });
      setTimeout(emitWakeStyleLifecycle, 5);
    } else if (message.method === "turn/interrupt") {
      reply(message.id, {});
    } else if (message.id !== undefined) {
      send({ jsonrpc: "2.0", id: message.id, error: { code: -32601, message: "unsupported mock method" } });
    }
  }
});

process.stdin.on("end", () => process.exit(0));
