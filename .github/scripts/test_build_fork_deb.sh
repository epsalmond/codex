#!/usr/bin/env bash
# Offline test for build-fork-deb.sh. Builds a fake tarball with tiny
# shell-script stand-ins for the binaries (not ELF, so dpkg-shlibdeps has
# nothing to scan -- exercised via FORK_DEB_SKIP_SHLIBDEPS=1 and via the
# builder's own ELF detection) and verifies the resulting .deb's control
# fields, payload paths, and wrapper content.
#
# Skips (exit 0) with a clear message when dpkg-deb is unavailable, e.g. on
# macOS. Runs fully on Linux CI, where dpkg-dev is preinstalled on the
# ubuntu-22.04 publish runner.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
builder="$script_dir/build-fork-deb.sh"

if ! command -v dpkg-deb >/dev/null 2>&1; then
  echo "test_build_fork_deb: dpkg-deb not available on this platform (e.g. macOS) -- skipping" >&2
  exit 0
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

stage="$work/stage"
mkdir -p "$stage"

write_stub() {
  local path=$1
  cat > "$path" <<'EOF'
#!/bin/sh
echo "codex-shake-stub 0.154.0"
EOF
  chmod 0755 "$path"
}

write_stub "$stage/codex"
write_stub "$stage/codex-code-mode-host"
write_stub "$stage/codex-shake-estimate"
cat > "$stage/shake-savings-estimate.py" <<'EOF'
# stand-in
EOF
cat > "$stage/pricing_lib.py" <<'EOF'
# stand-in
EOF
cat > "$stage/pricing.json" <<'EOF'
{}
EOF

tarball="$work/codex-x86_64-unknown-linux-gnu.tar.gz"
tar -C "$stage" -czf "$tarball" \
  codex codex-code-mode-host codex-shake-estimate \
  shake-savings-estimate.py pricing_lib.py pricing.json

# glibc-version.txt must sit next to the tarball; use whatever this host's
# ldd reports so the builder's ldd-consistency assertion passes (it compares
# against the runner's own ldd, exactly like on the ubuntu-22.04 publish
# runner).
if ! command -v ldd >/dev/null 2>&1; then
  echo "test_build_fork_deb: ldd not available -- skipping (non-Linux host without dpkg-deb parity)" >&2
  exit 0
fi
ldd --version | awk 'NR == 1 { print $NF }' > "$work/glibc-version.txt"

out_dir="$work/out"

release_tag="local-features-v0.154.0-main-r20260914110830.00000000000028b2ec76d3de4ab8"
source_version="0.154.0"

# Non-ELF stand-ins: force the libc6-floor fallback path explicitly (also
# exercised implicitly by the builder's own is_elf() check).
FORK_DEB_SKIP_SHLIBDEPS=1 bash "$builder" \
  "$tarball" "$release_tag" "$source_version" "$out_dir" > "$work/build-output.txt"

deb_path=$(sed -n 's/^deb_path=//p' "$work/build-output.txt")
[[ -s "$deb_path" ]] || { echo "test_build_fork_deb: no .deb produced" >&2; exit 1; }

expected_deb_version="0.154.0+r20260914110830.00000000000028b2ec76d3de4ab8"
expected_deb_name="codex-shake_${expected_deb_version}_amd64.deb"
[[ "$(basename "$deb_path")" == "$expected_deb_name" ]] || {
  echo "test_build_fork_deb: unexpected deb filename: $(basename "$deb_path") (want $expected_deb_name)" >&2
  exit 1
}

control="$(dpkg-deb -I "$deb_path" control)"
grep -Fq "Package: codex-shake" <<< "$control"
grep -Fq "Version: $expected_deb_version" <<< "$control"
grep -Fq "Architecture: amd64" <<< "$control"
grep -Fq "Section: utils" <<< "$control"
grep -Fq "Priority: optional" <<< "$control"
grep -Fq "Maintainer: Eric Psalmond <epsalmond@gmail.com>" <<< "$control"
grep -Fq "Homepage: https://github.com/epsalmond/codex" <<< "$control"
grep -Eq '^Depends: libc6 \(>= ' <<< "$control"

