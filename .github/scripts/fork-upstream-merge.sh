#!/usr/bin/env bash
# Prepare, verify, and conditionally land one exact upstream merge.
set -euo pipefail

mode=""
repo_root=""
fork_branch="eric/local-features"
upstream_url="https://github.com/openai/codex.git"
upstream_tag=""
upstream_sha=""
fork_sha=""
candidate_sha=""
candidate_branch=""
recovery_branch=""
output=""

usage() {
  printf 'usage: %s --mode MODE --repo-root DIR --upstream-tag TAG --upstream-sha SHA --fork-sha SHA [options]\n' "$0" >&2
}

die() {
  printf 'fork-upstream-merge: %s\n' "$1" >&2
  exit 1
}

while (($# > 0)); do
  case "$1" in
    --mode)
      mode=${2:?--mode requires a value}
      shift 2
      ;;
    --repo-root)
      repo_root=${2:?--repo-root requires a value}
      shift 2
      ;;
    --fork-branch)
      fork_branch=${2:?--fork-branch requires a value}
      shift 2
      ;;
    --upstream-url)
      upstream_url=${2:?--upstream-url requires a value}
      shift 2
      ;;
    --upstream-tag)
      upstream_tag=${2:?--upstream-tag requires a value}
      shift 2
      ;;
    --upstream-sha)
      upstream_sha=${2:?--upstream-sha requires a value}
      shift 2
      ;;
    --fork-sha)
      fork_sha=${2:?--fork-sha requires a value}
      shift 2
      ;;
    --candidate-sha)
      candidate_sha=${2-}
      shift 2
      ;;
    --candidate-branch)
      candidate_branch=${2-}
      shift 2
      ;;
    --recovery-branch)
      recovery_branch=${2-}
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

