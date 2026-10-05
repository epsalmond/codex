# Goal stall detector replay

These private modules are a foundation for the continuation guard. They do not
change runtime continuation yet. The detector assesses one completed automatic
goal turn; manual turns reset suspicion. Missing outcomes, unlinked Code Mode
calls, unsupported inbound messages, and capped observations remain unknown.

Run the deterministic tests from the repository root:

```sh
just test -p codex-goal-extension --test stall_detector --test stall_observation --test stall_replay
```

The empirical test is ignored by default. To replay local rollout windows:

```sh
CODEX_STALL_CORPUS_MANIFEST=/absolute/private/manifest.json just test -p codex-goal-extension --test stall_replay real_local_episode_replay --run-ignored only --no-capture
```

The private JSON manifest has a positive `threshold` and an `episodes` array. Each
episode specifies `id`, absolute local `path`, inclusive `start_line`/`end_line`,
`expected_stall`, and optional `counterfactual_auto`. Labels are used only to
evaluate the resulting assessments, never as extracted features. Use complete
turn windows, preserving interleaved user input, returned results, and unknown
observations. Keep manifests and original transcripts outside version control.

Counterfactual mode replaces only the original turn admission. Subsequent real
user input and inbound results keep their original reset behavior. Report it
separately from automatic turns observed in the source.

## Initial local evidence

The approved candidate order was 3, 2, 4, 5. Candidate 3 missed the verified
three-turn timer run: its first action is a baseline, leaving two repetitions.
Candidate 2 caught that run on its third turn and the 52-turn assistant waiting
sequence on its second turn. Both bad episodes came from one incident family.
Candidates 4 and 5 were not needed after candidate 2 passed.

Five manual productive controls produced no holds under original eligibility.
With only admission replaced, all remained unknown; one research turn exceeded
the 256-call cap. These demonstrate conservative compatibility, not productive
classification accuracy. Source read/edit activity does not establish a test
launch, and the two child repair windows do not prove the previously claimed
test pass counts. The corrected research invocation did report 14 passes.

A separate eight-turn automatic control preserved returned work/results with
interleaved waits: four observations were complete, four unknown, maximum
suspicion was one, and no hold occurred. It belongs to the same incident group.
Deterministic automatic tests additionally cover changed targets/results, real
input resets, and compaction preserving complete-turn boundaries.

The extraction boundary stores bounded call correlations and digests, not
payloads. Tool arguments and outcomes are borrowed for hashing. Only owned
transport envelope fields are removed; substantive code, targets, ranges,
status, and output remain part of the action fingerprint. A narrowly recognized
self-contained timer cell has a leaf outcome even without nested tracing; other
opaque Code Mode cells remain unknown. Integration must feed this same observer
from live, linked tool outcomes and trusted turn admission before enabling holds.
