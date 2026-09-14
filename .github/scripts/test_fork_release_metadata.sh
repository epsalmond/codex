#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
metadata_script="$script_dir/fork-release-metadata.sh"
workflow_file="$script_dir/../workflows/fork-release.yml"
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT

git -C "$fixture" init -q
git -C "$fixture" config user.email test@example.com
git -C "$fixture" config user.name "Release Test"
mkdir -p "$fixture/codex-rs"
printf '[workspace.package]\nversion = "0.154.0"\n' > "$fixture/codex-rs/Cargo.toml"
printf 'base\n' > "$fixture/README.md"
printf '[Doc](codex-rs/docs/example.md)\n' > "$fixture/RELEASE_NOTES.md"
git -C "$fixture" add .
git -C "$fixture" commit -q -m base
git -C "$fixture" tag rust-v1.2.3
git -C "$fixture" tag rust-v9.9.9-alpha.1
git -C "$fixture" branch -M eric/local-features
git -C "$fixture" commit --allow-empty -q -m 'fork change'
fork_tip=$(git -C "$fixture" rev-parse HEAD)

git -C "$fixture" switch -q -c upstream/main HEAD~1
printf 'upstream\n' >> "$fixture/README.md"
git -C "$fixture" add README.md
git -C "$fixture" commit -q -m 'upstream change'
upstream_tip=$(git -C "$fixture" rev-parse HEAD)
git -C "$fixture" switch -q -c integration-merge eric/local-features
git -C "$fixture" merge -q --no-ff --no-edit -m 'Integrate upstream main' upstream/main
integration_merge_sha=$(git -C "$fixture" rev-parse HEAD)
git -C "$fixture" switch -q eric/local-features
git -C "$fixture" merge -q --no-ff --no-edit -m 'GitHub merge pull request' integration-merge
merge_sha=$(git -C "$fixture" rev-parse HEAD)
outer_second_parent=$(git -C "$fixture" rev-list --parents -n 1 "$merge_sha" | awk '{ print $3 }')
[[ "$outer_second_parent" != "$upstream_tip" ]]

upstream_remote="$fixture/upstream.git"
git init --bare -q "$upstream_remote"
git -C "$fixture" push -q "$upstream_remote" \
  upstream/main:refs/heads/main \
  refs/tags/rust-v1.2.3:refs/tags/rust-v1.2.3 \
  refs/tags/rust-v9.9.9-alpha.1:refs/tags/rust-v9.9.9-alpha.1
git -C "$fixture" tag rust-v2.0.0 "$upstream_tip"
git -C "$fixture" push -q "$upstream_remote" \
  refs/tags/rust-v2.0.0:refs/tags/rust-v2.0.0
git -C "$fixture" tag -d rust-v9.9.9-alpha.1 rust-v2.0.0 >/dev/null

# A later local head proves preparation uses the event SHA, not a live branch.
git -C "$fixture" commit --allow-empty -q -m 'later local head'
event_output="$fixture/event-output"
bash "$metadata_script" \
  --repo-root "$fixture" \
  --event-name push \
  --event-ref refs/heads/eric/local-features \
  --event-sha "$merge_sha" \
  --event-before "$fork_tip" \
  --upstream-url "$upstream_remote" \
  --output "$event_output"

grep -Fxq "release_sha=$merge_sha" "$event_output"
grep -Fxq "source_version=0.154.0" "$event_output"
grep -Fxq "stable_lineage=rust-v2.0.0" "$event_output"
grep -Fxq "included_upstream_main_sha=$upstream_tip" "$event_output"
grep -Fxq 'source_kind=branch-merge' "$event_output"
release_tag=$(sed -n 's/^release_tag=//p' "$event_output")
source_sequence=$(git -C "$fixture" rev-list --first-parent --count "$merge_sha")
printf -v source_sequence_hex '%016x' "$source_sequence"
producer_sha=$(git -C "$fixture" rev-parse --short=12 "$merge_sha")
[[ "$release_tag" =~ ^local-features-v0\.154\.0-main-r[0-9]{14}\.[0-9a-f]{28}$ ]]
[[ "${release_tag##*.}" == "$source_sequence_hex$producer_sha" ]]

