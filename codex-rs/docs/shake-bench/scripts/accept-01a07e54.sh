#!/usr/bin/env bash
# Acceptance checks for the 01a07e54 replay benchmark (recoverable /shake with
# per-thread artifacts).
#
# Run INSIDE a candidate tree. The worker never sees this script.
#
#   scripts/accept-01a07e54.sh /path/to/candidate-tree
#
# BUILD-FREE BY DESIGN. Real cargo/nextest/just runs of this workspace take
# tens of minutes each and three concurrent ones take the machine down, so the
# replay arms run against stubbed build tools (fixtures/stubs/) and acceptance
# cannot rely on a compiler either. Every check below is a structural or
# content assertion about the tree: which files exist, which symbols and
# strings are present, and which pre-cutoff content survived. Each one maps to
# a requirement in fixtures/simulated-user-01a07e54.md.
#
# Set ACCEPT_FMT=1 to additionally run `cargo fmt --check` (rustfmt only, no
# build). It is off by default because it needs a real cargo on PATH.
#
# Exit 0 = accepted.
set -uo pipefail

TREE="${1:-$PWD}"
TREE="$(cd "$TREE" && pwd)"
RS="$TREE/codex-rs"
[[ -d "$RS" ]] || { echo "FAIL: $TREE is not a codex checkout (no codex-rs/)"; exit 2; }

fails=()
pass(){ echo "PASS  $1"; }
fail(){ echo "FAIL  $1"; fails+=("$1"); }

# has <label> <file> <extended-regex>
has() {
  local label="$1" file="$2" re="$3"
  if [[ ! -f "$RS/$file" ]]; then fail "$label — $file is missing"; return; fi
  if grep -Eq -- "$re" "$RS/$file"; then pass "$label"; else fail "$label — $file does not match /$re/"; fi
}
hasnt() {
  local label="$1" file="$2" re="$3"
  if [[ ! -f "$RS/$file" ]]; then fail "$label — $file is missing"; return; fi
  if grep -Eq -- "$re" "$RS/$file"; then fail "$label — $file still matches /$re/"; else pass "$label"; fi
}
exists() {
  local label="$1" file="$2"
  if [[ -f "$RS/$file" ]]; then pass "$label"; else fail "$label — missing $file"; fi
}

echo "tree      $TREE"
echo "mode      build-free structural acceptance"
echo

# ---- A. the app-server protocol surface is vendored -----------------------
# Requirement 3: any generated artefact the change affects must be regenerated.
SCHEMA=app-server-protocol/schema
exists "A1 vendored JSON schema for ThreadShakeStartParams"   "$SCHEMA/json/v2/ThreadShakeStartParams.json"
exists "A2 vendored JSON schema for ThreadShakeStartResponse" "$SCHEMA/json/v2/ThreadShakeStartResponse.json"
exists "A3 vendored TS type for ThreadShakeStartParams"       "$SCHEMA/typescript/v2/ThreadShakeStartParams.ts"
exists "A4 vendored TS type for ThreadShakeStartResponse"     "$SCHEMA/typescript/v2/ThreadShakeStartResponse.ts"
has    "A5 thread/shake/start is in the client request schema" "$SCHEMA/json/ClientRequest.json" 'thread/shake/start'
has    "A6 both shake types are re-exported from the v2 index" "$SCHEMA/typescript/v2/index.ts" 'ThreadShakeStart(Params|Response)'
has    "A7 thread/shake/start is documented in the app-server README" "../codex-rs/app-server/README.md" 'thread/shake/start'

