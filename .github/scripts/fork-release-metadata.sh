#!/usr/bin/env bash
# Resolve a fork-release event to immutable, validated release metadata.
# This script only reads the event commit and local Git history. The optional
# upstream fetch updates a local tracking ref so the integrated upstream parent
# can be checked against the public openai/codex main line.
set -euo pipefail

fork_branch="eric/local-features"
repo_root=""
event_name=""
event_ref=""
event_sha=""
event_before=""
upstream_url=""
output=""

usage() {
  printf 'usage: %s --repo-root DIR --event-name NAME --event-ref REF --event-sha SHA --event-before SHA --output FILE [--upstream-url URL]\n' "$0" >&2
}

die() {
  printf 'fork-release metadata: %s\n' "$1" >&2
  exit 1
}

while (($# > 0)); do
  case "$1" in
    --repo-root)
      repo_root=${2:?--repo-root requires a value}
      shift 2
      ;;
    --event-name)
      event_name=${2:?--event-name requires a value}
      shift 2
      ;;
    --event-ref)
      event_ref=${2:?--event-ref requires a value}
      shift 2
      ;;
    --event-sha)
      event_sha=${2:?--event-sha requires a value}
      shift 2
      ;;
    --event-before)
      event_before=${2:?--event-before requires a value}
      shift 2
      ;;
    --upstream-url)
      upstream_url=${2:?--upstream-url requires a value}
      shift 2
      ;;
    --output)
      output=${2:?--output requires a value}
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      usage
      die "unknown argument: $1"
      ;;
  esac
done

[[ -n "$repo_root" && -n "$event_name" && -n "$event_ref" && -n "$event_sha" && -n "$output" ]] || {
  usage
  exit 2
}
[[ "$event_name" == push ]] || die "unsupported event: $event_name"
[[ "$event_sha" =~ ^[0-9a-f]{40}$ ]] || die "event SHA is not a full lowercase commit SHA"

git_cmd=(git -C "$repo_root")
release_sha=$("${git_cmd[@]}" rev-parse --verify "$event_sha^{commit}") || die "event SHA is not a commit"
[[ "$release_sha" == "$event_sha" ]] || die "event SHA does not resolve exactly"

