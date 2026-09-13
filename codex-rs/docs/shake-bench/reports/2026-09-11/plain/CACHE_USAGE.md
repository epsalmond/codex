# Evaluation usage: plain

Model: `gpt-6-astra`

Values below are the usage updates recorded by the connected runtime. They are not API billing data; reported cache writes of zero do not prove billing behavior.

| Arm | Cells | Input tokens | Cached input | Noncached input | Reported cache writes | Output tokens | Mean first input | Mean outer continuations | Mean eval latency (s) | Mean prep latency (s) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| compact | 12 | 492,871 | 6,656 | 486,215 | 0 | 18,614 | 41072.6 | 0.0 | 49.1 | 134.2 |
| full | 12 | 776,476 | 6,656 | 769,820 | 0 | 18,614 | 64706.3 | 0.0 | 51.4 | 0.0 |
| shake-elide | 12 | 1,169,880 | 552,960 | 616,920 | 0 | 22,551 | 45700.0 | 1.0 | 63.1 | 0.6 |
| shake-elide-noread | 12 | 548,306 | 19,968 | 528,338 | 0 | 16,652 | 45692.2 | 0.0 | 46.0 | 0.6 |
| shake-then-compact | 12 | 1,116,087 | 517,248 | 598,839 | 0 | 22,972 | 41640.4 | 1.0 | 66.1 | 157.6 |

Nested attempt totals are reported when telemetry is available. An outer code-mode continuation may contain multiple nested attempts.
