#!/usr/bin/env bash
# Titles and expected Markdown deliberately contain literal shell syntax.
# shellcheck disable=SC2016
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
notes_script="$script_dir/fork-release-notes.sh"
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT

git -C "$fixture" init -q
git -C "$fixture" config user.email test@example.com
git -C "$fixture" config user.name 'Release Notes Test'
printf 'Huge static feature catalog must not be copied.\n' > "$fixture/RELEASE_NOTES.md"
git -C "$fixture" add .
git -C "$fixture" commit -q -m base
git -C "$fixture" branch upstream
printf 'old fix\n' > "$fixture/fork"
git -C "$fixture" add .
git -C "$fixture" commit -q -m 'Previous fork change must not repeat'
previous_tag=local-features-v0.156.1-main-r20260901000000.abcdef123456
git -C "$fixture" tag "$previous_tag"
printf 'new fix\n' >> "$fixture/fork"
git -C "$fixture" commit -qam 'Fix [session] status `display` $(touch never-run)'
git -C "$fixture" checkout -q upstream
printf 'upstream\n' > "$fixture/upstream"
git -C "$fixture" add .
git -C "$fixture" commit -q -m 'Improve upstream tool results (#123) @user with ``ticks``'
upstream_commit=$(git -C "$fixture" rev-parse HEAD)
git -C "$fixture" checkout -q -
git -C "$fixture" merge -q --no-ff upstream -m 'Merge upstream'
commit=$(git -C "$fixture" rev-parse HEAD)
release_tag=local-features-v0.156.1-main-r20260902000000.abcdef123456

render() {
  local lineage=${1:-rust-v0.156.1}
  local upstream=${2:-$upstream_commit}
  bash "$notes_script" "$fixture" epsalmond/codex main "$commit" \
    0.156.1 "$lineage" "$upstream" 2.35.0 branch-merge "$release_tag" \
    codex-shake_0.156.1_amd64.deb "${3-$previous_tag}"
}

body=$(render)
grep -Fq '### Fork changes' <<< "$body"
grep -Fq '### Upstream changes' <<< "$body"
grep -Fq '`` Fix [session] status `display` $(touch never-run) ``' <<< "$body"
grep -Fq '``` Improve upstream tool results (#123) @user with ``ticks`` ```' <<< "$body"
grep -Fq "https://github.com/epsalmond/codex/compare/$previous_tag...$release_tag" <<< "$body"
grep -Fq "https://github.com/epsalmond/codex/blob/$commit/RELEASE_NOTES.md" <<< "$body"
grep -Fq '**Build:** Based on upstream `rust-v0.156.1`' <<< "$body"
if grep -Eq 'Huge static feature catalog|Previous fork change' <<< "$body"; then
  echo 'release notes must describe only this source range, without the static catalog' >&2
  exit 1
fi
[[ ! -e never-run && ! -e "$fixture/never-run" ]]
# Omitted argument retains the legacy standalone tag lookup; explicit empty
# below means there is no known published baseline and must behave differently.
omitted_body=$(bash "$notes_script" "$fixture" epsalmond/codex main "$commit" \
  0.156.1 rust-v0.156.1 "$upstream_commit" 2.35.0 branch-merge "$release_tag" \
  codex-shake_0.156.1_amd64.deb)
[[ "$omitted_body" == "$body" ]]

# Reading committed history is stable when the checkout is dirty or the target
# tag has already been created (as on a rerun).
printf 'Uncommitted notes\n' > "$fixture/RELEASE_NOTES.md"
git -C "$fixture" tag "$release_tag"
[[ "$(render)" == "$body" ]]

# Failed publication can leave a tag at the target source. Only published,
# non-draft releases are candidates, even during tag-triggered recovery.
failed_tag=local-features-v0.156.1-main-failed-attempt
git -C "$fixture" tag "$failed_tag"
baseline() {
  printf '%s' "$1" | python3 "$script_dir/fork-release-baseline.py" "$fixture" "$commit" "$release_tag"
}
published="{\"tag_name\":\"$previous_tag\",\"draft\":false,\"published_at\":\"2026-09-01T00:00:00Z\"}"
[[ "$(baseline "[[$published,{\"tag_name\":\"$failed_tag\",\"draft\":true,\"published_at\":null}]]")" == "$previous_tag" ]]
failed_body=$(render rust-v0.156.1 "$upstream_commit" "$(baseline "[[$published]]")")
grep -Fq '### Fork changes' <<< "$failed_body"
if grep -Fq 'Rebuild of the same source' <<< "$failed_body"; then
  echo 'failed publication tag must not hide source changes' >&2
  exit 1
fi
for no_baseline in '' none "$(baseline '[[]]')"; do
  empty_body=$(render rust-v0.156.1 "$upstream_commit" "$no_baseline")
  grep -Fq 'Previous release baseline unavailable' <<< "$empty_body"
  if grep -Fq '/compare/' <<< "$empty_body"; then
    echo 'explicit missing published baseline must not fall back to source tags' >&2
    exit 1
  fi
done
published_rebuild="{\"tag_name\":\"$failed_tag\",\"draft\":false,\"published_at\":\"2026-09-02T00:00:00Z\"}"
target_release="{\"tag_name\":\"$release_tag\",\"draft\":false,\"published_at\":\"2026-09-03T00:00:00Z\"}"
[[ "$(baseline "[[$published],[$published_rebuild,$target_release]]")" == "$failed_tag" ]]
published_body=$(render rust-v0.156.1 "$upstream_commit" "$(baseline "[[$published_rebuild]]")")
grep -Fq 'Rebuild of the same source; no source changes.' <<< "$published_body"

none_body=$(render none none)
grep -Fq '### Source changes' <<< "$none_body"
grep -Fq 'Upstream commit baseline unavailable' <<< "$none_body"
grep -Fq '**Build:** No exact upstream release tag;' <<< "$none_body"
if grep -Fq 'https://github.com/openai/codex/releases/tag/' <<< "$none_body"; then
  echo 'release without stable lineage must not link an upstream release tag' >&2
  exit 1
fi

missing_body=$(render rust-v0.156.1 "$upstream_commit" missing-tag)
grep -Fq 'Previous release baseline unavailable' <<< "$missing_body"
if grep -Fq '/compare/' <<< "$missing_body"; then
  echo 'release without a baseline must not imply a complete comparison' >&2
  exit 1
fi

# Same-source releases are explicitly rebuilds rather than repeated old notes.
rebuild_body=$(render rust-v0.156.1 "$upstream_commit" "$release_tag")
grep -Fq 'Rebuild of the same source; no source changes.' <<< "$rebuild_body"
if grep -Fq '### Fork changes' <<< "$rebuild_body"; then
  echo 'same-source rebuild must not repeat previous changes' >&2
  exit 1
fi

# A long range stays concise and links the rest instead of implying completeness.
for i in {1..10}; do
  git -C "$fixture" commit -q --allow-empty -m "Extra fix $i"
done
commit=$(git -C "$fixture" rev-parse HEAD)
long_body=$(render rust-v0.156.1 "$upstream_commit" "$previous_tag")
grep -Fq 'Showing 8 of 11 commits; [Full comparison]' <<< "$long_body"
[[ "$(grep -c '^-' <<< "$long_body")" == 9 ]]
missing_long_body=$(render rust-v0.156.1 "$upstream_commit" missing-tag)
grep -Fq "[Source history](https://github.com/epsalmond/codex/commits/$commit) includes the rest." <<< "$missing_long_body"

echo 'fork-release notes tests passed'
