#!/usr/bin/env bash
# Build steps shared by .github/workflows/fork-release.yml,
# .github/scripts/fork-release-metadata.sh, and scripts/shake-local-install.sh,
# so a local codex-shake build stamps, verifies, and packages exactly like a
# published release.
#
#   fork-release-build.sh build-identity --repo-root DIR --sha SHA --version X.Y.Z \
#     --kind ci --run-number N --run-attempt N
#   fork-release-build.sh workspace-version --repo-root DIR --sha SHA
#   fork-release-build.sh stable-lineage --repo-root DIR --sha SHA \
#     [--upstream-tag-namespace REFPREFIX]
#   fork-release-build.sh stamp-version VERSION       (run in codex-rs/)
#   fork-release-build.sh verify-version VERSION TAG SHA  (run in codex-rs/)
#   fork-release-build.sh package-archive --repo-root DIR --target TRIPLE \
#     --version VERSION --stage-dir DIR --archive FILE [--bwrap-bin PATH]
#                                                     (run in codex-rs/)
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

die() {
  printf 'fork-release-build: %s\n' "$1" >&2
  exit 1
}

# Print the [workspace.package] version recorded at SHA.
workspace_version() {
  local repo_root="" sha=""
  while (($# > 0)); do
    case "$1" in
      --repo-root) repo_root=${2:?--repo-root requires a value}; shift 2 ;;
      --sha) sha=${2:?--sha requires a value}; shift 2 ;;
      *) die "workspace-version: unknown argument: $1" ;;
    esac
  done
  [[ -n "$repo_root" && -n "$sha" ]] || die "workspace-version needs --repo-root and --sha"
  git -C "$repo_root" show "$sha:codex-rs/Cargo.toml" |
    awk '
      /^\[workspace\.package\]$/ { in_workspace = 1; next }
      /^\[/ { in_workspace = 0 }
      in_workspace && /^version = / { print; exit }
    ' |
    sed -n 's/^version = "\([^"]*\)".*/\1/p'
}