read_source_version() {
  "${git_cmd[@]}" show "$release_sha:codex-rs/Cargo.toml" |
    awk '
      /^\[workspace\.package\]$/ { in_workspace = 1; next }
      /^\[/ { in_workspace = 0 }
      in_workspace && /^version = / { print; exit }
    ' |
    sed -n 's/^version = "\([^"]*\)".*/\1/p'
}

source_version=$(read_source_version)
[[ -n "$source_version" ]] || die "could not read workspace package version at $release_sha"

parent_line=$("${git_cmd[@]}" rev-list --parents -n 1 "$release_sha")
parent_count=$(printf '%s\n' "$parent_line" | awk '{ print NF - 1 }')
included_upstream_main_sha=none
source_kind=tag

case "$event_ref" in
  "refs/heads/$fork_branch")
    source_kind=branch-merge
    [[ "$event_before" =~ ^[0-9a-f]{40}$ ]] || die "branch event has no full before SHA"
    [[ "$event_before" != 0000000000000000000000000000000000000000 ]] || die "branch creation is not a fast-forward update"
    "${git_cmd[@]}" rev-parse --verify "$event_before^{commit}" >/dev/null || die "before SHA is not available"
    "${git_cmd[@]}" merge-base --is-ancestor "$event_before" "$release_sha" ||
      die "branch event is not a fast-forward update"
    [[ "$parent_count" -eq 2 ]] || die "branch event must point to a two-parent merge commit"
    ;;
  refs/tags/*)
    tag=${event_ref#refs/tags/}
    [[ "$tag" =~ ^local-features-v[0-9]+\.[0-9]+\.[0-9]+(-main)?(-r[0-9]+(\.[0-9a-fA-F]+)+|-[0-9a-fA-F]{12})$ ]] ||
      die "tag is not a supported local-features-v* identity: $tag"
    tag_sha=$("${git_cmd[@]}" rev-parse --verify "refs/tags/$tag^{commit}") ||
      die "tag does not resolve to a commit: $tag"
    [[ "$tag_sha" == "$release_sha" ]] ||
      die "tag $tag resolves to $tag_sha, event SHA is $release_sha"
    release_tag=$tag
    ;;
  *)
    die "unexpected event ref: $event_ref"
    ;;
esac

if [[ "$source_kind" == branch-merge ]]; then
  [[ -n "$upstream_url" ]] || die "branch release needs public openai/main for provenance"
  upstream_ref=refs/remotes/fork-release-upstream/main
  "${git_cmd[@]}" fetch --no-tags --quiet "$upstream_url" \
    "+refs/heads/main:$upstream_ref" \
    "+refs/tags/rust-v*:refs/tags/fork-release-upstream/rust-v*" ||
    die "could not fetch public openai/main and exact stable tags"
  included_upstream_main_sha=$("${git_cmd[@]}" merge-base "$release_sha" "$upstream_ref") ||
    die "public openai/main has no common history with $release_sha"
  [[ -n "$included_upstream_main_sha" ]] ||
    die "public openai/main merge-base is empty"
elif [[ -n "$upstream_url" ]]; then
  upstream_ref=refs/remotes/fork-release-upstream/main
  if "${git_cmd[@]}" fetch --no-tags --quiet "$upstream_url" "+refs/heads/main:$upstream_ref"; then
    if [[ "$parent_count" -eq 2 ]]; then
      included_upstream_main_sha=$("${git_cmd[@]}" merge-base "$release_sha" "$upstream_ref" || true)
      included_upstream_main_sha=${included_upstream_main_sha:-none}
    fi
  else
    printf 'fork-release metadata: public openai/main was unavailable; upstream provenance is none\\n' >&2
  fi
fi

if [[ "$source_kind" == branch-merge ]]; then
  short_sha=$("${git_cmd[@]}" rev-parse --short=12 "$release_sha")
  commit_epoch=$("${git_cmd[@]}" show -s --format=%ct "$release_sha")
  commit_timestamp=$(python3 - "$commit_epoch" <<'PY'
import datetime
import sys

print(datetime.datetime.fromtimestamp(int(sys.argv[1]), datetime.timezone.utc).strftime("%Y%m%d%H%M%S"))
PY
)
  release_tag="local-features-v${source_version}-main-r${commit_timestamp}.${short_sha}"
fi

if existing_tag_sha=$("${git_cmd[@]}" rev-parse --verify "refs/tags/$release_tag^{commit}" 2>/dev/null); then
  [[ "$existing_tag_sha" == "$release_sha" ]] ||
    die "existing tag $release_tag resolves to $existing_tag_sha, expected $release_sha"
fi

stable_lineage=$(
  "${git_cmd[@]}" for-each-ref --merged "$release_sha" \
    --sort=-version:refname --format='%(refname)' refs/tags refs/tags/fork-release-upstream |
    while IFS= read -r tag; do
      tag=${tag#refs/tags/fork-release-upstream/}
      tag=${tag#refs/tags/}
      if [[ "$tag" =~ ^rust-v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        printf '%s\n' "$tag"
      fi
    done
)
stable_lineage=$(printf '%s\n' "$stable_lineage" | awk 'NR == 1 { first = $0 } END { print first }')
stable_lineage=${stable_lineage:-none}

{
  printf 'release_sha=%s\n' "$release_sha"
  printf 'release_tag=%s\n' "$release_tag"
  printf 'source_version=%s\n' "$source_version"
  printf 'stable_lineage=%s\n' "$stable_lineage"
  printf 'included_upstream_main_sha=%s\n' "$included_upstream_main_sha"
  printf 'source_kind=%s\n' "$source_kind"
} >> "$output"