[[ -n "$mode" && -n "$repo_root" && -n "$upstream_tag" && -n "$upstream_sha" && -n "$fork_sha" ]] || {
  usage
  exit 2
}
[[ "$upstream_tag" =~ ^rust-v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "upstream tag is not stable: $upstream_tag"
[[ "$upstream_sha" =~ ^[0-9a-f]{40}$ ]] || die "upstream SHA is not a full commit SHA"
[[ "$fork_sha" =~ ^[0-9a-f]{40}$ ]] || die "fork SHA is not a full commit SHA"

git_cmd=(git -C "$repo_root")

resolve_commit() {
  local revision=$1
  local resolved
  resolved=$("${git_cmd[@]}" rev-parse --verify "$revision^{commit}") || return 1
  [[ "$resolved" == "$revision" ]] || return 1
  printf '%s\n' "$resolved"
}

fetch_upstream_tag() {
  local ref="refs/tags/upstream/$upstream_tag"
  "${git_cmd[@]}" fetch --quiet --no-tags "$upstream_url" \
    "+refs/tags/$upstream_tag:$ref" || die "could not fetch upstream tag $upstream_tag"
  local resolved
  resolved=$("${git_cmd[@]}" rev-parse --verify "$ref^{commit}") ||
    die "upstream tag does not resolve to a commit: $upstream_tag"
  [[ "$resolved" == "$upstream_sha" ]] ||
    die "upstream tag $upstream_tag resolves to $resolved, expected $upstream_sha"
}

verify_candidate() {
  resolve_commit "$candidate_sha" >/dev/null || die "candidate SHA is not an available commit"
  local parent_line actual_fork_sha actual_upstream_sha extra
  parent_line=$("${git_cmd[@]}" rev-list --parents -n 1 "$candidate_sha")
  read -r _ actual_fork_sha actual_upstream_sha extra <<< "$parent_line"
  [[ -z "${extra:-}" && "$actual_fork_sha" == "$fork_sha" && "$actual_upstream_sha" == "$upstream_sha" ]] ||
    die "candidate must have exactly fork_sha then upstream_sha as its parents"
}

write_ready_output() {
  [[ -n "$output" ]] || die "--output is required for preparation modes"
  {
    printf 'result=ready\n'
    printf 'candidate_sha=%s\n' "$candidate_sha"
    printf 'candidate_branch=%s\n' "$candidate_branch"
  } >> "$output"
}

fetch_upstream_tag

case "$mode" in
  verify)
    [[ -n "$candidate_sha" ]] || die "verify mode needs --candidate-sha"
    verify_candidate
    ;;
  prepare-upstream)
    [[ -n "$output" ]] || die "prepare-upstream needs --output"
    candidate_branch="fork-upstream-candidate/$upstream_tag/${fork_sha:0:12}"
    "${git_cmd[@]}" fetch --quiet --no-tags origin \
      "+refs/heads/$fork_branch:refs/remotes/origin/$fork_branch" ||
      die "could not fetch $fork_branch"
    current_fork_sha=$("${git_cmd[@]}" rev-parse --verify "refs/remotes/origin/$fork_branch^{commit}") ||
      die "current $fork_branch is unavailable"
    if [[ "$current_fork_sha" != "$fork_sha" ]]; then
      if "${git_cmd[@]}" merge-base --is-ancestor "$upstream_sha" "$current_fork_sha"; then
        printf 'result=already\n' >> "$output"
        exit 0
      fi
      die "$fork_branch moved from expected $fork_sha to $current_fork_sha"
    fi
    resolve_commit "$fork_sha" >/dev/null || die "fork SHA is not an available commit"
    existing_candidate=""
    if "${git_cmd[@]}" fetch --quiet --no-tags origin \
      "+refs/heads/$candidate_branch:refs/remotes/origin/$candidate_branch" 2>/dev/null; then
      existing_candidate=$("${git_cmd[@]}" rev-parse --verify "refs/remotes/origin/$candidate_branch^{commit}") ||
        die "candidate branch is not a commit: $candidate_branch"
      candidate_sha=$existing_candidate
      verify_candidate
    fi
    "${git_cmd[@]}" checkout --detach "$fork_sha" >/dev/null
    "${git_cmd[@]}" config user.name "github-actions[bot]"
    "${git_cmd[@]}" config user.email "41898282+github-actions[bot]@users.noreply.github.com"
    if "${git_cmd[@]}" merge --no-ff --no-edit "$upstream_sha"; then
      computed_candidate=$("${git_cmd[@]}" rev-parse --verify 'HEAD^{commit}')
      candidate_sha=$computed_candidate
      verify_candidate
      if [[ -n "$existing_candidate" ]]; then
        computed_tree=$("${git_cmd[@]}" show -s --format=%T "$computed_candidate")
        existing_tree=$("${git_cmd[@]}" show -s --format=%T "$existing_candidate")
        [[ "$computed_tree" == "$existing_tree" ]] ||
          die "existing candidate branch has a different merge tree: $candidate_branch"
        candidate_sha=$existing_candidate
      else
        "${git_cmd[@]}" push origin "$computed_candidate:refs/heads/$candidate_branch"
        candidate_sha=$computed_candidate
      fi
      write_ready_output
    elif [[ -n $("${git_cmd[@]}" ls-files --unmerged) ]]; then
      "${git_cmd[@]}" merge --abort
      printf 'result=conflict\n' >> "$output"
    else
      die "upstream merge failed without unresolved conflicts"
    fi
    ;;
  prepare-recovery)
    [[ -n "$output" && -n "$candidate_sha" && -n "$recovery_branch" ]] ||
      die "prepare-recovery needs --candidate-sha, --recovery-branch, and --output"
    expected_recovery_branch="fork-recovery/$upstream_tag/${fork_sha:0:12}"
    [[ "$recovery_branch" == "$expected_recovery_branch" ]] ||
      die "recovery branch must be $expected_recovery_branch"
    "${git_cmd[@]}" fetch --quiet --no-tags origin \
      "+refs/heads/$fork_branch:refs/remotes/origin/$fork_branch" \
      "+refs/heads/$recovery_branch:refs/remotes/origin/$recovery_branch" ||
      die "could not fetch the fork and recovery branches"
    current_fork_sha=$("${git_cmd[@]}" rev-parse --verify "refs/remotes/origin/$fork_branch^{commit}") ||
      die "current $fork_branch is unavailable"
    [[ "$current_fork_sha" == "$fork_sha" ]] ||
      die "$fork_branch moved from expected $fork_sha to $current_fork_sha"
    recovery_sha=$("${git_cmd[@]}" rev-parse --verify "refs/remotes/origin/$recovery_branch^{commit}") ||
      die "recovery branch is unavailable: $recovery_branch"
    [[ "$recovery_sha" == "$candidate_sha" ]] ||
      die "recovery branch points to $recovery_sha, expected $candidate_sha"
    verify_candidate
    candidate_branch=$recovery_branch
    write_ready_output
    ;;
  land)
    [[ -n "$candidate_sha" && -n "$candidate_branch" ]] ||
      die "land mode needs --candidate-sha and --candidate-branch"
    "${git_cmd[@]}" fetch --quiet --no-tags origin \
      "+refs/heads/$candidate_branch:refs/remotes/origin/$candidate_branch" ||
      die "candidate branch is unavailable: $candidate_branch"
    fetched_candidate=$("${git_cmd[@]}" rev-parse --verify "refs/remotes/origin/$candidate_branch^{commit}") ||
      die "candidate branch is not a commit: $candidate_branch"
    [[ "$fetched_candidate" == "$candidate_sha" ]] ||
      die "candidate branch points to $fetched_candidate, expected $candidate_sha"
    verify_candidate
    "${git_cmd[@]}" fetch --quiet --no-tags origin \
      "+refs/heads/$fork_branch:refs/remotes/origin/$fork_branch" ||
      die "could not recheck $fork_branch before landing"
    current_fork_sha=$("${git_cmd[@]}" rev-parse --verify "refs/remotes/origin/$fork_branch^{commit}") ||
      die "current $fork_branch is unavailable"
    [[ "$current_fork_sha" == "$fork_sha" ]] ||
      die "$fork_branch moved from expected $fork_sha to $current_fork_sha before landing"
    "${git_cmd[@]}" push \
      "--force-with-lease=refs/heads/$fork_branch:$fork_sha" \
      origin "$candidate_sha:refs/heads/$fork_branch"
    ;;
  *)
    usage
    die "unknown mode: $mode"
    ;;
esac
