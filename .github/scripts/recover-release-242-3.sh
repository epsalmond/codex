#!/usr/bin/env bash
set -euo pipefail

repo=epsalmond/codex
run=45419143302
attempt=3
workflow=357236570
source_sha=519814bd087017808daaa05f4e247275c8540031
release_tag=local-features-v0.172.0-main-r20261124135104.0000000000002972519814bd087000000000000000f200000003
staging_tag=staging-shake-recovery-0.172.0-242.3
previous_tag=local-features-v0.169.1-main-r20261118032617.00000000000029710edcfc480ecd00000000000000f000000001
tap_sha=07a2d4a3f13670ebf2d40507dfd7b2f1b3815d1e
publisher='epsalmond-codex-releases[bot]'
api="repos/$repo"
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
mac_manifest="$script_dir/recover-release-242-3-mac.json"

is_recovery_merge() {
  local head=$1 commit first_parent second_parent extra changed_paths
  [[ "$head" =~ ^[0-9a-f]{40}$ ]] || return 1
  read -r commit first_parent second_parent extra < <(git rev-list --parents -n 1 "$head")
  [[ "$commit" == "$head" && "$first_parent" == "$source_sha" && -n "$second_parent" && -z "$extra" ]] || return 1
  test "$(git rev-list --parents -n 1 "$second_parent")" = "$second_parent $source_sha" || return 1
  test "$(git show -s --format=%s "$second_parent")" = 'Release Shake 0.172.0 build 242.3 from verified artifacts' || return 1
  test "$(git rev-parse "$head^{tree}")" = "$(git rev-parse "$second_parent^{tree}")" || return 1
  # Only the reviewed build and test fixes may accompany the recovery workflow.
  test "$(git rev-parse "$head:.github/workflows/fork-ci.yml")" = d03e9ddf0d116404a20b58c2d85761f836930fbd || return 1
  test "$(git rev-parse "$head:codex-rs/code-mode-host/src/grpc/session.rs")" = 181118465e2a9724f7ccf8bec8356ab79c06afed || return 1
  test "$(git rev-parse "$head:codex-rs/code-mode-host/tests/stdio.rs")" = fdca4a4902f4a389969f5b0f31e4ae922f705709 || return 1
  test "$(git rev-parse "$head:codex-rs/core/src/session/session.rs")" = 9ae9704d963929bbbba17ed8c30fb45e3ad7295e || return 1
  test "$(git rev-parse "$head:codex-rs/core/src/session/step_context.rs")" = e91cbc77987d616bf3612c23e95a1ece222d0ade || return 1
  test "$(git rev-parse "$head:codex-rs/core/src/tools/handlers/extension_tools.rs")" = ff0b5e5c7b73e219f2989917139e4774581aea7c || return 1
  test "$(git rev-parse "$head:codex-rs/core/tests/suite/scenarios.rs")" = 195eea19fbe9825779ca71a0077df865cdf45593 || return 1
  test "$(git rev-parse "$head:codex-rs/core/tests/suite/worktree_trust.rs")" = 70f1e75136dc42c21eafe8342968a2a79a0864d1 || return 1
  test "$(git rev-parse "$head:codex-rs/exec/src/event_processor_with_jsonl_output_tests.rs")" = 115bd678f4cac30807b7a6dd6eb3350a1a9cb4e3 || return 1
  test "$(git rev-parse "$head:codex-rs/ext/goal/tests/injected_steering.rs")" = a022051c08f0087a8b82a38b510281e6ea47b2d1 || return 1
  test "$(git rev-parse "$head:codex-rs/ext/guardian-v3/tests/retrieval.rs")" = fc00c61a0c2e46afa2f4e1fce1fc76f70bc59a77 || return 1
  test "$(git rev-parse "$head:codex-rs/history/src/rollout_payload.rs")" = 1e6eb313adba1af6b21d66ea8c9fdda001d27155 || return 1
  test "$(git rev-parse "$head:codex-rs/thread-store/src/local/model_context_tests.rs")" = 94a700e597010148493e19e6ce9c5cd30dce322f || return 1
  test "$(git rev-parse "$head:codex-rs/thread-store/src/local/rollout_lineage.rs")" = 6b5c7de62dfa436fe6a5d07fb0ffabf531fd0600 || return 1
  test "$(git rev-parse "$head:scripts/smoke_codex_package.py")" = 76f903711f61873ce5d017cca45721641c292173 || return 1

  changed_paths=$(git diff --name-only --no-renames "$source_sha" "$head" | LC_ALL=C sort) || return 1
  test "$changed_paths" = "$(printf '%s\n' \
    .github/scripts/fork-release-build.sh \
    .github/scripts/recover-release-242-3-mac.json \
    .github/scripts/recover-release-242-3.sh \
    .github/scripts/test_fork_release_metadata.sh \
    .github/workflows/fork-ci.yml \
    .github/workflows/fork-release.yml \
    codex-rs/code-mode-host/src/grpc/session.rs \
    codex-rs/code-mode-host/tests/stdio.rs \
    codex-rs/core/src/session/session.rs \
    codex-rs/core/src/session/step_context.rs \
    codex-rs/core/src/tools/handlers/extension_tools.rs \
    codex-rs/core/tests/suite/scenarios.rs \
    codex-rs/core/tests/suite/worktree_trust.rs \
    codex-rs/exec/src/event_processor_with_jsonl_output_tests.rs \
    codex-rs/ext/goal/tests/injected_steering.rs \
    codex-rs/ext/guardian-v3/tests/retrieval.rs \
    codex-rs/history/src/rollout_payload.rs \
    codex-rs/thread-store/src/local/model_context_tests.rs \
    codex-rs/thread-store/src/local/rollout_lineage.rs \
    scripts/smoke_codex_package.py)"
}

