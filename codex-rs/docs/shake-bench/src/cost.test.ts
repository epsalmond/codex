import assert from "node:assert/strict";
import test from "node:test";

import {
  DEFAULT_SCENARIO,
  UnknownPrice,
  breakEvenTurns,
  loadPricing,
  loadProfile,
  profileNames,
  requestCost,
  requestCostUsd,
  simulate,
  strategyCostUsd,
  withoutLongContextMultipliers,
  type ModelPricing,
  type Scenario,
} from "./cost.ts";

const pricing = loadPricing();
const astra = pricing.models["gpt-6-astra"]!;

function scenario(overrides: Partial<Scenario> = {}): Scenario {
  return {
    ...DEFAULT_SCENARIO,
    model: "gpt-6-astra",
    contextTokens: 100,
    postShakeTokens: 40,
    postCompactTokens: 20,
    turns: 2,
    perTurnInputTokens: 10,
    outputPerTurn: 7,
    compactionOutputTokens: 0,
    cache: "warm",
    recoveryReads: 0,
    recoveryTokensPerRead: 5,
    recoveryOutputPerRead: 7,
    ...overrides,
  };
}

test("requestCostUsd charges input, cached input, cache writes, and output separately", () => {
  const cost = requestCostUsd(astra, {
    inputTokens: 1_000,
    cachedInputTokens: 400,
    cacheWriteInputTokens: 500,
    outputTokens: 100,
    label: "mixed request",
  });

  assert.ok(Math.abs(cost - 0.01265) < 1e-12);
});

test("omitting cache-write tokens keeps uncached input as ordinary input", () => {
  const cost = requestCostUsd(astra, {
    inputTokens: 1_000,
    cachedInputTokens: 400,
    outputTokens: 100,
    label: "ordinary miss",
  });

  assert.ok(Math.abs(cost - 0.0114) < 1e-12);
});

test("long-context multipliers apply to every published token category", () => {
  const cost = requestCostUsd(astra, {
    inputTokens: 300_000,
    cachedInputTokens: 100_000,
    cacheWriteInputTokens: 50_000,
    outputTokens: 1_000,
    label: "long request",
  });

  assert.ok(Math.abs(cost - 4.525) < 1e-12);
});

test("long-context pricing starts strictly above the threshold", () => {
  const atThreshold = requestCostUsd(astra, {
    inputTokens: 272_000,
    cachedInputTokens: 0,
    cacheWriteInputTokens: 0,
    outputTokens: 0,
    label: "threshold",
  });
  const aboveThreshold = requestCostUsd(astra, {
    inputTokens: 272_001,
    cachedInputTokens: 0,
    cacheWriteInputTokens: 0,
    outputTokens: 0,
    label: "above threshold",
  });

  assert.ok(Math.abs(atThreshold - 2.72) < 1e-12);
  assert.ok(Math.abs(aboveThreshold - 5.44002) < 1e-12);
});

test("missing long-context rates are unknown instead of silently defaulting", () => {
  const incomplete: ModelPricing = { ...astra, longContextCachedInputMultiplier: null };

  assert.throws(
    () =>
      requestCostUsd(incomplete, {
        inputTokens: 300_000,
        cachedInputTokens: 100_000,
        cacheWriteInputTokens: 0,
        outputTokens: 0,
        label: "unknown long cache rate",
      }),
    UnknownPrice,
  );
});

test("request token partitions are validated", () => {
  assert.throws(
    () =>
      requestCostUsd(astra, {
        inputTokens: 10,
        cachedInputTokens: 8,
        cacheWriteInputTokens: 3,
        outputTokens: 0,
        label: "over-partitioned",
      }),
    RangeError,
  );
  assert.throws(
    () =>
      requestCostUsd(astra, {
        inputTokens: -1,
        cachedInputTokens: 0,
        cacheWriteInputTokens: 0,
        outputTokens: 0,
        label: "negative",
      }),
    RangeError,
  );
});

