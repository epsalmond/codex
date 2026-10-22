# NAS semantic-navigation acceptance corpus

This directory is a fixed, dependency-free Python workspace and independent
source oracle for Codex #164. It can be used by any broker adapter. It does not
start a server, register a workspace, or depend on broker protocol code. The
broker contract is documented in
[`nas-lsp-broker.md`](../../../../.codex/design/nas-lsp-broker.md).

## Fixed tasks and coordinates

`oracle.json` defines twelve tasks: six definitions and six reference searches.
They cover cross-file calls, two unrelated functions named `render`, two calls
on the same line, astral Unicode before a queried symbol, and an empty answer
for a position inside a comment. Definition locations refer to identifier
tokens, not whole statements. Reference answers **include the declaration**.
Result order is immaterial; compare complete, deduplicated location sets.

The source explicitly imports `pkg.primary` and `pkg.secondary`, then calls
their respective `render` attributes. The oracle was enumerated from those
bindings and saved source, independently of a language-server response.
`corpus.py validate` separately resolves these limited Python constructs with
the standard-library AST and checks every expected location and encoding.
The resolver is specific to this fixture; it is not a general navigation tool.

Each location contains a relative `path`, one-based `line`, one-based Unicode
scalar `column`, and exclusive `end_column`. `utf8_column` and
`utf8_end_column` preserve one-based byte coordinates for auditing `rg` and AST
output. `lsp.start` and `lsp.end` use zero-based lines and UTF-16 code units.
For example, `astral_primary` in `pkg/consumer.py` is at human line 7,
scalar column 29, byte column 34, and LSP `{line: 6, character: 29}`.
The satellite emoji is one scalar and two UTF-16 units; its variation selector
is another scalar. This distinction makes an accidental byte/scalar/UTF-16
substitution observable.

`queries[].at` names a location at which to query, and `queries[].symbol`
states its independently audited binding (null for the comment).
`symbols[].definition` and `scenarios[].references` point to location IDs.
The helper `expected(query, scenario)` returns the complete normalized answer;
`ORACLE` is available to an adapter importing this file via `importlib`.

## Saved-file changes and worktree separation

Create two independent roots and register each through the broker's operator
configuration. These copies are acceptance workspace identities; they do not
need to be Git worktrees to exercise distinct canonical-root worker keys.

```sh
python3 scripts/fixtures/nas-lsp/corpus.py validate
python3 scripts/fixtures/nas-lsp/corpus.py prepare
```

`prepare` prints the roots, corpus digest, and oracle path. Keep the same broker
and worker alive while testing root A in this order:

1. Query all twelve tasks at `base` in both roots.
2. Run `python3 scripts/fixtures/nas-lsp/corpus.py stage ROOT_A changed`.
   The first consumer call now binds to `secondary.render`. Query all twelve
   tasks again. Primary references decrease from seven to six; secondary
   references increase from two to three, including their declarations.
3. Run `python3 scripts/fixtures/nas-lsp/corpus.py stage ROOT_A deleted`.
   This retains the changed call and deletes `pkg/transient.py`. Primary
   references decrease to five. Query all twelve tasks again.
4. Query root B after each mutation. It must retain the base answers and its
   own worker identity, including the transient file.

The stage helper changes saved files in place and refuses to revert a fixture.
It requires a digest-matching fixture marker. `materialize(root)` refuses an
existing destination. The script intentionally leaves prepared roots available
for the caller to inspect and clean up. `validate` uses temporary roots and
checks that A's mutations leave every Python byte in B unchanged.

The correctness threshold is **100% exact location sets for every fixed task
in every tested scenario and root**; zero-result queries must explicitly
succeed with an empty answer. Indexing/unavailable/stale answers cannot count
as successful empty answers. Cold/warm server latency, resource limits,
response byte limits, cancellation and failure statuses belong to the broker
acceptance runner; this fixture alone establishes none of those properties.

## Reproducible rg plus file-read baseline

```sh
python3 scripts/fixtures/nas-lsp/corpus.py baseline --rounds 2 \
  --output /tmp/nas-lsp-baseline.json
```

For each task this runs real `rg` with line/byte-column output for the queried
spelling, reads the complete unique Python files returned by the search, then
uses the fixture-only AST resolver to produce the answer. Searching `render`
deliberately returns both unrelated definitions, references and prose mentions;
the subsequent reads disambiguate them. The answer is compared with the fixed
oracle. A mismatch exits nonzero. It does not model a human or LLM solving the
task, and it does not call the oracle to decide the source binding.

The JSON receipt includes every task's wall latency, search bytes, complete
file-read bytes, compact answer bytes and their total. The timer includes the
`rg` subprocess, reads, fixture resolution and answer serialization. Latency
summaries use nearest-rank p50/p95. The first/repeated passes share OS caches;
they must not be called cold/warm language-server measurements. Baseline
output bytes include all three outputs, without shell framing. `ceil(bytes/4)`
is clearly labeled as a token estimate; no tokenizer or model token usage is
measured. `baseline-record.json` records one local run's summaries, with full
per-task rows available by rerunning the command. Performance thresholds and
broker comparisons must be fixed in the design before drawing success claims.

The external acceptance target is operator-managed Pyrefly 1.2.0 with
`lsp --indexing-mode lazy-blocking --threads 1`; this directory neither
installs that binary nor treats prior server qualification as current proof.