grep -Fq 'bash .github/scripts/fork-release-notes.sh' "$workflow_file"
body=$(bash "$script_dir/fork-release-notes.sh" \
  "$fixture" epsalmond/codex eric/local-features "$merge_sha" \
  0.154.0 rust-v2.0.0 "$upstream_tip" 2.35.0 branch-merge)
grep -Fq "Fork release commit: \`$merge_sha\`" <<< "$body"
grep -Fq "Integrated upstream main commit: \`$upstream_tip\`" <<< "$body"
grep -Fq 'Exact stable lineage: `rust-v2.0.0`' <<< "$body"
grep -Fq 'https://github.com/epsalmond/codex/blob/' <<< "$body"
grep -Fq 'https://github.com/epsalmond/codex/compare/rust-v2.0.0...' <<< "$body"

# A same-name tag pointing elsewhere is rejected before any publish step.
git -C "$fixture" tag "$release_tag" HEAD
if bash "$metadata_script" \
  --repo-root "$fixture" \
  --event-name push \
  --event-ref refs/heads/eric/local-features \
  --event-sha "$merge_sha" \
  --event-before "$fork_tip" \
  --upstream-url "$upstream_remote" \
  --output "$fixture/collision-output"; then
  echo "expected derived-tag collision to fail" >&2
  exit 1
fi

# Tag fallback still requires the pushed tag to resolve to the event SHA.
tag_name=local-features-v0.154.0-r202609140000.abcdef123456
git -C "$fixture" tag "$tag_name" "$merge_sha"
if bash "$metadata_script" \
  --repo-root "$fixture" \
  --event-name push \
  --event-ref "refs/tags/$tag_name" \
  --event-sha "$(git -C "$fixture" rev-parse HEAD)" \
  --output "$fixture/wrong-tag-output"; then
  echo "expected tag SHA mismatch to fail" >&2
  exit 1
fi

# Recovery tags on a one-parent descendant still fetch exact stable lineage
# and compute the public-main merge-base.
recovery_sha=$(git -C "$fixture" rev-parse HEAD)
recovery_tag=local-features-v0.154.0-r20260914000000.fedcba654321
git -C "$fixture" tag "$recovery_tag" "$recovery_sha"
recovery_output="$fixture/recovery-output"
bash "$metadata_script" \
  --repo-root "$fixture" \
  --event-name push \
  --event-ref "refs/tags/$recovery_tag" \
  --event-sha "$recovery_sha" \
  --upstream-url "$upstream_remote" \
  --output "$recovery_output"
grep -Fxq "release_sha=$recovery_sha" "$recovery_output"
grep -Fxq "release_tag=$recovery_tag" "$recovery_output"
grep -Fxq "stable_lineage=rust-v2.0.0" "$recovery_output"
grep -Fxq "included_upstream_main_sha=$upstream_tip" "$recovery_output"
grep -Fxq 'source_kind=tag' "$recovery_output"

rejected_recovery_tag=local-features-v0.154.0-r202609140000000.fedcba654321
git -C "$fixture" tag "$rejected_recovery_tag" "$recovery_sha"
if bash "$metadata_script" \
  --repo-root "$fixture" \
  --event-name push \
  --event-ref "refs/tags/$rejected_recovery_tag" \
  --event-sha "$recovery_sha" \
  --output "$fixture/rejected-recovery-output"; then
  echo "expected fifteen-digit recovery tag to fail" >&2
  exit 1
fi

# Publisher fallback identities use one hexadecimal suffix, matching the
# Rust updater and installer grammar; multi-dot variants are rejected.
invalid_tag=local-features-v0.154.0-r202609140001.abcd.ef
git -C "$fixture" tag "$invalid_tag" "$merge_sha"
if bash "$metadata_script" \
  --repo-root "$fixture" \
  --event-name push \
  --event-ref "refs/tags/$invalid_tag" \
  --event-sha "$merge_sha" \
  --output "$fixture/invalid-output"; then
  echo "expected multi-dot recovery tag to fail" >&2
  exit 1
fi

# Non-fast-forward and non-merge branch events are not publishable.
if bash "$metadata_script" \
  --repo-root "$fixture" \
  --event-name push \
  --event-ref refs/heads/eric/local-features \
  --event-sha "$upstream_tip" \
  --event-before "$merge_sha" \
  --output "$fixture/non-ff-output"; then
  echo "expected non-fast-forward event to fail" >&2
  exit 1
fi

echo "fork-release metadata tests passed"
