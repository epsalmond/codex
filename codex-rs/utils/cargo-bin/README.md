# codex-utils-cargo-bin runfiles strategy

We disable the directory-based runfiles strategy and rely on the manifest
strategy across all platforms. This avoids Windows path length issues and keeps
behavior consistent in local and remote builds on all platforms. Bazel sets
`RUNFILES_MANIFEST_FILE`, and the `codex-utils-cargo-bin` helpers use the
`runfiles` crate to resolve runfiles via that manifest.

Function behavior:
- `cargo_bin`: reads `CARGO_BIN_EXE_*` environment variables (set by Cargo or
  Bazel) and resolves them via the runfiles manifest when `RUNFILES_MANIFEST_FILE`
  is present. When not under runfiles, it only accepts absolute paths from
  `CARGO_BIN_EXE_*` and returns an error otherwise.
- `workspace_bin`: for tests that need a binary from another workspace package.
  Cargo does not export `CARGO_BIN_EXE_*` for those, and `cargo test -p <pkg>`
  does not build them, so the fallback is the uplifted `target/<profile>/<name>`.
  That file may be missing, or, with a `CARGO_TARGET_DIR` shared between
  checkouts, built from another checkout. `workspace_bin` panics with the exact
  `cargo build -p <package> --bin <name>` command when the binary is missing,
  when its Cargo dep-info (`<name>.d`) lists no sources from this checkout, or
  when one of those sources is newer than the binary. An explicit
  `CARGO_BIN_EXE_<name>` (Cargo, Bazel, or the caller) is trusted as-is.
- `find_resource!`: used by tests to locate fixtures. It chooses the Bazel
  runfiles resolution path when `RUNFILES_MANIFEST_FILE` is set, otherwise it
  falls back to a `CARGO_MANIFEST_DIR`-relative path for Cargo runs.

Background:
- https://bazel.build/docs/runfiles
- https://bazel.build/docs/runfiles#runfiles-manifest
