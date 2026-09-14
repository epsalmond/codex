#!/bin/sh
# Install the latest eric/local-features test build of Codex as `codex-shake`.
#
#   curl -fsSL https://raw.githubusercontent.com/epsalmond/codex/eric/local-features/install.sh | sh
#
# Rerun to update. Installs beside the official `codex`, never over it:
#   ~/.local/share/codex-shake/<tag>/   codex + codex-code-mode-host (real binaries)
#   ~/.local/share/codex-shake/current  -> <tag>
#   ~/.local/bin/codex-shake            wrapper that execs current/codex
#
# Env overrides: CODEX_SHAKE_REPO (owner/repo), CODEX_SHAKE_TAG (pin a release),
#                CODEX_SHAKE_BIN_DIR (wrapper dir), CODEX_SHAKE_HOME (install root).
set -eu

repo="${CODEX_SHAKE_REPO:-epsalmond/codex}"
home_dir="${CODEX_SHAKE_HOME:-$HOME/.local/share/codex-shake}"
bin_dir="${CODEX_SHAKE_BIN_DIR:-$HOME/.local/bin}"
tag_prefix="local-features-"

say() { printf 'codex-shake: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }

# Render one shell word without allowing an install path or repository name to
# become shell syntax in one of the generated wrappers. The generated scripts
# are deliberately self-contained so updates keep using the same effective
# locations even when the caller's environment changes later.
shell_quote() {
  escaped=$(printf '%s' "$1" | sed "s/'/'\\\\''/g")
  printf "'%s'" "$escaped"
}
need curl; need tar

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-gnu ;;
  *) die "unsupported OS: $(uname -s)" ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch=aarch64 ;;
  x86_64|amd64) arch=x86_64 ;;
  *) die "unsupported CPU: $(uname -m)" ;;
esac
target="$arch-$os"

if command -v sha256sum >/dev/null 2>&1; then
  sha() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die 'need sha256sum or shasum'
fi

# ---- pick the release -------------------------------------------------------
api="https://api.github.com/repos/$repo/releases?per_page=30"
if [ -n "${CODEX_SHAKE_TAG:-}" ]; then
  tag="$CODEX_SHAKE_TAG"
else
  # Releases come back newest first; take the first tag with our prefix.
  tag=$(curl -fsSL -H 'Accept: application/vnd.github+json' "$api" \
    | grep -o "\"tag_name\": *\"${tag_prefix}[^\"]*\"" \
    | head -n1 | sed 's/.*"\('"$tag_prefix"'[^"]*\)"/\1/')
  [ -n "$tag" ] || die "no $tag_prefix* release found in $repo"
fi
base="https://github.com/$repo/releases/download/$tag"
asset="codex-$target.tar.gz"

install_dir="$home_dir/$tag"
if [ -x "$install_dir/codex" ] && [ -x "$install_dir/codex-code-mode-host" ]; then
  say "$tag already installed"
