#!/usr/bin/env bash
# Decide whether an existing automatic recovery handoff is healthy.
#
# The management-plane processor reports progress as deployment statuses on the
# handoff deployment. A scheduled sync that finds an existing handoff exits
# non-zero when that handoff failed or stalled, so the run turns red instead of
# silently skipping the same conflict forever.
set -euo pipefail

deployment_file=""
statuses_file=""
now=""
stall_hours=6
processor="/usr/local/libexec/management-plane/codex-fork-release-processor"

usage() {
  printf 'usage: %s --deployment FILE --statuses FILE [--now EPOCH] [--stall-hours N]\n' "$0" >&2
}

while (($# > 0)); do
  case "$1" in
    --deployment)
      deployment_file=${2:?--deployment requires a value}
      shift 2
      ;;
    --statuses)
      statuses_file=${2:?--statuses requires a value}
      shift 2
      ;;
    --now)
      now=${2:?--now requires a value}
      shift 2
      ;;
    --stall-hours)
      stall_hours=${2:?--stall-hours requires a value}
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      usage
      exit 2
      ;;
  esac
done

[[ -n "$deployment_file" && -n "$statuses_file" ]] || {
  usage
  exit 2
}
now=${now:-$(date -u +%s)}
[[ "$now" =~ ^[0-9]+$ && "$stall_hours" =~ ^[0-9]+$ ]] || {
  usage
  exit 2
}
stall_seconds=$((stall_hours * 3600))

# GitHub workflow commands need %, CR, and LF escaped in their message.
annotation_escape() {
  local value=$1
  value=${value//'%'/'%25'}
  value=${value//$'\r'/'%0D'}
  value=${value//$'\n'/'%0A'}
  printf '%s' "$value"
}

fail() {
  local title=$1 message=$2
  printf '::error title=%s::%s\n' "$title" "$(annotation_escape "$message")"
  if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    printf '### %s\n\n%s\n' "$title" "$message" >> "$GITHUB_STEP_SUMMARY"
  fi
  exit 1
}

IFS=$'\t' read -r deployment_id deployment_created upstream_tag upstream_sha fork_sha < <(
  jq -r '[
      (.id // ""),
      (try (.created_at | fromdateiso8601) catch ""),
      (.payload.upstream_tag // ""),
      (.payload.upstream_sha // ""),
      (.payload.fork_sha // "")
    ] | @tsv' "$deployment_file"
) || true
[[ "$deployment_id" =~ ^[0-9]+$ && "$deployment_created" =~ ^[0-9]+$ ]] || {
  echo "fork-recovery-handoff-status: deployment JSON has no id or created_at" >&2
  exit 2
}

if ! jq -e '
    (.payload.upstream_tag | test("^rust-v[0-9]+\\.[0-9]+\\.[0-9]+$")) and
    (.payload.upstream_sha | test("^[0-9a-f]{40}$")) and
    (.payload.fork_sha | test("^[0-9a-f]{40}$")) and
    ((.payload.schema_version == 2 and .payload.recovery_kind == "merge_conflict") or
     (.payload.schema_version == 3 and .payload.recovery_kind == "validation_failure" and
      (.payload.candidate_sha | test("^[0-9a-f]{40}$")) and
      (.payload.validation_result == "source_failure" or .payload.validation_result == "infrastructure_failure")))
  ' "$deployment_file" >/dev/null; then
  echo "fork-recovery-handoff-status: unsupported recovery identity" >&2
  exit 2
fi

# Statuses are listed newest first, but pick the highest id so ordering never matters.
IFS=$'\t' read -r state status_created description < <(
  jq -r 'if type == "array" and length > 0
      then max_by(.id) | [.state, (.created_at | fromdateiso8601), (.description // "")]
      else ["", "", ""]
      end | @tsv' "$statuses_file"
) || true

state_id="$upstream_tag-$fork_sha-$upstream_sha"
retry_hint="Inspect the terminal processor status and fix its prerequisite before retry on nas: $processor --retry-recovery $state_id. Retry preserves the consumed model budget; exhausted attempts require operator escalation."
handoff="recovery handoff $deployment_id for $upstream_tag at fork ${fork_sha:0:12}"

if [[ $(jq -r '.payload.schema_version' "$deployment_file") == 3 ]]; then
  candidate=$(jq -r '.payload.candidate_sha' "$deployment_file")
  classification=$(jq -r '.payload.validation_result' "$deployment_file")
  handoff="$handoff, failed candidate $candidate ($classification)"
fi

case "$state" in
  failure|error)
    fail "Automatic recovery failed" \
      "$handoff failed: ${description:-no description}. $retry_hint"
    ;;
  success|inactive)
    echo "$handoff is $state; nothing to do"
    ;;
  ""|in_progress|queued|pending)
    since=$deployment_created
    label="has no status from the local processor"
    if [[ -n "$state" ]]; then
      since=$status_created
      label="has been $state"
    fi
    age=$((now - since))
    if ((age > stall_seconds)); then
      fail "Recovery handoff stalled" \
        "$handoff $label for $((age / 3600))h (limit ${stall_hours}h). Check the codex-fork-release-processor unit on nas. $retry_hint"
    fi
    echo "$handoff $label for $((age / 60))m; waiting for the local processor"
    ;;
  *)
    echo "::warning title=Unknown recovery handoff status::$handoff has status $(annotation_escape "$state")"
    ;;
esac
