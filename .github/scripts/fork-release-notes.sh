#!/usr/bin/env bash
# Compose a fork-release.yml release body, ported from the notes section of
# ~/.bin/codex-fork-release: RELEASE_NOTES.md (with relative codex-rs/ links
# rewritten to blob URLs) followed by the base-tag/glibc line, the install
# block, and the commit list.
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

sed -E "s#\]\((codex-rs/[^)]+)\)#](https://github.com/$fork_repo/blob/$branch/\1)#g" \
  "$repo_root/RELEASE_NOTES.md"
cat <<EOF

Based on upstream \`$base_tag\` (\`$short\`). Apple Silicon and x86_64 Linux
(glibc >= $glibc_version), unsigned, unstripped.

**Install** as \`codex-shake\` beside your official \`codex\` (rerun to update):

\`\`\`sh
curl -fsSL https://raw.githubusercontent.com/$fork_repo/$branch/install.sh | sh
\`\`\`

**Commits over \`$base_tag\`:**

EOF
git -C "$repo_root" log --reverse --format='- %h %s' "$base_tag..$commit"
