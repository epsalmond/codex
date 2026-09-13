#!/usr/bin/env node
// Cost arithmetic for the shake-or-not decision (plan addendum H1).
//
// No benchmark involved: this is a spreadsheet. Shake is a purely local
// operation — it makes no model call — but it rewrites history, so the next
// request cannot reuse the input cache. The question is whether the cheaper
// context on every later turn pays that one-time cache loss back, and how many
// turns that takes.
//
// Prices come from pricing.json, which is transcribed from the published
// pricing pages. A null price is printed as "unknown"; nothing here invents a
// number.
//
// TWO PROFILES, and which one applies depends on how the worker authenticates:
//   - `api` (default) is the API rate card in USD per 1M tokens, including the
//     272K long-context 2x/1.5x multipliers. It applies to API-key billing only.
//   - `codex-credits` is the Codex plan rate card in CREDITS per 1M tokens, with
//     a 2.5x Fast-mode multiplier for Astra, NO long-context multiplier and NO
//     cache-write charge. A ChatGPT-login worker (auth_mode chatgpt) spends
//     these, so for such a run every USD figure is a shadow cost and the credit
//     figure is the real one.

import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));

export type ModelPricing = {
  inputPerMTok: number | null;
  cachedInputPerMTok: number | null;
  cacheWriteInputPerMTok: number | null;
  outputPerMTok: number | null;
  longContextThresholdTokens: number | null;
  longContextInputMultiplier: number | null;
  longContextCachedInputMultiplier: number | null;
  longContextCacheWriteInputMultiplier: number | null;
  longContextOutputMultiplier: number | null;
  /**
   * Service-tier multiplier applied to every term of a request on the Fast
   * tier. Only the `codex-credits` profile publishes one, and the page states
   * it for Astra only, so it is null (unknown) elsewhere.
   */
  fastMultiplier?: number | null;
  contextWindowTokens: number | null;
  source: string;
  fetchedAt: string;
  cacheSource?: string;
  cacheFetchedAt?: string;
  note?: string;
};

export type PricingProfile = {
  unit: string;
  unitLabel: string;
  label: string;
  /** When set, the profile's models ARE the top-level map under this key (no copy). */
  modelsRef?: string;
  models?: Record<string, ModelPricing>;
  appliesTo?: string;
  source?: string;
  fetchedAt?: string;
  notes?: string[];
};

export type Pricing = {
  _fetchedAt: string;
  models: Record<string, ModelPricing>;
  profiles?: Record<string, PricingProfile>;
};

export function loadPricing(path = join(HERE, "..", "pricing.json")): Pricing {
  return JSON.parse(readFileSync(path, "utf8")) as Pricing;
}

export const DEFAULT_PROFILE = "api";

/** Service tier. `fast` applies the profile's `fastMultiplier`, if it has one. */
export type Tier = "standard" | "fast";

export type ResolvedProfile = {
  name: string;
  unit: string;
  unitLabel: string;
  label: string;
  models: Record<string, ModelPricing>;
  source?: string;
  fetchedAt?: string;
  notes?: string[];
};

export function profileNames(pricing: Pricing): string[] {
  return Object.keys(pricing.profiles ?? { [DEFAULT_PROFILE]: null });
}

/**
 * Resolve a named pricing profile to its model map. `modelsRef` points at a
 * top-level map rather than duplicating it, so the API profile and the legacy
 * top-level `models` can never drift apart.
 */
export function loadProfile(pricing: Pricing, name: string = DEFAULT_PROFILE): ResolvedProfile {
  const profile = pricing.profiles?.[name];
  if (!profile) {
    if (name === DEFAULT_PROFILE) {
      return { name, unit: "usd", unitLabel: "USD", label: "API rate card (USD per 1M tokens)", models: pricing.models };
    }
    throw new Error(`unknown pricing profile ${name}; have ${profileNames(pricing).join(", ")}`);
  }
  const models = profile.modelsRef
    ? ((pricing as unknown as Record<string, Record<string, ModelPricing>>)[profile.modelsRef] ?? {})
    : (profile.models ?? {});
  if (Object.keys(models).length === 0) throw new Error(`pricing profile ${name} has no models`);
  return {
    name,
    unit: profile.unit,
    unitLabel: profile.unitLabel,
    label: profile.label,
    models,
    source: profile.source,
    fetchedAt: profile.fetchedAt,
    notes: profile.notes,
  };
}

