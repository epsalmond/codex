# Evaluation usage: echo

Model: `gpt-6-astra`

Values below are the usage updates recorded by the connected runtime. They are not API billing data; reported cache writes of zero do not prove billing behavior.

| Arm | Cells | Input tokens | Cached input | Noncached input | Reported cache writes | Output tokens | Mean first input | Mean outer continuations | Mean eval latency (s) | Mean prep latency (s) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| full | 12 | 809,836 | 6,656 | 803,180 | 0 | 18,614 | 67486.3 | 0.0 | 51.2 | 0.0 |
| shake-elide-noread | 12 | 581,629 | 13,312 | 568,317 | 0 | 18,614 | 48469.1 | 0.0 | 50.8 | 0.6 |

Nested attempt totals are reported when telemetry is available. An outer code-mode continuation may contain multiple nested attempts.
