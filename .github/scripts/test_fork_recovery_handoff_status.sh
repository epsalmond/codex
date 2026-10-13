#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
status_script="$script_dir/fork-recovery-handoff-status.sh"
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT

upstream_sha=$(printf 'a%.0s' {1..40})
fork_sha=$(printf 'b%.0s' {1..40})
state_id="rust-v0.162.0-$fork_sha-$upstream_sha"
# 2026-10-08T20:15:00Z
created=1791490500
hour=3600

cat > "$fixture/deployment.json" <<EOF
{
  "id": 6945778021,
  "created_at": "2026-10-08T20:15:00Z",
  "payload": {
    "schema_version": 2,
    "recovery_kind": "merge_conflict",
    "upstream_tag": "rust-v0.162.0",
    "upstream_sha": "$upstream_sha",
    "fork_sha": "$fork_sha",
    "run_url": "https://github.com/epsalmond/codex/actions/runs/1"
  }
}
EOF

statuses() {
  # Each argument is "id state created_at description".
  local entries=() id state at description
  for entry in "$@"; do
    read -r id state at description <<< "$entry"
    entries+=("$(jq -n --argjson id "$id" --arg state "$state" --arg at "$at" --arg d "$description" \
      '{id: $id, state: $state, created_at: $at, description: $d}')")
  done
  if ((${#entries[@]} == 0)); then
    printf '[]\n'
  else
    printf '%s\n' "${entries[@]}" | jq -s .
  fi > "$fixture/statuses.json"
}

check() {
  local expected_exit=$1 now=$2
  shift 2
  local actual_exit=0
  GITHUB_STEP_SUMMARY="$fixture/summary.md" bash "$status_script" \
    --deployment "$fixture/deployment.json" \
    --statuses "$fixture/statuses.json" \
    --now "$now" > "$fixture/out" 2>&1 || actual_exit=$?
  if [[ "$actual_exit" != "$expected_exit" ]]; then
    echo "expected exit $expected_exit, got $actual_exit:" >&2
    cat "$fixture/out" >&2
    exit 1
  fi
  for needle in "$@"; do
    grep -Fq -- "$needle" "$fixture/out" || {
      echo "missing '$needle' in output:" >&2
      cat "$fixture/out" >&2
      exit 1
    }
  done
}

# Failure is red with the error and the processor retry command.
statuses \
  "2 failure 2026-10-08T20:16:00Z git merge failed: refusing to merge unrelated histories 100%" \
  "1 in_progress 2026-10-08T20:15:30Z local Luna merge recovery started"
check 1 $((created + hour)) \
  "::error title=Luna recovery failed::" \
  "refusing to merge unrelated histories 100%25" \
  "--retry-recovery $state_id"
grep -Fq -- "--retry-recovery $state_id" "$fixture/summary.md"

# Newest status wins even if GitHub lists it last.
statuses \
  "1 in_progress 2026-10-08T20:15:30Z started" \
  "3 success 2026-10-08T21:00:00Z dispatched" \
  "2 failure 2026-10-08T20:30:00Z transient"
check 0 $((created + 48 * hour)) "is success"

# No status yet: green while young, red once older than six hours.
statuses
check 0 $((created + 5 * hour)) "has no status" "waiting"
check 1 $((created + 7 * hour)) "::error title=Recovery handoff stalled::" "--retry-recovery $state_id"

# In progress: age counts from the latest status, so a fresh retry is green.
statuses "5 in_progress 2026-10-11T20:00:00Z local Luna merge recovery started"
check 0 $((created + 72 * hour + 30 * 60)) "has been in_progress"
check 1 $((created + 72 * hour + 7 * hour)) "Recovery handoff stalled" "has been in_progress for"

# Stale handoffs are superseded and stay green.
statuses "4 inactive 2026-10-08T20:20:00Z stale: fork branch advanced"
check 0 $((created + 100 * hour)) "is inactive"

# Malformed deployment JSON is a usage error, not a silent pass.
printf '{}\n' > "$fixture/deployment.json"
statuses
check 2 $((created + hour)) "no id or created_at"

echo "fork recovery handoff status tests passed"