test("warm cache request sequences account for each strategy's rewrite", () => {
  const warm = scenario();

  assert.deepEqual(simulate(warm, "no-shake"), [
    { inputTokens: 110, cachedInputTokens: 100, cacheWriteInputTokens: 10, outputTokens: 7, label: "turn 1" },
    { inputTokens: 127, cachedInputTokens: 110, cacheWriteInputTokens: 17, outputTokens: 7, label: "turn 2" },
  ]);
  assert.deepEqual(simulate(warm, "shake"), [
    { inputTokens: 50, cachedInputTokens: 0, cacheWriteInputTokens: 50, outputTokens: 7, label: "turn 1" },
    { inputTokens: 67, cachedInputTokens: 50, cacheWriteInputTokens: 17, outputTokens: 7, label: "turn 2" },
  ]);
  assert.deepEqual(simulate(warm, "compact"), [
    { inputTokens: 100, cachedInputTokens: 100, cacheWriteInputTokens: 0, outputTokens: 0, label: "compaction" },
    { inputTokens: 30, cachedInputTokens: 0, cacheWriteInputTokens: 30, outputTokens: 7, label: "turn 1" },
    { inputTokens: 47, cachedInputTokens: 30, cacheWriteInputTokens: 17, outputTokens: 7, label: "turn 2" },
  ]);
  assert.deepEqual(simulate(warm, "shake-then-compact"), [
    { inputTokens: 40, cachedInputTokens: 0, cacheWriteInputTokens: 40, outputTokens: 0, label: "compaction" },
    { inputTokens: 30, cachedInputTokens: 0, cacheWriteInputTokens: 30, outputTokens: 7, label: "turn 1" },
    { inputTokens: 47, cachedInputTokens: 30, cacheWriteInputTokens: 17, outputTokens: 7, label: "turn 2" },
  ]);
});

test("cold cache charges the initial context as a cache write", () => {
  const cold = scenario({ cache: "cold" });

  assert.deepEqual(simulate(cold, "no-shake"), [
    { inputTokens: 110, cachedInputTokens: 0, cacheWriteInputTokens: 110, outputTokens: 7, label: "turn 1" },
    { inputTokens: 127, cachedInputTokens: 110, cacheWriteInputTokens: 17, outputTokens: 7, label: "turn 2" },
  ]);
  assert.deepEqual(simulate(cold, "compact"), [
    { inputTokens: 100, cachedInputTokens: 0, cacheWriteInputTokens: 100, outputTokens: 0, label: "compaction" },
    { inputTokens: 30, cachedInputTokens: 0, cacheWriteInputTokens: 30, outputTokens: 7, label: "turn 1" },
    { inputTokens: 47, cachedInputTokens: 30, cacheWriteInputTokens: 17, outputTokens: 7, label: "turn 2" },
  ]);
});

test("recovery reads add continuation input and put the final answer on the last request", () => {
  const recovered = scenario({ recoveryReads: 2, recoveryOutputPerRead: 3 });

  assert.deepEqual(simulate(recovered, "shake"), [
    { inputTokens: 50, cachedInputTokens: 0, cacheWriteInputTokens: 50, outputTokens: 3, label: "turn 1" },
    { inputTokens: 58, cachedInputTokens: 50, cacheWriteInputTokens: 8, outputTokens: 3, label: "recovery read 1" },
    { inputTokens: 66, cachedInputTokens: 58, cacheWriteInputTokens: 8, outputTokens: 7, label: "recovery read 2" },
    { inputTokens: 83, cachedInputTokens: 66, cacheWriteInputTokens: 17, outputTokens: 7, label: "turn 2" },
  ]);
});

test("request thresholds use input before generated output is appended", () => {
  const boundary = scenario({
    contextTokens: 270_000,
    postShakeTokens: 270_000,
    postCompactTokens: 270_000,
    turns: 2,
    perTurnInputTokens: 1_000,
    outputPerTurn: 2_000,
    cache: "cold",
  });
  const requests = simulate(boundary, "no-shake");

  assert.deepEqual(requests, [
    { inputTokens: 271_000, cachedInputTokens: 0, cacheWriteInputTokens: 271_000, outputTokens: 2_000, label: "turn 1" },
    { inputTokens: 274_000, cachedInputTokens: 271_000, cacheWriteInputTokens: 3_000, outputTokens: 2_000, label: "turn 2" },
  ]);
  assert.ok(Math.abs(requestCostUsd(astra, requests[0]!) - 3.4875) < 1e-12);
});

