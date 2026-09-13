#!/usr/bin/env bash
# Kill any real Rust build a replay worker starts.
#
# PATH stubs are not enough: a worker can call the real `just` by absolute
# path under the operator's home directory, or shell out to the machine's own
# `run-subagent` script and get a fresh agent with an unstubbed PATH. Three
# concurrent workspace builds take this machine down, so this watchdog is the
# mechanical backstop.
#
#   scripts/bench-watchdog.sh "$BENCH_REPLAY" [logfile]
set -uo pipefail
ROOT="${1:-${BENCH_REPLAY:-$HOME/bench-replay}}"
LOG="${2:-$ROOT/watchdog.log}"
PATTERN='cargo-nextest|cargo nextest|/\.cargo/bin/(cargo|just|cargo-nextest)|rustc --crate-name|run-subagent|codex .*exec -m '

# Only ever kill something that belongs to THIS replay. Run 8 showed why:
# the previous version killed on `run-subagent` or `exec -m ` appearing
# anywhere in the argv, unscoped, and in one run it killed 25 of the
# operator's own unrelated agent processes and zero replay processes. A
# process counts as in-scope when $ROOT appears in its argv OR its cwd is
# under $ROOT -- the second half is what still catches a worker that shells
# out to run-subagent from inside its own tree with an argv that names no path.
in_scope() {
  local pid="$1" args="$2" cwd
  case "$args" in *"$ROOT"*) return 0 ;; esac
  cwd="$(readlink -f "/proc/$pid/cwd" 2>/dev/null || true)"
  case "$cwd" in "$ROOT"|"$ROOT"/*) return 0 ;; esac
  return 1
}

while true; do
  pgrep -af -E "$PATTERN" | while read -r pid rest; do
    if in_scope "$pid" "$rest"; then
      echo "$(date -u +%FT%TZ) killed $pid: ${rest:0:160}" >>"$LOG"
      kill -9 "$pid" 2>/dev/null
    fi
  done
  sleep 5
done
