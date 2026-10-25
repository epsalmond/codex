# NAS Rust navigation corpus

This is a broker-neutral saved-source corpus and an independent static oracle.
It neither starts a language server nor registers a broker or installs tools.
`workspace/` is an ordinary standalone dependency-free Cargo library with
local modules, edition 2021, and `#![no_std]`. It needs no `rust-project.json`.
Calls use imported modules and qualified `primary::render`/`secondary::render`
paths. There are no macros, conditional modules, generated code, build scripts,
features, dependencies, alternate targets, or external inputs. The deleted
scenario removes both `src/transient.rs` and its `mod` declaration, keeping
the remaining package graph valid. This corpus does not qualify std semantics,
Cargo workspaces, proc macros or dependency-heavy projects.

## Exact tasks and coordinates

`oracle.json` records fourteen fixed tasks, beginning with references at the
primary declaration. Run this first on a fresh worker before any consumer query;
there is no caller-side consumer opening prerequisite. The broker may admit
saved documents according to its own protocol. Cold secondary references need
a separate fresh worker if measuring them independently. Warm tasks include
cross-file definitions, references from declarations and uses, unrelated
same-named functions, all three repeated calls on one line, an astral Unicode
prefix, and successful empty definitions/references inside a comment.

Each location has a relative `path`, one-based `line` and Unicode scalar
`column`, exclusive `end_column`, one-based UTF-8 byte columns, and zero-based
LSP UTF-16 start/end coordinates. Definition ranges cover `render` alone.
References **include the declaration**. Compare full deduplicated sets with
order ignored; indexing, unavailable or stale responses do not pass empty tasks.
The acceptance threshold is 100% exact sets and zero cross-root leakage.

The primary/secondary reference counts are **8/2** at `base`, **7/3** at
`changed` (the first call retargets to secondary), and **6/3** at `deleted`
(the changed call is retained and the transient consumer is removed). The
`first_secondary` location audits the changed call even though no fixed query
lands there. Every remaining query is stable through those source changes.

`corpus.py validate` derives bindings from fixture source declarations and
explicit namespace imports with a deliberately limited text scanner. The
scanner does not read oracle reference/binding lists or language-server output.
It is not a production parser or general navigation implementation. It compares
complete inventories against the static JSON and separately audits identifier
slices and scalar/UTF-8/UTF-16 conversions. The source contains no shadowing,
reexports or strings/comments resembling qualified call expressions.

## Adapter API and linked worktree isolation

Import `corpus.py` through `importlib.util.spec_from_file_location`, or use its
CLI. The only Python runtime dependencies are the standard library; the CLI
uses existing `git` and `rg`. It never invokes npm, Cargo or a language server.

```python
materialize(root, variant="base")  # root must not already exist
set_scenario(root, "changed")      # only base -> changed -> deleted
query_location(query, variant="base")
expected(query, scenario="base", variant="base")
ORACLE["queries"]                 # fixed order; cold references first
```

`expected` and `query_location` return fresh dictionaries. `variant="shifted"`
prepends a comment and blank line to every source, moving all expected line
numbers by two. It deliberately makes same-named files and symbols occupy
different exact locations in the second root.

```sh
python3 scripts/fixtures/nas-lsp-rust/corpus.py validate
python3 scripts/fixtures/nas-lsp-rust/corpus.py prepare
python3 scripts/fixtures/nas-lsp-rust/corpus.py stage ROOT_A changed
python3 scripts/fixtures/nas-lsp-rust/corpus.py stage ROOT_A deleted
```

`prepare` creates a temporary control Git repository and two actual linked
Git worktrees: A/base and B/shifted. It prints their canonical roots, variants,
common Git directory, oracle path and corpus digest. The temporary seed commit
uses the configured Git identity, with signing and hooks disabled. No existing
repository is mutated. Register each root separately and keep the broker alive
while advancing A; query B at base after each change to A. Its bytes and shifted
answers must stay unchanged. The selfcheck also advances B through all scenarios.
Prepared roots remain available for inspection. The helper does not clean them
up; `validate` and `baseline` use their own temporary directories.

Scenario mutation requires the digest-matching fixture marker and refuses a
backward transition. All inputs are saved files. Configuration/model edits,
readiness, cancellation, quotas and process safety belong to adapter acceptance,
not this source oracle.

## Actual rg plus file-read baseline

```sh
python3 scripts/fixtures/nas-lsp-rust/corpus.py baseline --rounds 2 \
  --output /tmp/rust-baseline.json
```

For every task/scenario/variant, the baseline actually executes `rg` with line
and byte-column output, reads every complete unique candidate source file, and
uses the source scanner to resolve the queried position. The query's oracle
symbol is not used to resolve its answer. It records subprocess/read/resolution
latency, search-output bytes, full file-read bytes, compact answer bytes and
their total. `render` searches deliberately include both unrelated symbols and
prose mentions; reads disambiguate them. Every answer must match the fixed
oracle or the command fails. This is a fixture-only reconstruction cost, not
a claim about human/LLM problem solving or language-server output.

`baseline-record.json` contains one actual run's twelve grouped summaries;
rerun the command for full task rows. P50/P95 use nearest rank. First/repeated
passes share OS caches and are not cold/warm language-server measurements.
`ceil(UTF-8 bytes / 4)` is explicitly an estimated token count, with no tokenizer
or model usage measured. Language-server latency, output caps, server versions
and resource acceptance remain separate integration evidence.

## Negative recipes

`negative-cases.json` gives saved-file patches to apply one at a time to a fresh
materialized root. These are reproducible adapter test inputs, not passed broker
tests. TypeScript cases exercise JSDoc/triple-slash path escape, inherited config
escape and nested plugins. Rust cases exercise an orphan source and unsafe
path/include/environment/cfg_attr directives. Only cases for this language are
included. The orphan case intentionally leaves the exact rejection/indexing
status to the adapter contract; it must not be counted as a successful empty or
incomplete reference answer. Do not run repository sentinels to apply recipes.