else
  tmp=$(mktemp -d "${TMPDIR:-/tmp}/codex-shake.XXXXXX")
  trap 'rm -rf "$tmp"' EXIT INT TERM
  say "downloading $asset from $tag"
  curl -fL --progress-bar -o "$tmp/$asset" "$base/$asset" || \
    die "no asset $asset in release $tag (unsupported platform for this build?)"
  curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"
  expected=$(grep " $asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
  [ -n "$expected" ] || die "SHA256SUMS has no entry for $asset"
  actual=$(sha "$tmp/$asset")
  [ "$actual" = "$expected" ] || die "checksum mismatch for $asset"
  mkdir -p "$tmp/unpack"
  tar -xzf "$tmp/$asset" -C "$tmp/unpack"
  if [ ! -f "$tmp/unpack/codex" ] || [ ! -f "$tmp/unpack/codex-code-mode-host" ]; then
    die 'tarball did not contain codex and codex-code-mode-host'
  fi
  chmod 755 "$tmp/unpack/codex" "$tmp/unpack/codex-code-mode-host"
  if [ "$os" = apple-darwin ] && command -v xattr >/dev/null 2>&1; then
    xattr -d com.apple.quarantine "$tmp/unpack/codex" "$tmp/unpack/codex-code-mode-host" 2>/dev/null || true
  fi
  mkdir -p "$home_dir"
  rm -rf "$install_dir.partial"
  mv "$tmp/unpack" "$install_dir.partial"
  mv "$install_dir.partial" "$install_dir"
fi

# ---- point current + wrapper at it ------------------------------------------
# -n replaces an existing symlink instead of descending into its target.
ln -sfn "$tag" "$home_dir/current"

mkdir -p "$bin_dir"
wrapper="$bin_dir/codex-shake"
repo_q=$(shell_quote "$repo")
home_q=$(shell_quote "$home_dir")
bin_q=$(shell_quote "$bin_dir")
current_codex_q=$(shell_quote "$home_dir/current/codex")
cat > "$wrapper.tmp.$$" <<EOF
#!/bin/sh
# codex-shake: eric/local-features test build. Reinstall/update with install.sh.
CODEX_SHAKE_REPO=$repo_q
CODEX_SHAKE_HOME=$home_q
CODEX_SHAKE_BIN_DIR=$bin_q
export CODEX_SHAKE_REPO CODEX_SHAKE_HOME CODEX_SHAKE_BIN_DIR
dir=$(shell_quote "$home_dir/current")
PATH="\$dir:\$PATH" exec "\$dir/codex" "\$@"
EOF
chmod 755 "$wrapper.tmp.$$"
mv -f "$wrapper.tmp.$$" "$wrapper"

# Keep the convenience updater beside the launcher. It delegates to the
# installed binary's existing update implementation, so release lookup and
# comparison stay in one place. Persist the install settings in the generated
# environment assignments; shell quoting keeps spaces and metacharacters inert.
update_helper="$bin_dir/codex-shake-update"
cat > "$update_helper.tmp.$$" <<EOF
#!/bin/sh
# codex-shake-update: check and install the latest codex-shake release.
CODEX_SHAKE_REPO=$repo_q
CODEX_SHAKE_HOME=$home_q
CODEX_SHAKE_BIN_DIR=$bin_q
export CODEX_SHAKE_REPO CODEX_SHAKE_HOME CODEX_SHAKE_BIN_DIR
exec $current_codex_q update --yes "\$@"
EOF
chmod 755 "$update_helper.tmp.$$"
mv -f "$update_helper.tmp.$$" "$update_helper"

# Some releases also carry the offline savings estimator. Expose it through a
# PATH wrapper only when the complete payload is present; older releases keep
# working and never advertise a helper that would fail at runtime. If a
# previous managed wrapper points at a payload that is no longer installed,
# remove only that wrapper and leave any user-owned file alone.
estimator_wrapper="$bin_dir/codex-shake-estimate"
if [ -x "$home_dir/current/codex-shake-estimate" ] &&
  [ -f "$home_dir/current/shake-savings-estimate.py" ] &&
  [ -f "$home_dir/current/pricing_lib.py" ] &&
  [ -f "$home_dir/current/pricing.json" ]; then
  estimator_dir_q=$(shell_quote "$home_dir/current")
  cat > "$estimator_wrapper.tmp.$$" <<EOF
#!/bin/sh
# codex-shake-estimate: offline savings estimator bundled with this release.
exec $estimator_dir_q/codex-shake-estimate "\$@"
EOF
  chmod 755 "$estimator_wrapper.tmp.$$"
  mv -f "$estimator_wrapper.tmp.$$" "$estimator_wrapper"
elif [ -f "$estimator_wrapper" ] &&
  grep -q '^# codex-shake-estimate: offline savings estimator bundled with this release\.$' "$estimator_wrapper";
then
  rm -f "$estimator_wrapper"
fi

version=$("$home_dir/current/codex" --version 2>/dev/null || true)
say "installed $tag -> $wrapper (${version:-version check failed})"
case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) say "note: $bin_dir is not on your PATH; add it or run $wrapper directly" ;;
esac
say 'run: codex-shake'
say 'update: codex-shake-update'
