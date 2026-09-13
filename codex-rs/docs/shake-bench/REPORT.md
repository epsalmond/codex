# Shake retention benchmark report

## Plain-English summary

Shake preserved all the answers when the model could fetch deleted information
again. Think of a tool output as a long receipt: shake removes the receipt from
the conversation but keeps a copy that the model can retrieve.

We tested three situations:

- Delete the receipts and allow retrieval: the model answered 100% correctly.
  It fetched information back when needed.
- Delete the receipts and forbid retrieval: accuracy fell to 81%. The missing
  answers were facts that existed only in those receipts.
- Write the useful facts into conversation notes first, then delete the
  receipts: accuracy stayed at 100% without retrieval.

That last case tests the idea that once the assistant has repeated the
important result, keeping the original tool output may be unnecessary. We
deliberately repeated every result in this version, so it is an optimistic
case.

The retrieval overhead needs to be read carefully:

| Per plain evaluation cell | Full context | Shake with retrieval | Difference |
|---|---:|---:|---:|
| Initial request input | 64,706 | 45,700 | −19,006 (−29.4%) |
| Recovery follow-up input | 0 | 51,790 | 45,525 cached; 6,265 noncached (87.9% cached) |
| Total evaluation input | 64,706 | 97,490 | +32,784 (+50.7%) |
| Evaluation output | 1,551 | 1,879 | +328 |

The follow-up mostly resends the remaining context; it is not 51,790 tokens
of newly retrieved text. On a noncached basis, shake used 51,410 input tokens
per cell versus 64,152 for full context, a 19.9% reduction. The full-context
first requests were almost uncached, with only about 555 cached tokens out of
64,706 on average, so this does not establish savings for an already-warm real
session. Evaluation plus preparation latency was about 12.4 seconds higher
for shake. Cache discounts, quota weights, and billing prices were not
measured, so these numbers do not establish an actual dollar or quota saving.

Compared with the no-read arm, the first request was effectively the same at
about 45,700 input tokens; retrieval added the 51,790-token follow-up per
cell. That is why the useful question for a real transcript is how often the
assistant can answer from a compact summary without needing that round trip.

Shake reduced the initial context by about 29%, but recovery added another
model request. Ordinary compaction also preserved every answer here, but
preparation took about 134 seconds, compared with under one second for shake.

This was a synthetic test. It demonstrates the mechanism, but it does not
establish how often real coding sessions can discard tool outputs without
needing them again. A real transcript is needed to measure natural redundancy
and how often retrieval is actually necessary. Spark is a candidate to test for
that summary pass. A useful next experiment would compare keeping the original
transcript, shake without retrieval, shake with retrieval, and a Spark-written
summary on the same real transcript, using the same continuation and evaluator
while counting the summary pass and any recovery costs. This report makes no
claim about summary quality or tested Spark performance.

The 2026-09-11 runs completed and resumed successfully. Both runs used
`gpt-6-astra` / `codex-cli 0.153.4`, the padded fixture adapter, and the
unavailable nested-telemetry policy. The curated artifacts are under
[`reports/2026-09-11`](reports/2026-09-11); the arithmetic H1 analysis is in
[`COST_ANALYSIS.md`](COST_ANALYSIS.md).

## Measured outcome

The plain run covered 6 fixtures × 2 trials × 5 arms. The full, shake-elide,
compact, and shake-then-compact arms scored 900/900. The prompt-only
`shake-elide-noread` arm scored 732/900 (81.3%); its `tool_history` category
scored 12/180 (6.7%). The echo run covered the full and noread arms over the
same 6 × 2 cells. Both scored 900/900, including 180/180 in `tool_history`.

| Plain arm | Accuracy | Mean first input | Mean turn input | Mean prep latency |
|---|---:|---:|---:|---:|
| full | 900/900 (100.0%) | 64,706 | 64,706 | 0.0 s |
| shake-elide | 900/900 (100.0%) | 45,700 | 97,490 | 0.6 s |
| shake-elide-noread | 732/900 (81.3%) | 45,692 | 45,692 | 0.6 s |
| compact | 900/900 (100.0%) | 41,073 | 41,073 | 134.2 s |
| shake-then-compact | 900/900 (100.0%) | 41,640 | 93,007 | 157.6 s |

The echo run is a constructed verbatim-echo upper bound, not a measurement of
how often real assistants echo tool results. In this workload it restored the
padded noread `tool_history` score from 6.7% in plain to 100.0% in echo. This is
evidence about these synthetic variants, not a general claim about real
sessions.

## Usage and recovery

