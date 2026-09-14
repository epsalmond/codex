#!/usr/bin/env bash
# Compose a fork-release.yml release body, ported from the notes section of
# ~/.bin/codex-fork-release: RELEASE_NOTES.md (with relative codex-rs/ links
# rewritten to commit-pinned blob URLs), with the install block before it and
# the base-tag/glibc line and commit list after it.
#
# usage: fork-release-notes.sh <repo-root> <fork-repo> <branch> <commit> <glibc-version>
set -euo pipefail

repo_root=$1
fork_repo=$2
branch=$3
commit=$4
glibc_version=$5

short=$(git -C "$repo_root" rev-parse --short=12 "$commit")
base_tag=$(git -C "$repo_root" describe --tags --match 'rust-v[0-9]*' --abbrev=0 "$commit")

cat <<EOF

**Install and update** as \`codex-shake\` beside your official \`codex\`:

\`\`\`sh
curl -fsSL https://raw.githubusercontent.com/$fork_repo/$branch/install.sh | sh
\`\`\`

Run \`codex-shake\`. Rerun the installer or use \`codex-shake-update\` to update.
**Estimate sessions active in the past 7 days (offline; Python 3):** \`codex-shake-estimate --since 168\`.

EOF
sed -E "s#\]\((codex-rs/[^)]+)\)#](https://github.com/$fork_repo/blob/$commit/\1)#g" \
  "$repo_root/RELEASE_NOTES.md"
cat <<EOF

Based on upstream \`$base_tag\` (\`$short\`). Apple Silicon and x86_64 Linux
(glibc >= $glibc_version), unsigned, unstripped.

**Commits over \`$base_tag\`:**

EOF
git -C "$repo_root" log --reverse --format='- %h %s' "$base_tag..$commit"
