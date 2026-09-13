<!-- This file is the top of every GitHub release for this branch. codex-fork-release
     appends the base tag, install block and commit list after it. Plain markdown. -->

Test builds of Codex with **shake**, a context reducer that cuts instead of summarizing.

**What it does.** Compaction replaces the history with a model-written summary. Shake removes the parts of the history that are almost never needed again: old tool output, images, thinking. Each removed item is written to a file under `$CODEX_HOME/artifacts/` and the placeholder names the path. The rest of the thread is untouched.

**Why it helps.** Every request re-sends the whole history, and gpt-5.6 charges a long-context premium above 272k input tokens. A thread that shakes at 160k stays below that line, and every request after the shake is a fraction of the size.

**What it costs.** One uncached request of the surviving history. The thread is fully cached again on the next request.

**What was measured**, replaying a real 536k-token session from a frozen checkpoint:

- Shaken threads reach the same end state at 2.3x to 2.7x fewer credits.
- Nothing elided is ever read back: zero reads in nine benchmark runs and zero across 346 oh-my-pi sessions. So there is no read tool, only the path.
- Plan quota meters total input tokens, not uncached tokens. Warming the cache does not save quota. Shrinking the prompt does.
- Over one busy week of real rollouts, auto-shake would have cut 2.0B of 12.4B input tokens.

**Auto-shake is on by default** in this build: at 160k tokens on gpt-5.6, at 40% of the window on Astra, and on any resume after the prompt cache has expired. `/shake` in the TUI does it by hand, with a preview.

- Headline numbers: [RESULTS.md](codex-rs/docs/shake-bench/RESULTS.md)
- Docs and config: [shake.md](codex-rs/docs/shake.md)
- Benchmark scripts and full reports: [shake-bench](codex-rs/docs/shake-bench/README.md)