# ---- B. user-visible TUI output has accepted snapshots --------------------
# Requirement 4.
SNAP=tui/src/chatwidget/snapshots
exists "B1 shake-notice summary snapshot"  "$SNAP/codex_tui__chatwidget__tests__shake_notice_renders_summary.snap"
# B2 is BEHAVIOURAL, not a filename match. The reference tree happens to name
# its snapshot `..._ephemeral_shake_notice.snap`; run 8's worker named the same
# snapshot `..._shake_notice_renders_summary_for_ephemeral_thread.snap` after
# the test that generated it, and the original check scored a correct
# implementation down for spelling (replay-proof.md 11.6). What the requirement
# actually asks for is: the ephemeral/tool-free shake notice has an ACCEPTED
# snapshot of its own, distinct from B1's persistent-thread one. So: any .snap
# in the chatwidget snapshot directory that is about the shake notice and about
# the ephemeral case, matched on filename or on the snapshot's own body (insta
# records the generating expression and test name in the .snap header).
b2_snap=""
if [[ -d "$RS/$SNAP" ]]; then
  while IFS= read -r snap; do
    base="$(basename "$snap")"
    # B1's snapshot is the persistent-thread one; it must not satisfy B2.
    [[ "$base" == "codex_tui__chatwidget__tests__shake_notice_renders_summary.snap" ]] && continue
    if grep -Eqi 'shake|notice' <<<"$base" || grep -Eqi 'shake|notice' "$snap"; then
      if grep -qi 'ephemeral' <<<"$base" || grep -qi 'ephemeral' "$snap"; then
        b2_snap="$base"
        break
      fi
    fi
  done < <(find "$RS/$SNAP" -maxdepth 1 -name '*.snap' 2>/dev/null | sort)
fi
if [[ -n "$b2_snap" ]]; then
  pass "B2 ephemeral shake-notice case has its own accepted snapshot ($b2_snap)"
else
  fail "B2 ephemeral shake-notice case has no accepted snapshot in $SNAP"
fi
if compgen -G "$RS/**/*.snap.new" >/dev/null 2>&1 || [[ -n "$(find "$RS" -name '*.snap.new' -not -path '*/target/*' 2>/dev/null)" ]]; then
  fail "B3 pending snapshots left unaccepted"
else
  pass "B3 no pending .snap.new left in the tree"
fi

# ---- C. tool-free threads stay tool-free ----------------------------------
# Requirement 5: read_artifact must not be registered unconditionally.
SP="$RS/core/src/tools/spec_plan.rs"
if [[ -f "$SP" ]]; then
  if ! grep -q 'registry.add(ReadArtifactHandler)' "$SP"; then
    fail "C1 spec_plan.rs no longer registers read_artifact at all"
  # The guard must be a real conditional within a few lines of the
  # registration, whatever it keys on (config.ephemeral, a system-thread
  # predicate, ...). Comments alone do not count.
  elif grep -B5 'registry.add(ReadArtifactHandler)' "$SP" \
       | grep -Eq '^\s*(\}? *else )?if .*(ephemeral|system_thread|is_system|structured)'; then
    pass "C1 read_artifact registration is guarded for tool-free threads"
  else
    fail "C1 read_artifact is registered unconditionally in spec_plan.rs"
  fi
else
  fail "C1 core/src/tools/spec_plan.rs is missing"
fi
if grep -rqE 'ephemeral' --include='*.rs' "$RS/core/tests/suite" "$RS/tui/src/chatwidget/tests" 2>/dev/null \
   && grep -rlE 'ephemeral' --include='*.rs' "$RS/core/tests/suite" "$RS/tui/src/chatwidget/tests" 2>/dev/null \
      | xargs -r grep -lE 'shake|artifact' >/dev/null 2>&1; then
  pass "C2 an ephemeral/tool-free case is covered by a shake or artifact test"
else
  fail "C2 no test covers ephemeral threads staying artifact-free"
fi

