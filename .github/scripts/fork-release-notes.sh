#!/usr/bin/env bash
# Render the fork release's concise feature overview and exact build identity.
#
# usage: fork-release-notes.sh <repo-root> <fork-repo> <branch> <commit>
#   <source-version> <stable-lineage> <upstream-main-sha> <glibc-version>
#   <source-kind> <release-tag> <deb-asset-name>
set -euo pipefail

repo_root=$1
fork_repo=$2
commit=$4
source_version=$5
stable_lineage=$6
release_tag=${10}

commit=$(git -C "$repo_root" rev-parse --verify "$commit^{commit}")
short_commit=$(git -C "$repo_root" rev-parse --short=12 "$commit")

# Keep documentation links usable from the release page by pinning repository-
# relative links to the exact commit described by this release.
git -C "$repo_root" show "$commit:RELEASE_NOTES.md" |
  FORK_REPO="$fork_repo" COMMIT="$commit" perl -pe '
  s{\]\(([^)]+)\)}{
    my $path = $1;
    $path =~ m{^(?:https?://|\#|/)}
      ? "](" . $path . ")"
      : "](https://github.com/$ENV{FORK_REPO}/blob/$ENV{COMMIT}/" . $path . ")"
  }ge
'

echo
if [[ "$stable_lineage" == none ]]; then
  printf "**Build:** No exact upstream release tag; Codex \`%s\`; [fork release tag](https://github.com/%s/releases/tag/%s), source [\`%s\`](https://github.com/%s/commit/%s).\n" \
    "$source_version" "$fork_repo" "$release_tag" "$short_commit" \
    "$fork_repo" "$commit"
else
  printf "**Build:** Based on upstream \`%s\`; Codex \`%s\`; [fork release tag](https://github.com/%s/releases/tag/%s), source [\`%s\`](https://github.com/%s/commit/%s).\n" \
    "$stable_lineage" "$source_version" "$fork_repo" "$release_tag" \
    "$short_commit" "$fork_repo" "$commit"
fi
