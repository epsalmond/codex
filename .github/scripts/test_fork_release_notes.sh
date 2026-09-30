#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
notes_script="$script_dir/fork-release-notes.sh"
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT

git -C "$fixture" init -q
git -C "$fixture" config user.email test@example.com
git -C "$fixture" config user.name "Release Notes Test"
mkdir -p "$fixture/codex-rs/docs" "$fixture/releases"
printf 'fixture\n' > "$fixture/codex-rs/docs/example.md"
cat > "$fixture/RELEASE_NOTES.md" <<'EOF'
## Feature one

First sentence. See the [documentation](codex-rs/docs/example.md), an [external link](https://example.com/x), a [fragment](#section), and an [absolute path](/LICENSE).

## Feature two

One short sentence with [another doc link](releases/feature-two.md).
EOF
printf 'Detailed behavior.\n' > "$fixture/releases/feature-two.md"
git -C "$fixture" add .
git -C "$fixture" commit -q -m base
git -C "$fixture" branch -M eric/local-features
commit=$(git -C "$fixture" rev-parse HEAD)

body=$(bash "$notes_script" "$fixture" epsalmond/codex eric/local-features "$commit" \
  0.156.1 rust-v0.156.1 deadbeefdeadbeefdeadbeefdeadbeefdeadbeef 2.35.0 \
  branch-merge local-features-v0.156.1-main-r1 codex-shake_0.156.1_amd64.deb)

short_commit=$(git -C "$fixture" rev-parse --short=12 "$commit")
expected_body=$(cat <<EOF
## Feature one

First sentence. See the [documentation](https://github.com/epsalmond/codex/blob/$commit/codex-rs/docs/example.md), an [external link](https://example.com/x), a [fragment](#section), and an [absolute path](/LICENSE).

## Feature two

One short sentence with [another doc link](https://github.com/epsalmond/codex/blob/$commit/releases/feature-two.md).

**Build:** Based on upstream \`rust-v0.156.1\`; [upstream release notes](https://github.com/openai/codex/releases/tag/rust-v0.156.1); Codex \`0.156.1\`; [fork release tag](https://github.com/epsalmond/codex/releases/tag/local-features-v0.156.1-main-r1), source [\`$short_commit\`](https://github.com/epsalmond/codex/commit/$commit).
EOF
)
if [[ "$body" != "$expected_body" ]]; then
  diff -u <(printf '%s\n' "$expected_body") <(printf '%s\n' "$body")
  exit 1
fi

# Render the immutable source notes even when the checkout has later edits.
printf '## Working tree edit\n\nThis must not appear.\n' > "$fixture/RELEASE_NOTES.md"
immutable_body=$(bash "$notes_script" "$fixture" epsalmond/codex eric/local-features "$commit" \
  0.156.1 rust-v0.156.1 deadbeefdeadbeefdeadbeefdeadbeefdeadbeef 2.35.0 \
  branch-merge local-features-v0.156.1-main-r1 codex-shake_0.156.1_amd64.deb)
if [[ "$immutable_body" != "$expected_body" ]]; then
  echo 'release body should match the notes committed at its source SHA' >&2
  diff -u <(printf '%s\n' "$expected_body") <(printf '%s\n' "$immutable_body")
  exit 1
fi

none_body=$(bash "$notes_script" "$fixture" epsalmond/codex eric/local-features "$commit" \
  0.156.1 none deadbeefdeadbeefdeadbeefdeadbeefdeadbeef 2.35.0 \
  branch-merge local-features-v0.156.1-main-r1 codex-shake_0.156.1_amd64.deb)
expected_none_build=$(cat <<EOF
**Build:** No exact upstream release tag; Codex \`0.156.1\`; [fork release tag](https://github.com/epsalmond/codex/releases/tag/local-features-v0.156.1-main-r1), source [\`$short_commit\`](https://github.com/epsalmond/codex/commit/$commit).
EOF
)
if [[ "$(printf '%s\n' "$none_body" | tail -n 1)" != "$expected_none_build" ]] || \
  grep -Fq 'https://github.com/openai/codex/releases/tag/' <<<"$none_body"; then
  echo 'release without stable lineage must not link an upstream tag' >&2
  printf '%s\n' "$none_body" | tail -n 1 >&2
  exit 1
fi

echo "fork-release notes tests passed"
