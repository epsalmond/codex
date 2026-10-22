#!/usr/bin/env bash
# Build codex-shake from a commit on this machine and install it the way
# install.sh installs a published release. The steps match
# .github/workflows/fork-release.yml: the same version stamp, rusty_v8
# download, cargo flags, strip script, and package layout. The tag sorts
# above older releases and just below the official CI tag for the same
# commit, so a later `codex-shake-update` moves to the official build.
#
# Keep this compatible with macOS /bin/bash 3.2; --host runs it there over ssh.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/shake-local-install.sh [options] <commit-ish>

Builds <commit-ish> in a detached worktree, packages it like a fork
release, unpacks it into $CODEX_SHAKE_HOME/<tag>, and points `current` at it.
Earlier release directories are kept for rollback.

Options:
  --version X.Y.Z    Codex version to stamp. Default: the workspace version,
                     or the newest rust-vX.Y.Z tag merged into the commit
                     when the workspace is at 0.0.0 (as fork-release does).
  --repo DIR         Repository to build from. Default: this checkout.
  --work-dir DIR     Build worktree and outputs.
                     Default: ${XDG_CACHE_HOME:-~/.cache}/codex-shake-local/<source-sha12>
  --keep-worktree    Keep the build worktree and its target/ after success.
  --no-activate      Install under $CODEX_SHAKE_HOME/<tag> but leave `current`.
  --host SSH_HOST    Copy this script to SSH_HOST and run it there.
  --remote-repo DIR  Repository on SSH_HOST. Default: this repository's path
                     relative to $HOME.
  --dry-run          Resolve the commit, version, tag, Python, and bwrap
                     source, print the plan, and exit without building.
  -h, --help         Show this help.

Environment:
  CODEX_SHAKE_HOME     Install root (default ~/.local/share/codex-shake).
  CODEX_SHAKE_BUILD_COUNTER
                       Persistent per-host local build counter (default
                       $XDG_STATE_HOME/codex-shake/local-build-counter, or
                       ~/.local/state/codex-shake/local-build-counter).
  CODEX_SHAKE_BIN_DIR  Directory holding the codex-shake wrappers (default
                       ~/.local/bin); only checked, never rewritten.
  RUSTY_V8_ARCHIVE, RUSTY_V8_SRC_BINDING_PATH
                       Use these verified rusty_v8 files instead of downloading.
  CODEX_SKIP_BWRAP_BUILD
                       Linux: skip building bwrap and reuse the installed one.
EOF
}

say() { printf 'shake-local-install: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }
quote() { printf '%q' "$1"; }

commitish="" version="" repo="" work_dir="" keep_worktree=false activate=true
host="" remote_repo="" dry_run=false
while (($# > 0)); do
  case "$1" in
    --version) version=${2:?--version requires a value}; shift 2 ;;
    --repo) repo=${2:?--repo requires a value}; shift 2 ;;
    --work-dir) work_dir=${2:?--work-dir requires a value}; shift 2 ;;
    --keep-worktree) keep_worktree=true; shift ;;
    --no-activate) activate=false; shift ;;
    --host) host=${2:?--host requires a value}; shift 2 ;;
    --remote-repo) remote_repo=${2:?--remote-repo requires a value}; shift 2 ;;
    --dry-run) dry_run=true; shift ;;
    -h|--help) usage; exit 0 ;;
    -*) usage >&2; die "unknown option: $1" ;;
    *)
      [[ -z "$commitish" ]] || die "unexpected argument: $1"
      commitish=$1
      shift
      ;;
  esac
done
[[ -n "$commitish" ]] || { usage >&2; exit 2; }

script_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
own_helper="$script_root/.github/scripts/fork-release-build.sh"
[[ -f "$own_helper" ]] || die "missing $own_helper next to this script"
if [[ -z "$repo" ]]; then
  repo=$(git -C "$script_root" rev-parse --show-toplevel 2>/dev/null) ||
    die "$script_root is not a git checkout; pass --repo"
