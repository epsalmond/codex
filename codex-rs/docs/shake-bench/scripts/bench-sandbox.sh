#!/usr/bin/env bash
# bubblewrap jail for the replay worker.
#
# Why this exists: replay-proof.md §4/§7 blocker 4. One early run's worker
# reached the real `just` by absolute path under the invoking user's home
# directory (the PATH stubs only stop PATH-relative lookups) and shelled out
# to a host agent-launcher script, which starts a REAL codex process with an
# unstubbed PATH and a real `cargo nextest` in the candidate tree.
# scripts/bench-watchdog.sh is the mechanical backstop that kills such a
# process after the fact; this script is meant to make the escape unavailable
# in the first place by controlling what filesystem the worker can even see.
#
# This script derives the invoking user's real home directory once, into
# HOST_HOME below, and hides/rebinds everything relative to that path so it
# works for any operator's machine, not a specific hardcoded account.
#
# What this blocks:
#   - the real cargo/just/cargo-nextest/cargo-insta/rustup toolchain: the
#     directories that hold them (~/.cargo/bin, every ~/.rustup/toolchains/
#     */bin) are replaced wholesale by a bind-mount of fixtures/stubs, so an
#     absolute-path invocation hits a stub, not the real binary. Anything the
#     stub dir does not provide is ENOENT, which is the safe failure.
#   - $HOST_HOME/.local/bin/run-subagent (or wherever the host keeps its
#     agent-launcher tooling) and every other host agent-tooling path:
#     nothing under $HOST_HOME exists unless this script binds it in
#     (tmpfs first), and PATH inside the sandbox names only the stub dir, a
#     minimal system PATH, and one directory holding just
#     codex-code-mode-host.
#   - the operator's git checkouts (including the one holding the finished
#     branch), the checkpoint store, and every OTHER replay-tree sibling
#     under the replay workspace (reference trees, other runs' worktrees):
#     unreachable for the same tmpfs-first reason. Only --tree, --codex-home,
#     --out, and any --extra-ro dirs are bound in, each individually.
#
# Residual risks this does NOT close (see the caller's report for detail):
#   - The network namespace is deliberately left SHARED with the host (no
#     --unshare-net), because the worker has to reach the real API. Nothing
#     stops it from reaching any other host too -- this sandbox restricts the
#     filesystem, not egress.
#   - --unshare-user without a real user/group id map means the sandboxed
#     process still runs as the invoking uid from the kernel's point of view;
#     bwrap's namespace isolation is about filesystem/pid/ipc/uts visibility,
#     not privilege separation from this Unix user. A privilege-escalation
#     bug in bwrap itself, or in codex, is not something this script defends
#     against.
#   - /proc and /dev are real (bwrap's own device/proc setup), which is
#     necessary for a normal process to run at all; a sufficiently creative
#     worker may still learn host facts through them (e.g. cpuinfo, uptime)
#     that stop short of filesystem access.
#   - This script trusts its own argv. It does not re-verify that --tree,
#     --codex-home, --out, or any --extra-ro path is itself outside the
#     directories it is trying to hide (e.g. passing --tree pointed at the
#     checked-out fork under the operator's home directory would defeat the
#     point). The caller is responsible for passing paths under the replay
#     workspace.
#
# Usage:
#   bench-sandbox.sh --tree <dir> --codex-home <dir> --out <dir> \
#                     --stubs <dir> --codex-bin <path> \
#                     [--extra-ro <dir>]... [--dry-run] -- <argv for codex...>
set -euo pipefail

die() { echo "bench-sandbox: $*" >&2; exit 1; }

# The invoking user's real home directory. Everything below that would
# otherwise be a hardcoded personal path is derived from this instead, so the
# jail hides/rebinds "wherever this operator's home is" rather than one
# specific account's path.
HOST_HOME="${HOME:?HOME must be set}"