/** One model request, with cache-hit and cache-write input token counts. */
export type Request = {
  inputTokens: number;
  cachedInputTokens: number;
  /** Input tokens written to the prompt cache by this request. */
  cacheWriteInputTokens?: number;
  outputTokens: number;
  label: string;
};

export class UnknownPrice extends Error {}

function checkTokenCount(name: string, value: number): void {
  if (!Number.isInteger(value) || value < 0) throw new RangeError(`${name} must be a non-negative integer`);
}

function rate(pricing: ModelPricing, name: keyof ModelPricing, label: string): number {
  const value = pricing[name];
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) throw new UnknownPrice(`${label} price is unknown`);
  return value;
}

/**
 * Cost of one request in the profile's unit. `tier` is "standard" unless the
 * run was on the Fast tier, in which case the profile's `fastMultiplier`
 * scales every term; a profile or model with no published multiplier throws
 * UnknownPrice rather than assuming 1.
 */
export function requestCost(pricing: ModelPricing, request: Request, tier: Tier = "standard"): number {
  const base = requestCostUsd(pricing, request);
  if (tier === "standard") return base;
  return base * rate(pricing, "fastMultiplier", "Fast-tier");
}

export function requestCostUsd(pricing: ModelPricing, request: Request): number {
  checkTokenCount("inputTokens", request.inputTokens);
  checkTokenCount("cachedInputTokens", request.cachedInputTokens);
  checkTokenCount("outputTokens", request.outputTokens);
  const cacheWriteInputTokens = request.cacheWriteInputTokens ?? 0;
  checkTokenCount("cacheWriteInputTokens", cacheWriteInputTokens);
  if (request.cachedInputTokens + cacheWriteInputTokens > request.inputTokens) {
    throw new RangeError("cachedInputTokens + cacheWriteInputTokens cannot exceed inputTokens");
  }
  if (typeof pricing.contextWindowTokens === "number") {
    if (request.inputTokens > pricing.contextWindowTokens) {
      throw new RangeError(`inputTokens exceeds the ${pricing.contextWindowTokens}-token context window`);
    }
    if (request.inputTokens + request.outputTokens > pricing.contextWindowTokens) {
      throw new RangeError(`input and output tokens exceed the ${pricing.contextWindowTokens}-token context window`);
    }
  }

  const threshold = pricing.longContextThresholdTokens;
  const long = threshold !== null && request.inputTokens > threshold;
  const multiplier = (name: keyof ModelPricing, label: string): number => {
    if (!long) return 1;
    return rate(pricing, name, label);
  };
  const uncachedInputTokens = request.inputTokens - request.cachedInputTokens - cacheWriteInputTokens;
  const componentCost = (tokens: number, price: keyof ModelPricing, priceLabel: string, multiplierName: keyof ModelPricing, multiplierLabel: string): number => {
    if (tokens === 0) return 0;
    return (tokens / 1e6) * rate(pricing, price, priceLabel) * multiplier(multiplierName, multiplierLabel);
  };
  const inputCost = componentCost(
    uncachedInputTokens,
    "inputPerMTok",
    "input",
    "longContextInputMultiplier",
    "long-context input",
  );
  const cachedCost = componentCost(
    request.cachedInputTokens,
    "cachedInputPerMTok",
    "cached input",
    "longContextCachedInputMultiplier",
    "long-context cached input",
  );
  const cacheWriteCost = componentCost(
    cacheWriteInputTokens,
    "cacheWriteInputPerMTok",
    "cache-write input",
    "longContextCacheWriteInputMultiplier",
    "long-context cache-write input",
  );
  const outputCost = componentCost(
    request.outputTokens,
    "outputPerMTok",
    "output",
    "longContextOutputMultiplier",
    "long-context output",
  );
  return inputCost + cachedCost + cacheWriteCost + outputCost;
}

