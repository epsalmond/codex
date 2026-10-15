#!/usr/bin/env bash
# Print shell exports that point a local Cargo build at Codex-built rusty_v8 artifacts.
#
#   eval "$(scripts/rusty-v8-env.sh)"
#
# codex-code-mode-runtime enables the v8 crate's `v8_enable_sandbox` feature, so
# v8's build.rs downloads `librusty_v8_ptrcomp_sandbox_release_<target>.a.gz`
# from denoland/rusty_v8. That project does not publish sandbox builds, so the
# download 404s. Codex publishes them on its own `rusty-v8-v<version>` release,
# which CI uses via .github/actions/setup-rusty-v8. This script does the same for
# local builds: it fetches the archive and generated binding into a per-version
# cache when missing, verifies both against the hashes pinned in
# third_party/v8/rusty_v8_<version>.sha256, and prints RUSTY_V8_ARCHIVE and
# RUSTY_V8_SRC_BINDING_PATH.
#
# Environment:
#   RUSTY_V8_CACHE_DIR  cache root (default: ${XDG_CACHE_HOME:-$HOME/.cache}/rusty_v8)
#   RUSTY_V8_TARGET     Rust target triple (default: rustc host triple)
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

die() {
  echo "rusty-v8-env: $*" >&2
  exit 1
}

version=$(python3 "$repo_root/.github/scripts/rusty_v8_bazel.py" resolved-v8-crate-version) \
  || die "could not resolve the v8 crate version from codex-rs/Cargo.lock"
target=${RUSTY_V8_TARGET:-$(rustc -vV | sed -n 's/^host: //p')}
[[ -n "$target" ]] || die "could not determine the Rust host target; set RUSTY_V8_TARGET"
[[ "$target" != *-pc-windows-msvc ]] || die "Windows targets are not supported by this script"

profile=ptrcomp_sandbox_release
archive_name="librusty_v8_${profile}_${target}.a.gz"
binding_name="src_binding_${profile}_${target}.rs"
pinned="$repo_root/third_party/v8/rusty_v8_${version//./_}.sha256"
base_url="https://github.com/openai/codex/releases/download/rusty-v8-v${version}"
cache_dir="${RUSTY_V8_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/rusty_v8}/$version"

[[ -f "$pinned" ]] || die "no pinned checksums at $pinned"

expected_sha() {
  local sha
  sha=$(awk -v name="$1" '$2 == name { print $1 }' "$pinned")
  [[ -n "$sha" ]] || die "$pinned has no checksum for $1"
  echo "$sha"
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

# Download $1 into the cache unless a verified copy is already there.
ensure_artifact() {
  local name=$1 dest="$cache_dir/$1" expected tmp
  expected=$(expected_sha "$name")
  if [[ -f "$dest" && "$(sha256_of "$dest")" == "$expected" ]]; then
    return
  fi
  mkdir -p "$cache_dir"
  tmp=$(mktemp "$cache_dir/.${name}.XXXXXX")
  echo "rusty-v8-env: downloading $base_url/$name" >&2
  if ! curl -fsSL --retry 3 "$base_url/$name" -o "$tmp"; then
    rm -f "$tmp"
    die "download failed: $base_url/$name"
  fi
  local actual
  actual=$(sha256_of "$tmp")
  if [[ "$actual" != "$expected" ]]; then
    rm -f "$tmp"
    die "checksum mismatch for $name: expected $expected, got $actual"
  fi
  chmod 644 "$tmp"
  mv -f "$tmp" "$dest"
}

ensure_artifact "$archive_name"
ensure_artifact "$binding_name"

printf 'export RUSTY_V8_ARCHIVE=%q\n' "$cache_dir/$archive_name"
printf 'export RUSTY_V8_SRC_BINDING_PATH=%q\n' "$cache_dir/$binding_name"
