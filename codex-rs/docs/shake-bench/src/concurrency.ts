export type AsyncJob<T> = () => Promise<T>;

/** Run jobs with a bounded worker count; active jobs settle before a failure escapes. */
export async function runBounded<T>(jobs: readonly AsyncJob<T>[], concurrency: number): Promise<T[]> {
  if (!Number.isInteger(concurrency) || concurrency < 1) throw new Error("concurrency must be a positive integer");
  const results: T[] = new Array(jobs.length);
  let nextIndex = 0;
  let failure: unknown;

  async function worker(): Promise<void> {
    while (failure === undefined) {
      const index = nextIndex++;
      if (index >= jobs.length) return;
      try {
        results[index] = await jobs[index]!();
      } catch (error) {
        failure = error;
        return;
      }
    }
  }

  await Promise.all(Array.from({ length: Math.min(concurrency, jobs.length) }, () => worker()));
  if (failure !== undefined) throw failure;
  return results;
}
