# Shake release and update changes

Shake keeps a feature version (for example, `0.4.0`) separate from the build
revision. See [build identity](shake-build-identity.md) for how each binary is
identified.

## Release pages

Each new release describes source changes since the nearest published
`local-features-*` release on the source's first-parent history. Drafts, the
current target, and orphan tags from failed publication are excluded. For
multiple published releases of the same source, the latest publication is the
baseline. It lists up to eight
fork commit titles and eight upstream commit titles, with commit links and a full
comparison link. Titles come from the exact Git history being released; no model,
extra credential, or generated prose service is required. The feature guide is
linked rather than copied into every release.

Upstream commits are identified through the included upstream commit's ancestry.
The build footer links the exact upstream stable release notes when that lineage
is known. When the upstream baseline is unavailable, the release labels its list
as source changes rather than guessing origin. A build with the same source as
the preceding published release explicitly says it has no source changes.

If no valid published ancestor release is available, the release says that the
baseline is unavailable and shows a bounded recent history. It does not claim
that those commits were introduced by the release. The generator accepts an
optional twelfth argument specifying a known previous published tag. The
workflow always supplies it; an empty value or `none` explicitly disables tag
fallback. Only standalone callers omitting the argument use the legacy source
tag lookup, which does not establish publication.

## `codex-shake-update`

The updater runs the installed binary's existing update command and installer.
The installer captures the old `current` symlink before switching to the target
release, then prints:

- The installed and target tags, plus the full GitHub comparison and target
  release notes links.
- Up to twelve newest commit titles from that complete installed-to-target range when
  `jq` and the GitHub API are available. This range includes changes from builds
  skipped between updates, including upstream changes.
- The actual displayed count and total range size. For ranges over 100 commits,
  a second request fetches the final comparison page. A page boundary may leave
  fewer than twelve titles; the comparison link is always the full reference.
- An explicit message for an already current installation, a rebuild with the
  same source, a rollback/divergent range, or an unavailable installed baseline.

A new or unmanaged installation has no installed-tag baseline and receives the
target release link. On systems without `jq`, the POSIX shell installer still
works and prints the comparison/release links. API failures, rate limiting,
deleted tags, and malformed responses produce a notes-unavailable notice without
failing a valid installation. If the final page cannot be fetched or validated,
the installer provides links and an unavailable notice. Each of the at most two
advisory requests has a five-second connection timeout and fifteen-second
overall timeout.

Commit titles are displayed as data, with terminal control bytes removed. Release
page titles use code spans with safe backtick delimiters so upstream `#123`
references and `@user` mentions remain inert on the fork. They
are never sourced or evaluated. Summaries are deterministic commit subjects, not
curated feature descriptions; merge-only conflict resolutions may not appear in
the release's non-merge title list. The full source comparison and linked
upstream release notes remain authoritative.
