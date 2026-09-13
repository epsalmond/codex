#!/usr/bin/env bash
# One arm of the warm-cache matrix, end to end and idempotent.
#
# Design notes: reports/2026-09-12/plan-warm.md
#
# Every step of the per-run protocol in one place, because doing it by hand five
# times is how a run ends up differing from its row: classified census, an
# other-session rollout snapshot (the quota-attributability check), a worktree
# reset from the read-only checkpoint, the REJECTED-at-12 precondition, the
# replay itself, acceptance, the candidate tree, and both cost columns.
#
# Safe to re-run: the worktree is rebuilt from the checkpoint on entry and the
# candidate tree is replaced, so a crashed run leaves nothing that changes the
# next attempt.
#
#   scripts/run-warm-arm.sh --label B3-full-cold --arm full --cache cold
#   scripts/run-warm-arm.sh --label Aw-self --arm shake-elide --cache warm --warm self
#
# --tier fast adds the Fast service tier; the default is the account's standard
# tier, which is what this batch runs on.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(dirname "$HERE")"
BENCH="${BENCH_REPLAY:-$HOME/bench-replay}"
CHECKPOINT="${BENCH_CHECKPOINTS:-$HOME/bench-checkpoints}/01a07e54-r264"   # read-only source material
WORKTREE="$BENCH/worktree"
HISTORY="$REPO/fixtures/01a07e54-r264-redacted-trimC.flattened.json"
SCRIPTED="$REPO/fixtures/scripted-user-01a07e54.json"

LABEL="" ARM="" CACHE="cold" WARM="self" TIER=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --label) LABEL="$2"; shift 2 ;;
    --arm) ARM="$2"; shift 2 ;;
    --cache) CACHE="$2"; shift 2 ;;
    --warm) WARM="$2"; shift 2 ;;
    --tier) TIER="$2"; shift 2 ;;
    -h|--help) sed -n '2,25p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done
[[ -n "$LABEL" && -n "$ARM" ]] || { echo "--label and --arm are required" >&2; exit 2; }

OUT="$BENCH/results/run-$LABEL"
CANDIDATE="$BENCH/candidate-run$LABEL"
mkdir -p "$OUT" "$BENCH/logs"
LOG="$OUT/run.log"

say() { printf '\n=== %s ===\n' "$*" | tee -a "$LOG"; }

say "run $LABEL: arm=$ARM cache=$CACHE${CACHE:+/}$( [[ $CACHE == warm ]] && echo "$WARM" ) tier=${TIER:-standard}  $(date -Is)"

# 1. Classified census. Recorded, never fatal: --allow-busy-host is deliberate
#    for this batch (the operator's call), and the row carries the classification.
say "classified codex census"
python3 "$HERE/codex-census.py" --json > "$OUT/census-before.json" || true
python3 - "$OUT/census-before.json" <<'PY' | tee -a "$LOG"
import json, sys
d = json.load(open(sys.argv[1]))
print(f"total {d['total']}  personal {d['personal']}  other-account {d['otherAccount']}  unknown {d['unknown']}")
PY

# 2. Other-session rollout snapshot. Compared after the run: if any other
#    $CODEX_HOME session wrote a rollout while this run was in flight, its
#    quota delta is not attributable and the row says so.
python3 "$HERE/rollout-watch.py" snapshot > "$OUT/rollouts-before.json"

# 3. Worktree reset from the read-only checkpoint, and the REJECTED precondition.
say "resetting the worktree from $CHECKPOINT"
rm -rf "$WORKTREE"
cp -a "$CHECKPOINT" "$WORKTREE"
set +e
"$HERE/accept-01a07e54.sh" "$WORKTREE" > "$OUT/accept-before.txt" 2>&1
before_status=$?
set -e
tail -3 "$OUT/accept-before.txt" | tee -a "$LOG"
if [[ $before_status -eq 0 ]]; then
  echo "!!! the fresh worktree is already ACCEPTED; the checkpoint is wrong. Refusing to run." | tee -a "$LOG"
  exit 4
fi
grep -q 'REJECTED' "$OUT/accept-before.txt" || { echo "!!! no REJECTED verdict before the run" | tee -a "$LOG"; exit 4; }

# 4. The replay. --allow-busy-host is passed deliberately; the runner's own
#    precheck would otherwise refuse, and the row records why it was overridden.
say "replay"
ARGS=(
  --history "$HISTORY" --tree "$WORKTREE" --scripted "$SCRIPTED"
  --out "$OUT" --arm "$ARM" --cache "$CACHE" --allow-busy-host
)
[[ "$CACHE" == warm ]] && ARGS+=(--warm "$WARM")
[[ -n "$TIER" ]] && ARGS+=(--service-tier "$TIER")
set +e
( cd "$REPO" && npx tsx scripts/replay-01a07e54.ts "${ARGS[@]}" ) >> "$LOG" 2>&1
replay_status=$?
set -e
echo "replay exit $replay_status" | tee -a "$LOG"

# 5. Other-session activity verdict, while the evidence is fresh.
set +e
python3 "$HERE/rollout-watch.py" compare "$OUT/rollouts-before.json" > "$OUT/rollouts-verdict.json"
set -e
python3 - "$OUT/rollouts-verdict.json" <<'PY' | tee -a "$LOG"
import json, sys
d = json.load(open(sys.argv[1]))
print(f"other-session rollouts advanced during the run: {len(d['advanced'])} "
      f"-> quota {'ATTRIBUTABLE' if d['quotaAttributable'] else 'UNATTRIBUTABLE'}")
PY

# 6. Acceptance, and preserve the candidate tree.
say "acceptance"
set +e
"$HERE/accept-01a07e54.sh" "$WORKTREE" > "$BENCH/logs/accept-run-$LABEL.txt" 2>&1
accept_status=$?
set -e
tail -4 "$BENCH/logs/accept-run-$LABEL.txt" | tee -a "$LOG"
cp -a "$BENCH/logs/accept-run-$LABEL.txt" "$OUT/acceptance.txt"
rm -rf "$CANDIDATE"
cp -a "$WORKTREE" "$CANDIDATE"
echo "candidate tree preserved at $CANDIDATE" | tee -a "$LOG"

# 7. Both cost columns.
say "cost"
python3 "$HERE/arms-row.py" "$LABEL" "$OUT/replay.json" | tee "$OUT/row.txt" | tee -a "$LOG"
python3 "$HERE/run-cost.py" "$OUT/replay.json" > "$OUT/per-request-cost.txt" 2>&1 || true
head -8 "$OUT/per-request-cost.txt" | tee -a "$LOG"

say "run $LABEL done: replay exit $replay_status, acceptance exit $accept_status  $(date -Is)"
exit $(( replay_status != 0 ? replay_status : 0 ))
