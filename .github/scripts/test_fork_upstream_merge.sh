#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
merge_script="$script_dir/fork-upstream-merge.sh"
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT

make_fixture() {
  local name=$1 upstream_change=$2
  local root="$fixture/$name"
  mkdir -p "$root"
  git -C "$root" init -q
  git -C "$root" config user.email test@example.com
  git -C "$root" config user.name "Merge Test"
  printf 'base\n' > "$root/README.md"
  printf 'base\n' > "$root/upstream.txt"
  git -C "$root" add .
  git -C "$root" commit -q -m base
  local base_sha
  base_sha=$(git -C "$root" rev-parse HEAD)

  git -C "$root" branch -M eric/local-features
  printf 'fork\n' > "$root/README.md"
  git -C "$root" add README.md
  git -C "$root" commit -q -m fork
  local fork_sha
  fork_sha=$(git -C "$root" rev-parse HEAD)

  git -C "$root" switch -q -c upstream/main "$base_sha"
  if [[ "$upstream_change" == conflict ]]; then
    printf 'upstream\n' > "$root/README.md"
    git -C "$root" add README.md
  else
    printf 'upstream\n' > "$root/upstream.txt"
    git -C "$root" add upstream.txt
  fi
  git -C "$root" commit -q -m upstream
  local upstream_sha
  upstream_sha=$(git -C "$root" rev-parse HEAD)
  git -C "$root" tag rust-v1.2.3 "$upstream_sha"

  local upstream_remote="$root/upstream.git"
  local fork_remote="$root/fork.git"
  git init --bare -q "$upstream_remote"
  git -C "$root" push -q "$upstream_remote" \
    upstream/main:refs/heads/main refs/tags/rust-v1.2.3:refs/tags/rust-v1.2.3
  git init --bare -q "$fork_remote"
  git -C "$root" push -q "$fork_remote" \
    eric/local-features:refs/heads/eric/local-features
  git --git-dir="$fork_remote" symbolic-ref HEAD refs/heads/eric/local-features
  git -C "$root" remote add origin "$fork_remote"

  printf '%s\n%s\n%s\n%s\n' "$root" "$fork_remote" "$upstream_remote" "$fork_sha" \
    > "$fixture/$name.env"
  printf '%s\n' "$upstream_sha" >> "$fixture/$name.env"
}

read_fixture() {
  local name=$1
  mapfile -t fixture_values < "$fixture/$name.env"
  root=${fixture_values[0]}
  fork_remote=${fixture_values[1]}
  upstream_remote=${fixture_values[2]}
  fork_sha=${fixture_values[3]}
  upstream_sha=${fixture_values[4]}
}

make_fixture clean clean
read_fixture clean
output="$fixture/clean-output"
bash "$merge_script" \
  --mode prepare-upstream \
  --repo-root "$root" \
  --upstream-url "$upstream_remote" \
  --upstream-tag rust-v1.2.3 \
  --upstream-sha "$upstream_sha" \
  --fork-sha "$fork_sha" \
  --output "$output"
grep -Fxq 'result=ready' "$output"
candidate_sha=$(sed -n 's/^candidate_sha=//p' "$output")
candidate_branch=$(sed -n 's/^candidate_branch=//p' "$output")
[[ $(git -C "$root" rev-list --parents -n 1 "$candidate_sha" | wc -w) -eq 3 ]]
[[ "$candidate_branch" == "fork-upstream-candidate/rust-v1.2.3/${fork_sha:0:12}" ]]

recovery_branch="fork-recovery/rust-v1.2.3/${fork_sha:0:12}"
git -C "$root" push -q origin "$candidate_sha:refs/heads/$recovery_branch"
recovery_output="$fixture/recovery-output"
bash "$merge_script" \
  --mode prepare-recovery \
  --repo-root "$root" \
  --upstream-url "$upstream_remote" \
  --upstream-tag rust-v1.2.3 \
  --upstream-sha "$upstream_sha" \
  --fork-sha "$fork_sha" \
  --candidate-sha "$candidate_sha" \
  --recovery-branch "$recovery_branch" \
  --output "$recovery_output"
grep -Fxq 'result=ready' "$recovery_output"
if bash "$merge_script" \
  --mode prepare-recovery \
  --repo-root "$root" \
  --upstream-url "$upstream_remote" \
  --upstream-tag rust-v1.2.3 \
  --upstream-sha "$upstream_sha" \
  --fork-sha "$fork_sha" \
  --candidate-sha "$candidate_sha" \
  --recovery-branch "fork-recovery/rust-v1.2.3/${fork_sha:0:11}x" \
  --output "$fixture/wrong-recovery-output"; then
  echo "expected recovery branch naming mismatch to fail" >&2
  exit 1
fi

bash "$merge_script" \
  --mode land \
  --repo-root "$root" \
  --upstream-url "$upstream_remote" \
  --upstream-tag rust-v1.2.3 \
  --upstream-sha "$upstream_sha" \
  --fork-sha "$fork_sha" \
  --candidate-branch "$recovery_branch" \
  --candidate-sha "$candidate_sha"
landed_sha=$(git ls-remote "$fork_remote" refs/heads/eric/local-features | awk '{ print $1 }')
[[ "$landed_sha" == "$candidate_sha" ]]

make_fixture stale clean
read_fixture stale
stale_output="$fixture/stale-output"
bash "$merge_script" \
  --mode prepare-upstream \
  --repo-root "$root" \
  --upstream-url "$upstream_remote" \
  --upstream-tag rust-v1.2.3 \
  --upstream-sha "$upstream_sha" \
  --fork-sha "$fork_sha" \
  --output "$stale_output"
stale_candidate=$(sed -n 's/^candidate_sha=//p' "$stale_output")
stale_candidate_branch=$(sed -n 's/^candidate_branch=//p' "$stale_output")
other="$fixture/other-checkout"
git clone -q "$fork_remote" "$other"
git -C "$other" config user.email test@example.com
git -C "$other" config user.name "Concurrent Writer"
git -C "$other" commit --allow-empty -q -m "advance fork while candidate validates"
git -C "$other" push -q origin eric/local-features
if bash "$merge_script" \
  --mode land \
  --repo-root "$root" \
  --upstream-url "$upstream_remote" \
  --upstream-tag rust-v1.2.3 \
  --upstream-sha "$upstream_sha" \
  --fork-sha "$fork_sha" \
  --candidate-branch "$stale_candidate_branch" \
  --candidate-sha "$stale_candidate"; then
  echo "expected landing against a moved fork branch to fail" >&2
  exit 1
fi

make_fixture conflict conflict
read_fixture conflict
conflict_output="$fixture/conflict-output"
bash "$merge_script" \
  --mode prepare-upstream \
  --repo-root "$root" \
  --upstream-url "$upstream_remote" \
  --upstream-tag rust-v1.2.3 \
  --upstream-sha "$upstream_sha" \
  --fork-sha "$fork_sha" \
  --output "$conflict_output"
grep -Fxq 'result=conflict' "$conflict_output"
test -z "$(git -C "$root" status --porcelain --untracked-files=no)"
[[ $(git -C "$root" rev-parse HEAD) == "$fork_sha" ]]

echo "fork-upstream merge tests passed"