# ---- D. artifact store hardening ------------------------------------------
# Requirement 1/2: UTF-8 byte-offset handling on artifact reads.
# D1 is BEHAVIOURAL, not a message match. The requirement is that a read whose
# start offset lands mid-character is REJECTED rather than silently slid to the
# next boundary -- not that the rejection uses any particular wording. The
# original check grepped for the reference tree's exact sentence and failed run
# 8's `byte {offset} is not a UTF-8 boundary in {uri}`, which implements the
# same behaviour (replay-proof.md 11.6).
#
# What distinguishes an implementation from the checkpoint is structural and
# checkable without a compiler: a boundary predicate on the byte at the
# requested offset whose taken branch RETURNS AN ERROR. The checkpoint instead
# computes a `leading_continuation` skip count and returns content, so it has
# the predicate but no error return next to it -- hence the proximity window
# rather than two independent greps.
ART="$RS/core/src/artifacts.rs"
# A boundary predicate is either the std helper or an explicit UTF-8
# continuation-byte mask test (0b10xxxxxx), in either notation.
BOUNDARY_RE='is_char_boundary|0b1100_0000|0b11000000|0xC0|0xc0'
if [[ ! -f "$ART" ]]; then
  fail "D1 artifact reads reject mid-character byte offsets — core/src/artifacts.rs is missing"
elif awk -v re="$BOUNDARY_RE" '
      # Remember the last line that tested a char boundary inside an if/guard.
      $0 ~ re && $0 ~ /(^|[^A-Za-z_])(if|guard|assert|match)([^A-Za-z_]|$)/ { guard = NR }
      # An error return within 3 lines of that guard is the reject path.
      /return Err|bail!|\.ok_or|Err\(format!/ { if (guard && NR - guard <= 3) { found = 1 } }
      END { exit(found ? 0 : 1) }
    ' "$ART"; then
  pass "D1 artifact reads reject mid-character byte offsets (guarded error return)"
else
  fail "D1 artifact reads reject mid-character byte offsets — no char-boundary guard returning an error in core/src/artifacts.rs"
fi
has "D2 artifact paging still advertises a continuation offset" \
    core/src/artifacts.rs 'start_byte=\{next_offset\}'
has "D3 artifact store tests cover a non-boundary offset" \
    core/src/artifacts_tests.rs 'boundary|mid_character|utf8'

# ---- E. no new compiler warnings, checked by source ----------------------
# Requirement 1. The one warning the checkpoint carries is an unused import the
# session removed; it is the only warning `cargo check` reported at the cutoff.
hasnt "E1 unused wiremock body_json import removed" \
      core/tests/suite/openai_file_mcp.rs '^use wiremock::matchers::body_json;'
hasnt "E2 rate_limits.rs no longer clones a Copy timestamp" \
      tui/src/status/rate_limits.rs 'format_reset_timestamp\(dt\.clone\(\), captured_at\)'

# ---- F. nothing from the checkpoint regressed -----------------------------
# Requirement 7: the pre-cutoff work must still be there.
exists "F1 artifact store still present"        core/src/artifacts.rs
exists "F2 shake recovery module still present" core/src/shake/recovery.rs
exists "F3 read_artifact handler still present" core/src/tools/handlers/read_artifact.rs
exists "F4 end-to-end recovery test still present" core/tests/suite/artifact_recovery.rs
has    "F5 recovery test still exercises resume"   core/tests/suite/artifact_recovery.rs 'shake_saves_and_reads_artifact_after_resume'
has    "F6 delete_thread still cleans the artifact root" thread-store/src/local/delete_thread.rs 'artifact'

# ---- G. optional: formatting (rustfmt only, no build) --------------------
if [[ "${ACCEPT_FMT:-0}" == "1" ]]; then
  if ( cd "$RS" && cargo fmt --check ) >/dev/null 2>&1; then pass "G1 cargo fmt --check clean"
  else fail "G1 cargo fmt --check reports changes"; fi
else
  echo "SKIP  G1 cargo fmt --check (set ACCEPT_FMT=1 to enable; needs a real cargo)"
fi

echo
if [[ ${#fails[@]} -eq 0 ]]; then echo "ACCEPTED  $TREE"; exit 0; fi
echo "REJECTED  $TREE  (${#fails[@]} failing check(s))"
printf '  - %s\n' "${fails[@]}"
exit 1
