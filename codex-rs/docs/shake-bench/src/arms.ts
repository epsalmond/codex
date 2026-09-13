// The five arm procedures, expressed against a Driver so that `--dry-run` can
// walk the identical control flow without launching codex.

import type { Notification } from "./appserver.ts";

export const ARMS = ["full", "shake-elide", "shake-elide-noread", "compact", "shake-then-compact"] as const;
export type Arm = (typeof ARMS)[number];

export function isArm(value: string): value is Arm {
  return (ARMS as readonly string[]).includes(value);
}

/** Latin-square rotation so arm order is not confounded with order drift. */
export function armOrder(orderIndex: number, arms: readonly Arm[] = ARMS): Arm[] {
  const offset = ((orderIndex % arms.length) + arms.length) % arms.length;
  return [...arms.slice(offset), ...arms.slice(0, offset)];
}

/** Arms whose evaluation prompt must forbid tool use. */
export function promptVariant(arm: Arm): "default" | "noread" {
  return arm === "shake-elide-noread" ? "noread" : "default";
}

export type ShakePreview = {
  tokensBefore: number;
  tokensAfter: number;
  toolOutputs: number;
  textBlocks: number;
  thinkingBlocks: number;
  images: number;
  fingerprint: string;
  unavailableReason?: string | null;
};

export type ShakeRecord = {
  preview: ShakePreview;
  /** Warning message emitted on completion; contains "Shook" on success. */
  warning: string;
  applied: boolean;
  latencyMs: number;
};

export type CompactRecord = { latencyMs: number; completedVia: string };

export type ArmPreparation = {
  arm: Arm;
  shake?: ShakeRecord;
  compact?: CompactRecord;
  totalLatencyMs: number;
};

export interface Driver {
  request<T = unknown>(method: string, params?: Record<string, unknown>, timeoutMs?: number): Promise<T>;
  waitFor(predicate: (notification: Notification) => boolean, timeoutMs?: number, label?: string): Promise<Notification>;
  now(): number;
}

async function shakeElide(driver: Driver, threadId: string, expectedToolOutputs?: number): Promise<ShakeRecord> {
  const started = driver.now();
  const { preview } = await driver.request<{ preview: ShakePreview }>("thread/shake/preview", {
    threadId,
    mode: "elide",
  });
  if (preview.unavailableReason) {
    throw new Error(`shake elide unavailable for ${threadId}: ${preview.unavailableReason}`);
  }
  if (!(preview.toolOutputs > 0)) {
    throw new Error(`shake elide preview for ${threadId} contained no tool outputs`);
  }
  if (expectedToolOutputs !== undefined && preview.toolOutputs !== expectedToolOutputs) {
    throw new Error(
      `shake elide preview for ${threadId} saw ${preview.toolOutputs} tool outputs, expected ${expectedToolOutputs}`,
    );
  }
  // Register the warning listener before the request so a fast completion is
  // not missed.
  const warningPromise = driver.waitFor(
    (notification) =>
      notification.method === "warning" &&
      notification.params.threadId === threadId &&
      String(notification.params.message ?? "").startsWith("⛭ shake:"),
    300_000,
    "shake completion warning",
  );
  await driver.request("thread/shake/start", {
    threadId,
    mode: "elide",
    expectedFingerprint: preview.fingerprint,
  });
  const warning = await warningPromise;
  const message = String(warning.params.message ?? "");
  if (!message.startsWith("⛭ shake: Shook")) {
    throw new Error(`shake elide for ${threadId} did not apply: ${message || "missing success warning"}`);
  }
  if (!(preview.tokensAfter < preview.tokensBefore)) {
    throw new Error(
      `shake elide preview for ${threadId} did not reduce tokens (${preview.tokensBefore} -> ${preview.tokensAfter})`,
    );
  }
  return {
    preview,
    warning: message,
    applied: true,
    latencyMs: driver.now() - started,
  };
}

async function compact(driver: Driver, threadId: string): Promise<CompactRecord> {
  const started = driver.now();
  let compactTurnId: string | undefined;
  const completionPromise = driver.waitFor((notification) => {
    if (notification.params.threadId !== threadId) return false;

    const turn = notification.params.turn as { id?: unknown } | undefined;
    const eventTurnId =
      typeof notification.params.turnId === "string"
        ? notification.params.turnId
        : typeof turn?.id === "string"
          ? turn.id
          : undefined;
    if (notification.method === "turn/started") {
      if (eventTurnId) compactTurnId = eventTurnId;
      return false;
    }
    if (
      notification.method === "item/completed" &&
      (notification.params.item as { type?: string } | undefined)?.type === "contextCompaction"
    ) {
      if (eventTurnId) compactTurnId ??= eventTurnId;
      return false;
    }
    return (
      (notification.method === "turn/completed" || notification.method === "turn/failed") &&
      compactTurnId !== undefined &&
      eventTurnId === compactTurnId
    );
  }, 900_000, "compaction turn completion");
  await driver.request("thread/compact/start", { threadId });
  const notification = await completionPromise;
  const turn = notification.params.turn as { status?: unknown; error?: unknown } | undefined;
  if (notification.method === "turn/failed" || turn?.status === "failed") {
    const error = notification.params.error as { message?: unknown } | undefined;
    const message =
      typeof error?.message === "string"
        ? error.message
        : typeof turn?.error === "string"
          ? turn.error
          : "unknown error";
    throw new Error(`compaction for ${threadId} failed: ${message}`);
  }
  return { latencyMs: driver.now() - started, completedVia: notification.method };
}

export async function prepareArm(
  driver: Driver,
  threadId: string,
  arm: Arm,
  expectedToolOutputs?: number,
): Promise<ArmPreparation> {
  const started = driver.now();
  const preparation: ArmPreparation = { arm, totalLatencyMs: 0 };
  switch (arm) {
    case "full":
      break;
    case "shake-elide":
    case "shake-elide-noread":
      preparation.shake = await shakeElide(driver, threadId, expectedToolOutputs);
      break;
    case "compact":
      preparation.compact = await compact(driver, threadId);
      break;
    case "shake-then-compact":
      preparation.shake = await shakeElide(driver, threadId, expectedToolOutputs);
      preparation.compact = await compact(driver, threadId);
      break;
  }
  preparation.totalLatencyMs = driver.now() - started;
  return preparation;
}
