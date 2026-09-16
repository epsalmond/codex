#!/usr/bin/env bash
# Compose a fork-release.yml release body from RELEASE_NOTES.md, with
# commit-pinned links and compact first-parent provenance.
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
sed -E "s#\]\((codex-rs/[^)]+)\)#](https://github.com/$fork_repo/blob/$commit/\1)#g" \
  "$repo_root/RELEASE_NOTES.md"
cat <<EOF

Based on upstream \`$stable_lineage\` (exact stable lineage; the included
upstream-main SHA is recorded separately above). Apple Silicon and x86_64
Linux (glibc >= $glibc_version), unsigned, unstripped.

EOF

if [[ "$stable_lineage" != none ]]; then
  stable_ref="refs/tags/$stable_lineage"
  if ! git -C "$repo_root" rev-parse --verify "$stable_ref^{commit}" >/dev/null 2>&1; then
    stable_ref="refs/tags/fork-release-upstream/$stable_lineage"
  fi
  cat <<EOF
**Changes since \`$stable_lineage\`:**

[Compare the first-parent source history](https://github.com/$fork_repo/compare/$stable_lineage...$commit)

**First-parent commits since \`$stable_lineage\`:**

EOF
  if git -C "$repo_root" rev-parse --verify "$stable_ref^{commit}" >/dev/null 2>&1; then
    git -C "$repo_root" log --first-parent --reverse --format='- %h %s' "$stable_ref..$commit"
  else
    echo "Stable lineage ref is not present in this checkout; use the compare link above."
  fi
else
  cat <<'EOF'
No exact stable tag is reachable from this source commit; the full source
commit above is the authoritative identity.
EOF
fi