TREE="" CODEX_HOME_DIR="" OUT="" STUBS="" CODEX_BIN="" DRY_RUN=0
EXTRA_RO=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tree) TREE="$2"; shift 2 ;;
    --codex-home) CODEX_HOME_DIR="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --stubs) STUBS="$2"; shift 2 ;;
    --codex-bin) CODEX_BIN="$2"; shift 2 ;;
    --extra-ro) EXTRA_RO+=("$2"); shift 2 ;;
    --dry-run|-n) DRY_RUN=1; shift ;;
    --) shift; break ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ -n "$TREE" ]] || die "--tree is required"
[[ -n "$CODEX_HOME_DIR" ]] || die "--codex-home is required"
[[ -n "$OUT" ]] || die "--out is required"
[[ -n "$STUBS" ]] || die "--stubs is required"
[[ -n "$CODEX_BIN" ]] || die "--codex-bin is required"
[[ $# -gt 0 ]] || die "no command given after --"

TREE="$(readlink -f "$TREE")"
CODEX_HOME_DIR="$(readlink -f "$CODEX_HOME_DIR")"
OUT="$(readlink -f "$OUT")"
STUBS="$(readlink -f "$STUBS")"
CODEX_BIN="$(readlink -f "$CODEX_BIN")"
CODEX_DIR="$(dirname "$CODEX_BIN")"
CODEX_BASENAME="$(basename "$CODEX_BIN")"

[[ -d "$TREE" ]] || die "--tree $TREE does not exist"
[[ -d "$CODEX_HOME_DIR" ]] || die "--codex-home $CODEX_HOME_DIR does not exist"
[[ -d "$OUT" ]] || die "--out $OUT does not exist"
[[ -d "$STUBS" ]] || die "--stubs $STUBS does not exist"
[[ -x "$CODEX_BIN" ]] || die "--codex-bin $CODEX_BIN is not an executable file"

# Real codex dir is bound read-only at a path that is NOT on PATH, so the
# worker cannot `codex exec` a fresh agent with its own auth by name. It is
# exec'd by absolute path instead. codex-code-mode-host is resolved by the
# `which` crate against PATH at runtime (confirmed by inspecting the binary),
# so it must be reachable by name -- but codex itself must not be. We build a
# small "PATH helper" dir on the host containing only a symlink to
# codex-code-mode-host and bind that in separately.
#
# These per-invocation helper files live under $OUT/.sandbox (not /tmp via
# mktemp) because this script `exec`s into bwrap at the end -- once that
# happens this shell never runs its EXIT trap, so anything relying on trap
# cleanup would leak. $OUT already belongs to this run and is cleaned up (or
# not) on the same schedule as the rest of the run's artefacts.
HIDDEN_CODEX_DIR="$HOST_HOME/.sandbox-codex"
SANDBOX_HELPERS="$OUT/.sandbox"
rm -rf "$SANDBOX_HELPERS"
mkdir -p "$SANDBOX_HELPERS/path-helper"
PATH_HELPER_HOST="$SANDBOX_HELPERS/path-helper"
HAVE_CODE_MODE_HOST=0
if [[ -e "$CODEX_DIR/codex-code-mode-host" ]]; then
  # `-e` on the symlink we are about to create would check against the HOST
  # filesystem, where $HIDDEN_CODEX_DIR does not exist (it is only a real
  # path once bwrap sets up the sandbox mount namespace) -- so that check
  # would always be false and silently drop the bind. Remember the decision
  # instead of re-testing the dangling symlink later.
  ln -sf "$HIDDEN_CODEX_DIR/codex-code-mode-host" "$PATH_HELPER_HOST/codex-code-mode-host"
  HAVE_CODE_MODE_HOST=1
fi
PATH_HELPER_SANDBOX="$HOST_HOME/.sandbox-path-helper"

# Every real toolchain bin dir on the host that must be shadowed by stubs.
# $HOST_HOME/.cargo/bin plus each installed rustup toolchain's bin/.
TOOLCHAIN_DIRS=("$HOST_HOME/.cargo/bin")
if [[ -d "$HOST_HOME/.rustup/toolchains" ]]; then
  for d in "$HOST_HOME"/.rustup/toolchains/*/bin; do
    [[ -d "$d" ]] && TOOLCHAIN_DIRS+=("$d")
  done
fi

# Synthetic git identity + safe.directory for the worker's commits.
GITCONFIG_HOST="$SANDBOX_HELPERS/gitconfig"
cat >"$GITCONFIG_HOST" <<EOF
[user]
	name = shake-bench replay
	email = replay@localhost
[safe]
	directory = $TREE
	directory = *
[init]
	defaultBranch = main
EOF

SANDBOX_PATH="$HOST_HOME/.cargo/bin:${PATH_HELPER_SANDBOX}:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"

argv=(bwrap
  --tmpfs "$HOST_HOME"
  --ro-bind /usr /usr
  --ro-bind /etc /etc
  --symlink usr/bin /bin
  --symlink usr/sbin /sbin
  --symlink usr/lib /lib
)
[[ -e /lib64 ]] && argv+=(--symlink usr/lib64 /lib64)
[[ -e /lib32 ]] && argv+=(--symlink usr/lib32 /lib32)

# /etc/resolv.conf is commonly a symlink to /run/systemd/resolve/... on this
# host; /run is not bound, so the plain --ro-bind /etc above leaves the
# symlink dangling and DNS resolution fails inside the sandbox even though
# networking itself is shared with the host. Binding a real file directly
# over /etc/resolv.conf does not work when it is a dangling symlink (bwrap
# needs an existing regular file/dir at the target and /etc is read-only), so
# instead resolve the symlink on the host and bind its target's directory
# in at the SAME absolute path -- the existing /etc symlink then resolves.
if [[ -e /etc/resolv.conf ]]; then
  RESOLV_REAL="$(readlink -f /etc/resolv.conf 2>/dev/null || true)"
  if [[ -n "$RESOLV_REAL" && -e "$RESOLV_REAL" && "$RESOLV_REAL" != /etc/resolv.conf ]]; then
    argv+=(--ro-bind "$(dirname "$RESOLV_REAL")" "$(dirname "$RESOLV_REAL")")
  fi
fi

argv+=(
  --proc /proc
  --dev /dev
  --tmpfs /tmp
  --unshare-user
  --unshare-ipc
  --unshare-pid
  --unshare-uts
  --unshare-cgroup
  --die-with-parent
  --new-session
)

# Stubs over the real toolchain, one bind per real bin dir found on the host.
for d in "${TOOLCHAIN_DIRS[@]}"; do
  argv+=(--ro-bind "$STUBS" "$d")
done

# Real codex dir, hidden off PATH; PATH helper dir with only the code-mode
# host symlink, on PATH.
argv+=(--ro-bind "$CODEX_DIR" "$HIDDEN_CODEX_DIR")
if [[ "$HAVE_CODE_MODE_HOST" -eq 1 ]]; then
  argv+=(--ro-bind "$PATH_HELPER_HOST" "$PATH_HELPER_SANDBOX")
fi

# Read-write: the replay tree, this run's CODEX_HOME, and this run's output
# dir (the stub invocation log lives there). Nothing else is rw.
argv+=(
  --bind "$TREE" "$TREE"
  --bind "$CODEX_HOME_DIR" "$CODEX_HOME_DIR"
  --bind "$OUT" "$OUT"
)
for extra in "${EXTRA_RO[@]}"; do
  extra="$(readlink -f "$extra")"
  argv+=(--ro-bind "$extra" "$extra")
done

argv+=(--ro-bind "$GITCONFIG_HOST" "$HOST_HOME/.gitconfig")

argv+=(
  --setenv HOME "$HOST_HOME"
  --setenv PATH "$SANDBOX_PATH"
  --setenv CODEX_HOME "$CODEX_HOME_DIR"
  --chdir "$TREE"
)

argv+=(-- "$HIDDEN_CODEX_DIR/$CODEX_BASENAME" "$@")

if [[ "$DRY_RUN" -eq 1 ]]; then
  printf '%q ' "${argv[@]}"
  echo
  exit 0
fi

exec "${argv[@]}"