export type Scenario = {
  model: string;
  /** Current live context, in tokens, before any reduction. */
  contextTokens: number;
  /** Context after a shake. Measure this from a real thread/shake/preview. */
  postShakeTokens: number;
  /** Context after compaction. */
  postCompactTokens: number;
  /** Remaining turns to simulate. */
  turns: number;
  /** Input tokens appended before each ordinary turn request (user message and tool input). */
  perTurnInputTokens: number;
  /** Output tokens per ordinary turn. */
  outputPerTurn: number;
  /** Output tokens of a compaction call (the summary it writes). */
  compactionOutputTokens: number;
  /** Whether the current context is already cached upstream. */
  cache: "warm" | "cold";
  /** Expected read_artifact recovery calls after a shake, from the benchmark. */
  recoveryReads: number;
  /** Tokens a single read_artifact page returns (3072 bytes, ~4 bytes/token). */
  recoveryTokensPerRead: number;
  /** Output tokens used to issue and process one read_artifact call. */
  recoveryOutputPerRead: number;
};

export const DEFAULT_SCENARIO: Omit<Scenario, "model" | "contextTokens" | "postShakeTokens" | "cache"> = {
  postCompactTokens: 0, // filled per scenario
  turns: 5,
  perTurnInputTokens: 3_000,
  outputPerTurn: 1_000,
  compactionOutputTokens: 2_000,
  recoveryReads: 0,
  recoveryTokensPerRead: 768,
  recoveryOutputPerRead: 100,
};

export type Strategy = "no-shake" | "shake" | "compact" | "shake-then-compact";

function validateScenario(scenario: Scenario, contextWindowTokens?: number | null): void {
  checkTokenCount("contextTokens", scenario.contextTokens);
  checkTokenCount("postShakeTokens", scenario.postShakeTokens);
  checkTokenCount("postCompactTokens", scenario.postCompactTokens);
  checkTokenCount("turns", scenario.turns);
  checkTokenCount("perTurnInputTokens", scenario.perTurnInputTokens);
  checkTokenCount("outputPerTurn", scenario.outputPerTurn);
  checkTokenCount("compactionOutputTokens", scenario.compactionOutputTokens);
  checkTokenCount("recoveryReads", scenario.recoveryReads);
  checkTokenCount("recoveryTokensPerRead", scenario.recoveryTokensPerRead);
  checkTokenCount("recoveryOutputPerRead", scenario.recoveryOutputPerRead);
  if (scenario.postShakeTokens > scenario.contextTokens) {
    throw new RangeError("postShakeTokens cannot exceed contextTokens");
  }
  if (scenario.postCompactTokens > scenario.postShakeTokens) {
    throw new RangeError("postCompactTokens cannot exceed postShakeTokens");
  }
  if (scenario.cache !== "warm" && scenario.cache !== "cold") throw new RangeError("cache must be warm or cold");
  if (contextWindowTokens !== null && contextWindowTokens !== undefined) {
    checkTokenCount("contextWindowTokens", contextWindowTokens);
    if (contextWindowTokens === 0) throw new RangeError("contextWindowTokens must be positive");
    if (scenario.contextTokens > contextWindowTokens) throw new RangeError("contextTokens exceeds the context window");
    if (scenario.postShakeTokens > contextWindowTokens) throw new RangeError("postShakeTokens exceeds the context window");
    if (scenario.postCompactTokens > contextWindowTokens) throw new RangeError("postCompactTokens exceeds the context window");
  }
}

/**
 * Build the request sequence for one strategy.
 *
 * Cache model: a request reuses, as cache hits, the prefix that was already
 * sent by the previous request on the same unchanged history. Any operation
 * that edits earlier history (shake, compaction) drops that prefix, so the
 * next request pays full input rate for everything.
 */
