#!/usr/bin/env bash
# Compose a fork-release.yml release body: a changelog since the previous
# fork release, a short "what this fork adds" overview (from
# RELEASE_NOTES.md), commit-pinned links, the install block, provenance, and
# collapsed first-parent provenance since the upstream stable tag.
#
# usage: fork-release-notes.sh <repo-root> <fork-repo> <branch> <commit>
#   <source-version> <stable-lineage> <upstream-main-sha> <glibc-version>
#   <source-kind> <release-tag> <deb-asset-name>
set -euo pipefail

repo_root=$1
fork_repo=$2
branch=$3
commit=$4
source_version=$5
stable_lineage=$6
upstream_main_sha=$7
glibc_version=$8
source_kind=$9
release_tag=${10}
deb_asset_name=${11}

commit=$(git -C "$repo_root" rev-parse --verify "$commit^{commit}")
case "$source_kind" in
  branch-merge) source_label="validated branch merge (fork snapshot)" ;;
  tag) source_label="explicit tag fallback" ;;
  *) echo "unknown source kind: $source_kind" >&2; exit 2 ;;
esac

# Finds the closest local-features-v* tag that is an ancestor of $commit and
# is not $release_tag itself (the tag this release is about to become, if it
# already exists, e.g. on a recovery re-run). "Closest" is measured by
# first-parent commit distance to $commit, not tag creation time, so it is
# stable regardless of clock skew or same-second tags. Prints nothing if
# there is no such tag.
find_previous_release_tag() {
  local commit=$1 current_tag=$2 tag count best="" best_count=""
  while IFS= read -r tag; do
    [[ -z "$tag" || "$tag" == "$current_tag" ]] && continue
    git -C "$repo_root" merge-base --is-ancestor "$tag" "$commit" 2>/dev/null || continue
    count=$(git -C "$repo_root" rev-list --first-parent --count "$tag..$commit")
    if [[ -z "$best" || "$count" -lt "$best_count" ]]; then
      best=$tag
      best_count=$count
    fi
  done < <(git -C "$repo_root" for-each-ref --format='%(refname:short)' 'refs/tags/local-features-v*')
  if [[ -n "$best" ]]; then
    printf '%s\n' "$best"
  fi
  return 0
}

# Lists releases/*.md files (excluding releases/README.md) added between
# $prev (exclusive) and $commit (inclusive).
list_new_release_entries() {
  local prev=$1 commit=$2
  git -C "$repo_root" diff --diff-filter=A --name-only "$prev" "$commit" -- releases/ |
    grep -Fxv 'releases/README.md' || true
}

# Renders one releases/*.md entry as a changelog bullet, using its title
# (line 1, `# Title`) and one-sentence summary paragraph (starting at line 3,
# joined until the next blank line or end of file, in case it wraps).
render_release_entry() {
  local file=$1 commit=$2 content title summary
  content=$(git -C "$repo_root" show "$commit:$file")
  title=$(sed -n '1{s/^# //p}' <<<"$content")
  summary=$(awk 'NR<3{next} /^$/{exit} {buf = buf (buf=="" ? "" : " ") $0} END{print buf}' <<<"$content")
  printf -- '- **%s**: %s ([details](https://github.com/%s/blob/%s/%s))\n' \
    "$title" "$summary" "$fork_repo" "$commit" "$file"
}

