#!/bin/sh
# Install the latest main-branch test build of Codex as `codex-shake`.
#
#   curl -fsSL https://raw.githubusercontent.com/epsalmond/codex/main/install.sh | sh
#
# Also available via Homebrew (macOS Apple Silicon, or Linux x86_64):
#   brew install epsalmond/codex-shake/codex-shake
# ...or as a .deb on Debian/Ubuntu x86_64 (see the release assets for the
# exact filename, then `sudo dpkg -i codex-shake_<version>_amd64.deb`).
#
# Rerun to update. Installs beside the official `codex`, never over it:
#   ~/.local/share/codex-shake/<tag>/   Codex package (codex-package.json, bin/,
#                                       codex-path/, codex-resources/) plus
#                                       codex -> bin/codex
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

# Capture the installed baseline before current is repointed. The symlink is
# installer-owned; no running session or mutable version output is consulted.
previous_tag=""
if [ -x "$home_dir/current/codex" ] && command -v readlink >/dev/null 2>&1; then
  previous_target=$(readlink "$home_dir/current" 2>/dev/null || true)
  previous_tag=${previous_target##*/}
fi

# Notes are advisory: use at most two bounded API lookups for the source range,
# including skipped builds. Without a JSON parser, provide honest browser links
# rather than trying to parse arbitrary JSON/release bodies as shell text.
show_update_notes() {
  notes_url="https://github.com/$repo/releases/tag/$tag"
  if [ "$previous_tag" = "$tag" ]; then
    say "already current: $tag"
    say "release notes: $notes_url"
    return 0
  fi
  if [ -z "$previous_tag" ]; then
    say 'previous release baseline unavailable (new or unmanaged installation)'
    say "release notes: $notes_url"
    return 0
  fi
  compare_url="https://github.com/$repo/compare/$previous_tag...$tag"
  say "changes: $previous_tag -> $tag (including skipped builds)"
  say "full comparison: $compare_url"
  say "target release notes: $notes_url"
  if ! command -v jq >/dev/null 2>&1; then
    say 'changelog summary unavailable without jq; use the comparison link above'
    return 0
  fi
  # jq encodes URL path data; none of the returned text is evaluated as code.
  previous_ref=$(printf '%s' "$previous_tag" | jq -sRr @uri) || return 0
  target_ref=$(printf '%s' "$tag" | jq -sRr @uri) || return 0
  compare_api="https://api.github.com/repos/$repo/compare/$previous_ref...$target_ref?per_page=100"
  if ! changes=$(curl -fsSL --connect-timeout 5 --max-time 15 \
    -H 'Accept: application/vnd.github+json' \
    "$compare_api"); then
    say 'changelog summary unavailable; installation succeeded; use the links above'
    return 0
  fi
  if ! last_page=$(printf '%s' "$changes" | jq -er '
    if (.commits | type) != "array" or (.total_commits | type) != "number" or
       .total_commits < 0 or (.total_commits | floor) != .total_commits or
       (.status != "ahead" and .status != "identical" and .status != "behind" and .status != "diverged") then
      error("invalid comparison")
    elif .status == "ahead" then ([1, ((.total_commits + 99) / 100 | floor)] | max)
    else 1 end' 2>/dev/null); then
    say 'changelog summary unavailable; installation succeeded; use the links above'
    return 0
  fi
  total_commits=$(printf '%s' "$changes" | jq -r .total_commits)
  if [ "$last_page" -gt 1 ]; then
    if ! changes=$(curl -fsSL --connect-timeout 5 --max-time 15 \
      -H 'Accept: application/vnd.github+json' "$compare_api&page=$last_page"); then
      say 'newest changelog summary unavailable; installation succeeded; use the links above'
      return 0
    fi
  fi
  if ! summary=$(printf '%s' "$changes" | jq -er --argjson total "$total_commits" --argjson page "$last_page" '
    if (.commits | type) != "array" or .total_commits != $total or
       ($page > 1 and .status != "ahead") or
       (.status == "ahead" and (.commits | length) != ($total - 100 * ($page - 1))) then
      error("invalid final comparison page")
    elif .status == "identical" then "No source changes (rebuild of the same source)."
    elif .status != "ahead" then
      "Source histories are " + .status + "; see the full comparison for rollback or divergence."
    else
      (.commits[-12:] | reverse) as $recent |
      ($recent[] | "  - " + (.commit.message | split("\n")[0])),
      "Showing " + ($recent | length | tostring) + " of " + (.total_commits | tostring) + " commits (newest first); see the full comparison for all changes."
    end' 2>/dev/null); then
    say 'changelog summary unavailable; installation succeeded; use the links above'
    return 0
  fi
  printf '%s\n' "$summary" | LC_ALL=C tr -d '\000-\010\013-\037\177' >&2
}

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
  # GitHub's list order is publication order, not source order. Keep this
  # selector consistent with tui/src/fork_update.rs: normalize legacy
  # minute counters, accept second counters, and order generated same-second
  # tags by their fixed-width ancestry sequence before the producer SHA.
  release_feed=$(curl -fsSL -H 'Accept: application/vnd.github+json' "$api") || \
    die "could not fetch releases for $repo"
  if command -v jq >/dev/null 2>&1; then
    release_tags=$(printf '%s\n' "$release_feed" |
      jq -r '.[] | select(.draft != true) | .tag_name' 2>/dev/null || true)
  else
    release_tags=$(printf '%s\n' "$release_feed" |
      awk -v prefix="$tag_prefix" -v RS='}' '
        {
          record = $0
          if (record !~ /"tag_name"[[:space:]]*:/ ||
              record ~ /"draft"[[:space:]]*:[[:space:]]*true/) next
          tag = record
          sub(/^.*"tag_name"[[:space:]]*:[[:space:]]*"/, "", tag)
          sub(/".*$/, "", tag)
          if (substr(tag, 1, length(prefix)) == prefix) print tag
        }
      ' || true)
  fi
  tag=$(
    printf '%s\n' "$release_tags" |
      awk -v prefix="$tag_prefix" '
        function is_digits(value, i, character) {
          if (value == "") return 0
          for (i = 1; i <= length(value); i++) {
            character = substr(value, i, 1)
            if (character < "0" || character > "9") return 0
          }
          return 1
        }
        function is_hex(value, i, character) {
          if (value == "") return 0
          for (i = 1; i <= length(value); i++) {
            character = tolower(substr(value, i, 1))
            if (character !~ /^[0-9a-f]$/) return 0
          }
          return 1
        }
        function valid_version(value, count, parts, i) {
          if (substr(value, 1, 1) != "v") return 0
          value = substr(value, 2)
          if (substr(value, length(value) - 4) == "-main") {
            value = substr(value, 1, length(value) - 5)
          }
          count = split(value, parts, ".")
          if (count != 3) return 0
          for (i = 1; i <= count; i++) {
            if (!is_digits(parts[i])) return 0
          }
          return 1
        }
        {
          tag = $0
          if (substr(tag, 1, length(prefix)) != prefix) next
          rest = substr(tag, length(prefix) + 1)
          marker = index(rest, "-r")
          if (marker > 0) {
            version = substr(rest, 1, marker - 1)
            tail = substr(rest, marker + 2)
            dot = index(tail, ".")
            digits = substr(tail, 1, dot - 1)
            suffix = substr(tail, dot + 1)
            if (dot <= 1 || length(digits) > 14 || !valid_version(version) ||
                !is_digits(digits) || !is_hex(suffix)) next
            normalized = digits
            precision = length(digits)
            if (precision == 12) normalized = digits "00"
            source_sequence = ""
            if (length(suffix) == 28 || length(suffix) == 52) source_sequence = tolower(substr(suffix, 1, 16))
            rebuild_sequence = "0000000000000000"
            rebuild_attempt = "00000000"
            if (length(suffix) == 52) {
              rebuild_sequence = tolower(substr(suffix, 29, 16))
              rebuild_attempt = tolower(substr(suffix, 45, 8))
            }
            printf "%020d\t%02d\t%s\t%s\t%s\t%s:%s\t%s\n", normalized + 0, precision,
              source_sequence, rebuild_sequence, rebuild_attempt, tolower(version), tolower(suffix), tag
            next
          }
          dash = 0
          for (i = length(rest); i > 0; i--) {
            if (substr(rest, i, 1) == "-") {
              dash = i
              break
            }
          }
          if (dash <= 1) next
          version = substr(rest, 1, dash - 1)
          suffix = substr(rest, dash + 1)
          if (valid_version(version) && length(suffix) == 12 && is_hex(suffix)) {
            printf "%020d\t%02d\t%s\t%s\t%s\t%s:%s\t%s\n", 0, 0, "", "", "",
              tolower(version), tolower(suffix), tag
          }
        }
      ' |
      LC_ALL=C sort -t '	' -k1,1n -k2,2n -k3,3 -k4,4 -k5,5 -k6,6 |
      tail -n 1 |
      cut -f7-
  )
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
  # Package-layout releases ship bin/, codex-path/, and codex-resources/
  # with codex and codex-code-mode-host as root symlinks into bin/; older
  # releases are a flat set of files. chmod follows the symlinks either way.
  chmod 755 "$tmp/unpack/codex" "$tmp/unpack/codex-code-mode-host"
  if [ "$os" = apple-darwin ] && command -v xattr >/dev/null 2>&1; then
    xattr -dr com.apple.quarantine "$tmp/unpack" 2>/dev/null || true
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
# codex-shake: main-branch test build. Reinstall/update with install.sh.
CODEX_SHAKE_REPO=$repo_q
CODEX_SHAKE_HOME=$home_q
CODEX_SHAKE_BIN_DIR=$bin_q
export CODEX_SHAKE_REPO CODEX_SHAKE_HOME CODEX_SHAKE_BIN_DIR
# Work around https://github.com/openai/codex/issues/48068 for Ghostty.
if [ "\${TERM:-}" = xterm-ghostty ] && [ -z "\${COLORTERM:-}" ]; then
    COLORTERM=truecolor
fi
export COLORTERM
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
show_update_notes || say 'changelog unavailable; installation succeeded'
say 'run: codex-shake'
say 'update: codex-shake-update'