export function simulate(scenario: Scenario, strategy: Strategy): Request[] {
  validateScenario(scenario);
  const requests: Request[] = [];
  let context = scenario.contextTokens;
  // Tokens that the upstream cache already holds for the next request.
  let cached = scenario.cache === "warm" ? scenario.contextTokens : 0;

  if (scenario.turns === 0) return requests;

  const request = (outputTokens: number, label: string): void => {
    const cachedInputTokens = Math.min(cached, context);
    requests.push({
      inputTokens: context,
      cachedInputTokens,
      cacheWriteInputTokens: context - cachedInputTokens,
      outputTokens,
      label,
    });
    cached = context;
    context += outputTokens;
  };

  if (strategy === "compact" || strategy === "shake-then-compact") {
    if (strategy === "shake-then-compact") {
      // Shake itself makes no model call, but it invalidates the cache.
      context = scenario.postShakeTokens;
      cached = 0;
    }
    request(scenario.compactionOutputTokens, "compaction");
    context = scenario.postCompactTokens;
    cached = 0;
  } else if (strategy === "shake") {
    context = scenario.postShakeTokens;
    cached = 0;
  }

  for (let turn = 0; turn < scenario.turns; turn++) {
    context += scenario.perTurnInputTokens;
    const hasRecovery = turn === 0 && (strategy === "shake" || strategy === "shake-then-compact") && scenario.recoveryReads > 0;
    request(hasRecovery ? scenario.recoveryOutputPerRead : scenario.outputPerTurn, `turn ${turn + 1}`);
    // Recovery reads happen on the first turn after a shake: each one is an
    // extra model request over the same context plus the page it pulled back.
    if (hasRecovery) {
      for (let read = 0; read < scenario.recoveryReads; read++) {
        context += scenario.recoveryTokensPerRead;
        request(read + 1 === scenario.recoveryReads ? scenario.outputPerTurn : scenario.recoveryOutputPerRead, `recovery read ${read + 1}`);
      }
    }
  }
  return requests;
}

export function strategyCostUsd(pricing: ModelPricing, scenario: Scenario, strategy: Strategy, tier: Tier = "standard"): number {
  validateScenario(scenario, pricing.contextWindowTokens);
  return simulate(scenario, strategy).reduce((sum, request) => sum + requestCost(pricing, request, tier), 0);
}

/** Smallest turn count at which `shake` is no more expensive than `no-shake`. */
export function breakEvenTurns(pricing: ModelPricing, scenario: Scenario, max = 200, tier: Tier = "standard"): number | null {
  checkTokenCount("max", max);
  for (let turns = 1; turns <= max; turns++) {
    const candidate = { ...scenario, turns };
    if (strategyCostUsd(pricing, candidate, "shake", tier) <= strategyCostUsd(pricing, candidate, "no-shake", tier)) return turns;
  }
  return null;
}

// ---------------------------------------------------------------- CLI

const usd = (value: number): string => `$${value.toFixed(4)}`;

/** Money formatter for whichever profile is in force. */
function amount(profile: ResolvedProfile, value: number): string {
  return profile.unit === "credits" ? `${value.toFixed(1)} cr` : usd(value);
}

function readFlag(argv: string[], name: string): string | undefined {
  const index = argv.indexOf(name);
  return index >= 0 ? argv[index + 1] : undefined;
}

const STRATEGIES: Strategy[] = ["no-shake", "shake", "compact", "shake-then-compact"];

function renderScenario(profile: ResolvedProfile, label: string, scenario: Scenario, turnCounts: number[], tier: Tier = "standard"): string[] {
  const model = profile.models[scenario.model];
  const lines: string[] = [];
  lines.push(`\n### ${label}`);
  if (!model) {
    lines.push(`unknown model \`${scenario.model}\` — not in the ${profile.name} profile`);
    return lines;
  }
  lines.push(
      `model \`${scenario.model}\`, context ${scenario.contextTokens.toLocaleString("en-US")} -> ` +
      `shake ${scenario.postShakeTokens.toLocaleString("en-US")} / compact ${scenario.postCompactTokens.toLocaleString("en-US")}, ` +
      `cache ${scenario.cache}, ${scenario.perTurnInputTokens.toLocaleString("en-US")} input tokens per turn, ` +
      `${scenario.recoveryReads} recovery read(s) at ${scenario.recoveryTokensPerRead.toLocaleString("en-US")} tokens/read`,
  );
  lines.push(`| T | ${STRATEGIES.join(" | ")} | best |`);
  lines.push(`|---:|${STRATEGIES.map(() => "---:").join("|")}|---|`);
  for (const turns of turnCounts) {
    const candidate = { ...scenario, turns };
    const costs = STRATEGIES.map((strategy) => {
      try {
        return strategyCostUsd(model, candidate, strategy, tier);
      } catch {
        return null;
      }
    });
    const known = costs.filter((cost): cost is number => cost !== null);
    const best = known.length === costs.length ? STRATEGIES[costs.indexOf(Math.min(...known))] : "unknown";
    lines.push(`| ${turns} | ${costs.map((cost) => (cost === null ? "unknown" : amount(profile, cost))).join(" | ")} | ${best} |`);
  }
  let breakEven: number | null | "unknown" = "unknown";
  try {
    breakEven = breakEvenTurns(model, scenario, 200, tier);
  } catch {
    breakEven = "unknown";
  }
  lines.push(
    `\nBreak-even for shake vs. no-shake: ` +
      (breakEven === "unknown" ? "unknown (missing prices)" : breakEven === null ? "never within 200 turns" : `T = ${breakEven}`),
  );
  return lines;
}

