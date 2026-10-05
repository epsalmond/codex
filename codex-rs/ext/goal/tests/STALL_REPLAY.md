# Goal stall detector replay

These private modules share the active runtime continuation guard's extraction
and assessment rules. Default `defer` holds an Active goal after two suspicious
automatic turns; repeated actions first establish a baseline. `observe` warns
without holding and `off` skips assessment. The detector assesses one completed
automatic goal turn; manual turns reset suspicion. Missing outcomes, unlinked Code Mode
calls, unsupported inbound messages, and capped observations remain unknown.

Run the deterministic tests from the repository root:

```sh
just test -p codex-goal-extension --test stall_detector --test stall_observation --test stall_replay
```

The empirical test is ignored by default. To replay local rollout windows:

```sh
CODEX_STALL_CORPUS_MANIFEST=/absolute/private/manifest.json just test -p codex-goal-extension --test stall_replay real_local_episode_replay --run-ignored only --no-capture
```

The private JSON manifest has an `episodes` array and an optional positive
`threshold` (collector default 3; use 2 for the shipped guard candidate). This top-level candidate threshold takes precedence over
`goals.stall_after_no_progress_turns`; the latter is a live-runtime setting and
is not the replay collector's candidate threshold. Optional `goals` contains
settings in the same shape as `[goals]` in config.toml. Waiting-prefix replacement,
normalized text limit, and literal-timer recognition use the same validated
versioned packaged profile as live execution. Omitted fields use packaged defaults;
`stall_waiting_prefixes: []` disables prefix matching. Invalid effective settings
fail the collector instead of producing misleading zero-assessment statistics.
Replay reports candidate assessments regardless of live `continuation_guard_mode`;
it does not mutate a runtime goal or apply holds.

Each episode specifies `id`, absolute local `path`, inclusive `start_line`/`end_line`,
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
opaque Code Mode cells remain unknown. The active integration feeds this observer
accepted borrowed tool outcomes, accepted history items, and successful host goal
admission. Replay admits automatic turns only from structured `goal.internal_context`
provenance, so changing the human continuation prompt does not alter eligibility.
Provider-native work that lacks supported output extraction is Unknown in both
paths. Live recovery persists bounded receipt identities atomically with releasing
only the detector hold; fork deferrals retain their separate contract.
