#!/usr/bin/env bash
# Render actual changes since the preceding published fork release.
#
# usage: fork-release-notes.sh <repo-root> <fork-repo> <branch> <commit>
#   <source-version> <stable-lineage> <upstream-main-sha> <glibc-version>
#   <source-kind> <release-tag> <deb-asset-name> [previous-release-tag]
set -euo pipefail

repo_root=$1
fork_repo=$2
commit=$4
source_version=$5
stable_lineage=$6
upstream_commit=$7
release_tag=${10}
previous_tag=${12-}
[[ "$previous_tag" != none ]] || previous_tag=''
git_cmd=(git -C "$repo_root")
commit=$("${git_cmd[@]}" rev-parse --verify "$commit^{commit}")
short_commit=$("${git_cmd[@]}" rev-parse --short=12 "$commit")

# Legacy standalone callers may omit the baseline. The release workflow
# supplies it explicitly, including an empty value when no published ancestor
# exists. Never turn that empty result into an orphan source-tag baseline.
if (($# < 12)); then
  previous_tag=$("${git_cmd[@]}" describe --tags --first-parent --abbrev=0 \
    --match 'local-features-*' --exclude "$release_tag" "$commit" 2>/dev/null || true)
fi
previous_commit=''
if [[ -n "$previous_tag" ]]; then
  previous_commit=$("${git_cmd[@]}" rev-parse --verify "$previous_tag^{commit}" 2>/dev/null || true)
fi
summary_link="[Source history](https://github.com/$fork_repo/commits/$commit)"

# Titles are data. Render inert code spans and strip terminal controls;
# never evaluate commit text or copy arbitrary release text into shell code.
render_commits() {
  local heading=$1
  shift
  local count
  count=$("${git_cmd[@]}" rev-list --count --no-merges "$@")
  ((count > 0)) || return 0
  printf '\n### %s\n\n' "$heading"
  "${git_cmd[@]}" log --no-merges --format='%h%x09%s' -n 8 "$@" |
    LC_ALL=C tr -d '\000-\010\013-\037\177' |
    while IFS=$'\t' read -r sha title; do
      # Code spans make upstream issue numbers and mentions inert on the fork's
      # release page. Pick a delimiter absent from the data, including titles
      # that themselves contain backticks; padding protects boundary backticks.
      delimiter='`'
      while [[ "$title" == *"$delimiter"* ]]; do delimiter+='`'; done
      title="$delimiter $title $delimiter"
      # shellcheck disable=SC2016 # Markdown code spans are literal.
      printf -- '- %s ([`%s`](https://github.com/%s/commit/%s))\n' "$title" "$sha" "$fork_repo" "$sha"
    done
  if ((count > 8)); then
    printf '\nShowing 8 of %s commits; %s includes the rest.\n' "$count" "$summary_link"
  fi
}

printf '## Changes in this build\n\n'
if [[ -n "$previous_commit" ]] && "${git_cmd[@]}" merge-base --is-ancestor "$previous_commit" "$commit"; then
  summary_link="[Full comparison](https://github.com/$fork_repo/compare/$previous_tag...$release_tag)"
  printf 'Since [%s](https://github.com/%s/releases/tag/%s). [Full comparison](https://github.com/%s/compare/%s...%s).\n' \
    "$previous_tag" "$fork_repo" "$previous_tag" "$fork_repo" "$previous_tag" "$release_tag"
  if [[ "$previous_commit" == "$commit" ]]; then
    printf '\nRebuild of the same source; no source changes.\n'
  elif [[ "$upstream_commit" != none ]] &&
    "${git_cmd[@]}" cat-file -e "$upstream_commit^{commit}" 2>/dev/null &&
    "${git_cmd[@]}" merge-base --is-ancestor "$upstream_commit" "$commit"; then
    render_commits 'Fork changes' "$previous_commit..$commit" --not "$upstream_commit"
    render_commits 'Upstream changes' "$previous_commit..$upstream_commit"
  else
    render_commits 'Source changes' "$previous_commit..$commit"
    printf '\nUpstream commit baseline unavailable; source changes are not separated by origin.\n'
  fi
else
  printf 'Previous release baseline unavailable; this is not an installed-to-target comparison.\n'
  printf '\nRecent source changes (up to 8 commits):\n'
  render_commits 'Source history' "$commit"
fi
printf '\n[Feature guide](https://github.com/%s/blob/%s/RELEASE_NOTES.md).\n' "$fork_repo" "$commit"

# Keep this one-line identity compatible with fork-upstream-poll consumers.
echo
if [[ "$stable_lineage" == none ]]; then
  printf "**Build:** No exact upstream release tag; Codex \`%s\`; [fork release tag](https://github.com/%s/releases/tag/%s), source [\`%s\`](https://github.com/%s/commit/%s).\n" \
    "$source_version" "$fork_repo" "$release_tag" "$short_commit" \
    "$fork_repo" "$commit"
else
  printf "**Build:** Based on upstream \`%s\`; [upstream release notes](https://github.com/openai/codex/releases/tag/%s); Codex \`%s\`; [fork release tag](https://github.com/%s/releases/tag/%s), source [\`%s\`](https://github.com/%s/commit/%s).\n" \
    "$stable_lineage" "$stable_lineage" "$source_version" "$fork_repo" "$release_tag" \
    "$short_commit" "$fork_repo" "$commit"
fi