function canonicalScenarios(): Array<{ label: string; scenario: Scenario }> {
  const base = { ...DEFAULT_SCENARIO, postCompactTokens: 30_000 };
  return [
    {
      label: "gpt-5.6-sol, warm cache, 300k -> 120k",
      scenario: { ...base, model: "gpt-5.6-sol", contextTokens: 300_000, postShakeTokens: 120_000, cache: "warm" },
    },
    {
      label: "gpt-6-astra, warm cache, 300k -> 120k",
      scenario: { ...base, model: "gpt-6-astra", contextTokens: 300_000, postShakeTokens: 120_000, cache: "warm" },
    },
    {
      label: "gpt-5.6-sol, cold cache, 300k -> 120k",
      scenario: { ...base, model: "gpt-5.6-sol", contextTokens: 300_000, postShakeTokens: 120_000, cache: "cold" },
    },
    {
      label: "gpt-6-astra, cold cache, 300k -> 120k",
      scenario: { ...base, model: "gpt-6-astra", contextTokens: 300_000, postShakeTokens: 120_000, cache: "cold" },
    },
  ];
}

/** Copy pricing for the explicitly hypothetical no-long-context scenario. */
export function withoutLongContextMultipliers(pricing: Pricing): Pricing {
  return {
    ...pricing,
    models: Object.fromEntries(
      Object.entries(pricing.models).map(([name, model]) => [
        name,
        {
          ...model,
          longContextInputMultiplier: 1,
          longContextCachedInputMultiplier: 1,
          longContextCacheWriteInputMultiplier: 1,
          longContextOutputMultiplier: 1,
        },
      ]),
    ),
  };
}