The plain evaluation turns recorded 4,103,620 cumulative input tokens and
99,403 output tokens. The echo subset recorded 1,391,465 input and 37,228
output tokens. The detailed cached/uncached breakdown is in
[`plain/CACHE_USAGE.md`](reports/2026-09-11/plain/CACHE_USAGE.md) and
[`echo/CACHE_USAGE.md`](reports/2026-09-11/echo/CACHE_USAGE.md).

| Run / arm | Eval input | Cached input | Noncached input | Reported cache writes | Eval output |
|---|---:|---:|---:|---:|---:|
| Plain / full | 776,476 | 6,656 | 769,820 | 0 | 18,614 |
| Plain / shake-elide | 1,169,880 | 552,960 | 616,920 | 0 | 22,551 |
| Plain / shake-elide-noread | 548,306 | 19,968 | 528,338 | 0 | 16,652 |
| Plain / compact | 492,871 | 6,656 | 486,215 | 0 | 18,614 |
| Plain / shake-then-compact | 1,116,087 | 517,248 | 598,839 | 0 | 22,972 |
| Echo / full | 809,836 | 6,656 | 803,180 | 0 | 18,614 |
| Echo / shake-elide-noread | 581,629 | 13,312 | 568,317 | 0 | 18,614 |

The runtime reported zero cache writes. That is connected-runtime usage
telemetry, not proof of API billing. More cumulative input tokens can still be
cheaper when cached; API-price arithmetic and ChatGPT quota accounting are
separate.

The evaluation usage table does not include separate compaction-call token
accounting or API cache/quota prices. Compaction preparation latency is reported
separately; it should not be treated as a complete monetary cost.

For plain shake arms, the runtime observed one outer code-mode continuation on
average, but nested inventory was unavailable; nested attempt counts and
handler-returned bytes are therefore `n/a`. Observing an outer code-mode
continuation alone cannot establish which nested tools ran or their
exclusivity. The noread and echo cells had no
outer tool calls and used the known-zero no-wrapper shape. Derived page bytes
are estimates only and do not establish handler success.

## H1 cost interpretation

[`COST_ANALYSIS.md`](COST_ANALYSIS.md) gives generated warm/cold 300K→120K
scenarios for Astra and Sol, serial-read sensitivity, and the explicit
no-long-context-multiplier hypothesis. Published Astra pricing applies 2×
input/cache and 1.5× output above 272K. The no-multiplier values are labeled
hypothetical; none of these arithmetic values are billing data.

## Method and provenance

The fixture generator remains verbatim upstream, while the adapter pads all 65
tool outputs per fixture/variant to exercise the shipped elision threshold.
The runner starts with the full control, then rotates remaining arms by
fixture/trial. The manifest fingerprint and expected matrix were checked on
resume; the plain rerun skipped all 60 cells and the echo rerun skipped all 24.

Source fingerprints:

- Plain: `4a3aca6279a35f958581cd1596ddd7838963cd94d1c4c30373a10e2fb30cf409`
- Echo: `4a3aca6279a35f958581cd1596ddd7838963cd94d1c4c30373a10e2fb30cf409`

Commands used:

```bash
npm test
npm run run -- --fixtures 6 --trials 2 \
  --arms full,shake-elide,shake-elide-noread,compact,shake-then-compact \
  --variant plain --model gpt-6-astra --nested-telemetry unavailable \
  --concurrency 3 --out results/full-plain-2026-09-11
npm run run -- --fixtures 6 --trials 2 \
  --arms full,shake-elide-noread --variant echo --model gpt-6-astra \
  --nested-telemetry unavailable --concurrency 1 \
  --out results/full-echo-2026-09-11
npm run analyze -- results/full-plain-2026-09-11
npm run analyze -- results/full-echo-2026-09-11
npm run estimate
node --import tsx scripts/finalize-report.ts \
  results/full-plain-2026-09-11 results/full-echo-2026-09-11 reports/2026-09-11
```

## Limitations

- Synthetic workload with answer-neutral padded tool outputs.
- One model and one shake pass; repeated shake chains are unmeasured.
- Echo covered only the full and noread arms.
- Control-first execution and rotated remaining-arm order distribute order
  effects but do not make the design fully balanced.
- Nested attempt counts and handler-returned bytes are unavailable for wrapper
  cells under the current runtime metadata policy; nested-tool exclusivity is
  unverified when native inventory metadata is absent.
- Wilson intervals and paired p-values are descriptive; questions generated in
  one turn are not independent observations.
- Preparation latency and quota/cache effects are not represented by
  evaluation-token totals alone.