test("scenario sizes and counts are non-negative integers within reductions and model limits", () => {
  assert.throws(() => simulate(scenario({ turns: 1.5 })), RangeError);
  assert.throws(() => simulate(scenario({ recoveryReads: 1.5 })), RangeError);
  assert.throws(() => simulate(scenario({ perTurnInputTokens: 1.5 })), RangeError);
  assert.throws(() => simulate(scenario({ postShakeTokens: 101 })), RangeError);
  assert.throws(() => simulate(scenario({ postCompactTokens: 41 })), RangeError);
  assert.throws(
    () => strategyCostUsd(astra, scenario({ contextTokens: 1_050_001 }), "no-shake"),
    RangeError,
  );
  assert.throws(
    () => strategyCostUsd(astra, scenario({ contextTokens: 1_050_000, perTurnInputTokens: 0 }), "no-shake"),
    RangeError,
  );
  assert.throws(() => breakEvenTurns(astra, scenario(), 1.5), RangeError);
  assert.deepEqual(simulate(scenario({ turns: 0 }), "shake-then-compact"), []);
});

test("break-even includes the one-time shake cache rebuild", () => {
  const longWarm = scenario({
    contextTokens: 300_000,
    postShakeTokens: 120_000,
    postCompactTokens: 30_000,
    turns: 5,
    perTurnInputTokens: 3_000,
    cache: "warm",
  });

  assert.equal(breakEvenTurns(astra, longWarm, 10), 3);
});

test("no-long-multiplier pricing remains an explicit hypothetical copy", () => {
  const hypothetical = withoutLongContextMultipliers(pricing);

  assert.equal(astra.longContextInputMultiplier, 2);
  assert.equal(hypothetical.models["gpt-6-astra"]!.longContextInputMultiplier, 1);
  assert.equal(hypothetical.models["gpt-6-astra"]!.longContextOutputMultiplier, 1);
  assert.equal(pricing.models["gpt-6-astra"]!.longContextInputMultiplier, 2);
});

// --- the codex-credits profile ---------------------------------------------
//
// The real cost of a ChatGPT-login replay. Separate profile because the Codex
// plan rate card has no long-context multiplier and no cache-write charge,
// which the API rate card both have.

test("both pricing profiles resolve, and the api profile IS the top-level model map", () => {
  assert.deepEqual(profileNames(pricing).sort(), ["api", "codex-credits"]);
  const api = loadProfile(pricing);
  assert.equal(api.unit, "usd");
  assert.equal(api.models["gpt-6-astra"], astra, "api profile must reference, not copy, pricing.models");
  assert.throws(() => loadProfile(pricing, "invented"), /unknown pricing profile/);
});

test("codex credits price a request with no long-context term and no cache-write charge", () => {
  const credits = loadProfile(pricing, "codex-credits");
  assert.equal(credits.unit, "credits");
  const astraCredits = credits.models["gpt-6-astra"]!;
  assert.equal(astraCredits.longContextThresholdTokens, null);
  assert.equal(astraCredits.cacheWriteInputPerMTok, 0);
  // 1M input of which 800k cached and 100k newly written to cache, 10k output.
  // 100,000 uncached * 250/1M + 800,000 * 25/1M + 100,000 * 0 + 10,000 * 1250/1M
  const request = { inputTokens: 1_000_000, cachedInputTokens: 800_000, cacheWriteInputTokens: 100_000, outputTokens: 10_000, label: "r" };
  const standard = requestCost(astraCredits, request);
  assert.ok(Math.abs(standard - (25 + 20 + 12.5)) < 1e-9, `got ${standard}`);
  // The same request above the API profile's 272K threshold pays no multiplier
  // here: 1M input tokens would have doubled every input term on the API card.
  assert.ok(request.inputTokens > (astra.longContextThresholdTokens ?? 0));
});

test("the Fast tier multiplies every credit term by 2.5x, and is unknown where unpublished", () => {
  const credits = loadProfile(pricing, "codex-credits");
  const astraCredits = credits.models["gpt-6-astra"]!;
  const request = { inputTokens: 500_000, cachedInputTokens: 400_000, outputTokens: 5_000, label: "r" };
  const standard = requestCost(astraCredits, request);
  const fast = requestCost(astraCredits, request, "fast");
  assert.ok(Math.abs(fast - standard * 2.5) < 1e-9, `${fast} vs ${standard}`);
  // The page states the multiplier for Astra only; the 5.6 line must print
  // "unknown" rather than borrowing Astra's 2.5x.
  assert.equal(credits.models["gpt-5.6-sol"]!.fastMultiplier, null);
  assert.throws(() => requestCost(credits.models["gpt-5.6-sol"]!, request, "fast"), UnknownPrice);
  assert.ok(requestCost(credits.models["gpt-5.6-sol"]!, request) > 0);
});