function main(): void {
  const argv = process.argv.slice(2);
  const pricing = loadPricing(readFlag(argv, "--pricing") ? resolve(readFlag(argv, "--pricing")!) : undefined);
  const lines: string[] = ["# Shake cost model\n"];
  const hypotheticalNoLongMultiplier = argv.includes("--hypothetical-no-long-multiplier");
  const effectivePricing = hypotheticalNoLongMultiplier ? withoutLongContextMultipliers(pricing) : pricing;
  const profileName = readFlag(argv, "--profile") ?? DEFAULT_PROFILE;
  const tier = (readFlag(argv, "--tier") ?? "standard") as Tier;
  if (tier !== "standard" && tier !== "fast") throw new Error("--tier must be standard or fast");
  const profile = loadProfile(effectivePricing, profileName);
  lines.push(
    "Shake makes no model call; its only cost is the lost input cache on the next request " +
      "plus any `read_artifact` recovery round trips. Compaction does make a model call, billed at the pre-compaction context.\n",
  );
  if (hypotheticalNoLongMultiplier) {
    lines.push(
      "> Hypothetical only: all long-context multipliers are set to 1 for this run. " +
        "This is not the published model pricing.\n",
    );
  }
  lines.push(
    `Profile **${profile.name}** — ${profile.label}, ${tier} tier` +
      (profile.source ? ` (${profile.source}, fetched ${profile.fetchedAt})` : "") +
      (profile.unit === "credits"
        ? ". Credits are the REAL cost of a ChatGPT-login run; USD figures elsewhere are shadow costs."
        : ". USD applies to API billing only.") +
      "\n",
  );
  for (const note of profile.notes ?? []) lines.push(`> ${note}\n`);
  lines.push(`Prices (${profile.unitLabel} per 1M tokens):\n`);
  lines.push("| Model | Input | Cached input | Cache write | Output | Long-context threshold | Multipliers (in/cached/write/out) | Fast | Source |");
  lines.push("|---|---:|---:|---:|---:|---:|---|---:|---|");
  for (const [name, model] of Object.entries(profile.models)) {
    const unknown = (value: number | null | undefined): string => (typeof value !== "number" ? "unknown" : String(value));
    // A profile with no published long-context threshold has no long-context
    // RULE, so its null multipliers are "not applicable", not "unknown".
    const noLongContext = model.longContextThresholdTokens === null;
    const mult = (value: number | null | undefined): string =>
      typeof value === "number" ? `${value}x` : noLongContext ? "n/a" : "unknown";
    lines.push(
      `| ${name} | ${unknown(model.inputPerMTok)} | ${unknown(model.cachedInputPerMTok)} | ${unknown(model.cacheWriteInputPerMTok)} | ` +
        `${unknown(model.outputPerMTok)} | ` +
        `${model.longContextThresholdTokens === null ? (noLongContext ? "none published" : "unknown") : model.longContextThresholdTokens.toLocaleString("en-US")} | ` +
        `${mult(model.longContextInputMultiplier)} / ${mult(model.longContextCachedInputMultiplier)} / ` +
        `${mult(model.longContextCacheWriteInputMultiplier)} / ${mult(model.longContextOutputMultiplier)} | ` +
        `${typeof model.fastMultiplier === "number" ? `${model.fastMultiplier}x` : "unknown"} | ` +
        `${model.source} (${model.fetchedAt}) |`,
    );
  }
  for (const [name, model] of Object.entries(profile.models)) {
    if (model.note) lines.push(`\n> **${name}:** ${model.note}`);
  }

  const custom = readFlag(argv, "--model");
  if (custom) {
    const number = (name: string, fallback: number): number => {
      const value = Number(readFlag(argv, name) ?? fallback);
      if (!Number.isInteger(value) || value < 0) throw new Error(`${name} must be a non-negative integer`);
      return value;
    };
    const cache = readFlag(argv, "--cache") ?? "warm";
    if (cache !== "warm" && cache !== "cold") throw new Error("--cache must be warm or cold");
    const scenario: Scenario = {
      ...DEFAULT_SCENARIO,
      model: custom,
      contextTokens: number("--context", 300_000),
      postShakeTokens: number("--post-shake", 120_000),
      postCompactTokens: number("--post-compact", 30_000),
      turns: number("--turns", 5),
      perTurnInputTokens: number("--per-turn", 3_000),
      outputPerTurn: number("--output-per-turn", 1_000),
      compactionOutputTokens: number("--compaction-output", 2_000),
      recoveryReads: number("--reads", 0),
      recoveryTokensPerRead: number("--recovery-tokens-per-read", 768),
      recoveryOutputPerRead: number("--recovery-output", 100),
      cache,
    };
    lines.push(...renderScenario(profile, `custom: ${custom}`, scenario, [1, 5, 20, scenario.turns], tier));
  } else {
    lines.push("\n## Canonical scenarios");
    for (const { label, scenario } of canonicalScenarios()) {
      lines.push(...renderScenario(profile, label, scenario, [1, 5, 20], tier));
    }
    lines.push(
      "\nSet `--reads` from the benchmark's recovery-cost table once a full run exists; " +
        "`npm run estimate -- --model gpt-6-astra --context 300000 --post-shake 120000 --reads 3 --cache warm` " +
        "recomputes any single scenario. Add `--profile codex-credits --tier fast` for the plan cost " +
        "a ChatGPT-login worker actually pays.",
    );
  }
  console.log(lines.join("\n"));
}

if (import.meta.url === `file://${process.argv[1]}`) main();
