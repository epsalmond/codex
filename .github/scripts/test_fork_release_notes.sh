#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
notes_script="$script_dir/fork-release-notes.sh"
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT

git -C "$fixture" init -q
git -C "$fixture" config user.email test@example.com
git -C "$fixture" config user.name "Release Notes Test"
mkdir -p "$fixture/codex-rs" "$fixture/releases"
printf '[workspace.package]\nversion = "0.200.0"\n' > "$fixture/codex-rs/Cargo.toml"
printf 'base\n' > "$fixture/README.md"
printf '**Fork overview.** See [docs](codex-rs/docs/example.md), the\n[installer](install.sh), an [external link](https://example.com/x),\na [fragment link](#section), and an [absolute path](/LICENSE).\n' \
  > "$fixture/RELEASE_NOTES.md"
printf 'Entry format doc.\n' > "$fixture/releases/README.md"
git -C "$fixture" add .
git -C "$fixture" commit -q -m base
git -C "$fixture" branch -M eric/local-features

render() {
  local commit=$1 release_tag=$2
  bash "$notes_script" "$fixture" epsalmond/codex eric/local-features "$commit" \
    0.200.0 rust-v1.0.0 deadbeefdeadbeefdeadbeefdeadbeefdeadbeef 2.35.0 \
    branch-merge "$release_tag" codex-shake_0.200.0_amd64.deb
}

# --- Case: no previous tag at all -------------------------------------------
base_sha=$(git -C "$fixture" rev-parse HEAD)
no_prev_body=$(render "$base_sha" local-features-v0.200.0-main-r1)
grep -Fq "## What's new in this release" <<< "$no_prev_body"
grep -Fq 'No previous fork release tag was found' <<< "$no_prev_body"
grep -Fq "https://github.com/epsalmond/codex/commits/$base_sha" <<< "$no_prev_body"

# --- Case: every relative link is rewritten, others are left alone ---------
grep -Fq -- "[docs](https://github.com/epsalmond/codex/blob/$base_sha/codex-rs/docs/example.md)" <<< "$no_prev_body"
grep -Fq -- "[installer](https://github.com/epsalmond/codex/blob/$base_sha/install.sh)" <<< "$no_prev_body"
grep -Fq -- '[external link](https://example.com/x)' <<< "$no_prev_body"
grep -Fq -- '[fragment link](#section)' <<< "$no_prev_body"
grep -Fq -- '[absolute path](/LICENSE)' <<< "$no_prev_body"

# --- Set up a previous release tag ------------------------------------------
git -C "$fixture" tag local-features-v0.190.0-main-r0 "$base_sha"

# --- Case: new entries rendered in the right order --------------------------
# The second entry's summary wraps across two source lines, like real prose;
# the changelog bullet must join them into one sentence.
printf '# Second feature\n\nSecond feature summary sentence that wraps\nacross two source lines.\n\nDetail.\n' \
  > "$fixture/releases/2026-09-02-second.md"
git -C "$fixture" add releases/2026-09-02-second.md
git -C "$fixture" commit -q -m 'docs: second feature entry'

printf '# First feature\n\nFirst feature summary sentence.\n\nDetail.\n' \
  > "$fixture/releases/2026-09-01-first.md"
git -C "$fixture" add releases/2026-09-01-first.md
git -C "$fixture" commit -q -m 'docs: first feature entry'
entries_sha=$(git -C "$fixture" rev-parse HEAD)

entries_body=$(render "$entries_sha" local-features-v0.200.0-main-r2)
first_line=$(grep -n 'First feature' <<< "$entries_body" | head -1 | cut -d: -f1)
second_line=$(grep -n 'Second feature' <<< "$entries_body" | head -1 | cut -d: -f1)
[[ -n "$first_line" && -n "$second_line" && "$first_line" -lt "$second_line" ]]
grep -Fq -- '- **First feature**: First feature summary sentence. ([details](https://github.com/epsalmond/codex/blob/'"$entries_sha"'/releases/2026-09-01-first.md))' <<< "$entries_body"
grep -Fq -- '- **Second feature**: Second feature summary sentence that wraps across two source lines. ([details](https://github.com/epsalmond/codex/blob/'"$entries_sha"'/releases/2026-09-02-second.md))' <<< "$entries_body"
# releases/README.md is never rendered as an entry.
if grep -Fq 'releases/README.md' <<< "$entries_body"; then
  echo "releases/README.md should never be rendered as a changelog entry" >&2
  exit 1
fi
# The RELEASE_NOTES.md link rewrite also applies to releases/ links.
grep -Fq "https://github.com/epsalmond/codex/blob/$entries_sha/codex-rs/docs/example.md" <<< "$entries_body"

# --- Case: current release tag is excluded from "previous tag" search ------
git -C "$fixture" tag local-features-v0.200.0-main-r2 "$entries_sha"
excluded_body=$(render "$entries_sha" local-features-v0.200.0-main-r2)
# Must still resolve to the older tag, not itself, so the same entries render.
grep -Fq 'First feature summary sentence.' <<< "$excluded_body"
grep -Fq 'Second feature summary sentence that wraps across two source lines.' <<< "$excluded_body"

# --- Case: no new entries falls back to merged fork PRs ---------------------
git -C "$fixture" switch -q -c pr-branch
printf 'change\n' >> "$fixture/README.md"
git -C "$fixture" commit -q -am 'unrelated change, no release entry'
git -C "$fixture" switch -q eric/local-features
git -C "$fixture" merge -q --no-ff -m "$(cat <<'EOF'
Merge pull request #42 from epsalmond/pr-branch

Ship an unrelated change
EOF
)" pr-branch
fallback_sha=$(git -C "$fixture" rev-parse HEAD)

fallback_body=$(render "$fallback_sha" local-features-v0.200.0-main-r3)
grep -Fq -- '- **Ship an unrelated change** ([#42](https://github.com/epsalmond/codex/pull/42))' <<< "$fallback_body"
if grep -Fq 'First feature summary sentence.' <<< "$fallback_body"; then
  echo "fallback body should not include stale entry text from earlier releases" >&2
  exit 1
fi

echo "fork-release notes tests passed"