fi

# ---- --host: run this script on another machine -----------------------------
# Quoted values expand here on purpose and are re-quoted for the remote shell.
# shellcheck disable=SC2029
if [[ -n "$host" ]]; then
  if [[ -z "$remote_repo" ]]; then
    # Default to the main checkout's path relative to $HOME, not a worktree's.
    main_checkout=$(cd "$repo" && cd "$(git rev-parse --git-common-dir)/.." && pwd)
    case "$main_checkout" in
      "$HOME"/*) remote_repo=${main_checkout#"$HOME"/} ;;
      *) die "$main_checkout is not under \$HOME; pass --remote-repo" ;;
    esac
  fi
  # Send the full SHA so the remote builds exactly what this checkout names.
  sha=$(git -C "$repo" rev-parse --verify --quiet "$commitish^{commit}" || printf '%s' "$commitish")
  forward=(--repo "$remote_repo")
  [[ -n "$version" ]] && forward+=(--version "$version")
  [[ -n "$work_dir" ]] && forward+=(--work-dir "$work_dir")
  $keep_worktree && forward+=(--keep-worktree)
  $activate || forward+=(--no-activate)
  $dry_run && forward+=(--dry-run)
  forward+=("$sha")
  remote_args=""
  for arg in "${forward[@]}"; do
    remote_args="$remote_args $(quote "$arg")"
  done
  ssh_tty=()
  [[ -t 0 && -t 1 ]] && ssh_tty=(-t)
  rdir=$(ssh "$host" 'mktemp -d "${TMPDIR:-/tmp}/shake-local-install.XXXXXX"')
  [[ -n "$rdir" ]] || die "could not create a temporary directory on $host"
  if ! tar -C "$script_root" -cf - scripts/shake-local-install.sh .github/scripts/fork-release-build.sh .github/scripts/fork-build-identity.py |
    ssh "$host" "tar -C $(quote "$rdir") -xf -"; then
    ssh "$host" "rm -rf $(quote "$rdir")" 2>/dev/null || true
    die "could not copy this script to $host:$rdir"
  fi
  say "running on $host from $remote_repo"
  # The remote command removes $rdir itself; a dropped session (ssh exits
  # 255) skips that, so retry the removal here before passing on the status.
  rc=0
  ssh ${ssh_tty[@]+"${ssh_tty[@]}"} "$host" \
    "bash $(quote "$rdir/scripts/shake-local-install.sh")$remote_args; rc=\$?; rm -rf $(quote "$rdir"); exit \$rc" ||
    rc=$?
  if ((rc == 255)); then
    ssh "$host" "rm -rf $(quote "$rdir")" 2>/dev/null || true
  fi
  exit "$rc"
fi

# Non-interactive ssh sessions often lack the toolchain directories.
for dir in /opt/homebrew/bin "$HOME/.cargo/bin"; do
  if [[ -d "$dir" ]]; then
    case ":$PATH:" in *":$dir:"*) ;; *) PATH="$dir:$PATH" ;; esac
  fi
done
export PATH
need git; need rustc; need curl; need tar
$dry_run || need cargo

repo=$(cd "$repo" 2>/dev/null && git rev-parse --show-toplevel) || die "$repo is not a git checkout"
home_dir="${CODEX_SHAKE_HOME:-$HOME/.local/share/codex-shake}"
bin_dir="${CODEX_SHAKE_BIN_DIR:-$HOME/.local/bin}"

# ---- resolve commit, version, and tag ----------------------------------------
if ! sha=$(git -C "$repo" rev-parse --verify --quiet "$commitish^{commit}"); then
  say "$commitish is not in $repo; fetching all remotes"
  git -C "$repo" fetch --quiet --all
  sha=$(git -C "$repo" rev-parse --verify --quiet "$commitish^{commit}") ||
    die "$commitish is not a commit in $repo"
fi
sha12=${sha:0:12}

if [[ -n "$version" ]]; then
  version_source="--version"
else
  version=$(bash "$own_helper" workspace-version --repo-root "$repo" --sha "$sha")
  version_source="codex-rs/Cargo.toml"
  if [[ "$version" == 0.0.0 ]]; then
    # A checkout without the newest upstream tags would quietly resolve an
    # older version. Fetch them as fork-release-metadata.sh does, but outside
    # refs/tags so `git push --tags` from this checkout never sends them.
    upstream_tags=refs/fork-release-upstream/tags
    git -C "$repo" fetch --no-tags --quiet https://github.com/openai/codex.git \
      "+refs/tags/rust-v*:$upstream_tags/rust-v*" ||
      say "warning: could not fetch upstream rust-v* tags; using local tags"
    lineage=$(bash "$own_helper" stable-lineage --repo-root "$repo" --sha "$sha" \
      --upstream-tag-namespace "$upstream_tags")
    [[ "$lineage" != none ]] ||
      die "no rust-vX.Y.Z tag is merged into $sha12; fetch upstream tags or pass --version"
    version=${lineage#rust-v}
    version_source="merged tag $lineage"
  fi
fi
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version $version is not X.Y.Z"

target=$(rustc -vV | awk '$1 == "host:" { print $2 }')
case "$target" in
  *-apple-darwin) split_debuginfo=packed ;;
  *-unknown-linux-gnu) split_debuginfo=off ;;
  *) die "unsupported host target $target (fork releases cover Linux glibc and macOS)" ;;
esac

# ---- Python (the rusty_v8 resolver needs tomllib, so 3.11+) ------------------
python=""
for candidate in python3 python3.14 python3.13 python3.12 python3.11 /opt/homebrew/bin/python3.*; do
  candidate_path=$(command -v "$candidate" 2>/dev/null) || continue
  # sys.executable sees through version-manager shims, which the python3
  # symlink made for the build steps could not.
  if python=$("$candidate_path" -c 'import sys; assert sys.version_info >= (3, 11); print(sys.executable)' 2>/dev/null) &&
    [[ -n "$python" ]]; then
    break
  fi
  python=""
done
[[ -n "$python" ]] || die "need Python 3.11 or newer (python3, python3.1x, or /opt/homebrew/bin/python3.1x)"

# Allocate once per invocation, under a process lock. A dry run only previews
# the next counter; it never reserves an identity or touches the install root.
identity_args=(--repo-root "$repo" --sha "$sha" --version "$version" --kind local
  --counter-file "${CODEX_SHAKE_BUILD_COUNTER:-${XDG_STATE_HOME:-$HOME/.local/state}/codex-shake/local-build-counter}")
$dry_run && identity_args+=(--preview)
identity=$("$python" "$script_root/.github/scripts/fork-build-identity.py" allocate "${identity_args[@]}")
tag=$(sed -n 's/^release_tag=//p' <<< "$identity")
build_revision=$(sed -n 's/^build_revision=//p' <<< "$identity")

# ---- bwrap (Linux packages bundle codex-resources/bwrap) ---------------------
bwrap_mode=none bwrap_note=""
current_tag=$(readlink "$home_dir/current" 2>/dev/null || true)
if [[ "$target" == *linux* ]]; then
  if [[ -z "${CODEX_SKIP_BWRAP_BUILD:-}" ]] &&
    command -v pkg-config >/dev/null 2>&1 && pkg-config --exists libcap; then
    bwrap_mode=build
    bwrap_note="cargo build --bin bwrap"
  else
    # Reuse the installed bwrap only when its sources match this commit.
    reason="libcap headers are not available via pkg-config"
    [[ -n "${CODEX_SKIP_BWRAP_BUILD:-}" ]] && reason="CODEX_SKIP_BWRAP_BUILD is set"
    current_bwrap="$home_dir/current/codex-resources/bwrap"
    [[ -n "$current_tag" && -x "$current_bwrap" ]] ||
      die "cannot build bwrap ($reason) and the current release ($home_dir/current) has no codex-resources/bwrap to reuse; install pkg-config and libcap-dev (sudo apt-get install pkg-config libcap-dev)"
    # Structured tags put the SHA after the 16-hex source sequence; newer
    # tags also append the rebuild number and attempt.
    current_hex=${current_tag##*.}
    if ((${#current_hex} == 28 || ${#current_hex} == 52)); then
      current_hex=${current_hex:16:12}
    elif ((${#current_hex} > 12)); then
      current_hex=${current_hex: -12}
    fi
    current_sha=$(git -C "$repo" rev-parse --verify --quiet "$current_hex^{commit}") ||
      die "cannot build bwrap ($reason) and the commit of installed release $current_tag is unknown here; install pkg-config and libcap-dev"
    git -C "$repo" diff --quiet "$current_sha" "$sha" -- codex-rs/bwrap codex-rs/vendor/bubblewrap ||
      die "cannot build bwrap ($reason) and bwrap sources changed since $current_tag; install pkg-config and libcap-dev (sudo apt-get install pkg-config libcap-dev)"
    bwrap_mode=copy
    bwrap_note="copy $current_bwrap (sources unchanged since ${current_sha:0:12}; $reason)"
  fi
fi

work_dir=${work_dir:-${XDG_CACHE_HOME:-$HOME/.cache}/codex-shake-local/$sha12}
wt="$work_dir/src"
out="$work_dir/out"
install_dir="$home_dir/$tag"
bins=(--bin codex --bin codex-code-mode-host)
[[ "$bwrap_mode" == build ]] && bins+=(--bin bwrap)
install_note=""
if [[ -x "$install_dir/codex" && -x "$install_dir/codex-code-mode-host" ]]; then
  install_note=" (already present; reused)"
elif [[ -e "$install_dir" ]]; then
  install_note=" (incomplete; replaced)"
fi

cat <<EOF
commit        $sha
version       $version ($version_source)
tag           $tag
CLI build     $build_revision
target        $target (split-debuginfo=$split_debuginfo)
python        $python
repository    $repo
worktree      $wt$($keep_worktree && printf ' (kept)' || printf ' (removed on success)')
outputs       $out
cargo         CODEX_FORK_RELEASE_TAG=$tag CODEX_FORK_SOURCE_SHA=$sha cargo build --release ${bins[*]}
bwrap         ${bwrap_note:-not packaged on $target}
install       $install_dir$install_note
current       ${current_tag:-<none>}$($activate && printf ' -> %s' "$tag" || printf ' (unchanged; --no-activate)')
EOF
if $dry_run; then
  exit 0
fi
[[ ! -e "$install_dir" ]] ||
  die "allocated install path $install_dir already exists; preserve the persistent build counter"

# ---- build -------------------------------------------------------------------
mkdir -p "$work_dir"
# Keep the lock on the shell's open file description through packaging and
# cleanup. fcntl is available on both Linux and macOS; flock(1) is not.
exec 9>"$work_dir/.lock"
"$python" - "$work_dir" <<'PY' || exit 1
import fcntl
import sys

try:
    fcntl.flock(9, fcntl.LOCK_EX | fcntl.LOCK_NB)
except BlockingIOError:
    sys.exit(f"another build owns {sys.argv[1]}; wait for it or pass --work-dir")
PY
mkdir -p "$out"
if [[ -e "$wt" ]]; then
  [[ "$(git -C "$wt" rev-parse HEAD 2>/dev/null)" == "$sha" ]] ||
    die "$wt exists but is not a worktree at $sha12; remove it or pass --work-dir"
  # A failed Cargo build leaves only the version stamp and its lockfile
  # updates. Accept those generated changes, but never claim the source SHA
  # for a cached worktree containing additional source or dependency edits.
  "$python" - "$wt" "$sha" "$version" <<'PY' ||
import pathlib
import subprocess
import sys
import tomllib

root, sha, version = sys.argv[1:]

def git(*args):
    return subprocess.check_output(["git", "-C", root, *args], text=True)

changed = set(git("diff", "--name-only", sha).splitlines())
allowed = {"codex-rs/Cargo.toml", "codex-rs/Cargo.lock"}
if changed - allowed or git("ls-files", "--others", "--exclude-standard").strip():
    sys.exit("cached build worktree contains source edits")
for name in changed:
    original = tomllib.loads(git("show", f"{sha}:{name}"))
    actual = tomllib.loads((pathlib.Path(root) / name).read_text())
    if name.endswith("Cargo.toml"):
        if original["workspace"]["package"]["version"] == "0.0.0":
            original["workspace"]["package"]["version"] = version
    else:
        local_names = {
            package["name"] for package in original["package"]
            if package["version"] == "0.0.0" and "source" not in package
        }
        replacements = {f"{name} 0.0.0": f"{name} {version}" for name in local_names}
        for package in original["package"]:
            if package["name"] in local_names and "source" not in package:
                package["version"] = version
            if "dependencies" in package:
                package["dependencies"] = [replacements.get(dep, dep) for dep in package["dependencies"]]
    if actual != original:
        sys.exit(f"cached build worktree has unexpected changes in {name}")
PY
    die "preserving cached worktree $wt; use --work-dir for a separate build"
  say "resuming in existing worktree $wt"
else
  # A worktree deleted by hand is still registered until pruned.
  git -C "$repo" worktree prune
  git -C "$repo" worktree add --detach "$wt" "$sha"
  # A fresh worktree builds unstripped binaries, so outputs left by an
  # earlier run of this commit must not make the steps below skip them.
  rm -rf "$out/codex-symbols-$target.tar.gz" "$out/codex-$target.tar.gz" \
    "$out/codex-package-$target" "$out/runner-tmp"
fi
on_exit() {
  local status=$?
  if [[ $status -ne 0 ]]; then
    say "failed (exit $status); worktree kept at $wt, rerun the same command to resume"
  fi
}
trap on_exit EXIT
log="$out/build.log"
say "logging to $log"
exec > >(tee -a "$log" 9>&-) 2>&1

# Allocation, verification, and packaging all use this invocation's helper,
# matching CI's helper checkout from the workflow commit.
helper=$own_helper

# The release steps call `python3`; point it at the interpreter chosen above.
mkdir -p "$out/python-shim"
ln -sfn "$python" "$out/python-shim/python3"
PATH="$out/python-shim:$PATH"

# rusty_v8: run the setup-rusty-v8 action's own script so the download and
# checksum checks stay identical to the workflow.
if [[ -z "${RUSTY_V8_ARCHIVE:-}" || -z "${RUSTY_V8_SRC_BINDING_PATH:-}" ]]; then
  action="$wt/.github/actions/setup-rusty-v8/action.yml"
  v8_script=$(awk '/^ *run: \|$/ { body = 1; next } body' "$action" | sed 's/^        //')
  grep -q 'GITHUB_ENV' <<<"$v8_script" || die "could not read the run script from $action"
  : >"$out/rusty-v8.env"
  GITHUB_WORKSPACE="$wt" RUNNER_TEMP="$out" GITHUB_ENV="$out/rusty-v8.env" TARGET="$target" \
    bash -c "$v8_script"
  RUSTY_V8_ARCHIVE=$(sed -n 's/^RUSTY_V8_ARCHIVE=//p' "$out/rusty-v8.env")
  RUSTY_V8_SRC_BINDING_PATH=$(sed -n 's/^RUSTY_V8_SRC_BINDING_PATH=//p' "$out/rusty-v8.env")
fi
[[ -s "$RUSTY_V8_ARCHIVE" && -s "$RUSTY_V8_SRC_BINDING_PATH" ]] || die "rusty_v8 artifacts are missing"
export RUSTY_V8_ARCHIVE RUSTY_V8_SRC_BINDING_PATH

cd "$wt/codex-rs"
bash "$helper" stamp-version "$version"

export CARGO_TARGET_DIR="$wt/codex-rs/target"
export CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO="$split_debuginfo"
[[ "$bwrap_mode" == copy ]] && export CODEX_SKIP_BWRAP_BUILD=1
say "cargo build --release ${bins[*]}"
# Build tools may start persistent compiler-cache daemons; do not let those
# descendants retain the work-directory lock after this script exits.
CODEX_FORK_RELEASE_TAG="$tag" CODEX_FORK_SOURCE_SHA="$sha" cargo build --release "${bins[@]}" 9>&-
bash "$helper" verify-version "$version" "$tag" "$sha"

# ---- package -----------------------------------------------------------------
# Every invocation has a fresh identity. Never reuse a package from a previous
# invocation, including when --work-dir reuses the source/build directory.
rm -rf "$out/codex-symbols-$target.tar.gz" "$out/codex-$target.tar.gz" \
  "$out/codex-package-$target" "$out/runner-tmp"
if [[ ! -s "$out/codex-symbols-$target.tar.gz" ]]; then
  mkdir -p "$out/runner-tmp"
  RUNNER_TEMP="$out/runner-tmp" bash "$wt/.github/scripts/archive-release-symbols-and-strip-binaries.sh" \
    --target "$target" --artifact-name "$target" --release-dir target/release \
    --archive-dir "$out" --binaries "codex codex-code-mode-host"
fi

package_args=(
  --repo-root "$wt"
  --target "$target"
  --version "$version"
  --stage-dir "$out/codex-package-$target"
  --archive "$out/codex-$target.tar.gz"
)
case "$bwrap_mode" in
  build) package_args+=(--bwrap-bin target/release/bwrap) ;;
  copy)
    cp "$home_dir/current/codex-resources/bwrap" "$out/bwrap"
    package_args+=(--bwrap-bin "$out/bwrap")
    ;;
esac
rm -rf "$out/codex-package-$target"
CODEX_REPO_ROOT="$wt" bash "$helper" package-archive "${package_args[@]}"
archive="$out/codex-$target.tar.gz"

# ---- install like install.sh -------------------------------------------------
if [[ -x "$install_dir/codex" && -x "$install_dir/codex-code-mode-host" ]]; then
  say "$tag already installed; leaving it as is"
else
  mkdir -p "$home_dir"
  # An incomplete $install_dir can only be an earlier try at this same tag;
  # mv would nest the new copy inside it.
  rm -rf "$install_dir" "$install_dir.partial"
  mkdir -p "$install_dir.partial"
  tar -xzf "$archive" -C "$install_dir.partial"
  chmod 755 "$install_dir.partial/codex" "$install_dir.partial/codex-code-mode-host"
  if [[ "$target" == *apple-darwin ]] && command -v xattr >/dev/null 2>&1; then
    xattr -dr com.apple.quarantine "$install_dir.partial" 2>/dev/null || true
  fi
  mv "$install_dir.partial" "$install_dir"
fi
"$install_dir/codex" --version

if $activate; then
  ln -sfn "$tag" "$home_dir/current"
  printf '{\n  "oldTarget": "%s",\n  "newTarget": "%s",\n  "sourceCommit": "%s",\n  "activatedAtUtc": "%s"\n}\n' \
    "$current_tag" "$tag" "$sha12" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$home_dir/last-local-deploy.json"
  say "current: ${current_tag:-<none>} -> $(readlink "$home_dir/current")"
fi
[[ -x "$bin_dir/codex-shake" ]] ||
  say "note: $bin_dir/codex-shake is missing; run install.sh once to create the codex-shake wrappers"

# ---- clean up ----------------------------------------------------------------
cd "$repo"
rm -rf "$out/codex-package-$target" "$out/runner-tmp" "$out/rusty_v8" "$out/python-shim" "$out/bwrap"
if $keep_worktree; then
  say "kept worktree $wt"
else
  git -C "$repo" worktree remove --force "$wt"
fi
say "done: $tag (archive, symbols, and build.log in $out)"
