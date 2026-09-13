# shake retention benchmark: generated results

Run directory: `$BENCH_RESULTS/full-echo-2026-09-11`  
Model: `gpt-6-astra`  
Binary: `codex-cli 0.153.4`  
Fixture variant(s): echo  
Completed cells: 24/24; scored observations per arm: 900.
Validation: complete manifest matrix, matching model and variant identities, completed turns, parse success, and 100% full controls.

## Accuracy

| Arm | Correct | Accuracy | Descriptive Wilson 95% interval |
|---|---:|---:|---:|
| full | 900/900 | 100.0% | 99.6%-100.0% |
| shake-elide-noread | 900/900 | 100.0% | 99.6%-100.0% |

## Accuracy by category

| Category | full | shake-elide-noread |
|---|---:|---:|
| distractor_resolution | 100.0% | 100.0% |
| exact_recall | 100.0% | 100.0% |
| relational_state | 100.0% | 100.0% |
| task_continuation | 100.0% | 100.0% |
| tool_history | 100.0% | 100.0% |

## Paired outcomes against the full-context control

Per question, per (fixture, trial). `control only` counts retention losses caused by the arm. The paired p-value is descriptive because questions from one generated turn are not independent observations.

| Arm | Both correct | Control only | Arm only | Both wrong | Descriptive paired p |
|---|---:|---:|---:|---:|---:|
| shake-elide-noread | 900 | 0 | 0 | 0 | 1.00 |

## Downstream footprint and latency

`Downstream input tokens` is the input of the evaluation turn's **first** model request, i.e. the context the arm presented. `Turn input tokens` sums every request in the turn, so it includes recovery round trips.

| Arm | Downstream input tokens | Turn input tokens | Output tokens | Eval latency (s) | Prep latency (s) | Parse failures |
|---|---:|---:|---:|---:|---:|---:|
| full | 67,486 | 67,486 | 1,551 | 51.2 | 0.0 | 0 |
| shake-elide-noread | 48,469 | 48,469 | 1,551 | 50.8 | 0.6 | 0 |

## Recovery cost (shake arms)

| Arm | Cells with attempts | Outer code-mode continuations | Nested read attempts | Mean derived page bytes | Mean extra input tokens | Mean artifacts written | Mean artifact bytes | noread violations |
|---|---:|---:|---:|---:|---:|---:|---:|
| shake-elide-noread | 0/12 | 0.0 | 0.0 | 0 | 0 | 65.0 | 134,979 | 0 |

Nested read attempts come from executed-tool metadata and may be batched inside one outer code-mode continuation. Derived page bytes are estimates from artifact content and offsets; actual handler-returned bytes are unavailable. Any evaluation tool call in the `shake-elide-noread` arm is a protocol violation and is excluded only from the conditional compliant view.


## Shake preview estimate vs. realized downstream input

Preview numbers are local estimates that exclude base instructions and tool schemas; the table reports their raw difference and ratio against the realized first request.

| Arm | Applied | Preview tokensBefore | Preview tokensAfter | Estimated saving | Realized downstream input | Realized - preview | Realized / preview | Tool outputs elided | Shake latency (s) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| shake-elide-noread | 12/12 | 102,274 | 69,461 | 32,813 | 48,469 | -20,992 | 0.7 | 65.0 | 0.6 |

Full-context control realized downstream input: 67,486 tokens on average.


## Per-fixture accuracy

| Fixture | full | shake-elide-noread |
|---|---:|---:|
| fixture-01 | 100.0% | 100.0% |
| fixture-02 | 100.0% | 100.0% |
| fixture-03 | 100.0% | 100.0% |
| fixture-04 | 100.0% | 100.0% |
| fixture-05 | 100.0% | 100.0% |
| fixture-06 | 100.0% | 100.0% |

This generated document reports measurements only. Interpretive conclusions and limitations belong in REPORT.md.
