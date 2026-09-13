# shake retention benchmark: generated results

Run directory: `$BENCH_RESULTS/full-plain-2026-09-11`  
Model: `gpt-6-astra`  
Binary: `codex-cli 0.153.4`  
Fixture variant(s): plain  
Completed cells: 60/60; scored observations per arm: 900.
Validation: complete manifest matrix, matching model and variant identities, completed turns, parse success, and 100% full controls.

Nested recovery telemetry is unavailable for 24 cell(s); nested attempt counts and derived page bytes are shown as n/a, and nested-tool identity/exclusivity is unverified for those wrapper cells.

## Accuracy

| Arm | Correct | Accuracy | Descriptive Wilson 95% interval |
|---|---:|---:|---:|
| full | 900/900 | 100.0% | 99.6%-100.0% |
| shake-elide | 900/900 | 100.0% | 99.6%-100.0% |
| shake-elide-noread | 732/900 | 81.3% | 78.7%-83.7% |
| compact | 900/900 | 100.0% | 99.6%-100.0% |
| shake-then-compact | 900/900 | 100.0% | 99.6%-100.0% |

## Accuracy by category

| Category | full | shake-elide | shake-elide-noread | compact | shake-then-compact |
|---|---:|---:|---:|---:|---:|
| distractor_resolution | 100.0% | 100.0% | 100.0% | 100.0% | 100.0% |
| exact_recall | 100.0% | 100.0% | 100.0% | 100.0% | 100.0% |
| relational_state | 100.0% | 100.0% | 100.0% | 100.0% | 100.0% |
| task_continuation | 100.0% | 100.0% | 100.0% | 100.0% | 100.0% |
| tool_history | 100.0% | 100.0% | 6.7% | 100.0% | 100.0% |

## Paired outcomes against the full-context control

Per question, per (fixture, trial). `control only` counts retention losses caused by the arm. The paired p-value is descriptive because questions from one generated turn are not independent observations.

| Arm | Both correct | Control only | Arm only | Both wrong | Descriptive paired p |
|---|---:|---:|---:|---:|---:|
| shake-elide | 900 | 0 | 0 | 0 | 1.00 |
| shake-elide-noread | 732 | 168 | 0 | 0 | 5.35e-51 |
| compact | 900 | 0 | 0 | 0 | 1.00 |
| shake-then-compact | 900 | 0 | 0 | 0 | 1.00 |

## Downstream footprint and latency

`Downstream input tokens` is the input of the evaluation turn's **first** model request, i.e. the context the arm presented. `Turn input tokens` sums every request in the turn, so it includes recovery round trips.

| Arm | Downstream input tokens | Turn input tokens | Output tokens | Eval latency (s) | Prep latency (s) | Parse failures |
|---|---:|---:|---:|---:|---:|---:|
| full | 64,706 | 64,706 | 1,551 | 51.4 | 0.0 | 0 |
| shake-elide | 45,700 | 97,490 | 1,879 | 63.1 | 0.6 | 0 |
| shake-elide-noread | 45,692 | 45,692 | 1,388 | 46.0 | 0.6 | 0 |
| compact | 41,073 | 41,073 | 1,551 | 49.1 | 134.2 | 0 |
| shake-then-compact | 41,640 | 93,007 | 1,914 | 66.1 | 157.6 | 0 |

## Recovery cost (shake arms)

| Arm | Cells with attempts | Outer code-mode continuations | Nested read attempts | Mean derived page bytes | Mean extra input tokens | Mean artifacts written | Mean artifact bytes | noread violations |
|---|---:|---:|---:|---:|---:|---:|---:|
| shake-elide | n/a/12 | 1.0 | n/a | n/a | 51,790 | 65.0 | 134,979 | 0 |
| shake-elide-noread | 0/12 | 0.0 | 0.0 | 0 | 0 | 65.0 | 134,979 | 0 |
| shake-then-compact | n/a/12 | 1.0 | n/a | n/a | 51,367 | 65.0 | 134,979 | 0 |

Nested read attempts come from executed-tool metadata and may be batched inside one outer code-mode continuation. Derived page bytes are estimates from artifact content and offsets; actual handler-returned bytes are unavailable. Any evaluation tool call in the `shake-elide-noread` arm is a protocol violation and is excluded only from the conditional compliant view.


## Shake preview estimate vs. realized downstream input

Preview numbers are local estimates that exclude base instructions and tool schemas; the table reports their raw difference and ratio against the realized first request.

| Arm | Applied | Preview tokensBefore | Preview tokensAfter | Estimated saving | Realized downstream input | Realized - preview | Realized / preview | Tool outputs elided | Shake latency (s) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| shake-elide | 12/12 | 96,368 | 63,556 | 32,812 | 45,700 | -17,856 | 0.7 | 65.0 | 0.6 |
| shake-elide-noread | 12/12 | 96,367 | 63,555 | 32,811 | 45,692 | -17,863 | 0.7 | 65.0 | 0.6 |
| shake-then-compact | 12/12 | 96,371 | 63,557 | 32,814 | 41,640 | -21,917 | 0.7 | 65.0 | 0.6 |

Full-context control realized downstream input: 64,706 tokens on average.


## Per-fixture accuracy

| Fixture | full | shake-elide | shake-elide-noread | compact | shake-then-compact |
|---|---:|---:|---:|---:|---:|
| fixture-01 | 100.0% | 100.0% | 81.3% | 100.0% | 100.0% |
| fixture-02 | 100.0% | 100.0% | 81.3% | 100.0% | 100.0% |
| fixture-03 | 100.0% | 100.0% | 81.3% | 100.0% | 100.0% |
| fixture-04 | 100.0% | 100.0% | 81.3% | 100.0% | 100.0% |
| fixture-05 | 100.0% | 100.0% | 81.3% | 100.0% | 100.0% |
| fixture-06 | 100.0% | 100.0% | 81.3% | 100.0% | 100.0% |

This generated document reports measurements only. Interpretive conclusions and limitations belong in REPORT.md.
