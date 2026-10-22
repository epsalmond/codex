#!/usr/bin/env bash
# Offline test for render-brew-formula.sh. Renders with fixed inputs, diffs
# against a checked-in fixture, and runs `ruby -c` on the output when ruby is
# available.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
renderer="$script_dir/render-brew-formula.sh"
fixture="$script_dir/testdata/codex-shake.expected.rb"

[[ -f "$fixture" ]] || { echo "test_render_brew_formula: missing fixture $fixture" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

release_tag="local-features-v0.154.0-main-r20260914110830.00000000000028b2ec76d3de4ab8"
source_version="0.154.0"
darwin_sha="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
linux_sha="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

rendered="$work/codex-shake.rb"
bash "$renderer" "$release_tag" "$source_version" "$darwin_sha" "$linux_sha" > "$rendered"

if ! diff -u "$fixture" "$rendered"; then
  echo "test_render_brew_formula: rendered formula does not match fixture $fixture" >&2
  exit 1
fi

if command -v ruby >/dev/null 2>&1; then
  ruby -c "$rendered" >/dev/null
else
  echo "test_render_brew_formula: ruby not available -- skipping ruby -c check" >&2
fi

# Version must never contain the sha (only the counter digits), and the sha
# must appear only in a comment / URL, not in `version "..."`.
grep -Eq '^  version "0\.154\.0\.20260914110830"$' "$rendered"
if grep -E '^  version ' "$rendered" | grep -q "00000000000028b2ec76d3de4ab8"; then
  echo "test_render_brew_formula: version line must not include the release sha" >&2
  exit 1
fi

# Argument validation: reject a non-hex/non-64-char sha256.
if bash "$renderer" "$release_tag" "$source_version" "not-a-sha" "$linux_sha" \
  > "$work/bad-darwin.rb" 2>/dev/null; then
  echo "test_render_brew_formula: expected invalid darwin sha256 to fail" >&2
  exit 1
fi

# Argument validation: reject a release tag with no -rNNN.sha suffix.
if bash "$renderer" "not-a-release-tag" "$source_version" "$darwin_sha" "$linux_sha" \
  > "$work/bad-tag.rb" 2>/dev/null; then
  echo "test_render_brew_formula: expected unparseable release tag to fail" >&2
  exit 1
fi

# Short (12-digit) counter form of the tag must also render successfully.
short_tag="local-features-v0.154.0-r202609140816.abcdef123456"
bash "$renderer" "$short_tag" "$source_version" "$darwin_sha" "$linux_sha" \
  > "$work/short-tag.rb"
grep -Eq '^  version "0\.154\.0\.202609140816"$' "$work/short-tag.rb"

# Extended identities must upgrade deterministically for retries, fresh runs,
# and subsequent source commits even when the source timestamp is unchanged.
versions=()
for identity in \
  00000000000028b2ec76d3de4ab800000000000004d200000001 \
  00000000000028b2ec76d3de4ab800000000000004d200000002 \
  00000000000028b2ec76d3de4ab800000000000004d300000001 \
  00000000000028b3ec76d3de4ab8000000000000000100000001; do
  new_tag="local-features-v0.154.0-main-r20260914110830.${identity}"
  new_formula="$work/${identity}.rb"
  bash "$renderer" "$new_tag" "$source_version" "$darwin_sha" "$linux_sha" > "$new_formula"
  version=$(sed -n 's/^  version "\([^"]*\)"$/\1/p' "$new_formula")
  [[ "$version" =~ ^[0-9]+(\.[0-9]+)+$ ]]
  versions+=("$version")
  grep -Fxq '# Source commit (short): ec76d3de4ab8' "$new_formula"
  if command -v ruby >/dev/null 2>&1; then
    ruby -c "$new_formula" >/dev/null
  fi
done
[[ "${versions[0]}" == 0.154.0.20260914110830.10418.1234.1 ]]
[[ "${versions[1]}" == 0.154.0.20260914110830.10418.1234.2 ]]
[[ "${versions[2]}" == 0.154.0.20260914110830.10418.1235.1 ]]
[[ "${versions[3]}" == 0.154.0.20260914110830.10419.1.1 ]]
python3 - "${versions[@]}" <<'PYTHON'
import sys
versions = [tuple(map(int, value.split("."))) for value in sys.argv[1:]]
assert all(left < right for left, right in zip(versions, versions[1:])), versions
PYTHON

echo "render-brew-formula tests passed"