contents="$(dpkg-deb -c "$deb_path")"
grep -Fq "./usr/lib/codex-shake/codex" <<< "$contents"
grep -Fq "./usr/lib/codex-shake/codex-code-mode-host" <<< "$contents"
grep -Fq "./usr/lib/codex-shake/codex-shake-estimate" <<< "$contents"
grep -Fq "./usr/lib/codex-shake/shake-savings-estimate.py" <<< "$contents"
grep -Fq "./usr/lib/codex-shake/pricing_lib.py" <<< "$contents"
grep -Fq "./usr/bin/codex-shake" <<< "$contents"
grep -Fq "./usr/bin/codex-shake-estimate" <<< "$contents"
grep -Fq "./usr/share/doc/codex-shake/copyright" <<< "$contents"
grep -Fq "./usr/share/doc/codex-shake/README.gz" <<< "$contents"
grep -Fq "./DEBIAN" <<< "$contents" && true # DEBIAN itself must not appear in data.tar

extract_dir="$work/extract"
mkdir -p "$extract_dir"
dpkg-deb -x "$deb_path" "$extract_dir"

wrapper_content="$(cat "$extract_dir/usr/bin/codex-shake")"
grep -Fq '#!/bin/sh' <<< "$wrapper_content"
grep -Fq 'dir=/usr/lib/codex-shake' <<< "$wrapper_content"
grep -Fq 'PATH="$dir:$PATH" exec "$dir/codex" "$@"' <<< "$wrapper_content"
if grep -qi 'CODEX_SHAKE_HOME' <<< "$wrapper_content"; then
  echo "test_build_fork_deb: wrapper must not export CODEX_SHAKE_HOME" >&2
  exit 1
fi

estimate_wrapper_content="$(cat "$extract_dir/usr/bin/codex-shake-estimate")"
grep -Fq '#!/bin/sh' <<< "$estimate_wrapper_content"
grep -Fq 'dir=/usr/lib/codex-shake' <<< "$estimate_wrapper_content"
grep -Fq 'exec "$dir/codex-shake-estimate" "$@"' <<< "$estimate_wrapper_content"

[[ -f "$extract_dir/usr/lib/codex-shake/codex" ]]
[[ -x "$extract_dir/usr/lib/codex-shake/codex" ]]

# --- version ordering assertions (dpkg --compare-versions), if available ---
if command -v dpkg >/dev/null 2>&1; then
  earlier="0.154.0+r20260914110000.aaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  later="0.154.0+r20260914120000.bbbbbbbbbbbbbbbbbbbbbbbbbbbb"
  dpkg --compare-versions "$later" gt "$earlier"
  dpkg --compare-versions "$expected_deb_version" gt "$source_version"
else
  echo "test_build_fork_deb: dpkg not available -- skipping version-ordering assertions" >&2
fi

# --- second build without the estimator payload: no estimate wrapper ------
stage2="$work/stage2"
mkdir -p "$stage2"
write_stub "$stage2/codex"
write_stub "$stage2/codex-code-mode-host"
tarball2="$work/codex-x86_64-unknown-linux-gnu-noestimator.tar.gz"
tar -C "$stage2" -czf "$tarball2" codex codex-code-mode-host
out_dir2="$work/out2"
FORK_DEB_SKIP_SHLIBDEPS=1 bash "$builder" \
  "$tarball2" "$release_tag" "$source_version" "$out_dir2" > "$work/build-output2.txt"
deb_path2=$(sed -n 's/^deb_path=//p' "$work/build-output2.txt")
contents2="$(dpkg-deb -c "$deb_path2")"
if grep -Fq "./usr/bin/codex-shake-estimate" <<< "$contents2"; then
  echo "test_build_fork_deb: estimate wrapper must be absent without the full estimator payload" >&2
  exit 1
fi

# --- glibc mismatch must fail -----------------------------------------------
mismatch_dir="$work/mismatch"
mkdir -p "$mismatch_dir"
cp "$tarball" "$mismatch_dir/"
echo "9.99" > "$mismatch_dir/glibc-version.txt"
if bash "$builder" "$mismatch_dir/$(basename "$tarball")" "$release_tag" \
  "$source_version" "$work/out-mismatch" 2>/dev/null; then
  echo "test_build_fork_deb: expected glibc mismatch to fail the build" >&2
  exit 1
fi

echo "build-fork-deb tests passed"
