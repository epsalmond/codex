// The compaction assertion, against synthetic rollouts.
//
// Two sequences matter and the old check could not tell them apart, because it
// treated every `compacted` record as a violation:
//
//   shake-then-clean  a shake writes its own `compacted` record BEFORE the
//                     arm's first model request, then the run climbs
//                     monotonically. This is a full-context run and must PASS.
//                     Arms A and C were both this, and both exited 2
//                     (arms-2026-09-11.md finding 3).
//   real compaction   a `compacted` record mid-run, and/or the input-token
//                     cliff that follows it. Must FAIL.

import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { SHAKE_COMPACTION_MESSAGE, checkCompaction } from "../scripts/replay-01a07e54.ts";

const THREAD = "01a09435-e0dc-7960-84ee-685e4f97b1f6";

type Rec = Record<string, unknown>;

/** A `token_usage_record` as the worker writes it. */
function request(inputTokens: number, cachedInputTokens: number, outputTokens = 100, thread = THREAD): Rec {
  return {
    timestamp: "2026-09-12T06:02:42.735Z",
    type: "token_usage_record",
    payload: { thread_id: thread, usage: { input_tokens: inputTokens, cached_input_tokens: cachedInputTokens, output_tokens: outputTokens } },
  };
}

function compacted(message: string): Rec {
  return { timestamp: "2026-09-12T06:02:35.503Z", type: "compacted", payload: { message, replacement_history: [] } };
}

/** Write a rollout where the runner looks for one and return the CODEX_HOME. */
function home(records: Rec[]): string {
  const root = mkdtempSync(join(tmpdir(), "shake-compaction-test-"));
  const dir = join(root, "sessions", "2026", "09", "12");
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, `rollout-2026-09-12T06-02-34-${THREAD}.jsonl`), `${records.map((r) => JSON.stringify(r)).join("\n")}\n`);
  return root;
}

// The shake path also mints an internal `auto-compact-0` turn, which §11.9
// established is a naming collision and not a violation; the synthetic
// sequences carry it so the two signals stay independently tested.
const AUTO_COMPACT_TURN: Rec = {
  timestamp: "2026-09-12T06:02:34.707Z",
  type: "turn_context",
  payload: { turn_id: "auto-compact-0" },
};

test("a shake's pre-run compacted record is ignored and the run passes", () => {
  // Arm A's actual shape: auto-compact-0, the shake's `compacted` record, then
  // 51 monotonically climbing requests.
  const result = checkCompaction(
    home([
      AUTO_COMPACT_TURN,
      compacted(SHAKE_COMPACTION_MESSAGE),
      request(152_140, 0),
      request(152_500, 152_064),
      request(180_000, 152_064),
      request(216_055, 179_712),
    ]),
    THREAD,
  );
  assert.deepEqual(result.violations, []);
  assert.equal(result.passed, true);
  assert.equal(result.requests.length, 4);
  assert.equal(result.compactedEvents.length, 1, "the record is still recorded, just not counted");
  assert.equal(result.compactedEvents[0]!.shake, true);
  assert.equal(result.compactedEvents[0]!.ignored, true);
  assert.equal(result.compactedEvents[0]!.requestsBefore, 0);
  assert.equal(result.autoCompactTurnContexts.length, 1);
});

test("a real mid-run compaction still fails, by both signals", () => {
  const result = checkCompaction(
    home([
      AUTO_COMPACT_TURN,
      request(516_268, 0),
      request(516_409, 516_096),
      compacted("Context was compacted to fit the model's window"),
      request(19_566, 0),
      request(21_000, 19_456),
    ]),
    THREAD,
  );
  assert.equal(result.passed, false);
  // Both the record and the 96% input cliff it caused are reported.
  assert.equal(result.compactedEvents[0]!.ignored, false);
  assert.equal(result.compactedEvents[0]!.requestsBefore, 2);
  assert.equal(result.drops.length, 1);
  assert.deepEqual({ from: result.drops[0]!.from, to: result.drops[0]!.to }, { from: 2, to: 3 });
  assert.equal(result.violations.length, 2);
  assert.match(result.violations[0]!, /after 2 request\(s\)/);
});

test("a pre-run compacted record that is not the shake path is still a violation", () => {
  // If the injected history was genuinely compacted before the arm started, the
  // arm's recorded starting context is wrong. Only the shake marker excuses it.
  const result = checkCompaction(home([compacted("Context was compacted"), request(152_140, 0), request(152_500, 152_064)]), THREAD);
  assert.equal(result.passed, false);
  assert.equal(result.compactedEvents[0]!.ignored, false);
  assert.match(result.violations[0]!, /NOT the shake path/);
});

test("a shake-marked record that fires mid-run is a violation", () => {
  const result = checkCompaction(home([request(516_268, 0), compacted(SHAKE_COMPACTION_MESSAGE), request(175_000, 0)]), THREAD);
  assert.equal(result.passed, false);
  assert.match(result.violations[0]!, /fires mid-run/);
});

test("a warm-cache priming request is excluded from the cutoff and the drop check", () => {
  // --cache warm --warm session: prime on the FULL history, then shake, then
  // run the arm. The shake's record lands after the priming request, and the
  // arm's first request is a third the size of it. Neither is a violation.
  const records = [
    request(516_268, 0), // priming
    compacted(SHAKE_COMPACTION_MESSAGE),
    request(152_140, 0), // arm request 1
    request(152_500, 152_064),
  ];
  const warm = checkCompaction(home(records), THREAD, { firstArmRequestIndex: 2 });
  assert.deepEqual(warm.violations, []);
  assert.equal(warm.passed, true);
  assert.equal(warm.primingRequests, 1);
  assert.equal(warm.compactedEvents[0]!.ignored, true);
  // Without the index the same rollout fails twice over -- which is what makes
  // passing it mandatory for a warm run rather than cosmetic.
  const cold = checkCompaction(home(records), THREAD);
  assert.equal(cold.passed, false);
  assert.equal(cold.drops.length, 1);
});

test("requests belonging to another thread are not counted", () => {
  const result = checkCompaction(
    home([request(9_999, 0, 10, "some-other-thread"), compacted(SHAKE_COMPACTION_MESSAGE), request(152_140, 0)]),
    THREAD,
  );
  assert.equal(result.requests.length, 1);
  assert.equal(result.passed, true, "a foreign thread's request must not make the shake record look mid-run");
});
