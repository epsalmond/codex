# Shake build identity

Shake's feature version (`0.4.0`, for example) changes when the feature set
changes. A separate build revision identifies each release build and retry.
Neither number is chosen by a model.

## What a running client shows

- Startup: `Shake v0.4.0`, with `CLI build: 1234.2` on the next line.
- `/status`: `CLI build: Shake 0.4.0 · 1234.2` and the complete `CLI release`
  tag, wrapped without removing characters.
- `codex-shake --version`: upstream package version, Shake feature version,
  CLI build revision, full source commit, and exact release tag.
- `codex-shake -V`: the upstream-compatible `codex-cli X.Y.Z` line.

These values are compiled into the executable. Updating the install root's
`current` symlink cannot change an already running CLI's identity. A new
invocation loads the newly installed CLI.

The labels deliberately identify the **CLI**. A client may connect to an older
local or remote app-server daemon. Its existing server-version display describes
the server's upstream version; this change does not establish its Shake build
revision or replace a running daemon. An installed update is not proof that an
existing backend session has changed builds.

## Allocation and source order

CI uses GitHub's workflow `run_number` and `run_attempt`: build `1234.2` means
run 1234, attempt 2 of `fork-release`. A new run increments the run number; a
retry increments the attempt. Both targets in the release matrix receive the
same prepared identity. Use **Re-run all jobs** for a fresh release attempt:
rerunning only failed jobs can retain a successful target binary from an older
attempt. Build and publish reject that mixed-attempt case before proceeding.
Even a recovery triggered from an older tag validates that tag's source, then produces a fresh build tag. Assets are created once;
the publisher never clobbers existing assets.

Homebrew's generated numeric formula version appends the decimal source
sequence, run number, and attempt after the upstream version and source
timestamp (for example `0.163.0.20261021000000.10418.1234.2`). This makes
same-source rebuilds and retries visible to `brew upgrade` without putting a
hexadecimal SHA into the version. Legacy tags keep their previous formula
version output.

The release tag keeps its existing shape:

```text
local-features-v<upstream>-main-r<source-UTC-timestamp>.<hex-identity>
```

The new hexadecimal identity is exactly 52 characters:

| Characters | Meaning |
| --- | --- |
| 1–16 | First-parent source commit count, padded to 16 hex digits |
| 17–28 | First 12 characters of the exact source SHA |
| 29–44 | Workflow run number, padded to 16 hex digits |
| 45–52 | Workflow retry attempt, padded to 8 hex digits |

Source UTC timestamps keep second precision in CI. Selection retains the
existing ordering: normalized source timestamp, timestamp precision, source
sequence, then rebuild number and attempt. New builds from an older source do
not displace a newer source. The increasing build revision is scoped to this
workflow and is not an upstream semantic version. Recreating/renaming the
workflow with reset GitHub run counters needs an explicit identity migration.

Homebrew and Debian package versions encode source sequence, build number, and
attempt as decimal components so package upgrades follow numeric rebuild order.
Legacy package version rendering is unchanged.

Legacy tags with 28-character source identities, timestamp-plus-SHA identities,
and older bare SHA suffixes remain readable and updateable. A legacy compiled
build can show a source revision or its release tag when it has no rebuild
number. Legacy metadata cannot retroactively identify overwritten bytes.

The first new-format release must advance the source timestamp beyond the
last legacy release. Pre-change clients do not decode the 52-character source
sequence; at the exact same timestamp they can rank an old 28-character tag
above a new tag. A later source timestamp lets their existing timestamp
comparison select the migration release. The version verifier also prevents
publishing a new identity from historical source that lacks this support.

## Local builds

`scripts/shake-local-install.sh` shares the allocation, version verification,
and package helper with CI. Each invocation allocates `local N` from a persistent
per-host counter in `$XDG_STATE_HOME/codex-shake/local-build-counter` (default
`~/.local/state/codex-shake/local-build-counter`), independent of the install
root. The optional `CODEX_SHAKE_BUILD_COUNTER` override selects another
persistent file. Allocation
is serialized with a process lock and an atomic counter replacement on Linux
and macOS. Preserve this counter across builds. Failed builds may leave gaps.

Local tags encode that counter in characters 29–44 and a zero attempt in 45–52.
The build directory is locked through packaging and cleanup. A simultaneous
invocation using the same directory exits with a `--work-dir` hint; use a
different directory when intentionally building the same source concurrently.
Their source UTC timestamp retains the existing minute precision, so the
official CI release for the same source ranks above a local build. Local
counters are separate namespaces on different hosts and are not comparable
with CI run numbers. Rebuilding the same source locally produces a new tag,
package directory, and installation path. `--dry-run` previews the next counter
without reserving it, so simultaneous dry runs may print the same preview.

The default source/build cache remains
`$XDG_CACHE_HOME/codex-shake-local/<source-sha12>` (default
`~/.cache/codex-shake-local/<source-sha12>`), independent of the fresh tag.
A failed build retains its worktree and Cargo cache; rerunning the command
resumes them with a new build identity. `--work-dir` overrides this location.
Cached worktrees must still match the source SHA apart from generated workspace
and lockfile version stamps; unexpected edits are preserved and stop the build.
Each invocation repackages the newly verified binary under its new tag. Successful
builds remove the source worktree unless `--keep-worktree` is set.

Build from source that includes this identity support: the shared verifier
rejects binaries whose reported identity does not match the prepared tag and
source SHA. This applies to historical source commits that lack the new
compiled identity code as well.

See [update changelog behavior](shake-update-changelog.md) for the changes shown
when installing, including skipped releases and unavailable baselines.
