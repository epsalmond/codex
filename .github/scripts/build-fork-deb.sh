#!/usr/bin/env bash
# Build the codex-shake .deb package from a published fork-release Linux
# tarball.
#
#   build-fork-deb.sh <linux-tarball> <release-tag> <source-version> <out-dir>
#
# <linux-tarball>  codex-x86_64-unknown-linux-gnu.tar.gz produced by the
#                  build job (codex, codex-code-mode-host, and optionally the
#                  offline savings estimator payload).
# <release-tag>    e.g. local-features-v0.154.0-main-r20260914110830.<sha>
#                  or local-features-v0.154.0-r202609140816.<sha>. The digits
#                  after "-r" are the counter and the trailing hex is the sha;
#                  both feed the Debian version.
# <source-version> workspace Cargo version, e.g. 0.154.0.
# <out-dir>        directory to place the built .deb into.
#
# Emits the built package path on stdout as: deb_path=<path>
#
# Env:
#   FORK_DEB_SKIP_SHLIBDEPS=1   force the libc6 floor instead of running
#                               dpkg-shlibdeps (used by the offline test,
#                               whose stand-in binaries are plain shell
#                               scripts, not ELF).
set -euo pipefail
umask 022

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/../.." && pwd)

usage() {
  echo "usage: build-fork-deb.sh <linux-tarball> <release-tag> <source-version> <out-dir>" >&2
}