# Print the newest exact stable rust-vX.Y.Z tag merged into SHA, or "none".
# Upstream tags fetched under the namespace (default
# refs/tags/fork-release-upstream, where fork-release-metadata.sh puts them)
# count the same as plain local tags. scripts/shake-local-install.sh passes a
# namespace outside refs/tags so `git push --tags` never sends them.
stable_lineage() {
  local repo_root="" sha="" namespace=refs/tags/fork-release-upstream lineage
  while (($# > 0)); do
    case "$1" in
      --repo-root) repo_root=${2:?--repo-root requires a value}; shift 2 ;;
      --sha) sha=${2:?--sha requires a value}; shift 2 ;;
      --upstream-tag-namespace)
        namespace=${2:?--upstream-tag-namespace requires a value}
        namespace=${namespace%/}
        shift 2
        ;;
      *) die "stable-lineage: unknown argument: $1" ;;
    esac
  done
  [[ -n "$repo_root" && -n "$sha" ]] || die "stable-lineage needs --repo-root and --sha"
  lineage=$(
    git -C "$repo_root" for-each-ref --merged "$sha" --format='%(refname)' refs/tags "$namespace" |
      while IFS= read -r ref; do
        case "$ref" in
          ("$namespace"/rust-v*) tag=${ref#"$namespace"/} ;;
          (refs/tags/rust-v*) tag=${ref#refs/tags/} ;;
          (*) continue ;;
        esac
        if [[ "$tag" =~ ^rust-v([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
          printf '%s\t%s\t%s\t%s\n' \
            "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" "${BASH_REMATCH[3]}" "$tag"
        fi
      done |
      LC_ALL=C sort -t $'\t' -k1,1n -k2,2n -k3,3n -k4,4 |
      tail -n 1 |
      cut -f4-
  )
  printf '%s\n' "${lineage:-none}"
}

# Like upstream main, the branch keeps the workspace at 0.0.0 so version
# snapshots stay stable; stamp the release version into codex-rs/Cargo.toml so
# `codex --version`, the updater, and the package metadata all agree.
stamp_version() {
  local source_version=${1:?stamp-version needs VERSION} current
  [[ "$source_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
  current=$(awk '
    /^\[workspace\.package\]$/ { in_workspace = 1; next }
    /^\[/ { in_workspace = 0 }
    in_workspace && /^version = / { print; exit }
  ' Cargo.toml | sed -n 's/^version = "\([^"]*\)".*/\1/p')
  if [[ "$current" == 0.0.0 ]]; then
    sed -i.bak "/^\[workspace\.package\]\$/,/^\[/ s/^version = \"0\.0\.0\"\$/version = \"$source_version\"/" Cargo.toml
    rm -f Cargo.toml.bak
  elif [[ "$current" != "$source_version" ]]; then
    echo "Cargo.toml workspace version $current does not match $source_version" >&2
    exit 1
  fi
  grep -Fxq "version = \"$source_version\"" Cargo.toml
}

verify_version() {
  local source_version=${1:?verify-version needs VERSION} release_tag=${2:?verify-version needs TAG}
  local source_sha=${3:?verify-version needs SHA}
  local binary=${CODEX_VERSION_BINARY:-target/release/codex}
  local shake_version reported expected short_reported build_revision
  build_revision=$(python3 "$script_dir/fork-build-identity.py" describe "$release_tag")
  shake_version=$(sed -n 's/^[[:space:]]*pub(crate) const SHAKE_FEATURE_VERSION: &str = "\([^"]*\)";$/\1/p' tui/src/fork_update.rs)
  test -n "$shake_version"
  reported=$("$binary" --version)
  expected=$(printf 'codex-cli %s\nShake feature version: %s\nCLI build: %s\nSource commit: %s\nRelease tag: %s' \
    "$source_version" "$shake_version" "$build_revision" "$source_sha" "$release_tag")
  [[ "$reported" == "$expected" ]] || {
    echo "codex --version reported '$reported', expected source, Shake, and release versions" >&2
    exit 1
  }
  short_reported=$("$binary" -V)
  [[ "$short_reported" == "codex-cli $source_version" ]] || {
    echo "codex -V reported '$short_reported', expected codex-cli $source_version" >&2
    exit 1
  }
}

# Build the canonical Codex package layout (codex-package.json, bin/,
# codex-path/rg, codex-resources/) that the app-server daemon needs to seed its
# managed install, then add the Shake extras at the package root. Root-level
# codex and codex-code-mode-host symlinks keep older install.sh copies, the
# .deb builder, and the Homebrew formula working; codex canonicalizes to
# bin/codex so the layout is detected. The payload check makes an incomplete
# archive fail before it can be published or installed.
package_archive() {
  local repo_root="" target="" source_version="" stage="" archive="" bwrap_bin=""
  while (($# > 0)); do
    case "$1" in
      --repo-root) repo_root=${2:?--repo-root requires a value}; shift 2 ;;
      --target) target=${2:?--target requires a value}; shift 2 ;;
      --version) source_version=${2:?--version requires a value}; shift 2 ;;
      --stage-dir) stage=${2:?--stage-dir requires a value}; shift 2 ;;
      --archive) archive=${2:?--archive requires a value}; shift 2 ;;
      --bwrap-bin) bwrap_bin=${2:?--bwrap-bin requires a value}; shift 2 ;;
      *) die "package-archive: unknown argument: $1" ;;
    esac
  done
  [[ -n "$repo_root" && -n "$target" && -n "$source_version" && -n "$stage" && -n "$archive" ]] ||
    die "package-archive needs --repo-root, --target, --version, --stage-dir, and --archive"

  local package_args=(
    --target "$target"
    --variant codex
    --package-version "$source_version"
    --cargo-profile release
    --entrypoint-bin target/release/codex
    --code-mode-host-bin target/release/codex-code-mode-host
    --package-dir "$stage"
    --force
  )
  if [[ -n "$bwrap_bin" ]]; then
    package_args+=(--bwrap-bin "$bwrap_bin")
  fi
  python3 "$repo_root/scripts/build_codex_package.py" "${package_args[@]}"
  install -m 755 docs/shake-bench/scripts/codex-shake-estimate \
    docs/shake-bench/scripts/shake-savings-estimate.py "$stage/"
  install -m 644 docs/shake-bench/scripts/pricing_lib.py \
    docs/shake-bench/pricing.json "$stage/"
  ln -s bin/codex "$stage/codex"
  ln -s bin/codex-code-mode-host "$stage/codex-code-mode-host"
  tar -C "$stage" -czf "$archive" \
    codex-package.json bin codex-path codex-resources \
    codex codex-code-mode-host codex-shake-estimate \
    shake-savings-estimate.py pricing_lib.py pricing.json

  local required=(
    codex-package.json
    bin/codex
    bin/codex-code-mode-host
    codex-path/rg
    codex
    codex-code-mode-host
    codex-shake-estimate
    shake-savings-estimate.py
    pricing_lib.py
    pricing.json
  )
  # The package layout needs the bundled bubblewrap sandbox on Linux.
  if [[ "$target" == *linux* ]]; then
    required+=(codex-resources/bwrap)
  fi
  local listing entry
  listing=$(tar -tzf "$archive")
  for entry in "${required[@]}"; do
    grep -Fx "$entry" <<<"$listing" >/dev/null || {
      echo "$archive is missing $entry" >&2
      exit 1
    }
  done
}

command=${1:-}
[[ -n "$command" ]] || die "missing subcommand"
shift
case "$command" in
  build-identity) python3 "$script_dir/fork-build-identity.py" allocate "$@" ;;
  workspace-version) workspace_version "$@" ;;
  stable-lineage) stable_lineage "$@" ;;
  stamp-version) stamp_version "$@" ;;
  verify-version) verify_version "$@" ;;
  package-archive) package_archive "$@" ;;
  *) die "unknown subcommand: $command" ;;
esac