check_hash() {
  printf '%s  %s\n' "$1" "$2" | sha256sum --check -
}

download_linux() {
  local target=$1 job=$2 id=$3 digest=$4 jobs_json=$5
  local zip="dist/recovery-$target.zip" created_start created_end
  created_start=$(jq -er --argjson job "$job" '
    .jobs[] | select(.id == $job) | .steps[]
    | select(.name | startswith("Run actions/upload-artifact@")) | .started_at' <<< "$jobs_json")
  created_end=$(jq -er --argjson job "$job" '
    .jobs[] | select(.id == $job) | .steps[]
    | select(.name | startswith("Run actions/upload-artifact@")) | .completed_at' <<< "$jobs_json")

  gh api "$api/actions/artifacts/$id" | jq -e \
    --argjson id "$id" --argjson run "$run" --arg name "release-$target" \
    --arg sha "$source_sha" --arg digest "sha256:$digest" \
    --arg start "$created_start" --arg finish "$created_end" '
      .id == $id and .name == $name and .expired == false and .digest == $digest
      and .workflow_run.id == $run and .workflow_run.head_sha == $sha
      and (.created_at >= $start and .created_at <= $finish)' > /dev/null

  gh api "$api/actions/artifacts/$id/zip" > "$zip"
  check_hash "$digest" "$zip"
  python3 - "$zip" "$target" <<'PY'
from pathlib import Path
import shutil
import sys
import zipfile

zip_path, target = sys.argv[1:]
expected = {
    f"codex-{target}.tar.gz",
    f"codex-symbols-{target}.tar.gz",
    f"glibc-version-{target}.txt",
}
with zipfile.ZipFile(zip_path) as archive:
    if set(archive.namelist()) != expected or len(archive.infolist()) != len(expected):
        raise SystemExit("Linux Actions artifact contains unexpected or duplicate paths")
    for name in sorted(expected):
        with archive.open(name) as source, (Path("dist") / name).open("xb") as destination:
            shutil.copyfileobj(source, destination)
PY
}

verify_linux() {
  local jobs_json
  gh api "$api/actions/runs/$run/attempts/$attempt" | jq -e \
    --argjson run "$run" --argjson workflow "$workflow" --argjson attempt "$attempt" \
    --arg repo "$repo" --arg sha "$source_sha" '
      .id == $run and .workflow_id == $workflow and .path == ".github/workflows/fork-release.yml"
      and .head_sha == $sha and .head_branch == "main" and .repository.full_name == $repo
      and .event == "push" and .run_attempt == $attempt and .run_number == 242
      and .status == "completed" and .conclusion == "failure"' > /dev/null

  jobs_json=$(gh api "$api/actions/runs/$run/attempts/$attempt/jobs?per_page=50")
  jq -e --argjson run "$run" --argjson attempt "$attempt" --arg sha "$source_sha" '
    def job($id; $name):
      .jobs[] | select(.id == $id and .name == $name and .run_id == $run
        and .run_attempt == $attempt and .head_sha == $sha);
    def step($name): any(.steps[]; .name == $name and .conclusion == "success");
    def linux($id; $name):
      job($id; $name) | .conclusion == "success" and step("Verify binary version")
      and step("Archive symbols and strip binaries") and step("Record glibc version (Linux)")
      and step("Package archive")
      and any(.steps[]; (.name | startswith("Run actions/upload-artifact@")) and .conclusion == "success");
    (job(138644313574; "Prepare exact release identity") | .conclusion == "success")
    and (job(138645042929; "Validate workflow and updater / Validate exact upstream merge") | .conclusion == "success")
    and linux(138668518074; "Build - x86_64-unknown-linux-gnu")
    and linux(138668517966; "Build - aarch64-unknown-linux-gnu")
    and (job(138668518021; "Build - aarch64-apple-darwin") | .conclusion == "cancelled" and (.steps | length) == 0)
    and (job(138717352569; "Publish release") | .conclusion == "skipped")' <<< "$jobs_json" >/dev/null

  mkdir -p dist
  download_linux x86_64-unknown-linux-gnu 138668518074 14957991245 \
    d0a86d309f8260e916c1a402d2c8d8b9d155d0ea7d20941da4b0956d676b6c25 "$jobs_json"
  download_linux aarch64-unknown-linux-gnu 138668517966 14956401737 \
    e432d95d2703df570006efc00ebc7810f3d0a273b99d4c238e282470837074cb "$jobs_json"
}

check_mac_manifest() {
  jq -e '
    (.draft_release_id | type == "number" and . > 0) and
    (.assets | length == 3 and ([.[].name] | sort) == [
      "codex-aarch64-apple-darwin.tar.gz", "codex-symbols-aarch64-apple-darwin.tar.gz",
      "native-macos-receipt.json"
    ] and all(.[]; (.id | type == "number" and . > 0) and (.sha256 | test("^[0-9a-f]{64}$"))))' \
    "$mac_manifest" >/dev/null
}

download_mac() {
  local draft_id draft_json name id digest
  check_mac_manifest
  draft_id=$(jq -er '.draft_release_id' "$mac_manifest")
  draft_json=$(gh api --method GET "$api/releases/$draft_id")
  jq -e --argjson id "$draft_id" --arg tag "$staging_tag" --arg sha "$source_sha" '
    .id == $id and .draft == true and .tag_name == $tag and .target_commitish == $sha
    and (.assets | length) == 3' <<< "$draft_json" >/dev/null

  mkdir -p dist
  while IFS=$'\t' read -r name id digest; do
    jq -e --arg name "$name" --argjson id "$id" --arg digest "sha256:$digest" '
      any(.assets[]; .id == $id and .name == $name and .state == "uploaded"
        and .digest == $digest and .size > 0)' <<< "$draft_json" >/dev/null
    gh api --method GET -H 'Accept: application/octet-stream' "$api/releases/assets/$id" > "dist/$name"
  done < <(jq -r '.assets[] | [.name, .id, .sha256] | @tsv' "$mac_manifest")

  verify_mac_files
}

verify_mac_files() {
  local name digest package_digest symbols_digest binary actual_digest
  check_mac_manifest
  while IFS=$'\t' read -r name digest; do
    check_hash "$digest" "dist/$name"
    case "$name" in
      codex-aarch64-apple-darwin.tar.gz) package_digest=$digest ;;
      codex-symbols-aarch64-apple-darwin.tar.gz) symbols_digest=$digest ;;
    esac
  done < <(jq -r '.assets[] | [.name, .sha256] | @tsv' "$mac_manifest")

  jq -e --arg tag "$release_tag" --arg sha "$source_sha" \
    --arg package "$package_digest" --arg symbols "$symbols_digest" '
    def uuid: test("^[A-Fa-f0-9]{8}(-[A-Fa-f0-9]{4}){3}-[A-Fa-f0-9]{12}$");
    .source_sha == $sha and .release_tag == $tag and .target == "aarch64-apple-darwin"
    and .source_head == $sha and .codex_fork_source_sha == $sha and .codex_fork_release_tag == $tag
    and (.source_worktree_changes | sort) == [" M codex-rs/Cargo.lock", " M codex-rs/Cargo.toml"]
    and .build_host == "macOS arm64" and .build_target == "aarch64-apple-darwin"
    and .build_profile == "release" and .split_debuginfo == "packed"
    and .cli_version == ["codex-cli 0.172.0", "Shake feature version: 0.4.0", "CLI build: 242.3",
      "Source commit: " + $sha, "Release tag: " + $tag]
    and .cli_short_version == "codex-cli 0.172.0" and .package_smoke == "pass"
    and (.rustc | startswith("rustc 1.95.0 "))
    and (.codex_uuid | uuid) and .codex_uuid == .codex_dsym_uuid
    and (.codex_code_mode_host_uuid | uuid)
    and .codex_code_mode_host_uuid == .codex_code_mode_host_dsym_uuid
    and .package_sha256 == $package and .symbols_sha256 == $symbols' \
    dist/native-macos-receipt.json >/dev/null

  for binary in codex codex-code-mode-host; do
    digest=$(jq -er --arg key "${binary//-/_}_sha256" '
      .[$key] | select(test("^[0-9a-f]{64}$"))' dist/native-macos-receipt.json)
    actual_digest=$(tar -xOzf dist/codex-aarch64-apple-darwin.tar.gz "bin/$binary" | sha256sum)
    test "${actual_digest%% *}" = "$digest"
  done
}

verify_state() {
  local remote_refs workflow_sha=${WORKFLOW_SHA:-$source_sha}
  remote_refs=$(git ls-remote "https://github.com/$repo.git" refs/heads/main "refs/tags/$release_tag")
  test "$remote_refs" = "$workflow_sha"$'\trefs/heads/main'
  if [[ "$workflow_sha" != "$source_sha" ]]; then
    is_recovery_merge "$workflow_sha"
  fi
  test "$(git ls-remote https://github.com/epsalmond/homebrew-codex-shake.git HEAD)" = "$tap_sha"$'\tHEAD'
  gh api "$api/releases?per_page=50" | jq -e --arg tag "$release_tag" --arg previous "$previous_tag" '
    all(.[]; .tag_name != $tag) and
    (map(select(.prerelease and (.draft | not) and (.tag_name | startswith("local-features-v"))))
      | first | .tag_name == $previous)' >/dev/null
  GH_TOKEN="${GH_ACTIONS_TOKEN:-}" gh api "$api/actions/workflows/$workflow/runs?branch=main&per_page=10" \
    | jq -e --argjson current "${GITHUB_RUN_ID:-0}" '
      all(.workflow_runs[]; .id == $current or .status == "completed")' >/dev/null
}

verify_published() {
  local tag_json release_json asset digest
  tag_json=$(gh api "$api/git/ref/tags/$release_tag")
  jq -e --arg sha "$source_sha" '.object.type == "commit" and .object.sha == $sha' \
    <<< "$tag_json" >/dev/null

  release_json=$(gh api "$api/releases/tags/$release_tag")
  jq -e --arg tag "$release_tag" --arg sha "$source_sha" --arg publisher "$publisher" '
    .tag_name == $tag and .target_commitish == $sha and .author.login == $publisher
    and .draft == false and .prerelease == true and .published_at != null
    and .name == "Shake v0.4.0 · build 242.3 (Codex 0.172.0)" and (.assets | length) == 8' \
    <<< "$release_json" >/dev/null

  local assets=(
    codex-x86_64-unknown-linux-gnu.tar.gz
    codex-aarch64-unknown-linux-gnu.tar.gz
    codex-aarch64-apple-darwin.tar.gz
    codex-shake_0.172.0+r20261124135104.10610.242.3_amd64.deb
    codex-symbols-x86_64-unknown-linux-gnu.tar.gz
    codex-symbols-aarch64-unknown-linux-gnu.tar.gz
    codex-symbols-aarch64-apple-darwin.tar.gz
    SHA256SUMS
  )
  for asset in "${assets[@]}"; do
    digest=$(sha256sum "dist/$asset")
    jq -e --arg name "$asset" --arg digest "sha256:${digest%% *}" '
      any(.assets[]; .name == $name and .state == "uploaded" and .digest == $digest)' \
      <<< "$release_json" >/dev/null
  done
}

operation=${1:?pass is-recovery-merge, linux, download-mac, verify-mac, state, or published}
if [[ "$operation" != is-recovery-merge ]]; then
  test "${RELEASE_TAG:?}" = "$release_tag"
  test "${BUILD_REVISION:?}" = 242.3
  if [[ "$operation" == download-mac ]]; then
    test "$(git rev-parse HEAD)" = "${GITHUB_SHA:?}"
  else
    test "$(git rev-parse HEAD)" = "$source_sha"
  fi
fi

case "$operation" in
  is-recovery-merge) is_recovery_merge "${2:?merge commit required}" ;;
  linux) verify_linux ;;
  download-mac) download_mac ;;
  verify-mac) verify_mac_files ;;
  state) verify_state ;;
  published) verify_published ;;
  *) exit 2 ;;
esac
