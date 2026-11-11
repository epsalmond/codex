#!/usr/bin/env bash
# Execute the publisher's real shell steps offline, including older source
# helpers, distinct Linux glibc versions, and missing ARM artifact failures.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
workflow="$script_dir/../workflows/fork-release.yml"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Extract a named step's literal run block without a third-party YAML parser.
step() {
  awk -v name="$1" '
    /^      - / { selected = ($0 == "      - name: " name); run = 0 }
    selected && /^        run: \|$/ { run = 1; next }
    run && /^          / { print substr($0, 11) }
  ' "$workflow"
}

mkdir -p "$work/.github/scripts" "$work/fork-release-tools/.github/scripts" \
  "$work/stubs" "$work/dist" "$work/payload" "$work/codex-rs/tui/src"
cp "$script_dir/build-fork-deb.sh" "$script_dir/fork-release-baseline.py" "$work/.github/scripts/"
cp "$script_dir/fork-release-notes.sh" "$script_dir/render-brew-formula.sh" \
  "$work/fork-release-tools/.github/scripts/"
# Fail if a dispatch against old source accidentally uses its stale renderers.
printf '#!/bin/bash\nexit 99\n' > "$work/.github/scripts/fork-release-notes.sh"
cp "$work/.github/scripts/fork-release-notes.sh" "$work/.github/scripts/render-brew-formula.sh"
printf 'pub(crate) const SHAKE_FEATURE_VERSION: &str = "0.4.0";\n' > "$work/codex-rs/tui/src/fork_update.rs"
printf '#!/bin/sh\necho fixture\n' > "$work/payload/codex"
cp "$work/payload/codex" "$work/payload/codex-code-mode-host"

cd "$work"
git init -q
git -c user.name=Test -c user.email=test@example.com add .
git -c user.name=Test -c user.email=test@example.com commit -qm source
export RELEASE_SHA
RELEASE_SHA=$(git rev-parse HEAD)
export GITHUB_WORKSPACE="$work" FORK_REPO=epsalmond/codex FORK_BRANCH=main
export RELEASE_TAG="local-features-v0.167.1-main-r20261111000000.abcdef123456"
export SOURCE_VERSION=0.167.1 STABLE_LINEAGE=none INCLUDED_UPSTREAM_MAIN_SHA=none
export SOURCE_KIND=branch-merge GH_TOKEN=offline BUILD_REVISION=fixture
export GITHUB_OUTPUT="$work/outputs" FORK_DEB_SKIP_SHLIBDEPS=1
export PATH="$work/stubs:$PATH"
cat > stubs/gh <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$1" == api && "$2" == --paginate ]]; then
  printf '[]\n'
elif [[ "$1" == api && "$2" == repos/*/git/ref/tags/* ]]; then
  printf 'commit\t%s\n' "$RELEASE_SHA"
elif [[ "$1 $2" == 'release create' ]]; then
  printf '%s\n' "$@" > "$GITHUB_WORKSPACE/published-args"
else
  echo "Unexpected gh invocation: $*" >&2
  exit 1
fi
STUB
chmod +x stubs/gh

targets=(x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu aarch64-apple-darwin)
for target in "${targets[@]}"; do
  printf '%s\n' "$target" > payload/target
  tar -C payload -czf "dist/codex-$target.tar.gz" codex codex-code-mode-host target
  tar -C payload -czf "dist/codex-symbols-$target.tar.gz" codex
done
ldd --version | awk 'NR == 1 { print $NF }' > dist/glibc-version-x86_64-unknown-linux-gnu.txt
printf '9.99\n' > dist/glibc-version-aarch64-unknown-linux-gnu.txt
bash -c "$(step 'Prepare amd64 glibc metadata')"
cmp dist/glibc-version-x86_64-unknown-linux-gnu.txt dist/glibc-version.txt
bash -c "$(step 'Build .deb package')"
export DEB_NAME
DEB_NAME=$(sed -n 's/^deb_name=//p' "$GITHUB_OUTPUT")
[[ "$(dpkg-deb -f "dist/$DEB_NAME" Architecture)" == amd64 ]]
[[ "$(dpkg-deb -f "dist/$DEB_NAME" Depends)" == "libc6 (>= $(cat dist/glibc-version.txt))" ]]

verify=$(step 'Verify artifacts and compose release body')
bash -c "$verify"
expected=()
for target in "${targets[@]}"; do
  expected+=("codex-$target.tar.gz" "codex-symbols-$target.tar.gz")
done
expected+=("$DEB_NAME")
diff -u <(printf '%s\n' "${expected[@]}" | sort) <(awk '{print $2}' dist/SHA256SUMS | sort)
grep -Fq 'codex-aarch64-unknown-linux-gnu.tar.gz) (glibc 9.99+)' release-body.md

for missing in codex-aarch64-unknown-linux-gnu.tar.gz \
  codex-symbols-aarch64-unknown-linux-gnu.tar.gz glibc-version-aarch64-unknown-linux-gnu.txt; do
  mv "dist/$missing" "$work/missing"
  if bash -c "$verify" > "$work/negative.log" 2>&1; then
    echo "publisher accepted missing $missing" >&2
    exit 1
  fi
  mv "$work/missing" "dist/$missing"
done
# Recreate the manifest after the negative checks, then check exact upload args.
bash -c "$verify"
bash -c "$(step 'Create immutable release after identity verification')"
diff -u <(printf 'dist/%s\n' "${expected[@]}" SHA256SUMS | sort) \
  <(sed -n '/^dist\//p' published-args | sort)

# Exercise checksum selection and the actual formula caller before tap mutation.
export HOMEBREW_TAP_TOKEN=offline
bash -c "$(step 'Update Homebrew tap' | sed '/^tap_dir=/,$d')"
arm_sha=$(sha256sum dist/codex-aarch64-unknown-linux-gnu.tar.gz | cut -d ' ' -f1)
grep -Fq "sha256 \"$arm_sha\"" rendered-codex-shake.rb
grep -Fq '/codex-aarch64-unknown-linux-gnu.tar.gz"' rendered-codex-shake.rb

# Both updated renderers must come from a workflow-commit checkout in publish.
publish_job=$(sed -n '/^  publish:$/,$p' "$workflow")
# shellcheck disable=SC2016 # GitHub expression is literal workflow text.
grep -Fq 'ref: ${{ github.sha }}' <<< "$publish_job"
grep -Fq 'path: fork-release-tools' <<< "$publish_job"
grep -Fq 'runner: ubuntu-22.04-arm' "$workflow"
# The reusable validation also runs this regression from the workflow commit,
# since candidate_sha may predate both this test and the new publisher steps.
validation="$script_dir/../workflows/fork-upstream-validation.yml"
# shellcheck disable=SC2016 # Verify the literal path used by Actions.
grep -Fq 'run: bash "$GITHUB_WORKSPACE/fork-release-tools/.github/scripts/test_fork_release_artifacts.sh"' "$validation"
grep -Fxq '            .github/workflows' "$validation"
echo 'fork-release artifact tests passed'