# Fallback when no releases/ entries were added: lists merged fork PRs on the
# first-parent path since $prev, oldest first, using each merge commit's
# subject ("Merge pull request #N from ...") and the PR title from the second
# line of the merge commit body.
list_fallback_prs() {
  local prev=$1 commit=$2 sha subject body pr_num pr_title
  git -C "$repo_root" log --first-parent --reverse --format='%H' \
    --grep='^Merge pull request #[0-9]+ from' -E "$prev..$commit" |
  while IFS= read -r sha; do
    [[ -z "$sha" ]] && continue
    subject=$(git -C "$repo_root" show -s --format='%s' "$sha")
    [[ "$subject" =~ ^Merge\ pull\ request\ \#([0-9]+)\ from ]] || continue
    pr_num=${BASH_REMATCH[1]}
    body=$(git -C "$repo_root" show -s --format='%B' "$sha")
    pr_title=$(sed -n '3p' <<<"$body")
    [[ -n "$pr_title" ]] || pr_title=$subject
    printf -- '- **%s** ([#%s](https://github.com/%s/pull/%s))\n' \
      "$pr_title" "$pr_num" "$fork_repo" "$pr_num"
  done
  return 0
}

render_changelog() {
  local commit=$1 release_tag=$2 prev_tag entries prs
  echo "## What's new in this release"
  echo
  prev_tag=$(find_previous_release_tag "$commit" "$release_tag")
  if [[ -z "$prev_tag" ]]; then
    printf 'No previous fork release tag was found; see the [full commit history](https://github.com/%s/commits/%s).\n' \
      "$fork_repo" "$commit"
    echo
    return
  fi
  entries=$(list_new_release_entries "$prev_tag" "$commit")
  if [[ -n "$entries" ]]; then
    while IFS= read -r file; do
      [[ -z "$file" ]] && continue
      render_release_entry "$file" "$commit"
    done <<< "$entries"
  else
    prs=$(list_fallback_prs "$prev_tag" "$commit")
    if [[ -n "$prs" ]]; then
      printf '%s\n' "$prs"
    else
      printf 'No fork changes since [`%s`](https://github.com/%s/releases/tag/%s).\n' \
        "$prev_tag" "$fork_repo" "$prev_tag"
    fi
  fi
  echo
}

render_changelog "$commit" "$release_tag"

cat <<EOF
## What this fork adds

EOF
# Rewrites every relative markdown link (i.e. one that doesn't already start
# with http(s), a fragment, or an absolute path) to a commit-pinned blob URL,
# so links keep working on the release page regardless of source path.
FORK_REPO="$fork_repo" COMMIT="$commit" perl -pe '
  s{\]\(([^)]+)\)}{
    my $path = $1;
    $path =~ m{^(?:https?://|\#|/)}
      ? "](" . $path . ")"
      : "](https://github.com/$ENV{FORK_REPO}/blob/$ENV{COMMIT}/" . $path . ")"
  }ge
' "$repo_root/RELEASE_NOTES.md"
echo

cat <<EOF
**Install** as \`codex-shake\` beside your official \`codex\` (pick one):

**Homebrew** (macOS Apple Silicon, or Linux x86_64):

\`\`\`sh
brew install epsalmond/codex-shake/codex-shake
\`\`\`

**Debian/Ubuntu (x86_64)**, download and install the \`.deb\` from this release:

\`\`\`sh
curl -fsSL -o $deb_asset_name \\
  https://github.com/$fork_repo/releases/download/$release_tag/$deb_asset_name
sudo dpkg -i $deb_asset_name
\`\`\`

**curl | sh** (any supported platform, installs to \`~/.local\`):

\`\`\`sh
curl -fsSL https://raw.githubusercontent.com/$fork_repo/$branch/install.sh | sh
\`\`\`

Run \`codex-shake\`. To update: \`codex-shake-update\` (curl|sh installs),
\`brew upgrade epsalmond/codex-shake/codex-shake\` (Homebrew), or reinstall the
\`.deb\` for the latest release (Debian/Ubuntu).

Binaries are stripped; debug symbols are attached as
\`codex-symbols-<target>.tar.gz\` (\`.debug\` files for Linux, \`.dSYM\`
bundles for macOS) if you need to symbolicate a crash.

**Estimate sessions active in the past 7 days (offline; Python 3):** \`codex-shake-estimate --since 168\`.

EOF

cat <<EOF
**Release provenance**

- Source: $source_label
- Workspace version: \`$source_version\`
- Fork release commit: \`$commit\`
- Integrated upstream main commit: \`$upstream_main_sha\`
- Exact stable lineage: \`$stable_lineage\`

EOF

cat <<EOF
Based on upstream \`$stable_lineage\` (exact stable lineage; the included
upstream-main SHA is recorded separately above). Apple Silicon and x86_64
Linux (glibc >= $glibc_version), unsigned; binaries stripped, debug symbols
attached.

EOF

if [[ "$stable_lineage" != none ]]; then
  stable_ref="refs/tags/$stable_lineage"
  if ! git -C "$repo_root" rev-parse --verify "$stable_ref^{commit}" >/dev/null 2>&1; then
    stable_ref="refs/tags/fork-release-upstream/$stable_lineage"
  fi
  cat <<EOF
**Changes since \`$stable_lineage\`:**
[Compare the first-parent source history](https://github.com/$fork_repo/compare/$stable_lineage...$commit)

<details>
<summary>Upstream and fork commits since \`$stable_lineage\`</summary>

EOF
  if git -C "$repo_root" rev-parse --verify "$stable_ref^{commit}" >/dev/null 2>&1; then
    git -C "$repo_root" log --first-parent --reverse --format='- %h %s' "$stable_ref..$commit"
  else
    echo "Stable lineage ref is not present in this checkout; use the compare link above."
  fi
  cat <<EOF

</details>
EOF
else
  cat <<'EOF'
No exact stable tag is reachable from this source commit; the full source
commit above is the authoritative identity.
EOF
fi
