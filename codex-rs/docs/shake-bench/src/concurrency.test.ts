import assert from "node:assert/strict";
import test from "node:test";
import { runBounded } from "./concurrency.ts";

test("runBounded respects the limit and executes each job once", async () => {
  let active = 0;
  let maximum = 0;
  const seen: number[] = [];
  const jobs = Array.from({ length: 9 }, (_, index) => async () => {
    active += 1;
    maximum = Math.max(maximum, active);
    await new Promise((resolve) => setTimeout(resolve, index % 3));
    seen.push(index);
    active -= 1;
    return index * 2;
  });

  const results = await runBounded(jobs, 3);
  assert.equal(maximum, 3);
  assert.deepEqual([...results].sort((a, b) => a - b), jobs.map((_, index) => index * 2));
  assert.deepEqual([...seen].sort((a, b) => a - b), jobs.map((_, index) => index));
});

test("runBounded stops scheduling new jobs but lets active jobs settle after failure", async () => {
  const started: number[] = [];
  const settled: number[] = [];
  const jobs = Array.from({ length: 8 }, (_, index) => async () => {
    started.push(index);
    await new Promise((resolve) => setTimeout(resolve, index === 0 ? 5 : 1));
    settled.push(index);
    if (index === 1) throw new Error("job failed");
    return index;
  });

  await assert.rejects(() => runBounded(jobs, 3), /job failed/);
  assert.ok(started.length < jobs.length);
  assert.deepEqual([...settled].sort((a, b) => a - b), [...started].sort((a, b) => a - b));
});

test("runBounded rejects an invalid limit", async () => {
  await assert.rejects(() => runBounded([], 0), /positive integer/);
});