[[ $# -eq 4 ]] || { usage; exit 2; }

tarball=$1
release_tag=$2
source_version=$3
out_dir=$4

[[ -s "$tarball" ]] || { echo "build-fork-deb: missing tarball $tarball" >&2; exit 1; }

# Parse the counter and sha out of the release tag. Tags look like:
#   local-features-v0.154.0-main-r20260914110830.<12hex-or-more sha>
#   local-features-v0.154.0-r202609140816.<sha>
# The counter is the run of digits right after "-r"; the sha is the
# remainder after the following ".".
if [[ "$release_tag" =~ -r([0-9]{1,14})\.([0-9a-fA-F]+)$ ]]; then
  counter="${BASH_REMATCH[1]}"
  sha="${BASH_REMATCH[2]}"
else
  echo "build-fork-deb: could not parse counter/sha from release tag: $release_tag" >&2
  exit 1
fi

# Debian upstream version component may not contain '~' being used here
# (we deliberately use '+', never '~': '~' sorts *before* the empty suffix,
# so "0.154.0~foo" would be considered OLDER than plain "0.154.0" -- the
# opposite of what we want for a fork build that must always sort newer than
# the bare upstream version).
deb_version="${source_version}+r${counter}.${sha}"

mkdir -p "$out_dir"
out_dir=$(cd -- "$out_dir" && pwd)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

pkg_root="$work/pkg"
payload_dir="$pkg_root/usr/lib/codex-shake"
install -d -m 0755 "$payload_dir" "$pkg_root/usr/bin" \
  "$pkg_root/usr/share/doc/codex-shake" "$pkg_root/DEBIAN"

# Unpack the tarball flat into the payload dir. The tarball layout is a flat
# set of files at its root (codex, codex-code-mode-host, and optionally the
# offline savings estimator payload).
tar -xzf "$tarball" -C "$payload_dir"

[[ -f "$payload_dir/codex" ]] || { echo "build-fork-deb: tarball missing codex" >&2; exit 1; }
[[ -f "$payload_dir/codex-code-mode-host" ]] || {
  echo "build-fork-deb: tarball missing codex-code-mode-host" >&2
  exit 1
}
chmod 0755 "$payload_dir/codex" "$payload_dir/codex-code-mode-host"

estimator_present=0
if [[ -f "$payload_dir/codex-shake-estimate" && \
      -f "$payload_dir/shake-savings-estimate.py" && \
      -f "$payload_dir/pricing_lib.py" ]]; then
  estimator_present=1
  chmod 0755 "$payload_dir/codex-shake-estimate"
fi

# /usr/bin/codex-shake wrapper. Deliberately exports nothing shake-specific
# (in particular, never CODEX_SHAKE_HOME) -- the deb payload is a fixed,
# root-owned location, not a user-managed install directory.
cat > "$pkg_root/usr/bin/codex-shake" <<'WRAPPER'
#!/bin/sh
dir=/usr/lib/codex-shake
PATH="$dir:$PATH" exec "$dir/codex" "$@"
WRAPPER
chmod 0755 "$pkg_root/usr/bin/codex-shake"

if [[ "$estimator_present" -eq 1 ]]; then
  cat > "$pkg_root/usr/bin/codex-shake-estimate" <<'WRAPPER'
#!/bin/sh
dir=/usr/lib/codex-shake
exec "$dir/codex-shake-estimate" "$@"
WRAPPER
  chmod 0755 "$pkg_root/usr/bin/codex-shake-estimate"
fi

# Doc/copyright.
cat > "$pkg_root/usr/share/doc/codex-shake/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: codex-shake
Source: https://github.com/epsalmond/codex

Files: *
Copyright: OpenAI and codex-shake (epsalmond/codex) contributors
License: Apache-2.0

License: Apache-2.0
 See /usr/share/doc/codex-shake/LICENSE, or
 https://www.apache.org/licenses/LICENSE-2.0
EOF
if [[ -f "$repo_root/LICENSE" ]]; then
  install -m 0644 "$repo_root/LICENSE" "$pkg_root/usr/share/doc/codex-shake/LICENSE"
fi
cat > "$pkg_root/usr/share/doc/codex-shake/README" <<EOF
codex-shake is a fork build of Codex CLI (eric/local-features,
epsalmond/codex) installed as /usr/bin/codex-shake, beside your official
codex install. It is not affiliated with, and does not overwrite, an
existing "codex" binary.

Run: codex-shake
Release: $release_tag
EOF
gzip -n -9 "$pkg_root/usr/share/doc/codex-shake/README"

# glibc floor: prefer the ldd-derived value from the build job when present
# next to the tarball, else derive it from this runner's own ldd (the deb is
# built on the same publish runner class as the Linux build job).
glibc_version=""
glibc_file="$(dirname -- "$tarball")/glibc-version.txt"
if [[ -s "$glibc_file" ]]; then
  glibc_version=$(sed -n '1p' "$glibc_file")
fi
if [[ -z "$glibc_version" ]]; then
  echo "build-fork-deb: missing glibc-version.txt next to the tarball" >&2
  exit 1
fi

runner_glibc=$(ldd --version | awk 'NR == 1 { print $NF }')
if [[ "$runner_glibc" != "$glibc_version" ]]; then
  echo "build-fork-deb: builder runner glibc ($runner_glibc) != recorded build glibc ($glibc_version)" >&2
  exit 1
fi

# Depends: computed via dpkg-shlibdeps against a scratch source-format
# debian/control, matching oshioki's packaging/build-deb approach.
depends="libc6 (>= $glibc_version)"
skip_shlibdeps="${FORK_DEB_SKIP_SHLIBDEPS:-0}"

is_elf() {
  [[ "$(head -c4 "$1" 2>/dev/null | od -An -tx1 | tr -d ' \n')" == "7f454c46" ]]
}

if [[ "$skip_shlibdeps" != "1" ]] && command -v dpkg-shlibdeps >/dev/null 2>&1 \
  && is_elf "$payload_dir/codex"; then
  scan_dir="$work/scan"
  mkdir -p "$scan_dir/debian"
  printf 'Source: codex-shake\nMaintainer: Eric Psalmond <epsalmond@gmail.com>\n\nPackage: codex-shake\nArchitecture: any\n' \
    > "$scan_dir/debian/control"
  shlib_log="$work/shlib-errors.log"
  shlib_targets=("$payload_dir/codex" "$payload_dir/codex-code-mode-host")
  shlib_depends="$(
    cd "$scan_dir" && dpkg-shlibdeps -O "${shlib_targets[@]}" 2>"$shlib_log" \
      | sed -n 's/^shlibs:Depends=//p'
  )" || {
    cat "$shlib_log" >&2
    echo "build-fork-deb: dpkg-shlibdeps failed, falling back to libc6 floor" >&2
    shlib_depends=""
  }
  if [[ -n "$shlib_depends" ]]; then
    depends="$shlib_depends"
    if ! printf '%s' "$depends" | grep -Eq '(^|, )libc6( |,|\(|$)'; then
      depends="$depends, libc6 (>= $glibc_version)"
    fi
  fi
else
  echo "build-fork-deb: using libc6 floor (shlibdeps skipped or non-ELF payload)" >&2
fi

cat > "$pkg_root/DEBIAN/control" <<EOF
Package: codex-shake
Version: $deb_version
Architecture: amd64
Section: utils
Priority: optional
Maintainer: Eric Psalmond <epsalmond@gmail.com>
Homepage: https://github.com/epsalmond/codex
Depends: $depends
Description: eric/local-features fork build of Codex CLI, installed as codex-shake
 Installs the eric/local-features test build of Codex CLI (epsalmond/codex)
 as /usr/bin/codex-shake, alongside an existing official "codex" install.
 It never overwrites or otherwise touches an official codex install.
EOF

(cd "$pkg_root" && find . -type f ! -path './DEBIAN/*' -exec md5sum {} + \
  | sed 's| \./| |' > DEBIAN/md5sums)

deb_name="codex-shake_${deb_version}_amd64.deb"
deb_path="$out_dir/$deb_name"
# dpkg-deb reports progress on stdout; keep stdout reserved for deb_path=.
dpkg-deb --build --root-owner-group "$pkg_root" "$deb_path" >&2

echo "deb_path=$deb_path"
